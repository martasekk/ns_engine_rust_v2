use crate::client::{ApiError, OpenRouterClient};
use async_trait::async_trait;
use nscore::{Replier, ReplyContext, ReplyError};

pub struct CloudReplier {
    client: OpenRouterClient,
    model: String,
    max_tokens: u32,
}

impl CloudReplier {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        Self { client, model, max_tokens: 1024 }
    }
}

fn render_context(ctx: &ReplyContext) -> String {
    let mut s = String::new();
    if !ctx.facts.is_empty() {
        s.push_str("Standing facts:\n");
        for f in &ctx.facts {
            s.push_str(&format!("- {}: {}\n", f.key, f.value));
        }
    }
    s.push_str(&format!("Session: {}\n", ctx.session_summary));
    s.push_str(&format!(
        "This turn's trace (what actually happened, including refusals):\n{}\n",
        ctx.turn_trace
    ));
    s.push_str(
        "Write the user-facing reply. Narrate ONLY what the trace supports: report \
         outcomes and refusals truthfully; do not mention entities absent from it.",
    );
    s
}

#[async_trait]
impl Replier for CloudReplier {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        let persona = if ctx.persona.is_empty() {
            "You are a helpful assistant.".to_string()
        } else {
            ctx.persona.clone()
        };
        // Spec §5: cache-aware fixed block order — persona (static, cached) →
        // facts (stable) → session summary → this turn's trace (dynamic).
        // Content-parts form lets the cache_control breakpoint pass through
        // OpenRouter to Anthropic prompt caching. Never send sampling params.
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": persona, "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": render_context(&ctx)},
            ],
        });
        let body = self.client.chat(request).await.map_err(|e| match e {
            ApiError::Transport(d) => ReplyError::Transport(d),
            ApiError::Status { status, detail } => {
                ReplyError::Transport(format!("status {status}: {detail}"))
            }
        })?;
        let text: String = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if text.is_empty() {
            return Err(ReplyError::Transport("empty reply".into()));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::OpenRouterClient;
    use crate::transport::{MockTransport, TransportError};
    use nscore::{Fact, Provenance, Timestamp};

    fn ctx() -> ReplyContext {
        ReplyContext {
            persona: "You are Tomáš, a friendly sales assistant.".into(),
            facts: vec![Fact {
                key: "user.name".into(),
                value: serde_json::json!("Martin"),
                confidence: 1.0,
                uses: 0,
                last_validated: Timestamp(1),
                prov: Provenance::Constant,
            }],
            session_summary: "turn 2, 3 messages".into(),
            turn_trace: "Proposed(echo)\nToolReturned(ok: echo: hi)".into(),
        }
    }

    fn replier(mock: std::sync::Arc<MockTransport>) -> CloudReplier {
        let client = OpenRouterClient::new(mock, "k".into()).with_retry(1, 1);
        CloudReplier::new(client, "anthropic/claude-sonnet-5".into())
    }

    #[tokio::test]
    async fn returns_message_content() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "Hello Martin!"}
            }]
        })]);
        let text = replier(mock).reply(ctx()).await.unwrap();
        assert_eq!(text, "Hello Martin!");
    }

    #[tokio::test]
    async fn request_is_cache_aware_and_never_sends_temperature() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        replier(mock.clone()).reply(ctx()).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        let req = &reqs[0];
        assert_eq!(req["model"], "anthropic/claude-sonnet-5");
        assert!(req.get("temperature").is_none(), "sampling params are rejected on sonnet-5");
        assert_eq!(req["messages"][0]["role"], "system");
        assert_eq!(
            req["messages"][0]["content"][0]["text"],
            "You are Tomáš, a friendly sales assistant."
        );
        assert_eq!(req["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        let content = req["messages"][1]["content"].as_str().unwrap();
        let facts_at = content.find("user.name").unwrap();
        let summary_at = content.find("turn 2, 3 messages").unwrap();
        let trace_at = content.find("Proposed(echo)").unwrap();
        assert!(facts_at < summary_at && summary_at < trace_at, "stable-first block order");
    }

    #[tokio::test]
    async fn empty_persona_gets_default_system_text() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        let mut c = ctx();
        c.persona = String::new();
        replier(mock.clone()).reply(c).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(reqs[0]["messages"][0]["content"][0]["text"], "You are a helpful assistant.");
    }

    #[tokio::test]
    async fn transport_failure_maps_to_reply_error() {
        let mock = MockTransport::new(vec![Err(TransportError::Network("down".into()))]);
        let err = replier(mock).reply(ctx()).await.unwrap_err();
        assert!(matches!(err, nscore::ReplyError::Transport(_)));
    }
}
