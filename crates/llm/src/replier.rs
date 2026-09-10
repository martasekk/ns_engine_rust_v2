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

/// Reference material: the stable blocks, tagged and fenced. M6 §4.3 order
/// (facts → summary → verbatim window) is preserved; what changed is the
/// role it is sent in and the fence around it.
///
/// Both changes answer the same failure. Flat, untagged, in the same `user`
/// message as the task, the window reads as a document to continue rather
/// than as background to draw on, and the model continues it — 16 replies of
/// `fact user.previous_name = Tomas` in the live `cli` session, and t135's
/// `"hi\nwhat time is it?"`, which is the user's own line handed back.
/// Fencing untrusted or reference content behind explicit markers is
/// spotlighting's *delimiting* variant (Microsoft, CEUR Vol-3920), reported
/// at minimal task cost; datamarking and encoding are not used here — the
/// problem is a lazy model, not an adversary, and both cost legibility.
fn render_reference(ctx: &ReplyContext) -> String {
    let mut s = String::from(
        "<reference>\nBackground, so you know what is true and what has already been \
         said. Draw on it; never reproduce a line of it.\n",
    );
    if !ctx.facts.is_empty() {
        s.push_str("\n<facts>\n");
        for f in &ctx.facts {
            s.push_str(&format!("- {}\n", nscore::render_fact(f)));
        }
        s.push_str("</facts>\n");
    }
    if let Some(summary) = &ctx.summary {
        s.push_str("\n<summary>\n");
        s.push_str(&nscore::render_summary(summary));
        s.push_str("\n</summary>\n");
    }
    if !ctx.window.is_empty() {
        s.push_str("\n<transcript>\n");
        s.push_str(&nscore::render_window(
            &ctx.window,
            ctx.window.len(),
            &ctx.caps,
        ));
        s.push_str("\n</transcript>\n");
    }
    s.push_str("</reference>");
    s
}

