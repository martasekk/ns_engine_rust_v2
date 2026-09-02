use crate::client::{ApiError, OpenRouterClient};
use crate::schema::build_tools;
use async_trait::async_trait;
use nscore::{EmitError, Emitter, EmitterContext, LegalActionSet, Proposal};

const SYSTEM: &str = "You translate the user's latest message into exactly one action call \
from the provided tools. Choose respond_directly when no tool applies. Never invent argument \
values the user did not supply. The context lists actions already performed this turn with \
their results; never repeat a completed action — when those results answer the user, choose \
respond_directly.";

pub struct CloudEmitter {
    client: OpenRouterClient,
    model: String,
    max_tokens: u32,
}

impl CloudEmitter {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        // 4096, not 1024: reasoning models spend output tokens on reasoning
        // before the tool call; a tight cap yields finish_reason "length"
        // with null content and no tool_calls.
        Self {
            client,
            model,
            max_tokens: 4096,
        }
    }
}

fn render_context(ctx: &EmitterContext) -> String {
    let mut s = format!("State: {}\nRecent turns:\n", ctx.state_summary);
    for (speaker, text) in &ctx.recent_turns {
        s.push_str(&format!("{speaker}: {text}\n"));
    }
    if !ctx.rejections_this_turn.is_empty() {
        s.push_str("Rejected this turn:\n");
        for r in &ctx.rejections_this_turn {
            s.push_str(&format!("- {r}\n"));
        }
    }
    if !ctx.guidance.is_empty() {
        s.push_str("Guidance:\n");
        for g in &ctx.guidance {
            s.push_str(&format!("- {g}\n"));
        }
    }
    s.push_str("Propose the next action.");
    s
}

