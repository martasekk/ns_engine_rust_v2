use crate::state::fold;
// The trace renderer moved to its own module; the turn loop is its caller,
// not its owner. Imported by name so what the loop actually uses is visible.
use crate::trace::{
    clipped_results, emitter_manifest, inspect_page, parse_result_handle, reply_manifest,
    result_handle, result_text, result_trust, result_window, trace_for_prompt, window_range,
    DEFAULT_TOOL_RESULT_MAX_CHARS,
};
use nscore::{
    ClassifiedProposal, EventKind, EventLog, HarnessParts, Incoming, LegalActionSet, RejectReason,
    ReplyContext, ReplyPolicy, Timestamp, ToolCtx, ToolOutcome, Verdict,
};

pub struct EngineConfig {
    pub max_iterations: u32,
    pub max_emit_retries: u32,
    pub persona: String,
    /// Template registry for ReplyPolicy::Template; a registered "cant_help"
    /// replaces the hardcoded fallback text.
    pub templates: std::collections::HashMap<String, String>,
    /// Learned input repairs + guidance (spec M5). Hot-swappable: a driver
    /// replaces the set; each turn loads one snapshot at its start.
    pub learned: std::sync::Arc<arc_swap::ArcSwap<nscore::LearnedRules>>,
    /// Driver B (spec M5 §5): after this much silence on the channel, run the
    /// consolidator once if any turn ran since the last pass. None = off.
    pub idle_after: Option<std::time::Duration>,
    /// M6 §4.1: completed turns rendered verbatim into both model contexts.
    pub window_turns: usize,
    pub caps: nscore::Caps,
    /// M6 §6.5: standing facts shown to both models per turn.
    pub facts_in_context: usize,
    /// M6 §4.5: flag and regenerate (once) a reply that states numbers,
    /// quotes or names absent from everything the model was shown — or that
    /// copies its own prompt instead of answering. Off in replay and probes,
    /// where recorded doubles stand in for the replier.
    pub reply_grounding_check: bool,
    /// M12 T1.2: whether a flagged draft is regenerated, split out of
    /// `reply_grounding_check` so the observation and the billed second call
    /// can be decided apart. On by default, which is today's behaviour;
    /// `main.rs` turns it off under `Capability::Strong`, where the draft
    /// is still flagged and still logged but stands as written.
    pub reply_regenerate: bool,
    /// M12 T4.3: on a chat-tier turn, let the emitter call either act or
    /// answer, and take its answer as the reply. Off by default, and off is
    /// today's two-call chat turn, request for request and event for event.
    ///
    /// Chat only unless [`Self::act_or_answer_every_tier`] is on. A chat turn
    /// has no loop to speak of: its emitter call exists to say "no tool
    /// applies", which is a sentence the same call could have spent on the
    /// user instead.
    pub chat_act_or_answer: bool,
    /// M13 T2.1: make the offer on every tier, so each iteration of the loop
    /// is the model's own choice between calling the next tool and writing
    /// the reply. Off by default; off is the chat-only offer above.
    ///
    /// M12 kept this to Chat on the argument that the split is doing real
    /// work on Task and Deep — one model chooses, the other narrates what
    /// happened — and that a model answering mid-loop would be answering
    /// before the turn is over. The counter-argument, and the reason this
    /// knob exists: the emitter is holding the same trace the replier would
    /// narrate from, so "the turn is over" is a judgement it is in a position
    /// to make, and `respond_directly` was always that judgement in the shape
    /// of a tool call. What it costs is the second reading of the trace by a
    /// model that did not choose the actions, which is a real check on a long
    /// task and dead weight on a short one.
    ///
    /// Only ever read beside `chat_act_or_answer`: on its own it offers
    /// nothing, because the offer itself is that knob.
    pub act_or_answer_every_tier: bool,
    /// M13 T3.1: let one call do both — run the action *and* speak the text
    /// it came with, instead of the text becoming rationale and the turn
    /// buying a second call to say what it just did. Off by default, and
    /// inert unless the offer above is being made at all.
    ///
    /// "Act" and "answer" were alternatives, which left the commonest task
    /// turn there is — do this, and tell me you did — costing two requests to
    /// express. This is the third branch: act, and say so, in one.
    ///
    /// The text is written before the outcome is known, so it can say what is
    /// being done and never what came back. A reply that needs the result is
    /// one the model has to write on a later iteration, and it still can.
    pub act_and_answer: bool,
    /// Reporting threshold, not a gate: a draft at or over this fraction of
    /// one verbatim run out of its own prompt (`echo::echo_ratio`) is logged
    /// as `ReplyEchoed` and then sent as-is. Measured, never acted on — see
    /// plan §8 for the ablation that demoted it. Rides the
    /// `reply_grounding_check` gate; above `1.0` nothing is logged.
    pub max_echo_ratio: f32,
    /// M6 §6.6: the fact scope a session writes to and reads from. The CLI
    /// maps everything to `global`; a multi-user channel maps its chat id.
    pub scope_for: std::sync::Arc<dyn Fn(&nscore::SessionId) -> String + Send + Sync>,
    /// M6 §6.3: policy for `remember_fact` values with no grounding.
    pub remember_residual: RememberResidual,
    /// M6 §6.5: keys with these prefixes are always shown (newest first).
    pub pinned_prefixes: Vec<String>,
    pub pinned_max: usize,
    /// M6 §6.5: facts lexically relevant to the current message.
    pub relevant_max: usize,
    /// M9 T3.1: the activation prior's weight, and the half-life of its
    /// fact term in days. Default 0.0 — pre-M9 ranking exactly.
    ///
    /// Carried here because `[memory]` is this struct's mirror and every
    /// knob in that section has to be readable from one place, but *applied*
    /// on the store: `lexical_rank` and `search_turns` live behind
    /// `MemoryStore`, whose four implementors include two test doubles that
    /// have no ranking to knob. The composition root reads these two and
    /// hands them to `with_activation` on the store it opens; the engine
    /// itself never consults them.
    pub activation_weight: f32,
    pub activation_half_life_days: f32,
    /// M9 T2.1: how many obligations `obligations_for` may extract from one
    /// user message. 0 renders no block at all.
    pub obligations_max: usize,
    /// M9 T2.1: check the drafted reply against this turn's `answer:`
    /// obligations and regenerate once if one went unaddressed. Off until
    /// T0.4's ablation arm has priced the block — the extraction over-fires
    /// on cs+en mixed text, and the cost of a false positive is a second
    /// billed call.
    pub obligation_check: bool,
    /// M9 T2.2: guidance notes rendered into either context, at most. The
    /// tail is dropped and reported; file order is the priority order.
    pub guidance_max: usize,
    /// M12 T3.2: skip guidance notes learned on a model other than
    /// `learning_model`. Off, so the default renders every note, the way it
    /// always did.
    pub archive_foreign_notes: bool,
    /// M12 T3.2: the emitter model this deployment runs, as the composition
    /// root resolved it. Only `archive_foreign_notes` reads it; the engine
    /// otherwise has no model id at render time.
    pub learning_model: Option<String>,
    /// M9 T0.4: blank one context block after the fit, to measure what it
    /// was worth. Set programmatically by the evaluation harness only —
    /// there is deliberately no config key for it, because an ablated engine
    /// answering a real user is a worse engine on purpose.
    pub ablate: Option<nscore::Ablate>,
    /// M6 §5.1: summarize once this many turns have fallen out of the
    /// window since the last summary; 0 = no rolling summary.
    pub summary_every_turns: usize,
    /// Every Nth summary is rebuilt from all verbatim records with no
    /// previous summary, bounding drift; 0 = never rebuild.
    pub summary_rebuild_every: usize,
    pub summary_max_chars: usize,
    /// Verbatim input cap; the oldest records are dropped first.
    pub summary_input_max_chars: usize,
    /// M6 §7: hits per source the `recall` action returns.
    pub recall_top_k: usize,
    /// Whether an irreversible action must be confirmed before it runs.
    ///
    /// True is the default and the right answer for a harness a person is
    /// sitting in front of: `SideEffectGate` stages the proposal and asks,
    /// naming what will actually happen.
    ///
    /// False removes that gate entirely, for a session meant to run
    /// unattended for long stretches — there is nobody at the keyboard to
    /// answer, and a staged proposal would simply stall until there is. It is
    /// a real loosening and worth being deliberate about: every irreversible
    /// action the model proposes then happens, including clicks and typing on
    /// a real desktop, where there is no undo for "sent the email". What
    /// remains is not this gate but the machine's own brakes — the local
    /// override that suspends injection the moment a person touches the mouse,
    /// the arming chord, and the badge's pie menu.
    pub confirm_irreversible: bool,
    /// M7 T1.3: how many of this turn's own tool *outcomes* stay verbatim in
    /// the prompt. Older ones are folded into a single counted line; refusals
    /// are never folded, because they are what steer the next proposal.
    /// 0 disables the fold.
    pub trace_verbatim_lines: usize,
    /// M8 T0.2: how many characters of one tool result reach the prompt
    /// before the clip takes over and leaves a handle in its place.
    ///
    /// Settable because the right value is a property of the tools a
    /// deployment actually runs: a `pointer_ui_read` over a 14k control tree
    /// and a two-line shell result do not want the same cap, and until this
    /// could move, `ns-app budget`'s drop count had nothing to be compared
    /// against.
    pub tool_result_max_chars: usize,
    /// M7 Phase 4: how many earlier sessions of the scope `recall` searches
    /// beyond this one. 0 keeps recall inside the current conversation, as
    /// it was before digests existed.
    pub recall_sessions: usize,
    /// M10 T1.3: which spelling of every tool description the emitter is
    /// shown. `Full` — the default — is today's text unchanged.
    pub schema_profile: nscore::SchemaProfile,
    /// M12 T1.1: which class of model this deployment drives. `Small` — the
    /// default — is today's behaviour in every place that reads it.
    pub capability: nscore::Capability,
    /// M10 T1.4: whether the synthetic tools that cannot apply are left out
    /// of the legal set. On by default, and **off under replay**.
    ///
    /// Applicability is the one thing in the legal set that is read from the
    /// store rather than from this turn's own events, and replay runs against
    /// a fresh double: a session that had two facts when it was recorded has
    /// none when it is replayed, so `forget_all` would be narrowed away and a
    /// recorded proposal would come back `IllegalAction`. Replay must never
    /// *narrow* the set — offering a superset can only turn a rejection back
    /// into the recorded outcome — so it turns the pruning off and compares
    /// what the run actually did.
    pub prune_inapplicable: bool,
    /// M7 Phase 3: decides each turn's tier before the first model call.
    /// `None` — every scripted double and every replay — routes nothing and
    /// behaves exactly as the engine did before the router existed.
    pub router: Option<std::sync::Arc<dyn crate::router::Router>>,
    /// M7 T2.1: the ceiling the composed context is measured against, in
    /// estimated tokens. 0 turns the budget off.
    pub prompt_budget_tokens: u32,
    /// Whether the budget acts on what it finds. `Report` — the default —
    /// records the drops it would make and makes none, so the no-impact rate
    /// can be measured before anything is dropped for real.
    pub budget_mode: nscore::BudgetMode,
    /// M7 T2.3: show the emitter its own size against its own ceiling.
    /// An experiment, off by default: VISTA reports a large gain from it on
    /// a Flash-class model, and whether a 3B emitter acts on the line at all
    /// is exactly what the task set is for.
    pub show_budget_line: bool,
    /// Multi-conversation plan Phase 2: how many turns may run at once
    /// across all sessions (`dispatch::Dispatcher`). Every session is still
    /// one turn at a time; this bounds how many *sessions* are mid-turn.
    ///
    /// `1` is the CLI's serial behaviour — one conversation, one turn, the
    /// next message waits. A larger number buys overlap of *waiting* (the
    /// user typing, tool latency, a desktop action in flight) under a
    /// request budget that stays global: every model call still goes
    /// through the one per-provider throttle and counts against the same
    /// daily allowance, so N slots never mean N times the requests
    /// (findings §2.9).
    pub worker_slots: usize,
    /// M8 T3.2/T3.3: whether recall may take the hybrid path — bm25 ∪ vector
    /// candidates, fused by rank, reranked — instead of staying lexical.
    ///
    /// Off by default, as M8 §8 writes it, and the store is the second gate:
    /// without an encoder and without stored vectors `search_turns_hybrid`
    /// *is* `search_turns_in`, so turning this on against a store that has
    /// neither changes nothing at all.
    ///
    /// **Never reached from `Chat`.** A conversational turn's recall stays
    /// lexical however this is set, which is the tier rule stated once here
    /// and asserted by `a_chat_tier_turn_issues_no_embed_call`. The reason is
    /// the one M8 §6 gives for the whole phase: coarse-vector-to-rerank costs
    /// a round trip of about half a second, and a tier whose budget is half
    /// the ceiling is not where that is spent.
    pub recall_hybrid: bool,
    /// M9 T5.3 / M10 T3.6: how many nearest earlier conversations the deep
    /// tier's pre-emptive step may bring in as exemplars. **0 — off — until
    /// `--ablate` decides otherwise**, which is the M9 rule for every knob
    /// whose fixture does not exist yet.
    ///
    /// An exemplar enters as a `ToolReturned`, never as a context block.
    /// That is not presentation: a context block has no provenance, no
    /// trust and no call to point at, and a digest built from External tool
    /// output stays External (M6 §5.1, the laundering rule). As a tool
    /// return it carries the digests' *lowest* trust, it is in the
    /// provenance index, and it replays.
    pub exemplars_max: usize,
    /// M12 T6.1: the hard ceiling on model requests this engine may spend
    /// before it stops taking turns. `None` — the default — is no ceiling,
    /// which is every deployment but a metered one.
    ///
    /// Counted across every role, the summarizer included, from the moment
    /// the engine was built; checked at the start of a turn, never inside
    /// one, so a run stops between turns rather than mid-flight with a tool
    /// called and nothing said about it.
    pub max_requests: Option<u32>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            max_emit_retries: 3,
            persona: String::new(),
            templates: Default::default(),
            learned: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                nscore::LearnedRules::default(),
            )),
            idle_after: None,
            window_turns: 6,
            caps: nscore::Caps::default(),
            facts_in_context: 10,
            reply_grounding_check: true,
            reply_regenerate: true,
            chat_act_or_answer: false,
            act_or_answer_every_tier: false,
            act_and_answer: false,
            max_echo_ratio: 0.6,
            scope_for: std::sync::Arc::new(|_| "global".to_string()),
            remember_residual: RememberResidual::Flag,
            pinned_prefixes: vec!["user.".into()],
            pinned_max: 5,
            relevant_max: 5,
            activation_weight: 0.5,
            activation_half_life_days: 7.0,
            obligations_max: 5,
            obligation_check: false,
            guidance_max: 6,
            archive_foreign_notes: false,
            learning_model: None,
            ablate: None,
            summary_every_turns: 4,
            summary_rebuild_every: 3,
            summary_max_chars: 800,
            summary_input_max_chars: 6000,
            recall_top_k: 5,
            // The safe default: ask before anything irreversible.
            confirm_irreversible: true,
            trace_verbatim_lines: 5,
            tool_result_max_chars: DEFAULT_TOOL_RESULT_MAX_CHARS,
            recall_sessions: 3,
            schema_profile: nscore::SchemaProfile::Full,
            capability: nscore::Capability::Small,
            prune_inapplicable: true,
            router: None,
            prompt_budget_tokens: 6000,
            budget_mode: nscore::BudgetMode::Report,
            show_budget_line: false,
            worker_slots: 1,
            recall_hybrid: false,
            exemplars_max: 0,
            max_requests: None,
        }
    }
}

