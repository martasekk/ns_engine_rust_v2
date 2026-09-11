use crate::client::{ApiError, OpenRouterClient};
use crate::schema::build_tools;
use async_trait::async_trait;
use nscore::{EmitError, Emitter, EmitterContext, LegalActionSet, Proposal};

/// The instruction every tool's `_rationale` property used to carry (M10
/// T1.1). Sent once, in the system prompt, instead of once per tool: on the
/// recorded turn-21 array the per-tool copy was 106 chars × 7 tools and a
/// quarter of every call's tool tokens (findings §8.1). The schema keeps only
/// a short label ([`crate::schema::RATIONALE_HINT`]); this is the sentence
/// that says what to put there.
pub const RATIONALE_INSTRUCTION: &str = "Every tool takes `_rationale` first: one sentence \
saying why this action, grounded in the user's words.";

const SYSTEM_PREAMBLE: &str = "You translate the user's latest message into exactly one action \
call from the provided tools. Choose respond_directly when no tool applies. Never invent \
argument values the user did not supply. The context lists actions already performed this turn \
with their results; never repeat a completed action — when those results answer the user, \
choose respond_directly. A line marked done is such an action: proposing it again is refused \
and costs a step. ";

/// The same preamble for a model that does not need the repeat gate
/// explained to it (M12 T1.4).
///
/// First two sentences verbatim — they say what the task is, and that is not
/// a small-model concession. What goes is the narration that follows: three
/// clauses telling a weak emitter what a `done` line means and what happens
/// if it proposes one anyway, kept as the one clause that is actually a
/// rule. Sent on every emitter call and every iteration of every turn, which
/// is why a few dozen tokens are worth the second string.
const SYSTEM_PREAMBLE_STRONG: &str = "You translate the user's latest message into exactly one \
action call from the provided tools. Choose respond_directly when no tool applies. Never invent \
argument values the user did not supply. Never repeat an action the context lists as done. ";

/// Assembled rather than written out so the rationale instruction has exactly
/// one home in the workspace and the test can assert it appears once.
fn system_prompt(capability: nscore::Capability) -> String {
    let preamble = capability.pick(SYSTEM_PREAMBLE, SYSTEM_PREAMBLE_STRONG);
    format!("{preamble}{RATIONALE_INSTRUCTION}")
}

/// The two system prompts, for the cost report that prices them
/// (`ns-app budget`). Nothing else needs them: the emitter renders its own.
pub fn system_prompts() -> (String, String) {
    (
        system_prompt(nscore::Capability::Small),
        system_prompt(nscore::Capability::Strong),
    )
}

/// 4096, not 1024: reasoning models spend output tokens on reasoning before
/// the tool call; a tight cap yields finish_reason "length" with null content
/// and no tool_calls. A floor, and `[llm.emitter] max_tokens` may lower it
/// where the model's effort is known (M11 T0.4 recommends 2048 on Sonnet at
/// effort low).
pub const MAX_TOKENS: u32 = 4096;

/// The request shape this emitter has always sent: `temperature: 0`, no
/// reasoning block, 4096 output tokens. What `[llm.emitter]`'s shaping
/// fields are folded onto (M11 T0.2).
pub fn default_shape() -> crate::provider::RequestShape {
    crate::provider::RequestShape::pinned(MAX_TOKENS)
}

pub struct CloudEmitter {
    client: OpenRouterClient,
    model: String,
    shape: crate::provider::RequestShape,
    prompt_cache: bool,
    capability: nscore::Capability,
}

impl CloudEmitter {
    pub fn new(client: OpenRouterClient, model: String) -> Self {
        Self {
            client,
            model,
            shape: default_shape(),
            prompt_cache: false,
            capability: nscore::Capability::Small,
        }
    }

    /// `[llm.emitter]`'s `sampling`, `reasoning`, `thinking` and `max_tokens`,
    /// already resolved against the model id (M11 T0.2). Unset everywhere is
    /// [`default_shape`], which is byte-for-byte the request above.
    pub fn with_shape(mut self, shape: crate::provider::RequestShape) -> Self {
        self.shape = shape;
        self
    }

