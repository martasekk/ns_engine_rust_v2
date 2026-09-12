//! Every knob a deployment can turn, and the defaults it gets for free.
//!
//! Split out of the turn loop so the loop reads as a sequence of steps and
//! this file reads as the settings mirror it is — `[engine]` and `[memory]`
//! in `config.toml` are this struct, field for field.

use crate::trace::DEFAULT_TOOL_RESULT_MAX_CHARS;

/// What to do with a remembered value nothing in the session grounds
/// (M6 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberResidual {
    /// Store at confidence 0.5, shown as `(unverified)`; restatement promotes.
    Flag,
    /// Deny like any NeverResidual arg; forced clarification follows.
    Never,
}

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