pub struct Engine {
    parts: HarnessParts,
    cfg: EngineConfig,
    clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
    /// Always-on guard chain, checked before plugin guards. Plugins cannot
    /// remove these (spec §5.4).
    builtin_guards: Vec<Box<dyn nscore::Guard>>,
    /// M12 T6.1: model requests recorded since this engine was built, what
    /// `max_requests` is measured against. Every role counts, and it is
    /// incremented where the calls are already counted once —
    /// `record_model_calls` — so a role that books usage is capped by the
    /// fact that it books usage, with nothing to keep in step.
    spent: std::sync::atomic::AtomicU32,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("store: {0}")]
    Store(#[from] nscore::StoreError),
    #[error("channel: {0}")]
    Channel(String),
    /// M12 T6.1: `max_requests` is spent, so no further turn is started.
    /// Not a failure of the turn it is returned from — that turn made no
    /// call at all — but the end of a metered run.
    #[error("request cap {cap} reached after {spent} requests")]
    RequestCap { spent: u32, cap: u32 },
}

pub const FALLBACK_REPLY: &str = "Sorry, I couldn't complete that.";

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Turn a model-side error detail into a short, user-facing cause. Recognizes
/// the client's "status NNN: <body>" shape and quotes the provider's own
/// message when the body carries one (OpenAI/OpenRouter `error.message`,
/// Mistral `message`), so a 429/402 says why instead of a bare "Sorry".
fn explain_error(detail: &str) -> String {
    let detail = detail.strip_prefix("transport: ").unwrap_or(detail);
    if let Some(rest) = detail.strip_prefix("malformed: ") {
        return format!(
            "the model's answer was unusable ({})",
            truncate_chars(rest, 120)
        );
    }
    if let Some(rest) = detail.strip_prefix("status ") {
        let (code, body) = rest.split_once(':').unwrap_or((rest, ""));
        let (code, body) = (code.trim(), body.trim());
        let message = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                [&v["error"]["message"], &v["message"]]
                    .into_iter()
                    .find_map(|m| m.as_str().map(str::to_string))
            });
        return match message {
            Some(m) => format!(
                "the model provider answered HTTP {code}: {}",
                truncate_chars(&m, 160)
            ),
            None => format!("the model provider answered HTTP {code}"),
        };
    }
    format!(
        "couldn't reach the model provider ({})",
        truncate_chars(detail, 120)
    )
}