    /// `[llm] prompt_cache_emitter` (M10 P4, decision 2b). Off by default,
    /// and off is byte-identical to the request this emitter has always
    /// sent.
    ///
    /// Worth turning on only where the two conditions hold together: the
    /// provider forwards cache breakpoints (the OpenRouter preset), and the
    /// `tools` array is stable across the turn's iterations, which is P2's
    /// rule. On the recorded turn 21 the array (731 tokens) plus
    /// facts+summary+window (538) clears the 1,024-token floor a breakpoint
    /// needs; the replier's prefix (225) does not, which is why only this
    /// one is behind a knob. Verified by `cached_tokens` on a paid-tier
    /// session, never by reading this code.
    pub fn with_prompt_cache(mut self, on: bool) -> Self {
        self.prompt_cache = on;
        self
    }

    /// `[llm] capability` (M12 T1.1). `Small` is the default and is the
    /// request this emitter has always sent; `Strong` only shortens the
    /// system preamble (T1.4).
    pub fn with_capability(mut self, capability: nscore::Capability) -> Self {
        self.capability = capability;
        self
    }
}

/// M6 §4.2/§4.4: facts → summary → obligations → verbatim window → current
/// turn → this turn's actions → pending/rejections → guidance. Stable blocks
/// first.
fn render_context(ctx: &EmitterContext) -> String {
    let (stable, rest) = render_context_split(ctx);
    format!("{stable}{rest}")
}

