use crate::client::{ApiError, OpenRouterClient};
use async_trait::async_trait;
use nscore::{Replier, ReplyContext, ReplyError};

pub struct CloudReplier {
    client: OpenRouterClient,
    model: String,
    max_tokens: u32,
    prompt_cache: bool,
}

impl CloudReplier {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        // 4096, not 1024: models with reasoning enabled by default (e.g.
        // Claude Sonnet 5) spend output tokens on reasoning before content;
        // a tight cap yields finish_reason "length" with null content.
        Self {
            client,
            model,
            max_tokens: 4096,
            prompt_cache: true,
        }
    }

    /// Whether to mark the persona block with an Anthropic `cache_control`
    /// breakpoint. Only OpenRouter forwards it; other OpenAI-compatible
    /// providers may reject the unknown field, so pass `false` for them.
    pub fn with_prompt_cache(mut self, on: bool) -> Self {
        self.prompt_cache = on;
        self
    }
}

/// M6 §4.3: facts → summary → verbatim window → current turn (user text,
/// this turn's actions) → reply guidance → instruction. Stable blocks first
/// so a cached prefix survives; the user's message is the one thing the
/// reply must answer (seen live: without it the model narrated the session
/// counters instead).
fn render_context(ctx: &ReplyContext) -> String {
    let mut s = String::new();
    if !ctx.facts.is_empty() {
        s.push_str("Standing facts:\n");
        for f in &ctx.facts {
            s.push_str(&format!("- {}\n", nscore::render_fact(f)));
        }
    }
    if let Some(summary) = &ctx.summary {
        s.push_str(&nscore::render_summary(summary));
        s.push('\n');
    }
    if !ctx.window.is_empty() {
        s.push_str("Recent turns:\n");
        s.push_str(&nscore::render_window(
            &ctx.window,
            ctx.window.len(),
            &ctx.caps,
        ));
        s.push('\n');
    }
    s.push_str(&format!("Current turn:\nuser: {}\ndid:", ctx.user_text));
    if ctx.turn_trace.trim().is_empty() {
        s.push_str(" (nothing)\n");
    } else {
        s.push('\n');
        for line in ctx.turn_trace.lines() {
            s.push_str(&format!("  {line}\n"));
        }
    }
    if !ctx.guidance.is_empty() {
        s.push_str("Guidance:\n");
        for g in &ctx.guidance {
            s.push_str(&format!("- {g}\n"));
        }
    }
    if !ctx.do_not_state.is_empty() {
        s.push_str(&format!(
            "Do not state these; nothing above supports them: {}\n",
            ctx.do_not_state.join(", ")
        ));
    }
    s.push_str(
        "Reply to the user's current message. Use the recent turns and standing facts for \
         context. State only outcomes and values that appear above; if something failed or \
         was refused, say so plainly. Do not invent tool results, names, or numbers. Plain \
         text, no markdown.",
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
        let system_content = if self.prompt_cache {
            serde_json::json!([
                {"type": "text", "text": persona, "cache_control": {"type": "ephemeral"}}
            ])
        } else {
            serde_json::Value::String(persona)
        };
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [
                {"role": "system", "content": system_content},
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
                ..Default::default()
            }],
            summary: Some(nscore::SessionSummary {
                through_turn: 1,
                topic: "greeting".into(),
                established: vec![],
                open: vec![],
                trust: nscore::Trust::User,
                rebuilt_from: 1,
            }),
            window: vec![nscore::TurnRecord {
                turn: 2,
                user: "what now".into(),
                did: vec![],
                reply: "Hello!".into(),
                trust: nscore::Trust::User,
            }],
            caps: Default::default(),
            user_text: "say hi".into(),
            turn_trace: "Proposed(echo)\nToolReturned(ok: echo: hi)".into(),
            guidance: vec![],
            do_not_state: vec![],
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
        assert!(
            req.get("temperature").is_none(),
            "sampling params are rejected on sonnet-5"
        );
        assert_eq!(req["messages"][0]["role"], "system");
        assert_eq!(
            req["messages"][0]["content"][0]["text"],
            "You are Tomáš, a friendly sales assistant."
        );
        assert_eq!(
            req["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        let content = req["messages"][1]["content"].as_str().unwrap();
        let at = |needle: &str| {
            content
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {content}"))
        };
        // M6 §4.3: persona (system) → facts → summary → window → current turn.
        assert!(
            at("Standing facts:\n- user.name") < at("Conversation so far (turns 1–1): greeting")
        );
        assert!(at("Conversation so far") < at("Recent turns:\n[t2] user: what now"));
        assert!(
            at("[t2] user: what now") < at("Current turn:\nuser: say hi\ndid:\n  Proposed(echo)")
        );
        assert!(at("Proposed(echo)") < at("Reply to the user's current message."));
        assert!(!content.contains("Do not state"));
    }

    #[tokio::test]
    async fn empty_trace_and_flagged_spans_are_rendered() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        let mut c = ctx();
        c.turn_trace = String::new();
        c.do_not_state = vec!["185 messages".into(), "Oslo".into()];
        c.guidance = vec!["Answer the question first.".into()];
        replier(mock.clone()).reply(c).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        let content = reqs[0]["messages"][1]["content"].as_str().unwrap();
        assert!(content.contains("did: (nothing)\n"), "{content}");
        assert!(content.contains("Guidance:\n- Answer the question first.\n"));
        assert!(content
            .contains("Do not state these; nothing above supports them: 185 messages, Oslo\n"));
    }

    #[tokio::test]
    async fn without_prompt_cache_system_content_is_a_plain_string() {
        // Providers other than OpenRouter (Mistral, Ollama, Groq) don't know
        // the Anthropic cache_control extension; some 400 on unknown fields.
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        let client = OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1);
        let r = CloudReplier::new(client, "mistral-small-latest".into()).with_prompt_cache(false);
        r.reply(ctx()).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        let system = &reqs[0]["messages"][0];
        assert_eq!(system["role"], "system");
        assert_eq!(
            system["content"],
            "You are Tomáš, a friendly sales assistant."
        );
        assert!(!reqs[0].to_string().contains("cache_control"));
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
        assert_eq!(
            reqs[0]["messages"][0]["content"][0]["text"],
            "You are a helpful assistant."
        );
    }

    #[tokio::test]
    async fn transport_failure_maps_to_reply_error() {
        let mock = MockTransport::new(vec![Err(TransportError::Network("down".into()))]);
        let err = replier(mock).reply(ctx()).await.unwrap_err();
        assert!(matches!(err, nscore::ReplyError::Transport(_)));
    }
}