/// M13 T3.1: did the proposal this turn most recently decided on actually
/// run? Read backwards over this turn's own events and stop at the first
/// thing that answers it: a `ToolReturned` means it ran, a `Rejected` means a
/// guard refused it. Neither means nothing was decided yet.
///
/// Asked of the log rather than tracked in a local, because "the action ran"
/// is true at six different places in the loop — one per builtin plus the
/// registered-tool path — and a flag set at six sites is a flag that is
/// eventually set at five.
fn last_proposal_ran(events: &[nscore::Event], turn: u32) -> bool {
    events
        .iter()
        .rev()
        .take_while(|e| e.turn == turn)
        .find_map(|e| match &e.kind {
            EventKind::ToolReturned { .. } => Some(true),
            EventKind::Rejected { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// Every action the engine itself puts in a legal set, in one list.
///
/// The registry holds the tools a deployment wired in; these seven are
/// compiled in, and a report that prices a recorded tool array needs both or
/// it can price neither (M10 T0.1). Exposed as specs rather than as names
/// because the price is the schema, not the label. Nothing here is a
/// statement about which of them were *legal* on any given call — that is
/// what the manifest's `tool_names` records.
pub fn synthetic_specs(profile: nscore::SchemaProfile) -> Vec<nscore::ActionSpec> {
    vec![
        ask_clarification_spec(profile),
        confirm_pending_spec(profile),
        remember_fact_spec(profile),
        forget_fact_spec(profile),
        forget_all_spec(profile),
        recall_spec(profile),
        inspect_result_spec(profile),
    ]
}

/// Engine-owned synthetic action: ask the user one question (spec §5.1).
pub const ASK_CLARIFICATION: &str = "ask_clarification";

fn ask_clarification_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: ASK_CLARIFICATION.into(),
        description: profile
            .pick(
                "Ask the user one short question to resolve missing or ungrounded \
                 information required by the next action.",
                "Ask the user one short question the next action needs answered.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: the user just confirmed the staged action.
pub const CONFIRM_PENDING: &str = "confirm_pending";

fn confirm_pending_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: CONFIRM_PENDING.into(),
        description: profile
            .pick(
                "The user has just confirmed the pending action; execute it.",
                "Execute the pending action the user just confirmed.",
            )
            .into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: store one durable fact.
pub const REMEMBER_FACT: &str = "remember_fact";

fn remember_fact_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: REMEMBER_FACT.into(),
        description: profile
            .pick(
                "Store one durable fact about the user or task as key/value \
                 (dotted keys, e.g. user.name).",
                "Store one durable fact under a dotted key, e.g. user.name.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["key", "value"]
        }),
        side_effect: nscore::SideEffect::Reversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: soft-delete one fact (M6 §6.2).
pub const FORGET_FACT: &str = "forget_fact";

fn forget_fact_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_FACT.into(),
        description: profile
            .pick(
                "Delete one stored fact by its key (e.g. user.name). Only when the user \
                 explicitly asks to forget or remove something stored.",
                "Delete one stored fact by key, only when the user asks to forget it.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "key": { "type": "string" } },
            "required": ["key"]
        }),
        side_effect: nscore::SideEffect::Reversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: purge every fact in the session's scope
/// (M6 §6.2). Irreversible: staged behind the confirmation flow.
pub const FORGET_ALL: &str = "forget_all";

fn forget_all_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_ALL.into(),
        description: profile
            .pick(
                "Erase everything stored about the user; asks for confirmation first. Only \
                 when the user explicitly asks to reset or wipe the memory.",
                "Erase every stored fact, only when the user asks to wipe the memory; \
                 confirmation is asked first.",
            )
            .into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Irreversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: search memory beyond the context (M6 §7).
pub const RECALL: &str = "recall";

fn recall_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: RECALL.into(),
        description: profile
            .pick(
                "Search earlier turns of this conversation and stored facts for words the \
                 user is asking about. Use when the answer is not in the recent turns \
                 or facts shown.",
                "Search earlier turns and stored facts when the answer is not in what is \
                 shown.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned, engine-*run* action: the nearest earlier conversations
/// (M9 T5.3, M10 T3.6).
///
/// It has a spec because every call in the log has one — `classify` reads it
/// for provenance and a replay resolves the call through it — but it is
/// never put in a legal set and never offered to the emitter. The deep tier
/// runs it for the same reason it runs `recall` itself: an emitter iteration
/// spent asking for context is a request that bought no progress.
pub const EXEMPLARS: &str = "exemplars";

fn exemplars_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: EXEMPLARS.into(),
        description: "Earlier conversations most like this one, by meaning.".into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: read more of a clipped tool result
/// (M7 T1.2).
pub const INSPECT_RESULT: &str = "inspect_result";

fn inspect_result_spec(profile: nscore::SchemaProfile) -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: INSPECT_RESULT.into(),
        description: profile
            .pick(
                "Read more of a tool result that was shown clipped. `id` is the handle in \
                 the trace, like r42. With `query`, returns the part of the result around \
                 the first match; without one, the next part. For a desktop, prefer \
                 pointer_ui_find, which searches the live screen instead.",
                "Read more of a clipped tool result by its trace handle, like r42; with a \
                 query, the part around the first match.",
            )
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "query": { "type": "string" }
            },
            "required": ["id"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}


/// What to do with a remembered value nothing in the session grounds
/// (M6 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberResidual {
    /// Store at confidence 0.5, shown as `(unverified)`; restatement promotes.
    Flag,
    /// Deny like any NeverResidual arg; forced clarification follows.
    Never,
}

impl Engine {
    pub fn new(parts: HarnessParts, cfg: EngineConfig) -> Self {
        Self::with_clock(
            parts,
            cfg,
            Box::new(|| {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Timestamp(ms)
            }),
        )
    }

    pub fn with_clock(
        parts: HarnessParts,
        cfg: EngineConfig,
        clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> Self {
        let guards = builtin_guards(cfg.confirm_irreversible);
        Self {
            parts,
            cfg,
            clock,
            builtin_guards: guards,
            spent: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// The wiring, for the dispatcher (`dispatch.rs`): it runs the
    /// consolidator against the memory and hands the channel to the session
    /// tasks. Crate-private so the roles stay the engine's to call.
    pub(crate) fn parts(&self) -> &HarnessParts {
        &self.parts
    }

    pub(crate) fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// Identity of a call within a turn: action plus its args as JSON
    /// (serde_json's Map is ordered, so equal objects serialize identically).
    fn call_key(p: &nscore::Proposal) -> String {
        format!("{}\u{0}{}", p.action, p.args)
    }

    /// M6 §6.5: the facts a turn shows both models — a pinned core (keys
    /// under `pinned_prefixes`, newest validated first, never cold) plus the
    /// facts lexically relevant to the current message, within
    /// `facts_in_context`. Dumping the whole store masks precision failures
    /// and irrelevant facts measurably degrade replies (findings §1).
    /// The pinned core alone (M6 §6.5): current facts under
    /// `pinned_prefixes`, newest-validated first. Shown at every tier —
    /// a `Chat` turn that has forgotten the user's name is the failure the
    /// facts block was added to fix, not a saving.
    async fn pinned_facts(&self, scope: &str) -> Vec<nscore::Fact> {
        let live = self.parts.memory.facts(scope, "").await.unwrap_or_default();
        let mut pinned: Vec<nscore::Fact> = live
            .iter()
            .filter(|f| f.state == nscore::FactState::Current)
            .filter(|f| self.is_pinned(f))
            .cloned()
            .collect();
        pinned.sort_by(|a, b| {
            b.last_validated
                .cmp(&a.last_validated)
                .then_with(|| a.key.cmp(&b.key))
        });
        pinned.truncate(self.cfg.pinned_max);
        pinned
    }

    /// The pinned core plus the query-relevant slice.
    ///
    /// `hybrid` is the caller's decision, never this function's: M11 T1.1
    /// landed `search_facts_hybrid` in the store and left every caller on
    /// `search_facts`, and the follow-up is that the fact path takes the same
    /// gate the turn path already takes at `recall_outcome` —
    /// `cfg.recall_hybrid` **and** a tier above `Chat`. A `Chat` turn is the
    /// cheap one by construction (it is already denied the query-relevant
    /// facts on the emitter side and every registered tool), and paying a
    /// round trip to `/embed` and `/rerank` on it would spend the tier's whole
    /// saving on its reply prompt. With `hybrid` false this is byte-for-byte
    /// what it always was, and with no encoder `search_facts_hybrid` is
    /// `search_facts` anyway — the knob can only ever add.
    async fn select_facts(&self, scope: &str, user_text: &str, hybrid: bool) -> Vec<nscore::Fact> {
        let pinned = self.pinned_facts(scope).await;
        let k = self.cfg.relevant_max + pinned.len();
        let relevant = if hybrid {
            self.parts
                .memory
                .search_facts_hybrid(scope, user_text, k)
                .await
        } else {
            self.parts.memory.search_facts(scope, user_text, k).await
        }
        .unwrap_or_default();
        let mut out = pinned;
        for f in relevant {
            if out.len() >= self.cfg.facts_in_context
                || out.iter().filter(|o| !self.is_pinned(o)).count() >= self.cfg.relevant_max
            {
                break;
            }
            if !out.iter().any(|o| o.key == f.key) {
                out.push(f);
            }
        }
        out.truncate(self.cfg.facts_in_context);
        out
    }

    fn is_pinned(&self, f: &nscore::Fact) -> bool {
        self.cfg
            .pinned_prefixes
            .iter()
            .any(|p| f.key.starts_with(p))
    }

    /// M6 §5.1: fold the turns that have fallen out of the window into the
    /// rolling summary, off the user's critical path (called after the
    /// reply is sent). Returns whether a `Summarized` event was appended.
    /// A summarizer failure appends nothing; the next boundary retries with
    /// the larger range.
    pub async fn maybe_summarize(&self, sid: &nscore::SessionId) -> Result<bool, EngineError> {
        let every = self.cfg.summary_every_turns as u32;
        if every == 0 {
            return Ok(false);
        }
        let stored = self.parts.memory.load(sid).await?;
        let n_loaded = stored.len();
        let state = fold(&stored);
        let through = state.turn.saturating_sub(self.cfg.window_turns as u32);
        let last = state.summary.as_ref().map(|s| s.through_turn).unwrap_or(0);
        if through < 1 || through.saturating_sub(last) < every {
            return Ok(false);
        }
        // Drift control: every Nth summary is rebuilt from verbatim records
        // alone, so summary-of-summary chains stay short (findings §2).
        let rebuild = self.cfg.summary_rebuild_every > 0
            && (state.summaries + 1).is_multiple_of(self.cfg.summary_rebuild_every as u32);
        let from = if rebuild { 1 } else { last + 1 };
        let mut records = state.records_in(from, through);
        while records.len() > 1
            && nscore::render_window(&records, records.len(), &self.cfg.caps)
                .chars()
                .count()
                > self.cfg.summary_input_max_chars
        {
            records.remove(0);
        }
        let Some(first) = records.first() else {
            return Ok(false);
        };
        let rebuilt_from = first.turn;
        let scope = (self.cfg.scope_for)(sid);
        // Never hybrid: the query is the empty string, so there is nothing
        // for a cosine arm to be near, and the summary runs off the user's
        // critical path precisely so it costs no round trips it does not
        // need.
        let selected = self.select_facts(&scope, "", false).await;
        let facts = self.fact_views(&scope, &selected).await;
        let previous = if rebuild {
            None
        } else {
            state.summary.as_ref()
        };
        // This call's own sink (M7 T0.1; multi-conversation plan Phase 1):
        // once more than one session is live the summary can run beside a
        // turn, and a sink shared with that turn would hand this call's
        // cost to whichever of the two drained first.
        let usage = std::sync::Arc::new(nscore::UsageSink::new());
        let input = nscore::SummaryInput {
            previous,
            records: &records,
            caps: &self.cfg.caps,
            facts: &facts,
            usage: Some(usage.clone()),
        };
        // The manifest for this call: the summarizer is shown the standing
        // facts and a range of verbatim records, and no tools or trace.
        let manifest = nscore::ContextManifest {
            fact_keys: facts.iter().map(|f| f.key.clone()).collect(),
            summary_through: previous.map(|s| s.through_turn),
            window: window_range(&records),
            scope: Some(scope.clone()),
            ..Default::default()
        };
        let summarized = self.parts.summarizer.summarize(input).await;
        // The log is opened before the outcome is known, because a summarizer
        // that spent a request and produced nothing usable has still spent
        // it. Leaving that call undrained would attribute it to whichever
        // turn came next; recording it says plainly that a request bought
        // no summary.
        let mut log = EventLog::from_events(sid.clone(), stored);
        self.record_model_calls(&usage, &mut log, state.turn, &manifest);
        let draft = match summarized {
            Ok(Some(d)) => d,
            Ok(None) => return self.persist_summary_events(sid, &log, n_loaded).await,
            Err(e) => {
                eprintln!("summarizer: {e}");
                return self.persist_summary_events(sid, &log, n_loaded).await;
            }
        };
        // A summary built from external tool output stays external: the
        // summarizer is a laundering channel otherwise (findings §5).
        let trusts: Vec<nscore::Trust> = records.iter().map(|r| r.trust).collect();
        let mut summary = nscore::SessionSummary {
            through_turn: through,
            topic: draft.topic,
            established: draft.established,
            open: draft.open,
            trust: nscore::min_trust(&trusts),
            rebuilt_from,
        };
        summary.clamp(self.cfg.summary_max_chars);
        log.append(
            state.turn,
            (self.clock)(),
            EventKind::Summarized { summary },
        );
        self.parts
            .memory
            .append(sid, &log.events()[n_loaded..])
            .await?;
        Ok(true)
    }

    /// Persist whatever the summary attempt appended and report that no
    /// summary was written. On the failure paths that is the `ModelCall` of
    /// a request that bought nothing — the one thing worth keeping from a
    /// summary that did not happen.
    async fn persist_summary_events(
        &self,
        sid: &nscore::SessionId,
        log: &EventLog,
        from: usize,
    ) -> Result<bool, EngineError> {
        if log.events().len() > from {
            self.parts.memory.append(sid, &log.events()[from..]).await?;
        }
        Ok(false)
    }

    /// Views of `facts` for the contexts; pinned keys carry the value they
    /// superseded (M6 §6.1: "what was my name before" from context alone).
    async fn fact_views(&self, scope: &str, facts: &[nscore::Fact]) -> Vec<nscore::FactView> {
        let mut views = Vec::with_capacity(facts.len());
        for f in facts {
            let mut view: nscore::FactView = f.into();
            if self.is_pinned(f) {
                let history = self
                    .parts
                    .memory
                    .fact_history(scope, &f.key)
                    .await
                    .unwrap_or_default();
                view.previous = history
                    .iter()
                    .find(|h| h.state == nscore::FactState::Superseded && h.value != f.value)
                    .and_then(|h| h.valid_to.map(|t| (h.value.clone(), t)));
            }
            views.push(view);
        }
        views
    }

    /// Write everything appended so far to the store, without waiting for the
    /// end of the turn.
    ///
    /// `run_turn` otherwise appends once, after `Replied`. Between an action
    /// that really happened — a click on a desktop, a purged fact table — and
    /// that append sit the turn's remaining iterations and the reply model, a
    /// network call that can hang or fail. A crash anywhere in there leaves
    /// the world changed and nothing in the log saying so, which is the one
    /// inconsistency this engine's design does not otherwise permit: every
    /// context is a projection of the log, so what the log missed did not
    /// happen. Pure results are recomputable and do not pay for this.
    ///
    /// `MemoryStore::append` skips ids it already holds, so this costs one
    /// statement and leaves the end-of-turn append writing exactly the
    /// remainder. A failure here is reported and not fatal: the same append
    /// runs again at the end of the turn, and *that* one propagates. Ending
    /// the turn early on a store error would abandon it after the side effect
    /// rather than before.
    /// Route this turn, or hand back `Task` when no router is installed —
    /// which is what the engine did before there was one, so an engine
    /// without a router is unchanged rather than differently behaved.
    fn route_turn(
        &self,
        user_text: &str,
        events: &[nscore::Event],
        turn: u32,
    ) -> crate::router::Route {
        let Some(router) = &self.cfg.router else {
            return crate::router::Route {
                tier: nscore::Tier::Task,
                cues: Vec::new(),
                // No router, no narrowing: the full set, which is what every
                // scripted double and every replay has always been sent.
                tools: None,
            };
        };
        let state = fold(events);
        // The same expiry rule the loop applies: a pending confirmation is
        // live only on the turn after the one that staged it.
        let pending_confirmation = state
            .pending_confirmation
            .filter(|_| state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false))
            .is_some();
        // Precise, and cheaper than reading it back out of a rendered record:
        // did the turn immediately before this one actually call a tool.
        let previous_turn_used_tools = events
            .iter()
            .any(|e| e.turn + 1 == turn && matches!(e.kind, EventKind::ToolCalled { .. }));
        let tool_names: Vec<String> = self
            .parts
            .tools
            .iter()
            .map(|t| t.spec().name.clone())
            .collect();
        router.route(&crate::router::RouteInput {
            user_text,
            pending_confirmation,
            previous_turn_used_tools,
            tool_names: &tool_names,
        })
    }

    /// The `recall` search itself (M6 §7): verbatim turns beyond the window
    /// first, then live facts, with the lowest trust among them.
    ///
    /// One function because two callers need it — the `recall` action, and
    /// the `Deep` tier running it pre-emptively (M7 Phase 3). Two copies
    /// would drift, and the one that drifted would be the one a model reached
    /// for after the other had already failed it.
    ///
    /// M8 T3.3: `tier` is here and not inferred because the hybrid arm is
    /// tier-gated, and both callers know their tier. `Chat` recall stays
    /// lexical — a conversational turn never dials a model — and the
    /// `recall` action is reachable from `Chat`, so the gate cannot live at
    /// the pre-emptive call site alone.
    async fn recall_outcome(
        &self,
        sid: &nscore::SessionId,
        scope: &str,
        query: &str,
        turn: u32,
        tier: nscore::Tier,
    ) -> ToolOutcome {
        let k = self.cfg.recall_top_k;
        let hybrid = self.cfg.recall_hybrid && tier != nscore::Tier::Chat;
        // Turns already visible in the window (and this one) add nothing.
        let visible_from = turn.saturating_sub(self.cfg.window_turns as u32);
        let mut lines: Vec<String> = Vec::new();
        let mut trusts: Vec<nscore::Trust> = Vec::new();
        let mut failure: Option<String> = None;
        let this_session = std::slice::from_ref(sid);
        let within = if hybrid {
            self.parts
                .memory
                .search_turns_hybrid(this_session, query, k * 3)
                .await
        } else {
            self.parts.memory.search_turns(sid, query, k * 3).await
        };
        match within {
            Ok(hits) => {
                for h in hits.into_iter().filter(|h| h.turn < visible_from).take(k) {
                    trusts.push(if h.speaker == "user" {
                        nscore::Trust::User
                    } else {
                        nscore::Trust::System
                    });
                    lines.push(format!("t{} {}: {}", h.turn, h.speaker, h.text));
                }
            }
            Err(e) => failure = Some(e.to_string()),
        }
        // Then the sessions before this one (M7 Phase 4). Only digested
        // sessions are searched, which is exactly what "an earlier
        // conversation" means here: the consolidator writes a digest once a
        // session has a summary, so one still in progress is not among them.
        if self.cfg.recall_sessions > 0 {
            match self
                .parts
                .memory
                .session_digests(scope, self.cfg.recall_sessions + 1)
                .await
            {
                Ok(digests) => {
                    let earlier: Vec<nscore::SessionId> = digests
                        .into_iter()
                        .map(|d| d.session)
                        .filter(|s| s != sid)
                        .take(self.cfg.recall_sessions)
                        .collect();
                    if !earlier.is_empty() {
                        let across = if hybrid {
                            self.parts
                                .memory
                                .search_turns_hybrid(&earlier, query, k)
                                .await
                        } else {
                            self.parts.memory.search_turns_in(&earlier, query, k).await
                        };
                        match across {
                            Ok(hits) => {
                                for h in hits.into_iter().take(k) {
                                    trusts.push(if h.speaker == "user" {
                                        nscore::Trust::User
                                    } else {
                                        nscore::Trust::System
                                    });
                                    lines.push(format!(
                                        "in an earlier conversation, t{} {}: {}",
                                        h.turn, h.speaker, h.text
                                    ));
                                }
                            }
                            Err(e) => failure = Some(e.to_string()),
                        }
                    }
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        match self.parts.memory.search_facts(scope, query, k).await {
            Ok(facts) => {
                for f in facts {
                    trusts.push(f.trust);
                    lines.push(format!(
                        "from memory, {} is {}",
                        f.key,
                        value_text(&f.value)
                    ));
                }
            }
            Err(e) => failure = Some(e.to_string()),
        }
        // Digests last. They are summaries of summaries, and the controlled
        // ablation this design rests on puts extracted artifacts 16–22 points
        // below verbatim text (findings 2026-09-02 §1). They earn their place
        // by answering what a whole earlier conversation was about, which no
        // single verbatim line does.
        if self.cfg.recall_sessions > 0 {
            match self.parts.memory.search_digests(scope, query, k).await {
                Ok(digests) => {
                    for d in digests.into_iter().filter(|d| &d.session != sid) {
                        trusts.push(d.summary.trust);
                        lines.push(format!(
                            "an earlier conversation (through t{}) was about: {}",
                            d.last_turn, d.summary.topic
                        ));
                    }
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        match failure {
            Some(detail) => ToolOutcome::Err {
                kind: "store".into(),
                detail,
            },
            None if lines.is_empty() => ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: "no matches".into(),
                    artifact: None,
                    trust: nscore::Trust::System,
                },
            },
            // Joined, not JSON: brackets and escaped quotes are pure
            // copy-bait for the reply model and buy nothing, since nothing
            // parses this back (plan §3, phase 1). One line, because
            // `turn_trace` is line-per-event.
            None => ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: lines.join("; "),
                    artifact: None,
                    trust: nscore::min_trust(&trusts),
                },
            },
        }
    }

    /// The exemplars step (M9 T5.3, M10 T3.6): at most `exemplars_max`
    /// digests of this scope nearest the message by cosine, as **one**
    /// `ToolReturned` carrying their lowest trust.
    ///
    /// One return and not one per digest: they are a single answer to a
    /// single question, and N returns would be N entries in the trace
    /// competing with the turn's real tool results for the verbatim lines
    /// `trace_verbatim_lines` allows.
    ///
    /// Lowest trust, not each digest's own: they arrive folded into one
    /// text, a reader cannot tell which sentence came from which
    /// conversation, and trust that cannot be attributed has to be the
    /// weakest of what it is made of (M6 §5.1).
    ///
    /// The current session is excluded — a conversation is not an exemplar
    /// of itself — and so is a store with no digest vectors, which returns
    /// an empty list and therefore "no similar conversations".
    async fn exemplars_outcome(
        &self,
        sid: &nscore::SessionId,
        scope: &str,
        query: &str,
    ) -> ToolOutcome {
        let digests = match self
            .parts
            .memory
            .nearest_digests(scope, query, self.cfg.exemplars_max + 1)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                return ToolOutcome::Err {
                    kind: "store".into(),
                    detail: e.to_string(),
                }
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let mut trusts: Vec<nscore::Trust> = Vec::new();
        for d in digests
            .into_iter()
            .filter(|d| &d.session != sid)
            .take(self.cfg.exemplars_max)
        {
            trusts.push(d.summary.trust);
            lines.push(format!(
                "a similar earlier conversation (through t{}) was about: {}",
                d.last_turn, d.summary.topic
            ));
        }
        if lines.is_empty() {
            return ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: "no similar conversations".into(),
                    artifact: None,
                    trust: nscore::Trust::System,
                },
            };
        }
        ToolOutcome::Ok {
            output: nscore::ToolOutput {
                summary: lines.join("; "),
                artifact: None,
                trust: nscore::min_trust(&trusts),
            },
        }
    }

    /// Append one `ModelCall` for every provider call recorded in `sink`
    /// since its last drain (M7 T0.1).
    ///
    /// Called immediately after each of the engine's own model calls, with
    /// the sink that call's context carried — one per turn, one per summary
    /// — so what the drain returns is that call's and the manifest describes
    /// what it was shown, even while another session's turn is in flight
    /// (multi-conversation plan Phase 1, findings §2.6). Nothing here reads
    /// a process-wide sink: a client whose context carries none records into
    /// its own, and that one belongs to the calls made outside a turn.
    ///
    /// Retries inside one call do not appear as separate events — they are
    /// counted in `Usage::attempts`, because the thing a reader wants to
    /// know is what one decision cost, requests included.
    fn record_model_calls(
        &self,
        sink: &nscore::UsageSink,
        log: &mut EventLog,
        turn: u32,
        manifest: &nscore::ContextManifest,
    ) {
        let mut calls = 0u32;
        for usage in sink.drain() {
            log.append(
                turn,
                (self.clock)(),
                EventKind::ModelCall {
                    usage,
                    manifest: manifest.clone(),
                },
            );
            calls += 1;
        }
        // M12 T6.1: what the request cap is measured against. Here because
        // this is the one place the engine's own calls are already counted.
        self.spent
            .fetch_add(calls, std::sync::atomic::Ordering::SeqCst);
    }

    /// M12 T6.1: whether this engine may start another turn.
    ///
    /// Checked at the start of a turn and again once it has ended — an
    /// engine over its ceiling refuses the *next* turn, rather than cutting
    /// the one in flight, whose tool calls have already happened and whose
    /// reply is owed to whoever is reading. The check after the turn is the
    /// same check: it is the one the next `run_turn` makes.
    fn cap_reached(&self) -> Result<(), EngineError> {
        let Some(cap) = self.cfg.max_requests else {
            return Ok(());
        };
        let spent = self.spent.load(std::sync::atomic::Ordering::SeqCst);
        if spent >= cap {
            return Err(EngineError::RequestCap { spent, cap });
        }
        Ok(())
    }

    async fn flush(&self, sid: &nscore::SessionId, log: &EventLog, from: usize) {
        if let Err(e) = self.parts.memory.append(sid, &log.events()[from..]).await {
            eprintln!("store: {e}");
        }
    }

    pub async fn run_turn(&self, incoming: Incoming) -> Result<String, EngineError> {
        // Before anything is loaded or called: a capped run stops between
        // turns, with the message it could not afford left unanswered
        // rather than half answered.
        self.cap_reached()?;
        let sid = incoming.session.clone();
        let scope = (self.cfg.scope_for)(&sid);
        let stored = self.parts.memory.load(&sid).await?;
        let n_loaded = stored.len();
        let mut log = EventLog::from_events(sid.clone(), stored);
        // This turn's own sink for what its model calls cost (M7 T0.1). It
        // travels in every context the turn builds and is drained right
        // after each call, so a turn on another session running at the same
        // time cannot land its records on this one's `ModelCall`s
        // (multi-conversation plan Phase 1, findings §2.6).
        let usage = std::sync::Arc::new(nscore::UsageSink::new());

        let turn = fold(log.events()).turn + 1;
        let now = &self.clock;
        // One rules snapshot per turn: a driver may swap the set mid-session.
        let rules = self.cfg.learned.load_full();
        log.append(
            turn,
            now(),
            EventKind::UserSaid {
                text: incoming.text.clone(),
            },
        );

        // M7 Phase 3: what kind of turn this is, decided once and before any
        // model call, from the message and this turn's own history only.
        let routed = self.route_turn(&incoming.text, log.events(), turn);
        let mut tier = routed.tier;
        // M10 T2.1: which registered tools ride this turn, chosen once here
        // and held across every iteration. `None` is the full set.
        //
        // Once per turn rather than once per iteration is the whole rule: a
        // set recomputed each pass would change the `tools` array under a
        // provider prefix cache and buy nothing, since nothing between two
        // iterations of one turn changes what the *message* asked for
        // (decision 2, 2026-09-11). Escalation below is the one thing
        // allowed to move it, and it only ever widens.
        let mut selected_tools = routed.tools.clone();
        // `Deep` runs the recall itself rather than waiting to be asked for
        // it. That is the saving: on a fifty-request day an emitter iteration
        // spent proposing `recall` is a request that bought no progress, and
        // the query the emitter would have passed is the user's own message.
        // Recorded as a real call rather than injected as prompt text, so it
        // enters the provenance index, carries its own trust, and replays.
        if tier == nscore::Tier::Deep {
            let args = serde_json::json!({ "query": incoming.text });
            let spec = recall_spec(self.cfg.schema_profile);
            let classified = classify(log.events(), &args, &spec, turn);
            let call_id = log
                .append(
                    turn,
                    now(),
                    EventKind::ToolCalled {
                        action: RECALL.into(),
                        args: classified,
                    },
                )
                .id;
            let outcome = self
                .recall_outcome(&sid, &scope, &incoming.text, turn, tier)
                .await;
            log.append(
                turn,
                now(),
                EventKind::ToolReturned {
                    call: call_id,
                    outcome,
                },
            );

            // M10 T3.6: exemplars — the nearest earlier *conversations*,
            // by cosine over their digests' stored vectors.
            //
            // A second call rather than more lines inside the recall return,
            // because it answers a different question: recall finds the line
            // that says the thing, an exemplar is a whole conversation shaped
            // like this one. Keeping them apart is also what lets `--ablate`
            // decide the default later — a knob folded into another step's
            // output cannot be turned off and measured.
            //
            // Off at `exemplars_max = 0`, which is every deployment today,
            // and the store returns nothing without an encoder, so this is
            // two comparisons on the ordinary path.
            if self.cfg.exemplars_max > 0 {
                let args = serde_json::json!({ "query": incoming.text });
                let spec = exemplars_spec();
                let classified = classify(log.events(), &args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: EXEMPLARS.into(),
                            args: classified,
                        },
                    )
                    .id;
                let outcome = self
                    .exemplars_outcome(&sid, &scope, &incoming.text)
                    .await;
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
            }
        }

        // M10 T1.4: applicability, asked once per turn rather than per
        // iteration. A tool in the schema that cannot do anything is tokens
        // spent on a choice that can only fail — `forget_fact` and
        // `forget_all` were sent on all 21 recorded turns while the store
        // held zero facts (~216 tokens a turn), and `recall` was sent on
        // turn 1 (findings §8.1).
        //
        // This is store state deciding legality, which the narrowing below
        // deliberately avoids for *this turn's own* events. The difference
        // is that these two questions are answered before the loop and held
        // fixed across it, so an iteration cannot see the set change under
        // it; and both fail **open** — a store that errors keeps the tool.
        let scope_holds_facts = !self.cfg.prune_inapplicable
            || self
                .parts
                .memory
                .facts(&scope, "")
                .await
                .map(|f| !f.is_empty())
                .unwrap_or(true);
        // Recall is worth its schema when there is something out of sight:
        // turns older than the verbatim window, or an earlier conversation
        // in the same scope.
        let recall_applies = !self.cfg.prune_inapplicable || turn > self.cfg.window_turns as u32 || {
            self.cfg.recall_sessions > 0
                && match self
                    .parts
                    .memory
                    .session_digests(&scope, self.cfg.recall_sessions + 1)
                    .await
                {
                    Ok(digests) => digests.into_iter().any(|d| d.session != sid),
                    Err(_) => true,
                }
        };

        let mut rejections_this_turn: Vec<String> = Vec::new();
        let mut denied_this_turn: std::collections::HashSet<String> = Default::default();
        let mut calls_this_turn: std::collections::HashSet<String> = Default::default();
        let mut never_residual_this_turn = false;
        let mut forget_misses: u32 = 0;
        let mut emit_failures: u32 = 0;
        let mut last_emit_error: Option<String> = None;
        // Whether the emitter ever produced a proposal this turn; decides
        // which fallback reason the user is given.
        let mut proposed_this_turn = false;
        let mut settled: Option<ReplyPolicy> = None;
        // M12 T4.3: the reply text the emitter call already produced, when
        // the turn settled on an answer rather than an action. `None` is
        // every turn before M12 and every turn with the knob off.
        let mut pre_draft: Option<String> = None;
        // M13 T3.1: an answer that arrived *beside* an action, held until the
        // action has actually run. It cannot be settled on at proposal time:
        // a guard may still refuse the call, and a reply saying "opening it
        // now" on a turn that opened nothing is worse than a second request.
        let mut answer_with_action: Option<String> = None;

        for _ in 0..self.cfg.max_iterations {
            // M13 T3.1: the held answer, collected one iteration later so the
            // log can say whether the action it was written beside happened.
            // A refused proposal leaves a `Rejected` last, not a
            // `ToolReturned`, and the answer is dropped with it.
            if let Some(text) = answer_with_action.take() {
                if last_proposal_ran(log.events(), turn) {
                    pre_draft = Some(text);
                    let e = log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: ReplyPolicy::Generate,
                        },
                    );
                    settled = Some(match &e.kind {
                        EventKind::Settled { policy } => policy.clone(),
                        _ => unreachable!(),
                    });
                    break;
                }
            }
            // a. project
            let state = fold(log.events());
            // A pending confirmation is active only on the turn immediately
            // following its creation (expiry rule, spec §9).
            let active_pending = state.pending_confirmation.filter(|_| {
                state.pending_turn == Some(turn)
                    || state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false)
            });
            let legal = if never_residual_this_turn {
                // Forced clarification (spec §5.1): a NeverResidual rejection
                // occurred and nothing grounds the arg — the only way forward
                // is to ask (respond_directly stays available at schema level).
                LegalActionSet {
                    actions: vec![ask_clarification_spec(self.cfg.schema_profile)],
                }
            } else {
                // Narrowed schema (spec §2): actions rejected this turn are
                // removed from the set the emitter sees next.
                // A `Chat` turn carries no tool schemas at all. With a desktop
                // wired in that is ten of the seventeen schemas the emitter
                // would otherwise re-send on every iteration of a turn that
                // was never going to click anything. The synthetic actions
                // stay legal at every tier: they are how a turn ends.
                //
                // M12 T2.1: unless the route selected some. A chat turn that
                // asked the time carries exactly the tools its own cue named
                // (`[router] chat_tools`) and nothing else — the selection is
                // the whole allowance there, so the filter below narrows to
                // it the same way, once per turn.
                let mut actions: Vec<_> = if tier.allows_tools() || selected_tools.is_some() {
                    self.parts
                        .tools
                        .iter()
                        .map(|t| t.spec().clone())
                        .filter(|s| !denied_this_turn.contains(&s.name))
                        // M10 T2.1. A *turn*-level decision consulted here
                        // rather than re-taken here: `selected_tools` is
                        // fixed for the loop except when escalation widens
                        // it, so this filter yields the same names on every
                        // iteration and the array's bytes do not move.
                        .filter(|s| {
                            selected_tools
                                .as_ref()
                                .map_or(true, |sel| sel.contains(&s.name))
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                actions.push(ask_clarification_spec(self.cfg.schema_profile));
                if !denied_this_turn.contains(REMEMBER_FACT) {
                    actions.push(remember_fact_spec(self.cfg.schema_profile));
                }
                if !denied_this_turn.contains(RECALL) && recall_applies {
                    actions.push(recall_spec(self.cfg.schema_profile));
                }
                // Offered only while there is something to inspect. An
                // action in the schema that can only fail is a way for a
                // small model to spend an iteration discovering that.
                if !denied_this_turn.contains(INSPECT_RESULT)
                    && !clipped_results(log.events(), turn, self.cfg.tool_result_max_chars).is_empty()
                {
                    actions.push(inspect_result_spec(self.cfg.schema_profile));
                }
                // Forgetting is legal only while it can mean something: not
                // after a fact was written this turn (seen live: "my name is
                // now Peter" ended in forget_fact + a staged forget_all) and
                // not after a forget already ran. Both are this turn's own
                // events, so replay reproduces them; store state ("any facts
                // at all?") must never decide legality.
                let wrote_fact = calls_this_turn
                    .iter()
                    .any(|k| k.starts_with(&format!("{REMEMBER_FACT}\u{0}")));
                let forgot = calls_this_turn.iter().any(|k| k.starts_with("forget_"));
                if !wrote_fact && !forgot && scope_holds_facts {
                    if !denied_this_turn.contains(FORGET_FACT) {
                        actions.push(forget_fact_spec(self.cfg.schema_profile));
                    }
                    if !denied_this_turn.contains(FORGET_ALL) {
                        actions.push(forget_all_spec(self.cfg.schema_profile));
                    }
                }
                if active_pending.is_some() {
                    actions.push(confirm_pending_spec(self.cfg.schema_profile));
                }
                LegalActionSet { actions }
            };

            // b. emitter context (M6 §4.2): the same projection of the log
            // the replier sees. The emitter must see what this turn has
            // already done — otherwise it re-proposes completed actions until
            // max_iterations exhausts — and the standing facts, or it
            // re-remembers them every turn (seen live).
            // Clipped: this is the line that is re-sent on every iteration,
            // so an uncapped tool result is paid for again at every step
            // after it.
            let (trace_so_far, clipped_chars) =
                trace_for_prompt(
                    log.events(),
                    turn,
                    self.cfg.trace_verbatim_lines,
                    self.cfg.tool_result_max_chars,
                );
            // The pinned core is shown at every tier — it is what stops the
            // emitter asking again for a name it already has (M6 F2). The
            // query-relevant slice is what a `Chat` turn does without.
            let selected = if tier.allows_relevant_facts() {
                // M11 T1.1 follow-up: the same gate `recall_outcome` takes.
                // Read here rather than bound once above because `tier` is
                // still mutable at this point — a tool-cued turn is upgraded
                // to `Task` mid-loop, and the next iteration must see it.
                self.select_facts(
                    &scope,
                    &incoming.text,
                    self.cfg.recall_hybrid && tier != nscore::Tier::Chat,
                )
                .await
            } else {
                self.pinned_facts(&scope).await
            };
            let facts = self.fact_views(&scope, &selected).await;
            let legal_names: Vec<String> = legal.actions.iter().map(|a| a.name.clone()).collect();
            // Notes and their hashes together, so the manifest can say which
            // note sat in this prompt (M9 T0.3). The texts go into the
            // context; the hashes are cut to whatever survived to be sent.
            // M12 T3.2: with the archive knob on, a note learned on another
            // emitter never reaches this prompt.
            let guidance_notes = if self.cfg.archive_foreign_notes {
                rules.guidance_notes_for_model(&legal_names, self.cfg.learning_model.as_deref())
            } else {
                rules.guidance_notes_for(&legal_names)
            };
            let mut ctx = nscore::EmitterContext {
                facts,
                summary: state.summary.clone(),
                window: state.window(self.cfg.window_turns),
                caps: self.cfg.caps,
                user_text: incoming.text.clone(),
                // M9 T2.1: a pure function of the message, recomputed each
                // iteration rather than carried, for the same reason the
                // trace is — nothing per-turn is persisted as a column.
                obligations: nscore::obligations_for(&incoming.text, self.cfg.obligations_max),
                trace_so_far,
                pending_confirmation: active_pending.is_some(),
                rejections_this_turn: rejections_this_turn.clone(),
                guidance: guidance_notes.iter().map(|(_, t)| t.clone()).collect(),
                budget_line: None,
                usage: Some(usage.clone()),
                answer: None,
            };

            // c. propose
            let mut confirmed_now = false;
            // The budget runs before the manifest, so the manifest describes
            // the context as sent rather than as composed (M7 T2.1). Under
            // the default `report` mode nothing is dropped and the two are
            // the same; the report still says what enforcing would have cost.
            let budget = nscore::fit_emitter(
                &mut ctx,
                tier.budget(self.cfg.prompt_budget_tokens),
                self.cfg.budget_mode,
                &self.cfg.pinned_prefixes,
                self.cfg.guidance_max,
            );
            if self.cfg.show_budget_line {
                let clipped: Vec<String> =
                    clipped_results(log.events(), turn, self.cfg.tool_result_max_chars)
                    .into_iter()
                    .map(result_handle)
                    .collect();
                ctx.budget_line = Some(budget.line(&clipped));
            }
            // M9 T0.4. After the fit, so the budget report above still
            // counts the block as it was composed and the ablation shows up
            // only in what was rendered and in the manifest's keys.
            match self.cfg.ablate {
                Some(nscore::Ablate::Facts) => ctx.facts.clear(),
                Some(nscore::Ablate::Summary) => ctx.summary = None,
                Some(nscore::Ablate::Guidance) => ctx.guidance.clear(),
                None => {}
            }
            // M12 T4.3: chat-tier only, and only with the knob on. Filled
            // after the fit and the ablation, so `memory_silent` is a
            // statement about the context as sent rather than as composed —
            // the same thing the replier's silence line says.
            // M13 T2.1: on every tier the offer is the same sentence — call
            // the next tool or write the reply — so the loop ends when the
            // model says it is done rather than when it names the action that
            // says so.
            let offered_answer = self.cfg.chat_act_or_answer
                && (self.cfg.act_or_answer_every_tier || tier == nscore::Tier::Chat);
            if offered_answer {
                let reply_guidance = if self.cfg.archive_foreign_notes {
                    rules.guidance_for_reply_model(self.cfg.learning_model.as_deref())
                } else {
                    rules.guidance_for_reply()
                };
                ctx.answer = Some(nscore::AnswerBlocks {
                    persona: self.cfg.persona.clone(),
                    reply_guidance,
                    memory_silent: ctx.facts.is_empty()
                        && ctx.summary.is_none()
                        && !ctx.trace_so_far.iter().any(|l| l.contains("recall")),
                    with_action: self.cfg.act_and_answer,
                });
            }
            // Cut to what survived: nothing drops guidance from the middle,
            // so a prefix is exact, and it keeps `note_hashes.len() ==
            // guidance` true whether the list was clamped or blanked.
            let note_hashes: Vec<String> = guidance_notes
                .iter()
                .take(ctx.guidance.len())
                .map(|(h, _)| h.clone())
                .collect();
            // The names, not just the count (M10 T0.1): `tools_tokens` says
            // what the array cost and nothing about which tool carried it,
            // and the whole of P1 is a decision about which text to cut.
            // `respond_directly` is absent because it is not in the legal
            // set — `build_tools` appends it, and a report adds it back the
            // same way.
            let tool_names: Vec<String> =
                legal.actions.iter().map(|s| s.name.clone()).collect();
            let mut manifest =
                emitter_manifest(&scope, &ctx, tool_names, clipped_chars, note_hashes);
            manifest.budget = Some(budget);
            manifest.ablated = self.cfg.ablate;
            manifest.tier = self.cfg.router.is_some().then_some(tier);
            manifest.route_cues = routed.cues.clone();
            let proposed = self.parts.emitter.propose_or_answer(ctx, &legal).await;
            self.record_model_calls(&usage, &mut log, turn, &manifest);
            // Carried only as far as the `respond_directly` branch below:
            // an answer that arrives beside any other action is not an
            // answer to this turn, and a stale one must not reach the reply.
            let mut emitted_answer: Option<String>;
            let mut proposal = match proposed {
                Ok(e) => {
                    // An answer is only ever taken from a call that was
                    // offered the choice. A double may return one anyway;
                    // with the knob off this turn must be the turn it was
                    // before M12, event for event.
                    emitted_answer = offered_answer.then_some(e.answer).flatten();
                    e.proposal
                }
                Err(e) => {
                    // Four failure classes, three recoveries. A refused or
                    // failed endpoint is not a malformed proposal, and a
                    // terminal one (a model name that does not exist, an
                    // empty balance) will not become one by asking again.
                    let retryable = e.is_retryable();
                    let reason = match &e {
                        nscore::EmitError::Provider { status, detail } => {
                            RejectReason::ProviderUnavailable {
                                status: *status,
                                detail: detail.clone(),
                            }
                        }
                        other => RejectReason::Malformed {
                            detail: other.to_string(),
                        },
                    };
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: nscore::EventId(0),
                            reason,
                        },
                    );
                    rejections_this_turn.push(match &e {
                        nscore::EmitError::Provider { status, .. } => {
                            format!("provider unavailable: HTTP {status}")
                        }
                        other => format!("emitter failure: {other}"),
                    });
                    last_emit_error = Some(e.to_string());
                    emit_failures += 1;
                    if !retryable || emit_failures >= self.cfg.max_emit_retries {
                        break;
                    }
                    continue;
                }
            };

            // d. record proposal
            proposed_this_turn = true;
            let pid = log
                .append(
                    turn,
                    now(),
                    EventKind::Proposed {
                        proposal: proposal.clone(),
                    },
                )
                .id;

            // f0. learned input repairs (spec M5 §3.2). The Proposed event above
            // keeps the raw model output; ToolCalled records what actually ran.
            // Repairs only rewrite the proposal — legality, validation and
            // guards below judge the rewritten proposal exactly as raw output.
            if let Some(to) = rules.alias(&proposal.action) {
                proposal.action = to.to_string();
            }
            rules.normalize(&proposal.action, &mut proposal.args);

            // e. direct reply
            if proposal.action == "respond_directly" {
                // M12 T4.3. `Settled { Generate }` either way: what changes
                // is who drafts, not what the log says happened, so a replay
                // of this turn is the shape it always was.
                pre_draft = emitted_answer.take();
                let e = log.append(
                    turn,
                    now(),
                    EventKind::Settled {
                        policy: ReplyPolicy::Generate,
                    },
                );
                settled = Some(match &e.kind {
                    EventKind::Settled { policy } => policy.clone(),
                    _ => unreachable!(),
                });
                break;
            }

            // e2. M13 T3.1: an answer beside a real action. The model said
            // both what it is doing and that it is doing it, which is the one
            // turn shape act-or-answer could not express: "act" and "answer"
            // were alternatives, so a turn that did something always bought a
            // second call to say so.
            //
            // Held rather than settled, because the action has not run yet
            // (the check at the top of the next iteration collects it), and
            // the text is written *before* the outcome is known. That is the
            // real limit of this knob: the reply can say what is being done
            // and never what came back. An answer that needs the result is
            // one the model must write on a later iteration.
            if self.cfg.act_and_answer {
                answer_with_action = emitted_answer.take();
            }

            // f. legality
            if !legal.contains(&proposal.action) {
                // A misroute is not the model's mistake. If the action exists
                // and only the tier was hiding it, widen the tier and ask
                // again rather than recording a refusal: a refusal here would
                // teach the emitter that a real action is illegal, and the
                // narrowed schema would then keep it illegal for the rest of
                // the turn. One iteration is the honest price of a wrong
                // guess about the message (MemFlow's validator-retries, with
                // no second model). The tier only ever rises, so this cannot
                // loop.
                let registered = self
                    .parts
                    .tools
                    .iter()
                    .any(|t| t.spec().name == proposal.action);
                let tiered_out = tier < nscore::Tier::Task && registered;
                // M10 T2.2: escalation is adaptive depth's discovery path,
                // and the same argument as the tier's. The cue table is a
                // guess about the message; a proposal naming a real tool is
                // the model telling us the guess was wrong, and one widening
                // is cheaper than a turn that cannot reach the action at
                // all. It widens to the *full* set for the rest of the turn
                // rather than adding one name, because a task that needed
                // `pointer_scroll` needs whatever comes after it too — and
                // it is the one legitimate mid-turn change to the `tools`
                // array, so it happens once and never again.
                //
                // A tool already refused this turn is excluded: widening
                // would not make it legal, and the loop would spin.
                let withheld = registered
                    && !denied_this_turn.contains(&proposal.action)
                    && selected_tools
                        .as_ref()
                        .is_some_and(|sel| !sel.contains(&proposal.action));
                if tiered_out || withheld {
                    if tiered_out {
                        tier = nscore::Tier::Task;
                    }
                    if withheld {
                        selected_tools = None;
                        // Recorded, because an escalation is a request
                        // already spent and T0.2's `rejections by reason` is
                        // where that is read — the rate this depth is gated
                        // on (under 5 per 100 proposals) has to come from
                        // the log rather than from a counter nothing
                        // persists.
                        //
                        // Recorded but *not* denied: `denied_this_turn`
                        // would keep the tool illegal for the rest of the
                        // turn, which is exactly what the widening just
                        // undid. It differs from the tier's escalation
                        // (which records nothing) for one reason — the tier
                        // is bounded and self-announcing, while the cue
                        // table is a guess whose error rate is the number
                        // `depth = adaptive` ships on, and a guess nobody
                        // counts is a guess nobody can retire. The line
                        // reaches the emitter through the trace, and it is
                        // true: that proposal was refused on that
                        // iteration. It is left out of
                        // `rejections_this_turn` so it is said once.
                        log.append(
                            turn,
                            now(),
                            EventKind::Rejected {
                                proposal_of: pid,
                                reason: RejectReason::IllegalAction {
                                    action: proposal.action.clone(),
                                },
                            },
                        );
                    }
                    continue;
                }
                let reason = RejectReason::IllegalAction {
                    action: proposal.action.clone(),
                };
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason,
                    },
                );
                rejections_this_turn.push(format!("illegal action: {}", proposal.action));
                denied_this_turn.insert(proposal.action.clone());
                continue;
            }

            // f1. repeat gate (engine-owned): an identical (action, args) call
            // already executed this turn yields no new information. Seen live
            // with small models that ignore "never repeat a completed action"
            // in the prompt. Recorded as a guard denial so the narrowed schema
            // drops the action for the rest of the turn.
            // `inspect_result` is the exception, and not a weakening of the
            // gate: the gate's premise is that an identical call yields no
            // new information, and for a paging action that premise is
            // simply false — the same call is how the next page is asked
            // for. It cannot run away either: the pages end, and an
            // exhausted result leaves the schema.
            if proposal.action != INSPECT_RESULT
                && calls_this_turn.contains(&Self::call_key(&proposal))
            {
                let reason = format!(
                    "identical call to '{}' already executed this turn",
                    proposal.action
                );
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason: RejectReason::GuardDenied {
                            guard: "repeat_gate".into(),
                            reason: reason.clone(),
                        },
                    },
                );
                rejections_this_turn.push(format!("guard repeat_gate: {reason}"));
                denied_this_turn.insert(proposal.action.clone());
                continue;
            }

            // f3. confirmation: legality already guaranteed an ACTIVE pending
            // exists (confirm_pending is legal only then). Append the Confirmed
            // event and swap in the original staged proposal — it re-enters the
            // normal classify→guards→perform pipeline with the gate unlocked.
            if proposal.action == CONFIRM_PENDING {
                let pending_id = active_pending.expect("legality guaranteed an active pending");
                log.append(
                    turn,
                    now(),
                    EventKind::Confirmed {
                        pending: pending_id,
                    },
                );
                let original = log
                    .events()
                    .iter()
                    .find(|e| e.id == pending_id)
                    .and_then(|e| match &e.kind {
                        EventKind::PendingConfirmation { proposal_of, .. } => Some(*proposal_of),
                        _ => None,
                    })
                    .and_then(|orig_id| log.events().iter().find(|e| e.id == orig_id))
                    .and_then(|e| match &e.kind {
                        EventKind::Proposed { proposal } => Some(proposal.clone()),
                        _ => None,
                    });
                match original {
                    Some(orig) => {
                        proposal = orig;
                        confirmed_now = true;
                        // fall through to g with the staged proposal
                    }
                    None => {
                        log.append(
                            turn,
                            now(),
                            EventKind::Rejected {
                                proposal_of: pid,
                                reason: RejectReason::Malformed {
                                    detail: "pending confirmation chain is broken".into(),
                                },
                            },
                        );
                        rejections_this_turn.push("broken confirmation chain".into());
                        continue;
                    }
                }
            }

            // f2. clarification: the question IS the reply (spec §5.1). Runs
            // through classification and guards — TaintPolicy applies to
            // questions; a gated question is re-emitted, not asked.
            if proposal.action == ASK_CLARIFICATION {
                let question = proposal
                    .args
                    .get("question")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let Some(question) = question else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "ask_clarification without question".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("ask_clarification missing question".into());
                    continue;
                };
                let ask_spec = ask_clarification_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &ask_spec, turn);
                let classified = ClassifiedProposal {
                    proposal: proposal.clone(),
                    args: classified_args,
                };
                let guard_ctx = nscore::GuardCtx {
                    spec: &ask_spec,
                    turn,
                    confirmed_this_turn: state.confirmed_this_turn_of == Some(turn),
                    fired_actions: &state.fired_tags,
                    pending_confirmation: None,
                };
                let mut denied: Option<(String, String)> = None;
                for g in self.builtin_guards.iter().chain(self.parts.guards.iter()) {
                    match g.check(&classified, &guard_ctx) {
                        Verdict::Allow => continue,
                        Verdict::Deny { reason } => {
                            denied = Some((g.name().to_string(), reason));
                            break;
                        }
                        Verdict::NeedsConfirmation { prompt } => {
                            denied = Some((g.name().to_string(), prompt));
                            break;
                        }
                    }
                }
                if let Some((guard, reason)) = denied {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: guard.clone(),
                                reason: reason.clone(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard {guard}: {reason}"));
                    denied_this_turn.insert(ASK_CLARIFICATION.to_string());
                    continue;
                }
                let policy = ReplyPolicy::Verbatim { text: question };
                log.append(
                    turn,
                    now(),
                    EventKind::Settled {
                        policy: policy.clone(),
                    },
                );
                settled = Some(policy);
                break;
            }

            // f4. remember_fact: classify (the stored provenance IS the
            // classification of the value), write the fact, log the paper
            // trail, and let the emitter decide what happens next.
            if proposal.action == REMEMBER_FACT {
                let key = proposal
                    .args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let value = proposal
                    .args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let (Some(key), Some(value)) = (key, value) else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "remember_fact needs string key and value".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("remember_fact missing key/value".into());
                    continue;
                };
                // Keys are dotted identifiers (spec of the action). Normalize
                // stray edge punctuation first (seen live: a model reliably
                // emitting ":user.name" — same spirit as the trim normalizer),
                // then reject what remains degenerate (seen live: key ", ").
                let key = key
                    .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .to_string();
                let key_ok = !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c));
                if !key_ok || value.trim().is_empty() {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!(
                                    "remember_fact key must be a dotted identifier and value \
                                     non-empty (got key {key:?})"
                                ),
                            },
                        },
                    );
                    rejections_this_turn
                        .push(format!("remember_fact rejected malformed key {key:?}"));
                    continue;
                }
                let fact_spec = remember_fact_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &fact_spec, turn);
                let prov = classified_args
                    .iter()
                    .find(|(k, _)| k == "value")
                    .map(|(_, tv)| tv.prov.clone())
                    .unwrap_or(nscore::Provenance::Residual);
                // Lifecycle merge (M6 §6.1): a restatement keeps the usage
                // count and re-validates; the same value gains confidence,
                // a new value replaces it at full confidence. Seen live: every
                // re-remember reset `uses` to 0, erasing the consolidation
                // pass's only signal.
                let value_json = serde_json::json!(value);
                let value_trust = classified_args
                    .iter()
                    .find(|(k, _)| k == "value")
                    .map(|(_, tv)| tv.trust)
                    .unwrap_or(nscore::Trust::System);
                let residual = crate::guards::contains_residual(&prov);
                // M6 §6.3: a value nothing grounds is either flagged
                // (stored at half confidence, shown as unverified) or, for
                // deployments where facts drive side effects, refused.
                if residual && self.cfg.remember_residual == RememberResidual::Never {
                    let reason = "NeverResidual: arg 'value' has no grounding in this session";
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: "residual_policy".into(),
                                reason: reason.into(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard residual_policy: {reason}"));
                    never_residual_this_turn = true;
                    denied_this_turn.insert(REMEMBER_FACT.to_string());
                    continue;
                }
                let grounded_confidence = if residual { 0.5 } else { 1.0 };
                // Key canonicalization (M6 §6.1): a spelling variant of an
                // existing key is that key (seen live: memory_reset_requested
                // next to memory.reset.requested).
                let current = self
                    .parts
                    .memory
                    .facts(&scope, "")
                    .await
                    .unwrap_or_default();
                let key = match current.iter().find(|f| f.key == key) {
                    Some(_) => key,
                    None => current
                        .iter()
                        .find(|f| nscore::squash(&f.key) == nscore::squash(&key))
                        .map(|f| f.key.clone())
                        .unwrap_or(key),
                };
                let existing = current.into_iter().find(|f| f.key == key);
                // A new version must sort after the one it supersedes even
                // under a coarse clock.
                let version_at = |prev: &nscore::Fact| {
                    let t = now();
                    if t > prev.valid_from {
                        t
                    } else {
                        Timestamp(prev.valid_from.0 + 1)
                    }
                };
                let fact = match existing {
                    Some(prev) if prev.value == value_json => nscore::Fact {
                        confidence: if residual {
                            (prev.confidence + 0.1).min(1.0)
                        } else {
                            1.0
                        },
                        last_validated: now(),
                        prov,
                        trust: value_trust,
                        // a restated cold fact is current again (M6 §6.2)
                        state: nscore::FactState::Current,
                        ..prev
                    },
                    Some(prev) => nscore::Fact {
                        key: key.clone(),
                        value: value_json,
                        confidence: grounded_confidence,
                        uses: prev.uses,
                        last_validated: now(),
                        prov,
                        scope: scope.clone(),
                        trust: value_trust,
                        valid_from: version_at(&prev),
                        valid_to: None,
                        state: nscore::FactState::Current,
                        last_used: prev.last_used,
                        // M9 T4.1: a new *value* is a new version, and it has
                        // not been shown to anything yet. The counters stay
                        // with the version whose exposures earned them —
                        // inheriting them would credit "Peter" for the calls
                        // that showed "Martin". The restatement arm above
                        // keeps them, via `..prev`, because there the version
                        // is the same one.
                        exposures: 0,
                        credits: 0,
                    },
                    None => nscore::Fact {
                        key: key.clone(),
                        value: value_json,
                        confidence: grounded_confidence,
                        uses: 0,
                        last_validated: now(),
                        prov,
                        scope: scope.clone(),
                        trust: value_trust,
                        valid_from: now(),
                        ..Default::default()
                    },
                };
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: REMEMBER_FACT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.put_fact(fact).await {
                    Ok(()) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("remembered {key}"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // f7. recall (M6 §7): progressive disclosure. Verbatim turns
            // beyond the window first, then live facts; results become
            // CopiedOutput sources with the lowest trust among them.
            if proposal.action == RECALL {
                let query = proposal
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(String::from);
                let Some(query) = query else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "recall needs a non-empty query".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("recall missing query".into());
                    continue;
                };
                let spec = recall_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: RECALL.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = self.recall_outcome(&sid, &scope, &query, turn, tier).await;
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                continue;
            }

            // f8. inspect_result (M7 T1.2): the other half of the cap. The
            // whole result is in the log; this pages through it without
            // running the tool again, which on a desktop is neither free nor
            // guaranteed to return the same screen.
            if proposal.action == INSPECT_RESULT {
                let raw_id = proposal.args.get("id").and_then(|v| v.as_str());
                let query = proposal
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|q| !q.is_empty());
                let handle = raw_id.and_then(parse_result_handle);
                let available =
                    clipped_results(log.events(), turn, self.cfg.tool_result_max_chars);
                let Some(id) = handle.filter(|id| available.contains(id)) else {
                    let known: Vec<String> = available.iter().map(|i| result_handle(*i)).collect();
                    let detail = format!(
                        "no clipped result named {:?} this turn; available: {}",
                        raw_id.unwrap_or(""),
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    );
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!("{INSPECT_RESULT}: {detail}"),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("{INSPECT_RESULT}: {detail}"));
                    denied_this_turn.insert(INSPECT_RESULT.to_string());
                    continue;
                };
                let spec = inspect_result_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let page = inspect_page(log.events(), turn, id);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: INSPECT_RESULT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let text = result_text(log.events(), turn, id).unwrap_or_default();
                let total = text.chars().count();
                let (window, start, end) = result_window(&text, query, page, self.cfg.tool_result_max_chars);
                let outcome = if window.is_empty() {
                    // Either the query matched nothing or the pages ran out.
                    // Both are answers, and both mean asking again is a
                    // wasted iteration — so the action leaves the schema.
                    denied_this_turn.insert(INSPECT_RESULT.to_string());
                    ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: match query {
                                Some(q) => format!("{} has no match for {q:?}", result_handle(id)),
                                None => format!("no more of {}", result_handle(id)),
                            },
                            artifact: None,
                            trust: result_trust(log.events(), turn, id),
                        },
                    }
                } else {
                    ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!(
                                "{} chars {start}-{end} of {total}: {window}",
                                result_handle(id)
                            ),
                            artifact: None,
                            trust: result_trust(log.events(), turn, id),
                        },
                    }
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                continue;
            }

            // f5. forget_fact (M6 §6.2): soft-delete one current fact. An
            // unknown key is malformed so the emitter can retry or ask.
            if proposal.action == FORGET_FACT {
                let key = proposal
                    .args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(|k| {
                        k.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                            .to_string()
                    })
                    .filter(|k| !k.is_empty());
                let Some(key) = key else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "forget_fact needs a string key".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("forget_fact missing key".into());
                    continue;
                };
                let current = self
                    .parts
                    .memory
                    .facts(&scope, "")
                    .await
                    .unwrap_or_default();
                let key = current
                    .iter()
                    .find(|f| f.key == key || nscore::squash(&f.key) == nscore::squash(&key))
                    .map(|f| f.key.clone())
                    .unwrap_or(key);
                if !current.iter().any(|f| f.key == key) {
                    let detail = format!("no current fact named {key}");
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!("forget_fact: {detail}"),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("forget_fact: {detail}"));
                    // One miss may be a fixable key; a second one is a loop
                    // (seen live: the same wrong key three times).
                    forget_misses += 1;
                    if forget_misses >= 2 {
                        denied_this_turn.insert(FORGET_FACT.to_string());
                    }
                    continue;
                }
                let spec = forget_fact_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: FORGET_FACT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.forget_fact(&scope, &key, now()).await {
                    Ok(_) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("forgot {key}"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // f6. forget_all (M6 §6.2): irreversible, so it is staged behind
            // the same two-turn confirmation as any irreversible tool, and
            // purges the scope once confirmed.
            if proposal.action == FORGET_ALL {
                let confirmed = confirmed_now || state.confirmed_this_turn_of == Some(turn);
                if !confirmed {
                    // No count in the prompt: replay runs from a fresh store
                    // and a Verbatim reply must be reproducible from the log.
                    let description =
                        format!("This will forget every stored fact in scope {scope}.");
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation {
                            proposal_of: pid,
                            staged: Some(nscore::StagedEffect {
                                description: description.clone(),
                            }),
                        },
                    );
                    let policy = ReplyPolicy::Verbatim {
                        text: format!(
                            "'{FORGET_ALL}' is irreversible. Confirm to proceed.\nPlanned: {description}"
                        ),
                    };
                    log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: policy.clone(),
                        },
                    );
                    settled = Some(policy);
                    break;
                }
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: FORGET_ALL.into(),
                            args: vec![],
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.purge_facts(&scope).await {
                    Ok(n) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("forgot {n} facts"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // g0. find the tool (legality guaranteed it exists)
            let tool = self
                .parts
                .tools
                .iter()
                .find(|t| t.spec().name == proposal.action)
                .expect("legality checked above")
                .clone();

            // g1. schema validation (spec M5 §3.2): malformed args are
            // rejected before classification; the action stays legal so the
            // emitter can retry with repaired args.
            if let Err(detail) = nscore::validate_args(&tool.spec().args_schema, &proposal.args) {
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason: RejectReason::Malformed {
                            detail: format!("{}: {detail}", proposal.action),
                        },
                    },
                );
                rejections_this_turn
                    .push(format!("malformed args for {}: {detail}", proposal.action));
                continue;
            }

            // g. classify args against the session's history (spec §5.4)
            let classified_args = classify(log.events(), &proposal.args, tool.spec(), turn);
            let classified = ClassifiedProposal {
                proposal: proposal.clone(),
                args: classified_args.clone(),
            };

            // h. guards
            let guard_ctx = nscore::GuardCtx {
                spec: tool.spec(),
                turn,
                confirmed_this_turn: confirmed_now || state.confirmed_this_turn_of == Some(turn),
                fired_actions: &state.fired_tags,
                pending_confirmation: active_pending,
            };
            let mut verdict = Verdict::Allow;
            let mut guard_name = String::new();
            for g in self.builtin_guards.iter().chain(self.parts.guards.iter()) {
                match g.check(&classified, &guard_ctx) {
                    Verdict::Allow => continue,
                    v => {
                        guard_name = g.name().to_string();
                        verdict = v;
                        break;
                    }
                }
            }
            match verdict {
                Verdict::Allow => {}
                Verdict::Deny { reason } => {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: guard_name.clone(),
                                reason: reason.clone(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard {guard_name}: {reason}"));
                    if reason.contains("NeverResidual") {
                        never_residual_this_turn = true;
                    }
                    denied_this_turn.insert(proposal.action.clone());
                    continue;
                }
                Verdict::NeedsConfirmation { prompt } => {
                    // Dry-run when the tool supports it; show the user what
                    // would happen (spec §5.4, §9).
                    let staged = tool
                        .stage(
                            &proposal.args,
                            &ToolCtx {
                                session: sid.clone(),
                                artifacts: Some(self.parts.memory.clone()),
                            },
                        )
                        .await;
                    let mut text = prompt;
                    if let Some(s) = &staged {
                        text.push_str(&format!("\nPlanned: {}", s.description));
                    }
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation {
                            proposal_of: pid,
                            staged,
                        },
                    );
                    let policy = ReplyPolicy::Verbatim { text };
                    log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: policy.clone(),
                        },
                    );
                    settled = Some(policy);
                    break;
                }
            }

            // i. perform
            let call_id = log
                .append(
                    turn,
                    now(),
                    EventKind::ToolCalled {
                        action: proposal.action.clone(),
                        args: classified_args,
                    },
                )
                .id;
            calls_this_turn.insert(Self::call_key(&proposal));
            let outcome = match tool
                .call(
                    &proposal.args,
                    &ToolCtx {
                        session: sid.clone(),
                        artifacts: Some(self.parts.memory.clone()),
                    },
                )
                .await
            {
                Ok(output) => ToolOutcome::Ok { output },
                Err(nscore::ToolError::Failed { kind, detail }) => {
                    ToolOutcome::Err { kind, detail }
                }
            };
            log.append(
                turn,
                now(),
                EventKind::ToolReturned {
                    call: call_id,
                    outcome,
                },
            );
            if tool.spec().side_effect != nscore::SideEffect::Pure {
                self.flush(&sid, &log, n_loaded).await;
            }
            // loop: the emitter decides what happens next (typically respond_directly)
        }

        // M13 T3.1: the iteration budget ran out holding an answer written
        // beside the last action, and that action ran. It is a reply to a
        // turn that did what it said; the fallback below would throw it away
        // and tell the user the loop ran out of steps.
        if settled.is_none() {
            if let Some(text) = answer_with_action.take() {
                if last_proposal_ran(log.events(), turn) {
                    pre_draft = Some(text);
                    let e = log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: ReplyPolicy::Generate,
                        },
                    );
                    settled = Some(match &e.kind {
                        EventKind::Settled { policy } => policy.clone(),
                        _ => unreachable!(),
                    });
                }
            }
        }

        // 3. fallback settle (a registered cant_help template wins). The
        // reply names the cause — the user shouldn't need the event log to
        // learn it was a rate limit rather than a refusal.
        let policy = settled.unwrap_or_else(|| {
            // The test is "did the emitter ever answer", not "was the retry
            // budget spent". A terminal provider status breaks out after one
            // attempt (phase 5), and calling that "ran out of steps after 5
            // actions" would blame the loop for an endpoint that was down.
            let reason = if emit_failures > 0 && !proposed_this_turn {
                last_emit_error
                    .as_deref()
                    .map(explain_error)
                    .unwrap_or_else(|| "the model was unavailable".into())
            } else {
                let mut r = format!(
                    "ran out of steps after {} actions without reaching an answer",
                    self.cfg.max_iterations
                );
                if let Some(last) = rejections_this_turn.last() {
                    r.push_str(&format!("; last problem: {last}"));
                }
                r
            };
            let p = if self.cfg.templates.contains_key("cant_help") {
                ReplyPolicy::Template {
                    id: "cant_help".into(),
                    vars: serde_json::json!({ "reason": reason }),
                }
            } else {
                ReplyPolicy::Verbatim {
                    text: format!("{FALLBACK_REPLY} Reason: {reason}."),
                }
            };
            log.append(turn, now(), EventKind::Settled { policy: p.clone() });
            p
        });

        // 4. reply
        let text = match policy {
            ReplyPolicy::Verbatim { text } => text,
            ReplyPolicy::Template { id, vars } => match self.cfg.templates.get(&id) {
                Some(template) => render_template(template, &vars),
                None => format!("[{id}] {vars}"),
            },
            ReplyPolicy::Generate => {
                self.generate_reply(
                    &scope,
                    &incoming.text,
                    &rules,
                    &mut log,
                    turn,
                    &usage,
                    self.cfg.recall_hybrid && tier != nscore::Tier::Chat,
                    pre_draft,
                )
                .await
            }
        };

        // 5. record + persist new events only
        log.append(turn, now(), EventKind::Replied { text: text.clone() });
        self.parts
            .memory
            .append(&sid, &log.events()[n_loaded..])
            .await?;
        Ok(text)
    }

    /// Draft the user-facing reply from the trace of what happened.
    ///
    /// Was the tail of a match arm inside `run_turn`, which left the most
    /// expensive phase of the turn as the one with no name. It is a phase:
    /// it selects facts, fits a budget, calls a model, and may call it a
    /// second time when the grounding check fires — the only place besides
    /// the emitter loop that spends a request.
    async fn generate_reply(
        &self,
        scope: &str,
        user_text: &str,
        rules: &nscore::LearnedRules,
        log: &mut EventLog,
        turn: u32,
        usage: &std::sync::Arc<nscore::UsageSink>,
        // Whether this turn's tier and config allow the hybrid fact path
        // (M11 T1.1 follow-up). Passed in rather than re-derived: the tier
        // is the caller's, and it may have been upgraded mid-turn.
        hybrid_facts: bool,
        // M12 T4.3: the answer the emitter call already produced. `Some`
        // skips the replier and its `ModelCall` — the turn costs one
        // request — and then runs the identical echo, grounding, obligation
        // and citation block on the text, because an emitted answer is a
        // draft like any other and is not owed a lighter check.
        pre_draft: Option<String>,
    ) -> String {
        let now = &self.clock;
        let state = fold(log.events());
        // Clipped for the same reason, though this one is built once
        // per turn rather than once per iteration. `turn_trace` itself
        // stays uncapped: `render_echo` measures the reply against the
        // full material, and capping there would change what that
        // number means.
        let (trace_lines, reply_clipped_chars) =
            trace_for_prompt(
                log.events(),
                turn,
                self.cfg.trace_verbatim_lines,
                self.cfg.tool_result_max_chars,
            );
        let trace = trace_lines.join("\n");
        // Implicit recall (spec §5): standing facts enter the reply
        // context; each recall bumps `uses` (lifecycle metadata for
        // the future consolidation pass).
        let mut selected = self.select_facts(scope, user_text, hybrid_facts).await;
        for f in selected.iter_mut() {
            f.uses += 1;
            f.last_used = now();
            let _ = self.parts.memory.put_fact(f.clone()).await;
        }
        let facts = self.fact_views(scope, &selected).await;
        // M6 §4.3: the reply model gets the user's message, the
        // verbatim window and the summary — not a counter string.
        let window = state.window(self.cfg.window_turns);
        // M12 T3.2: the reply path archives by the same rule as the emitter.
        let guidance_notes = if self.cfg.archive_foreign_notes {
            rules.guidance_notes_for_reply_model(self.cfg.learning_model.as_deref())
        } else {
            rules.guidance_notes_for_reply()
        };
        let guidance: Vec<String> = guidance_notes.iter().map(|(_, t)| t.clone()).collect();
        // M9 T2.1: the same pure function the emitter path calls, on the
        // same message.
        let obligations = nscore::obligations_for(user_text, self.cfg.obligations_max);
        // `extra_guidance` is how the obligation interceptor speaks to the
        // second draft: one added note, the shape `do_not_state` already has
        // on the grounding path.
        let make_ctx = |do_not_state: Vec<String>,
                        do_not_repeat: Vec<String>,
                        extra_guidance: Vec<String>| {
            let mut notes = guidance.clone();
            notes.extend(extra_guidance);
            ReplyContext {
                persona: self.cfg.persona.clone(),
                facts: facts.clone(),
                summary: state.summary.clone(),
                window: window.clone(),
                caps: self.cfg.caps,
                user_text: user_text.to_string(),
                obligations: obligations.clone(),
                turn_trace: trace.clone(),
                guidance: notes,
                do_not_state,
                do_not_repeat,
                usage: Some(usage.clone()),
            }
        };
        // The reply context is fitted too, and reported on the same
        // way. Its `turn_trace` is exempt: it is the material the
        // reply narrates from, and the grounding interceptor flags a
        // reply for stating anything absent from it — trimming it
        // would manufacture the fabrications the interceptor catches.
        let mut budgeted = make_ctx(vec![], vec![], vec![]);
        let budget = nscore::fit_reply(
            &mut budgeted,
            self.cfg.prompt_budget_tokens,
            self.cfg.budget_mode,
            &self.cfg.pinned_prefixes,
            self.cfg.guidance_max,
        );
        // M9 T0.4, as on the emitter path: after the fit, so only the
        // rendered prompt and the manifest's keys change.
        match self.cfg.ablate {
            Some(nscore::Ablate::Facts) => budgeted.facts.clear(),
            Some(nscore::Ablate::Summary) => budgeted.summary = None,
            Some(nscore::Ablate::Guidance) => budgeted.guidance.clear(),
            None => {}
        }
        let note_hashes: Vec<String> = guidance_notes
            .iter()
            .take(budgeted.guidance.len())
            .map(|(h, _)| h.clone())
            .collect();
        let mut manifest = reply_manifest(scope, &budgeted, reply_clipped_chars, note_hashes);
        manifest.budget = Some(budget);
        manifest.ablated = self.cfg.ablate;
        // M12 T4.3. An emitted answer is already drafted and already paid
        // for; everything below it is the same.
        let drafted = match pre_draft {
            Some(text) => Ok(text),
            None => {
                let d = self.parts.replier.reply(budgeted).await;
                self.record_model_calls(usage, log, turn, &manifest);
                d
            }
        };
        match drafted {
            Ok(draft) if self.cfg.reply_grounding_check => {
                // M6 §4.5. Two checks, one of which acts.
                //
                // `ungrounded` gates: a claim nothing above supports
                // is named and the reply regenerated once, and the
                // second draft stands whatever it says.
                //
                // `echoed` only observes. The 2026-09-04 ablation
                // (plan §7–§8) scored it over four control arms: 21
                // firings, zero true positives. `echo_ratio` is
                // reference-free, so it cannot tell a copied engine
                // artifact from the same short correct answer given
                // twice — the two have identical verbatim overlap,
                // and the historical parrots (0.80–1.00) and the
                // false positives (0.60–1.00) overlap completely, so
                // no threshold separates them either. Both loop
                // detectors this borrows from are monitors, at far
                // more conservative thresholds. So it is logged, and
                // nothing is regenerated on it: the observability is
                // what found all of this, and it is free.
                let ctx = make_ctx(vec![], vec![], vec![]);
                let echo_material = crate::ground::echo_material(&ctx);
                if let Some(span) =
                    crate::echo::echoed(&draft, &echo_material, self.cfg.max_echo_ratio)
                {
                    log.append(
                        turn,
                        now(),
                        EventKind::ReplyEchoed {
                            draft: draft.clone(),
                            span,
                            ratio: crate::echo::echo_ratio(&draft, &echo_material),
                        },
                    );
                }
                let material = crate::ground::Material::from_context(&ctx);
                let spans = crate::ground::ungrounded(&draft, &material);
                if !spans.is_empty() {
                    log.append(
                        turn,
                        now(),
                        EventKind::ReplyFlagged {
                            draft: draft.clone(),
                            spans: spans.clone(),
                        },
                    );
                    // M12 T1.2: the flag above is free and always written;
                    // the call below is billed and exists to talk a weak
                    // model out of its fabrication. Where the model is not
                    // weak, the draft stands and falls through to the same
                    // obligations and citation path any draft takes.
                    if self.cfg.reply_regenerate {
                        let regenerated =
                            self.parts.replier.reply(make_ctx(spans, vec![], vec![])).await;
                        // The regeneration is a second billed call, and
                        // the point of counting it is to know what the
                        // grounding check costs.
                        self.record_model_calls(usage, log, turn, &manifest);
                        let final_reply = regenerated.unwrap_or(draft);
                        Self::record_cited(log, turn, now(), &ctx, &final_reply);
                        return final_reply;
                    }
                }
                // M9 T2.1. The obligation interceptor, behind its own
                // knob and *after* grounding: a draft that already had
                // to be regenerated has spent this turn's one spare
                // call. No new event kind — the mechanics are the
                // `ReplyFlagged` path's, with a distinct guidance
                // line, because the extraction is measured weak and a
                // signature written from it would be counted as if it
                // were not.
                match self
                    .cfg
                    .obligation_check
                    .then(|| crate::ground::unaddressed(&obligations, &draft))
                    .flatten()
                {
                    Some(clause) => {
                        let regenerated = self
                            .parts
                            .replier
                            .reply(make_ctx(
                                vec![],
                                vec![],
                                vec![format!("Not yet addressed: {clause}")],
                            ))
                            .await;
                        self.record_model_calls(usage, log, turn, &manifest);
                        let final_reply = regenerated.unwrap_or(draft);
                        Self::record_cited(log, turn, now(), &ctx, &final_reply);
                        final_reply
                    }
                    None => {
                        Self::record_cited(log, turn, now(), &ctx, &draft);
                        draft
                    }
                }
            }
            Ok(draft) => draft,
            Err(e) => {
                // F7: a replier failure is an event, not just a
                // fallback text — mining and audits must see it.
                log.append(
                    turn,
                    now(),
                    EventKind::ReplyFailed {
                        detail: e.to_string(),
                    },
                );
                format!(
                    "{FALLBACK_REPLY} Reason: the reply could not be generated — {}.",
                    explain_error(&e.to_string())
                )
            }
        }
    }

    /// M9 T4.2: record which reference parts the *final* reply drew on.
    ///
    /// The final draft, not the first: a flagged draft was regenerated, and
    /// what the discarded one quoted is not what the user was told. Written
    /// only when something was cited — an empty list is not a fact about the
    /// turn, and the join downstream reads absence as "nothing cited".
    fn record_cited(
        log: &mut EventLog,
        turn: u32,
        at: nscore::Timestamp,
        ctx: &ReplyContext,
        reply: &str,
    ) {
        let sources = crate::ground::cited(ctx, reply);
        if !sources.is_empty() {
            log.append(turn, at, EventKind::ReplyCited { sources });
        }
    }

    /// Runs the engine on its channel until the channel closes: every
    /// message goes to its session's mailbox, a session runs one turn at a
    /// time, `worker_slots` sessions run at once, a quiet channel runs the
    /// consolidator (driver B, spec M5 §5), and the rolling summary (M6
    /// §5.1) runs while its session waits for the next message. The loop is
    /// `dispatch::Dispatcher`; this builds it, so the CLI and the tests keep
    /// the one call they had.
    pub async fn run(self) -> Result<(), EngineError> {
        let channel = self.parts.channel.clone();
        let slots = self.cfg.worker_slots;
        crate::dispatch::Dispatcher::new(std::sync::Arc::new(self), channel, slots)
            .run()
            .await
    }
}