#[async_trait]
impl Emitter for CloudEmitter {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "temperature": 0,
            "tool_choice": "required",
            "tools": build_tools(legal),
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": render_context(&ctx)},
            ],
        });
        let body = self.client.chat(request).await.map_err(|e| match e {
            ApiError::Transport(d) => EmitError::Transport(d),
            ApiError::Status { status, detail } => {
                EmitError::Transport(format!("status {status}: {detail}"))
            }
        })?;

        let message = &body["choices"][0]["message"];
        let tool_call = match message["tool_calls"]
            .as_array()
            .and_then(|calls| calls.first())
        {
            Some(call) => call,
            None => {
                // Models that ignore tool_choice "required" answer in plain
                // text; treat that as respond_directly — the engine stays in
                // control, and the replier narrates from the trace as usual.
                let text = message["content"].as_str().unwrap_or_default().trim();
                if text.is_empty() {
                    return Err(EmitError::Malformed("no tool_calls in response".into()));
                }
                let mut rationale = format!("model answered in text: {text}");
                rationale.truncate(300);
                return Ok(Proposal {
                    rationale,
                    action: crate::schema::RESPOND_DIRECTLY.to_string(),
                    args: serde_json::json!({}),
                });
            }
        };

        let action = tool_call["function"]["name"]
            .as_str()
            .ok_or_else(|| EmitError::Malformed("tool call has no name".into()))?
            .to_string();
        let args_str = tool_call["function"]["arguments"].as_str().unwrap_or("{}");
        let parsed: serde_json::Value = serde_json::from_str(args_str)
            .map_err(|e| EmitError::Malformed(format!("unparseable arguments: {e}")))?;
        let mut input = parsed.as_object().cloned().unwrap_or_default();
        let rationale = input
            .remove("rationale")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
        Ok(Proposal {
            rationale,
            action,
            args: serde_json::Value::Object(input),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::OpenRouterClient;
    use crate::transport::{HttpResponse, MockTransport, TransportError};
    use nscore::{ActionSpec, EmitterContext, LegalActionSet, SideEffect};

    fn legal() -> LegalActionSet {
        LegalActionSet {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "echo text back".into(),
                args_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            }],
        }
    }

    fn ctx() -> EmitterContext {
        EmitterContext {
            state_summary: "turn 1, 1 messages".into(),
            recent_turns: vec![("user".into(), "say hi".into())],
            rejections_this_turn: vec!["guard g: nope".into()],
            guidance: vec![],
        }
    }

    fn tool_call_response(name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": name, "arguments": args.to_string()}
                    }]
                }
            }]
        })
    }

    fn emitter(mock: std::sync::Arc<MockTransport>) -> CloudEmitter {
        let client = OpenRouterClient::new(mock, "k".into()).with_retry(1, 1);
        CloudEmitter::new(client, "anthropic/claude-haiku-4.5".into())
    }

    #[tokio::test]
    async fn parses_tool_call_into_proposal_and_strips_rationale_from_args() {
        let mock = MockTransport::ok(vec![tool_call_response(
            "echo",
            serde_json::json!({"rationale": "user asked", "text": "hi"}),
        )]);
        let p = emitter(mock.clone())
            .propose(ctx(), &legal())
            .await
            .unwrap();
        assert_eq!(p.action, "echo");
        assert_eq!(p.rationale, "user asked");
        assert_eq!(p.args, serde_json::json!({"text": "hi"}));
    }

    #[tokio::test]
    async fn request_carries_schema_context_and_forced_tool_choice() {
        let mock = MockTransport::ok(vec![tool_call_response(
            "respond_directly",
            serde_json::json!({"rationale": "chat"}),
        )]);
        let p = emitter(mock.clone())
            .propose(ctx(), &legal())
            .await
            .unwrap();
        assert_eq!(p.action, "respond_directly");

        let reqs = mock.requests.lock().unwrap();
        let req = &reqs[0];
        assert_eq!(req["model"], "anthropic/claude-haiku-4.5");
        assert_eq!(req["temperature"], 0);
        assert_eq!(req["tool_choice"], "required");
        assert_eq!(
            req["tools"].as_array().unwrap().len(),
            2,
            "echo + respond_directly"
        );
        assert_eq!(req["messages"][0]["role"], "system");
        let text = req["messages"][1]["content"].as_str().unwrap();
        assert!(text.contains("turn 1, 1 messages"));
        assert!(text.contains("user: say hi"));
        assert!(text.contains("guard g: nope"));
    }

    #[tokio::test]
    async fn text_only_response_maps_to_respond_directly() {
        // Models that ignore tool_choice "required" (common on free tiers)
        // answer in plain text; that is a respond_directly proposal, not an
        // error — the engine stays in control either way.
        let mock = MockTransport::ok(vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": "The time is noon."}
            }]
        })]);
        let p = emitter(mock).propose(ctx(), &legal()).await.unwrap();
        assert_eq!(p.action, "respond_directly");
        assert!(p.rationale.contains("The time is noon."));
    }

    #[tokio::test]
    async fn empty_response_is_malformed() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": null}
            }]
        })]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Malformed(_)));
    }

    #[tokio::test]
    async fn unparseable_arguments_string_is_malformed() {
        let mock = MockTransport::ok(vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "echo", "arguments": "{not json"}
                    }]
                }
            }]
        })]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Malformed(_)));
    }

    #[tokio::test]
    async fn transport_failure_maps_to_emit_transport_error() {
        let mock = MockTransport::new(vec![Err(TransportError::Network("down".into()))]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Transport(_)));
    }

    #[tokio::test]
    async fn api_4xx_maps_to_emit_transport_error() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 400,
            body: serde_json::json!({"error": {"message": "bad"}}),
        })]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Transport(_)));
    }

    #[tokio::test]
    async fn guidance_notes_are_rendered_in_the_user_message_not_the_system_prompt() {
        let mock = MockTransport::ok(vec![tool_call_response(
            "echo",
            serde_json::json!({"text": "x"}),
        )]);
        let e = emitter(mock.clone());
        let mut c = ctx();
        c.guidance = vec!["Call get_time before answering time questions.".into()];
        e.propose(c, &legal()).await.unwrap();
        let reqs = mock.requests.lock().unwrap();
        let req = &reqs[0];
        let user = req["messages"][1]["content"].as_str().unwrap();
        assert!(
            user.contains("Guidance:\n- Call get_time before answering time questions."),
            "{user}"
        );
        let system = req["messages"][0]["content"].as_str().unwrap();
        assert!(!system.contains("Guidance"));
    }
}