/// The same text, cut where a cache breakpoint belongs (M10 P4, decision
/// 2b): everything through the verbatim window, then everything from the
/// current turn on.
///
/// The cut is not arbitrary. Above it is the run of blocks that does not
/// change between two iterations of one turn — facts, summary, obligations,
/// window — and below it is the trace, which is the thing that *does* change
/// on every iteration and is why the emitter prefix is worth caching at all.
/// Concatenated, the two halves are the string `render_context` has always
/// produced, which is what makes the knob's off position byte-identical.
fn render_context_split(ctx: &EmitterContext) -> (String, String) {
    let mut s = String::new();
    if !ctx.facts.is_empty() {
        s.push_str("Facts:\n");
        for f in &ctx.facts {
            s.push_str(&format!("- {}\n", nscore::render_fact(f)));
        }
    }
    if let Some(summary) = &ctx.summary {
        s.push_str(&nscore::render_summary(summary));
        s.push('\n');
    }
    // Directly above the window (M9 T2.1): after the stable blocks, so the
    // cacheable prefix is unchanged, and before the transcript, so what the
    // turn owes is read before what earlier turns said.
    if !ctx.obligations.is_empty() {
        s.push_str("Obligations this turn:\n");
        for o in &ctx.obligations {
            s.push_str(&format!("- {o}\n"));
        }
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
    // ---- breakpoint ----
    let stable = std::mem::take(&mut s);
    s.push_str(&format!("Current turn:\nuser: {}\n", ctx.user_text));
    if !ctx.trace_so_far.is_empty() {
        s.push_str("This turn so far:\n");
        for line in &ctx.trace_so_far {
            s.push_str(&format!("- {line}\n"));
        }
    }
    // After the trace, because the results it names are in it (M7 T2.3).
    if let Some(line) = &ctx.budget_line {
        s.push_str(line);
        s.push('\n');
    }
    if ctx.pending_confirmation {
        s.push_str("Pending confirmation: awaiting the user's yes/no on the staged action.\n");
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
    (stable, s)
}

#[async_trait]
impl Emitter for CloudEmitter {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        // Content-parts only when the knob is on, and the same form the
        // replier already uses: one `cache_control` breakpoint on the part
        // that ends the window block, so what the provider is asked to keep
        // is the tool array plus the blocks that do not move inside a turn.
        // Off, this is `serde_json::Value::String` and the request is the
        // one this emitter has always sent, byte for byte.
        let user_content = if self.prompt_cache {
            let (stable, rest) = render_context_split(&ctx);
            serde_json::json!([
                {"type": "text", "text": stable, "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": rest},
            ])
        } else {
            serde_json::Value::String(render_context(&ctx))
        };
        let mut request = serde_json::json!({
            "model": self.model,
            "tool_choice": "required",
            "tools": build_tools(legal),
            "messages": [
                {"role": "system", "content": system_prompt(self.capability)},
                {"role": "user", "content": user_content},
            ],
        });
        // `max_tokens`, `temperature` and the `reasoning` block — the only
        // keys that differ per model — are written in one place.
        self.shape.apply(&mut request);
        let body = self
            .client
            .chat_into(request, ctx.usage.as_deref())
            .await
            .map_err(|e| match e {
                ApiError::Transport(d) => EmitError::Transport(d),
                ApiError::Status { status, detail } => EmitError::Provider { status, detail },
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
                if !text.is_empty() {
                    // M12 T1.3: the prefix is `nscore`'s so the counter that
                    // reads the log and the line that writes it cannot drift.
                    let mut rationale = format!("{} {text}", nscore::TEXT_FALLBACK_PREFIX);
                    rationale.truncate(300);
                    return Ok(Proposal {
                        rationale,
                        action: crate::schema::RESPOND_DIRECTLY.to_string(),
                        args: serde_json::json!({}),
                    });
                }
                // Neither a tool call nor text. Seen live with Ollama: the
                // model insists on an action the engine has just made
                // illegal (repeat gate), and the OpenAI shim drops the call
                // because its name is not in `tools` — leaving an empty
                // message. When this turn has already done something, that
                // trace is the answer: narrate it instead of burning the
                // retries and settling on the canned failure. With nothing
                // done yet there is nothing to narrate, so it stays
                // malformed and the engine retries.
                if ctx.trace_so_far.is_empty() {
                    return Err(EmitError::Malformed("no tool_calls in response".into()));
                }
                return Ok(Proposal {
                    rationale: "model returned nothing; answering from this turn's results".into(),
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
        // Strict first, always: the salvage pass sees nothing that parses, so
        // the primary path keeps the behaviour it had. Only the arguments are
        // repaired — the action name above stays whatever the provider sent,
        // because the illegality guarantee rests on generation being
        // constrained to the legal set (plan §T2.5).
        let parsed: serde_json::Value = match serde_json::from_str(args_str) {
            Ok(v) => v,
            Err(e) => match crate::salvage::salvage_arguments(args_str) {
                Some((v, repair)) => {
                    // Repairing model output in silence would hide a provider
                    // that needs replacing, and one salvaged call looks
                    // exactly like a well-formed one from here on.
                    eprintln!("emitter: repaired malformed tool arguments ({repair})");
                    v
                }
                None => return Err(EmitError::Malformed(format!("unparseable arguments: {e}"))),
            },
        };
        let mut input = parsed.as_object().cloned().unwrap_or_default();
        // `_rationale` is the schema's name for it (see `schema::RATIONALE`).
        // The bare name is still accepted, because a shim that ignores
        // `strict` generates from the description rather than the schema and
        // several of them are exactly the endpoints this workspace ships
        // presets for. Leaving a stray `rationale` in `args` would not fail
        // validation — `validate_args` checks required keys, not unknown
        // ones — it would quietly travel into classification and into the
        // repeat gate's identity.
        let rationale = input
            .remove(crate::schema::RATIONALE)
            .or_else(|| input.remove("rationale"))
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

    /// M10 T1.1. The instruction moved out of every tool and into the one
    /// message that is sent once — so it has to actually be there, and it has
    /// to be there exactly once, or the cut traded a repeated instruction for
    /// a missing one.
    #[test]
    fn the_system_prompt_carries_the_rationale_instruction_once() {
        // M12 T1.4: both profiles, because the trimmed one is a different
        // string and a dropped instruction would be invisible otherwise.
        for capability in [nscore::Capability::Small, nscore::Capability::Strong] {
            let prompt = system_prompt(capability);
            assert_eq!(
                prompt.matches(RATIONALE_INSTRUCTION).count(),
                1,
                "the rationale instruction is not in the {} system prompt exactly once: {prompt:?}",
                capability.as_str()
            );
            assert!(
                prompt.contains("_rationale"),
                "the instruction must name the field the schema injects"
            );
            assert_eq!(
                prompt.matches("_rationale").count(),
                1,
                "one mention, not a restatement per paragraph"
            );
        }
        // And the schema is now the short label, not this sentence.
        assert!(
            !crate::schema::RATIONALE_HINT.contains("grounded"),
            "the per-tool property is still carrying the instruction"
        );
    }

    /// M12 T1.4. The preamble's third sentence explains the repeat gate to a
    /// model that needs it explained; a strong model needs the rule, not the
    /// explanation. The saving is per emitter call and per iteration, so the
    /// test prints it rather than merely asserting an inequality - the
    /// number is the point.
    #[test]
    fn the_strong_preamble_is_shorter_and_says_by_how_much() {
        let small = nscore::estimate_tokens(system_prompt(nscore::Capability::Small).len());
        let strong = nscore::estimate_tokens(system_prompt(nscore::Capability::Strong).len());
        assert!(
            strong < small,
            "strong preamble is not shorter: small {small} tokens, strong {strong} tokens"
        );
        // The rule it keeps, in one clause.
        let prompt = system_prompt(nscore::Capability::Strong);
        assert!(
            prompt.contains("Never repeat an action the context lists as done."),
            "the strong preamble dropped the repeat-gate rule: {prompt:?}"
        );
        assert!(
            !prompt.contains("costs a step"),
            "the strong preamble kept the narration: {prompt:?}"
        );
    }
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
            usage: None,
            facts: vec![nscore::Fact {
                key: "user.name".into(),
                value: serde_json::json!("Martin"),
                confidence: 1.0,
                uses: 0,
                last_validated: nscore::Timestamp(1),
                prov: nscore::Provenance::Constant,
                ..Default::default()
            }
            .into()],
            summary: None,
            window: vec![nscore::TurnRecord {
                turn: 1,
                user: "earlier question".into(),
                did: vec!["echo -> ok: echo: x".into()],
                reply: "x".into(),
                trust: nscore::Trust::User,
            }],
            caps: Default::default(),
            user_text: "say hi".into(),
            obligations: vec![],
            trace_so_far: vec!["ToolReturned(ok: echo: hi)".into()],
            pending_confirmation: false,
            rejections_this_turn: vec!["guard g: nope".into()],
            guidance: vec![],
            budget_line: None,
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

    /// The context's sink is the one the call records into, and the client's
    /// own is left alone (multi-conversation plan Phase 1): that is how the
    /// engine keeps two overlapping turns' costs on their own `ModelCall`s.
    #[tokio::test]
    async fn the_calls_cost_lands_in_the_contexts_sink_not_the_clients() {
        let mock = MockTransport::ok(vec![tool_call_response(
            "respond_directly",
            serde_json::json!({"rationale": "chat"}),
        )]);
        let own = std::sync::Arc::new(nscore::UsageSink::new());
        let client =
            OpenRouterClient::new(mock, "k".into()).with_usage_sink(own.clone(), "emitter");
        let e = CloudEmitter::new(client, "m".into());
        let turn = std::sync::Arc::new(nscore::UsageSink::new());
        let mut ctx = ctx();
        ctx.usage = Some(turn.clone());
        e.propose(ctx, &legal()).await.unwrap();
        let recorded = turn.drain();
        assert_eq!(recorded.len(), 1, "the turn's sink took the call");
        assert_eq!(recorded[0].role, "emitter");
        assert!(own.drain().is_empty(), "the client's own sink was not used");
    }

    /// M10 P4 (decision 2b). The breakpoint ends the window block, because
    /// that is the last thing in an emitter prompt that does not change
    /// between two iterations of one turn — the trace below it changes on
    /// every one. And off is not "nearly the same request": it is the same
    /// string, in the same shape, which is what lets this ship default off
    /// and be turned on by one line of config.
    #[tokio::test]
    async fn the_emitter_breakpoint_falls_after_the_window_block_and_is_absent_when_off() {
        let on = MockTransport::ok(vec![tool_call_response(
            "respond_directly",
            serde_json::json!({"rationale": "chat"}),
        )]);
        emitter(on.clone())
            .with_prompt_cache(true)
            .propose(ctx(), &legal())
            .await
            .unwrap();
        let reqs = on.requests.lock().unwrap();
        let parts = reqs[0]["messages"][1]["content"]
            .as_array()
            .expect("content-parts form");
        assert_eq!(parts.len(), 2);
        let head = parts[0]["text"].as_str().unwrap();
        let tail = parts[1]["text"].as_str().unwrap();
        assert_eq!(
            parts[0]["cache_control"],
            serde_json::json!({"type": "ephemeral"}),
            "the breakpoint is on the part that ends the window"
        );
        assert!(parts[1].get("cache_control").is_none());
        assert!(head.contains("Facts:"), "{head}");
        assert!(head.contains("Recent turns:"), "{head}");
        assert!(head.ends_with("\n"), "the window block ends it: {head:?}");
        assert!(!head.contains("Current turn:"), "{head}");
        assert!(tail.starts_with("Current turn:"), "{tail}");
        assert!(tail.contains("This turn so far:"), "{tail}");
        let joined = format!("{head}{tail}");
        drop(reqs);

        let off = MockTransport::ok(vec![tool_call_response(
            "respond_directly",
            serde_json::json!({"rationale": "chat"}),
        )]);
        emitter(off.clone()).propose(ctx(), &legal()).await.unwrap();
        let reqs = off.requests.lock().unwrap();
        let plain = reqs[0]["messages"][1]["content"]
            .as_str()
            .expect("a plain string, as it has always been");
        assert_eq!(plain, joined, "the two halves are the one prompt");
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

    /// M11 T0.2, the exit criterion: `sampling = "none"` removes the key,
    /// it does not send a different value. Claude Sonnet 5 400s on the
    /// *presence* of `temperature`, so `temperature: 1` would fail exactly
    /// as `temperature: 0` does.
    #[tokio::test]
    async fn sampling_none_omits_temperature_entirely() {
        let mock = MockTransport::ok(vec![tool_call_response(
            "respond_directly",
            serde_json::json!({"rationale": "chat"}),
        )]);
        let client = OpenRouterClient::new(mock.clone(), "k".into()).with_retry(1, 1);
        let (shape, coercion) = crate::provider::RoleShaping {
            reasoning: Some("low".into()),
            max_tokens: Some(2048),
            ..Default::default()
        }
        .resolve(
            "emitter",
            "anthropic/claude-sonnet-5",
            crate::emitter::default_shape(),
        );
        assert!(coercion.is_some(), "the safety net announced itself");
        CloudEmitter::new(client, "anthropic/claude-sonnet-5".into())
            .with_shape(shape)
            .propose(ctx(), &legal())
            .await
            .unwrap();

        let reqs = mock.requests.lock().unwrap();
        let req = &reqs[0];
        assert!(
            req.get("temperature").is_none(),
            "not a different value — no key at all: {req}"
        );
        assert!(!req.to_string().contains("temperature"));
        assert_eq!(req["reasoning"], serde_json::json!({"effort": "low"}));
        assert_eq!(req["max_tokens"], 2048);
        // Everything else about the emitter's request is untouched.
        assert_eq!(req["tool_choice"], "required");
        assert_eq!(req["messages"][0]["role"], "system");
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
        // M11 T0.2: the default shape is the request that has always gone
        // out — an integer 0, 4096 tokens, and no `reasoning` block.
        assert_eq!(req["max_tokens"], 4096);
        assert!(req.get("reasoning").is_none(), "{req}");
        assert_eq!(req["tool_choice"], "required");
        assert_eq!(
            req["tools"].as_array().unwrap().len(),
            2,
            "echo + respond_directly"
        );
        assert_eq!(req["messages"][0]["role"], "system");
        let text = req["messages"][1]["content"].as_str().unwrap();
        // M6 §4.2: facts, verbatim window, the current message, this turn's
        // actions and the rejections all reach the emitter, in that order.
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {text}"))
        };
        assert!(at("Facts:\n- user.name: \"Martin\"") < at("[t1] user: earlier question"));
        assert!(at("[t1] user: earlier question") < at("Current turn:\nuser: say hi"));
        assert!(at("user: say hi") < at("This turn so far:\n- ToolReturned(ok: echo: hi)"));
        assert!(at("ToolReturned(ok: echo: hi)") < at("guard g: nope"));
        assert!(text.ends_with("Propose the next action."));
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

    fn empty_message() -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": null}
            }]
        })]
    }

    /// Nothing done yet, nothing said: unusable, so the engine retries.
    #[tokio::test]
    async fn empty_response_before_any_action_is_malformed() {
        let mut ctx = ctx();
        ctx.trace_so_far.clear();
        let err = emitter(MockTransport::ok(empty_message()))
            .propose(ctx, &legal())
            .await
            .unwrap_err();
        assert!(matches!(err, nscore::EmitError::Malformed(_)));
    }

    /// Seen live with Ollama: after the repeat gate makes the model's chosen
    /// action illegal it keeps calling it, and the shim drops the call as
    /// unknown, leaving an empty message. This turn's results are the
    /// answer — narrate them rather than settling on the canned failure.
    #[tokio::test]
    async fn empty_response_after_an_action_narrates_the_trace() {
        let p = emitter(MockTransport::ok(empty_message()))
            .propose(ctx(), &legal())
            .await
            .unwrap();
        assert_eq!(p.action, "respond_directly");
        assert!(p.args.as_object().is_some_and(|o| o.is_empty()));
    }

    fn raw_arguments(args: &str) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "id": "gen_1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant", "content": null,
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"name": "echo", "arguments": args}
                    }]
                }
            }]
        })]
    }

    /// Nothing in `{not json` is an object, so the salvage pass must not
    /// rescue it: the engine still spends a retry, which is the right answer.
    #[tokio::test]
    async fn unparseable_arguments_string_is_malformed() {
        let mock = MockTransport::ok(raw_arguments("{not json"));
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Malformed(_)));
    }

    /// A fenced, single-quoted, trailing-comma'd argument object — one weak
    /// shim's whole repertoire at once — used to cost one of
    /// `max_emit_retries` per occurrence. It now proposes, and the rationale
    /// is still stripped out of `args`.
    #[tokio::test]
    async fn salvageable_arguments_yield_a_proposal_instead_of_a_retry() {
        let mock = MockTransport::ok(raw_arguments(
            "```json\n{'_rationale': 'user asked', 'text': 'hi',}\n```",
        ));
        let p = emitter(mock).propose(ctx(), &legal()).await.unwrap();
        assert_eq!(p.action, "echo");
        assert_eq!(p.rationale, "user asked");
        assert_eq!(p.args, serde_json::json!({"text": "hi"}));
    }

    #[tokio::test]
    async fn transport_failure_maps_to_emit_transport_error() {
        let mock = MockTransport::new(vec![Err(TransportError::Network("down".into()))]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        assert!(matches!(err, nscore::EmitError::Transport(_)));
    }

    /// M9 T2.1. Position is the whole design: the block sits after the
    /// stable facts and summary, so the cacheable prefix is unchanged, and
    /// directly above the transcript, so what the turn owes is read before
    /// what earlier turns said.
    #[test]
    fn obligations_render_above_the_recent_turns_block() {
        let mut c = ctx();
        c.obligations = nscore::obligations_for("where is my order? send me the invoice", 5);
        let rendered = render_context(&c);
        let block = rendered
            .find("Obligations this turn:\n")
            .expect("a block: {rendered}");
        let window = rendered.find("Recent turns:").expect("a window");
        let summary_end = rendered.find("Current turn:").expect("a current turn");
        assert!(block < window, "above the window: {rendered}");
        assert!(window < summary_end);
        assert!(
            rendered.contains("- answer: where is my order\n- do: send me the invoice\n"),
            "{rendered}"
        );
        // Facts stay first, so the prefix in front of the block is the
        // stable one.
        assert!(rendered.find("Facts:").unwrap() < block, "{rendered}");
        // No obligations, no block and no blank heading.
        c.obligations.clear();
        assert!(!render_context(&c).contains("Obligations"));
    }

    /// An HTTP status keeps its status. The engine decides recovery by class,
    /// and a status parsed back out of a formatted string is a recovery
    /// decision resting on a formatting accident.
    #[tokio::test]
    async fn api_status_maps_to_a_structured_provider_error() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 400,
            body: serde_json::json!({"error": {"message": "bad"}}),
        })]);
        let err = emitter(mock).propose(ctx(), &legal()).await.unwrap_err();
        let nscore::EmitError::Provider { status, detail } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(*status, 400);
        assert!(detail.contains("bad"), "{detail}");
        assert!(!err.is_retryable(), "a 400 will not become a 200");
    }

    /// The classes that separate a bad afternoon from a bad request.
    #[tokio::test]
    async fn only_transient_provider_statuses_are_retryable() {
        let p = |status| nscore::EmitError::Provider {
            status,
            detail: String::new(),
        };
        for status in [429, 408, 500, 503] {
            assert!(p(status).is_retryable(), "{status} is transient");
        }
        // 404: the model name is wrong. 402: the account is empty. Session
        // `cli` t154/t155 spent three emit retries each on a 404.
        for status in [400, 401, 402, 403, 404] {
            assert!(!p(status).is_retryable(), "{status} is terminal");
        }
        assert!(nscore::EmitError::Malformed("x".into()).is_retryable());
        assert!(nscore::EmitError::Transport("x".into()).is_retryable());
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