/// A fact value as prose: a JSON string without its quotes, anything else as
/// it serializes. Model-visible text carries no engine syntax — no `k = v`,
/// no JSON envelope — because whatever the reply model is shown it may
/// reproduce verbatim (plan §3, phase 1).
fn value_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace "{name}" with vars["name"] (strings unquoted); unknown
/// placeholders are left verbatim. Deterministic fill-in, no escaping (M4).
fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut out = template.to_string();
    if let Some(map) = vars.as_object() {
        for (k, v) in map {
            let replacement = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{k}}}"), &replacement);
        }
    }
    out
}

/// Establish where each of a proposal's arguments came from.
///
/// Seven places in the turn loop built this pair by hand — rebuild the value
/// index from the session's events, then classify against it — and provenance
/// is the mechanism that decides what a guard is allowed to conclude about an
/// argument. Seven copies of it is seven places a change has to be made and
/// six places it can be forgotten, which is the shape of an invariant that
/// eventually holds in most of the codebase.
///
/// The index is rebuilt per call rather than cached because it is derived
/// from the log, and the log grows within a turn: a value copied out of a
/// tool result three steps ago must be groundable now.
fn classify(
    events: &[nscore::Event],
    args: &serde_json::Value,
    spec: &nscore::ActionSpec,
    turn: u32,
) -> Vec<(String, nscore::TaggedValue)> {
    let index = nsprovenance::index::ValueIndex::from_events(events);
    nsprovenance::classify::classify_args(args, spec, &index, turn)
}