/// The task: the user's message, what this turn did, and the instruction.
/// Everything here is live; nothing here is cacheable.
fn render_task(ctx: &ReplyContext) -> String {
    let mut s = format!("The user's message, right now:\n{}\n", ctx.user_text);
    s.push_str("\n<did>\n");
    if ctx.turn_trace.trim().is_empty() {
        s.push_str("(nothing this turn)\n");
    } else {
        for line in ctx.turn_trace.lines() {
            s.push_str(&format!("{line}\n"));
        }
    }
    s.push_str("</did>\n");
    if !ctx.guidance.is_empty() {
        s.push_str("\nGuidance:\n");
        for g in &ctx.guidance {
            s.push_str(&format!("- {g}\n"));
        }
    }
    if !ctx.do_not_state.is_empty() {
        s.push_str(&format!(
            "\nDo not state these; nothing above supports them: {}\n",
            ctx.do_not_state.join(", ")
        ));
    }
    if !ctx.do_not_repeat.is_empty() {
        s.push_str(&format!(
            "\nYour last draft copied this out of the material instead of answering: \
             {}. Say it a different way, or leave it out.\n",
            ctx.do_not_repeat.join(" / ")
        ));
    }
    // The old closing line asked the model to "state only outcomes and values
    // that appear above", which read literally is a request for a copy — and
    // got one. Answering comes first now; grounding is the constraint on the
    // answer, not the task.
    s.push_str(
        "\nAnswer the user's message, in your own words, speaking to them. Never reproduce \
         a line from <reference>, <did> or the user's message verbatim — a copied line is \
         not an answer. State no outcome, value or name that does not appear above; if \
         something failed or was refused, say so plainly. Do not invent tool results, \
         names, or numbers. Plain text, no markdown.",
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
        // facts (stable) → session summary → window. All of it now rides in
        // the `system` message, so the `user` message holds only what the
        // reply must answer; the breakpoint still sits right after the
        // persona, which is the only part that never changes.
        // Content-parts form lets the cache_control breakpoint pass through
        // OpenRouter to Anthropic prompt caching. Never send sampling params.
        let reference = render_reference(&ctx);
        let system_content = if self.prompt_cache {
            serde_json::json!([
                {"type": "text", "text": persona, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": reference},
            ])
        } else {
            serde_json::Value::String(format!("{persona}\n\n{reference}"))
        };
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [
                {"role": "system", "content": system_content},
                {"role": "user", "content": render_task(&ctx)},
            ],
        });
        let body = self
            .client
            .chat_into(request, ctx.usage.as_deref())
            .await
            .map_err(|e| match e {
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
            usage: None,
            persona: "You are Tomáš, a friendly sales assistant.".into(),
            facts: vec![Fact {
                key: "user.name".into(),
                value: serde_json::json!("Martin"),
                confidence: 1.0,
                uses: 0,
                last_validated: Timestamp(1),
                prov: Provenance::Constant,
                ..Default::default()
            }
            .into()],
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
            do_not_repeat: vec![],
        }
    }

    fn replier(mock: std::sync::Arc<MockTransport>) -> CloudReplier {
        let client = OpenRouterClient::new(mock, "k".into()).with_retry(1, 1);
        CloudReplier::new(client, "anthropic/claude-sonnet-5".into())
    }

    /// See the emitter's twin: the context's sink takes the call's cost,
    /// the client's own stays empty.
    #[tokio::test]
    async fn the_calls_cost_lands_in_the_contexts_sink_not_the_clients() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "Hello!"}}]
        })]);
        let own = std::sync::Arc::new(nscore::UsageSink::new());
        let client =
            OpenRouterClient::new(mock, "k".into()).with_usage_sink(own.clone(), "replier");
        let r = CloudReplier::new(client, "m".into());
        let turn = std::sync::Arc::new(nscore::UsageSink::new());
        let mut ctx = ctx();
        ctx.usage = Some(turn.clone());
        r.reply(ctx).await.unwrap();
        let recorded = turn.drain();
        assert_eq!(recorded.len(), 1, "the turn's sink took the call");
        assert_eq!(recorded[0].role, "replier");
        assert!(own.drain().is_empty(), "the client's own sink was not used");
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
        // The reference blocks ride in `system`, after the cached persona.
        let reference = req["messages"][0]["content"][1]["text"].as_str().unwrap();
        let at_ref = |needle: &str| {
            reference
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {reference}"))
        };
        // M6 §4.3 order survives the move: facts → summary → window.
        assert!(at_ref("<facts>\n- user.name") < at_ref("<summary>"));
        assert!(at_ref("Conversation so far (turns 1–1): greeting") < at_ref("<transcript>"));
        assert!(at_ref("<transcript>\n[t2] user: what now") < at_ref("</reference>"));
        assert!(reference.contains("never reproduce a line of it"));

        // The `user` message carries only what the reply must answer, so the
        // transcript cannot read as a document to continue.
        let content = req["messages"][1]["content"].as_str().unwrap();
        assert!(!content.contains("[t2] user: what now"), "{content}");
        assert!(!content.contains("user.name"), "{content}");
        let at = |needle: &str| {
            content
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {content}"))
        };
        assert!(at("The user's message, right now:\nsay hi") < at("<did>\nProposed(echo)"));
        assert!(at("<did>") < at("Answer the user's message, in your own words"));
        assert!(content.contains("Never reproduce a line from <reference>"));
        assert!(!content.contains("Do not state"));
        assert!(!content.contains("Your last draft copied"));
    }

    /// Without the cache breakpoint the two blocks still both reach `system`.
    #[tokio::test]
    async fn reference_rides_in_system_without_prompt_cache_too() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        let client = OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1);
        CloudReplier::new(client, "mistral-small-latest".into())
            .with_prompt_cache(false)
            .reply(ctx())
            .await
            .unwrap();
        let reqs = mock.requests.lock().unwrap();
        let system = reqs[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("You are Tomáš, a friendly sales assistant."));
        assert!(system.contains("<transcript>\n[t2] user: what now"));
        let user = reqs[0]["messages"][1]["content"].as_str().unwrap();
        assert!(!user.contains("[t2] user: what now"), "{user}");
    }

    #[tokio::test]
    async fn empty_trace_and_flagged_spans_are_rendered() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}]
        })]);
        let mut c = ctx();
        c.turn_trace = String::new();
        c.do_not_state = vec!["185 messages".into(), "Oslo".into()];
        c.do_not_repeat = vec!["from memory, user.name is Peter".into()];
        c.guidance = vec!["Answer the question first.".into()];
        replier(mock.clone()).reply(c).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        let content = reqs[0]["messages"][1]["content"].as_str().unwrap();
        assert!(
            content.contains("<did>\n(nothing this turn)\n</did>"),
            "{content}"
        );
        assert!(content.contains("Guidance:\n- Answer the question first.\n"));
        assert!(content
            .contains("Do not state these; nothing above supports them: 185 messages, Oslo\n"));
        assert!(
            content.contains(
                "Your last draft copied this out of the material instead of answering: \
                 from memory, user.name is Peter."
            ),
            "{content}"
        );
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
        assert!(system["content"]
            .as_str()
            .unwrap()
            .starts_with("You are Tomáš, a friendly sales assistant."));
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