/// The guards every engine runs, before the harness's own.
///
/// `SideEffectGate` is left out rather than neutered when confirmation is off,
/// so a trace shows no side-effect gate at all instead of one that silently
/// allows everything. An unattended session has nobody to answer the prompt,
/// and a staged proposal there is not a safeguard, it is a stall.
fn builtin_guards(confirm_irreversible: bool) -> Vec<Box<dyn nscore::Guard>> {
    let mut guards: Vec<Box<dyn nscore::Guard>> = vec![
        Box::new(crate::guards::ResidualPolicy),
        Box::new(crate::guards::TaintPolicy),
        Box::new(crate::guards::DedupeGate),
    ];
    if confirm_irreversible {
        guards.push(Box::new(crate::guards::SideEffectGate));
    }
    guards
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builtin specs never pass through `HarnessBuilder::add_tool`, so
    /// the assembly gate does not see them. They need the same check, or the
    /// half of the legal set the engine owns itself is the unchecked half.
    #[test]
    fn every_builtin_spec_keeps_the_rationale_first() {
        for spec in [nscore::SchemaProfile::Full, nscore::SchemaProfile::Slim]
            .into_iter()
            .flat_map(synthetic_specs)
        {
            let name = spec.name.clone();
            spec.check_arg_names()
                .unwrap_or_else(|e| panic!("builtin `{name}` breaks think-then-commit: {e}"));
        }
    }

    /// The default must stay the safe one: a config that says nothing about
    /// confirmation gets the gate.
    #[test]
    fn irreversible_actions_are_confirmed_unless_asked_otherwise() {
        assert!(EngineConfig::default().confirm_irreversible);
    }

    /// Switching it off removes the gate rather than leaving one that always
    /// allows, so a trace shows honestly that nothing was gating.
    #[test]
    fn the_side_effect_gate_is_absent_when_confirmation_is_off() {
        fn names(confirm: bool) -> Vec<String> {
            builtin_guards(confirm)
                .iter()
                .map(|g| g.name().to_string())
                .collect()
        }
        assert!(names(true).contains(&"side_effect_gate".to_string()));
        assert!(!names(false).contains(&"side_effect_gate".to_string()));
        assert_eq!(
            names(false).len() + 1,
            names(true).len(),
            "only the one guard differs"
        );
    }


    #[test]
    fn render_substitutes_known_placeholders_only() {
        let vars = serde_json::json!({"name": "Martin", "n": 3});
        assert_eq!(
            render_template("Hi {name}, {n} items, {missing} stays", &vars),
            "Hi Martin, 3 items, {missing} stays"
        );
    }
}
