use crate::state::fold;
use nscore::{
    Channel, ChannelError, ClassifiedProposal, EventKind, EventLog, HarnessParts, Incoming,
    LegalActionSet, RejectReason, ReplyContext, ReplyPolicy, Timestamp, ToolCtx, ToolOutcome,
    Verdict,
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
    /// M7 T0.1: where the provider clients leave what each call cost. When
    /// set, the engine appends one `ModelCall` per call, with the manifest
    /// of what that call was shown. `None` — every scripted double, every
    /// replay — measures nothing and writes nothing.
    pub usage: Option<std::sync::Arc<nscore::UsageSink>>,
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
            max_echo_ratio: 0.6,
            scope_for: std::sync::Arc::new(|_| "global".to_string()),
            remember_residual: RememberResidual::Flag,
            pinned_prefixes: vec!["user.".into()],
            pinned_max: 5,
            relevant_max: 5,
            summary_every_turns: 4,
            summary_rebuild_every: 3,
            summary_max_chars: 800,
            summary_input_max_chars: 6000,
            recall_top_k: 5,
            // The safe default: ask before anything irreversible.
            confirm_irreversible: true,
            usage: None,
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
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("store: {0}")]
    Store(#[from] nscore::StoreError),
    #[error("channel: {0}")]
    Channel(String),
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

/// Engine-owned synthetic action: ask the user one question (spec §5.1).
pub const ASK_CLARIFICATION: &str = "ask_clarification";

fn ask_clarification_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: ASK_CLARIFICATION.into(),
        description: "Ask the user one short question to resolve missing or ungrounded \
                      information required by the next action."
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

fn confirm_pending_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: CONFIRM_PENDING.into(),
        description: "The user has just confirmed the pending action; execute it.".into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: store one durable fact.
pub const REMEMBER_FACT: &str = "remember_fact";

fn remember_fact_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: REMEMBER_FACT.into(),
        description: "Store one durable fact about the user or task as key/value \
                      (dotted keys, e.g. user.name)."
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

fn forget_fact_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_FACT.into(),
        description: "Delete one stored fact by its key (e.g. user.name). Only when the user \
                      explicitly asks to forget or remove something stored."
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

fn forget_all_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: FORGET_ALL.into(),
        description: "Erase everything stored about the user; asks for confirmation first. Only \
                      when the user explicitly asks to reset or wipe the memory."
            .into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Irreversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: search memory beyond the context (M6 §7).
pub const RECALL: &str = "recall";

fn recall_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: RECALL.into(),
        description: "Search earlier turns of this conversation and stored facts for words the \
                      user is asking about. Use when the answer is not in the recent turns \
                      or facts shown."
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

/// Engine-owned synthetic action: read more of a clipped tool result
/// (M7 T1.2).
pub const INSPECT_RESULT: &str = "inspect_result";

fn inspect_result_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: INSPECT_RESULT.into(),
        description: "Read more of a tool result that was shown clipped. `id` is the handle in \
                      the trace, like r42. With `query`, returns the part of the result around \
                      the first match; without one, the next part. For a desktop, prefer \
                      pointer_ui_find, which searches the live screen instead."
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

/// The window of a clipped result to show for one `inspect_result` call.
///
/// With a query, the window is anchored a little before the first
/// case-insensitive match, so the match arrives with the context that makes
/// it usable — for a control tree, the role and name that precede a point.
/// Without one, it is the next page: `page` is how many unqueried
/// inspections of this result the turn has already made, which is a fact
/// about this turn's own events and so survives replay.
///
/// Deliberately a character window and not a split on the separator some
/// tool happens to use. The engine knows nothing about any tool's output
/// format, and a per-tool table of separators here is the same magic-number
/// table the pointer design refused for modal keywords.
fn result_window(
    text: &str,
    query: Option<&str>,
    page: usize,
    max: usize,
) -> (String, usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let start = match query {
        Some(q) if !q.trim().is_empty() => {
            let hay = text.to_lowercase();
            match hay.find(&q.trim().to_lowercase()) {
                // `find` is a byte offset; convert to a character index so the
                // window never splits a Czech control name in half.
                Some(byte) => {
                    let char_index = text[..byte].chars().count();
                    char_index.saturating_sub(max / 8)
                }
                None => return (String::new(), 0, chars.len()),
            }
        }
        _ => page.saturating_mul(max),
    };
    if start >= chars.len() {
        return (String::new(), start, chars.len());
    }
    let end = (start + max).min(chars.len());
    (chars[start..end].iter().collect(), start, end)
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
        }
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
    async fn select_facts(&self, scope: &str, user_text: &str) -> Vec<nscore::Fact> {
        let live = self.parts.memory.facts(scope, "").await.unwrap_or_default();
        let mut pinned: Vec<nscore::Fact> = live
            .iter()
            .filter(|f| f.state == nscore::FactState::Current)
            .filter(|f| {
                self.cfg
                    .pinned_prefixes
                    .iter()
                    .any(|p| f.key.starts_with(p))
            })
            .cloned()
            .collect();
        pinned.sort_by(|a, b| {
            b.last_validated
                .cmp(&a.last_validated)
                .then_with(|| a.key.cmp(&b.key))
        });
        pinned.truncate(self.cfg.pinned_max);
        let relevant = self
            .parts
            .memory
            .search_facts(scope, user_text, self.cfg.relevant_max + pinned.len())
            .await
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
    pub async fn maybe_summarize(&mut self, sid: &nscore::SessionId) -> Result<bool, EngineError> {
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
        let selected = self.select_facts(&scope, "").await;
        let facts = self.fact_views(&scope, &selected).await;
        let previous = if rebuild {
            None
        } else {
            state.summary.as_ref()
        };
        let input = nscore::SummaryInput {
            previous,
            records: &records,
            caps: &self.cfg.caps,
            facts: &facts,
        };
        // The manifest for this call: the summarizer is shown the standing
        // facts and a range of verbatim records, and no tools or trace.
        let manifest = nscore::ContextManifest {
            fact_keys: facts.iter().map(|f| f.key.clone()).collect(),
            summary_through: previous.map(|s| s.through_turn),
            window: window_range(&records),
            ..Default::default()
        };
        let summarized = self.parts.summarizer.summarize(input).await;
        // The log is opened before the outcome is known, because a summarizer
        // that spent a request and produced nothing usable has still spent
        // it. Leaving that call undrained would attribute it to whichever
        // turn came next; recording it says plainly that a request bought
        // no summary.
        let mut log = EventLog::from_events(sid.clone(), stored);
        self.record_model_calls(&mut log, state.turn, &manifest);
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
    /// Append one `ModelCall` for every provider call recorded since the
    /// last drain (M7 T0.1).
    ///
    /// Called immediately after each of the engine's own model calls, so
    /// what the sink returns is that call's and the manifest describes what
    /// it was shown. The three call sites never overlap: the rolling summary
    /// runs while the loop waits for the next message, never beside a turn.
    ///
    /// Retries inside one call do not appear as separate events — they are
    /// counted in `Usage::attempts`, because the thing a reader wants to
    /// know is what one decision cost, requests included.
    fn record_model_calls(
        &self,
        log: &mut EventLog,
        turn: u32,
        manifest: &nscore::ContextManifest,
    ) {
        let Some(sink) = &self.cfg.usage else { return };
        for usage in sink.drain() {
            log.append(
                turn,
                (self.clock)(),
                EventKind::ModelCall {
                    usage,
                    manifest: manifest.clone(),
                },
            );
        }
    }

    async fn flush(&self, sid: &nscore::SessionId, log: &EventLog, from: usize) {
        if let Err(e) = self.parts.memory.append(sid, &log.events()[from..]).await {
            eprintln!("store: {e}");
        }
    }

    pub async fn run_turn(&mut self, incoming: Incoming) -> Result<String, EngineError> {
        let sid = incoming.session.clone();
        let scope = (self.cfg.scope_for)(&sid);
        let stored = self.parts.memory.load(&sid).await?;
        let n_loaded = stored.len();
        let mut log = EventLog::from_events(sid.clone(), stored);

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

        for _ in 0..self.cfg.max_iterations {
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
                    actions: vec![ask_clarification_spec()],
                }
            } else {
                // Narrowed schema (spec §2): actions rejected this turn are
                // removed from the set the emitter sees next.
                let mut actions: Vec<_> = self
                    .parts
                    .tools
                    .iter()
                    .map(|t| t.spec().clone())
                    .filter(|s| !denied_this_turn.contains(&s.name))
                    .collect();
                actions.push(ask_clarification_spec());
                if !denied_this_turn.contains(REMEMBER_FACT) {
                    actions.push(remember_fact_spec());
                }
                if !denied_this_turn.contains(RECALL) {
                    actions.push(recall_spec());
                }
                // Offered only while there is something to inspect. An
                // action in the schema that can only fail is a way for a
                // small model to spend an iteration discovering that.
                if !denied_this_turn.contains(INSPECT_RESULT)
                    && !clipped_results(log.events(), turn).is_empty()
                {
                    actions.push(inspect_result_spec());
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
                if !wrote_fact && !forgot {
                    if !denied_this_turn.contains(FORGET_FACT) {
                        actions.push(forget_fact_spec());
                    }
                    if !denied_this_turn.contains(FORGET_ALL) {
                        actions.push(forget_all_spec());
                    }
                }
                if active_pending.is_some() {
                    actions.push(confirm_pending_spec());
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
            let (trace_so_far, clipped_chars) = trace_for_prompt(log.events(), turn);
            let selected = self.select_facts(&scope, &incoming.text).await;
            let facts = self.fact_views(&scope, &selected).await;
            let legal_names: Vec<String> = legal.actions.iter().map(|a| a.name.clone()).collect();
            let ctx = nscore::EmitterContext {
                facts,
                summary: state.summary.clone(),
                window: state.window(self.cfg.window_turns),
                caps: self.cfg.caps,
                user_text: incoming.text.clone(),
                trace_so_far,
                pending_confirmation: active_pending.is_some(),
                rejections_this_turn: rejections_this_turn.clone(),
                guidance: rules.guidance_for(&legal_names),
            };

            // c. propose
            let mut confirmed_now = false;
            let manifest = emitter_manifest(&ctx, legal.actions.len(), clipped_chars);
            let proposed = self.parts.emitter.propose(ctx, &legal).await;
            self.record_model_calls(&mut log, turn, &manifest);
            let mut proposal = match proposed {
                Ok(p) => p,
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

            // f. legality
            if !legal.contains(&proposal.action) {
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
                let ask_spec = ask_clarification_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args =
                    nsprovenance::classify::classify_args(&proposal.args, &ask_spec, &index, turn);
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
                let fact_spec = remember_fact_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args =
                    nsprovenance::classify::classify_args(&proposal.args, &fact_spec, &index, turn);
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
                let spec = recall_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args =
                    nsprovenance::classify::classify_args(&proposal.args, &spec, &index, turn);
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
                let k = self.cfg.recall_top_k;
                // Turns already visible in the window (and this one) add nothing.
                let visible_from = turn.saturating_sub(self.cfg.window_turns as u32);
                let mut lines: Vec<String> = Vec::new();
                let mut trusts: Vec<nscore::Trust> = Vec::new();
                let mut failure: Option<String> = None;
                match self.parts.memory.search_turns(&sid, &query, k * 3).await {
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
                match self.parts.memory.search_facts(&scope, &query, k).await {
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
                let outcome = match failure {
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
                    // copy-bait for the reply model and buy nothing, since
                    // nothing parses this back (plan §3, phase 1). One line,
                    // because `turn_trace` is line-per-event.
                    None => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: lines.join("; "),
                            artifact: None,
                            trust: nscore::min_trust(&trusts),
                        },
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
                let available = clipped_results(log.events(), turn);
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
                let spec = inspect_result_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args =
                    nsprovenance::classify::classify_args(&proposal.args, &spec, &index, turn);
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
                let (window, start, end) = result_window(&text, query, page, TRACE_LINE_MAX_CHARS);
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
                let spec = forget_fact_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args =
                    nsprovenance::classify::classify_args(&proposal.args, &spec, &index, turn);
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
            let index = nsprovenance::index::ValueIndex::from_events(log.events());
            let classified_args =
                nsprovenance::classify::classify_args(&proposal.args, tool.spec(), &index, turn);
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
                let state = fold(log.events());
                // Clipped for the same reason, though this one is built once
                // per turn rather than once per iteration. `turn_trace` itself
                // stays uncapped: `render_echo` measures the reply against the
                // full material, and capping there would change what that
                // number means.
                let (trace_lines, reply_clipped_chars) = trace_for_prompt(log.events(), turn);
                let trace = trace_lines.join("\n");
                // Implicit recall (spec §5): standing facts enter the reply
                // context; each recall bumps `uses` (lifecycle metadata for
                // the future consolidation pass).
                let mut selected = self.select_facts(&scope, &incoming.text).await;
                for f in selected.iter_mut() {
                    f.uses += 1;
                    f.last_used = now();
                    let _ = self.parts.memory.put_fact(f.clone()).await;
                }
                let facts = self.fact_views(&scope, &selected).await;
                // M6 §4.3: the reply model gets the user's message, the
                // verbatim window and the summary — not a counter string.
                let window = state.window(self.cfg.window_turns);
                let guidance = rules.guidance_for_reply();
                let make_ctx =
                    |do_not_state: Vec<String>, do_not_repeat: Vec<String>| ReplyContext {
                        persona: self.cfg.persona.clone(),
                        facts: facts.clone(),
                        summary: state.summary.clone(),
                        window: window.clone(),
                        caps: self.cfg.caps,
                        user_text: incoming.text.clone(),
                        turn_trace: trace.clone(),
                        guidance: guidance.clone(),
                        do_not_state,
                        do_not_repeat,
                    };
                let manifest = reply_manifest(&make_ctx(vec![], vec![]), reply_clipped_chars);
                let drafted = self.parts.replier.reply(make_ctx(vec![], vec![])).await;
                self.record_model_calls(&mut log, turn, &manifest);
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
                        let ctx = make_ctx(vec![], vec![]);
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
                        if spans.is_empty() {
                            draft
                        } else {
                            log.append(
                                turn,
                                now(),
                                EventKind::ReplyFlagged {
                                    draft: draft.clone(),
                                    spans: spans.clone(),
                                },
                            );
                            let regenerated =
                                self.parts.replier.reply(make_ctx(spans, vec![])).await;
                            // The regeneration is a second billed call, and
                            // the point of counting it is to know what the
                            // grounding check costs.
                            self.record_model_calls(&mut log, turn, &manifest);
                            regenerated.unwrap_or(draft)
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
        };

        // 5. record + persist new events only
        log.append(turn, now(), EventKind::Replied { text: text.clone() });
        self.parts
            .memory
            .append(&sid, &log.events()[n_loaded..])
            .await?;
        Ok(text)
    }

    /// Outer loop: recv → run_turn → send, until the channel closes. With
    /// `idle_after` set, a quiet period runs the consolidator once (driver B,
    /// spec M5 §5) — only when at least one turn ran since the last pass, and
    /// never interleaved with a turn (same task).
    ///
    /// The rolling summary (M6 §5.1) runs *concurrently with the wait for the
    /// next message*, not before it: it is sleep-time work, and a local
    /// summarizer can take tens of seconds — long enough to hold up the
    /// prompt if it sits on the critical path.
    pub async fn run(&mut self) -> Result<(), EngineError> {
        // The channel lives outside `self` for the loop's duration so the
        // summary can borrow the engine while `recv` is still pending.
        let mut channel = std::mem::replace(&mut self.parts.channel, Box::new(DetachedChannel));
        let outcome = self.run_loop(&mut channel).await;
        self.parts.channel = channel;
        outcome
    }

    async fn run_loop(&mut self, channel: &mut Box<dyn Channel>) -> Result<(), EngineError> {
        let mut turns_since_pass: u32 = 0;
        // The session whose turn just ended: it may owe a rolling summary.
        let mut summary_due: Option<nscore::SessionId> = None;
        loop {
            let due = summary_due.take();
            let next = match &due {
                None => next_message(&mut **channel, self.cfg.idle_after).await?,
                Some(sid) => {
                    // `recv` is polled first (biased), so the prompt appears
                    // before the summary starts; the summary then runs while
                    // the user reads the reply and types. If the user gets
                    // there first the summary is dropped mid-flight — it is
                    // recomputed from the store at the next boundary, and its
                    // input range is capped by summary_input_max_chars, so an
                    // abandoned summary cannot make the next one unbounded.
                    let mut pending =
                        std::pin::pin!(next_message(&mut **channel, self.cfg.idle_after));
                    tokio::select! {
                        biased;
                        next = &mut pending => next?,
                        summarized = self.maybe_summarize(sid) => {
                            if let Err(e) = summarized {
                                eprintln!("summary: {e}");
                            }
                            pending.await?
                        }
                    }
                }
            };
            let incoming = match next {
                Next::Closed => return Ok(()),
                Next::Idle => {
                    // Silence is not a message: the summary is still owed.
                    summary_due = due;
                    if turns_since_pass > 0 {
                        if let Err(e) = self.parts.consolidator.run(&*self.parts.memory).await {
                            eprintln!("evolution pass failed: {e}");
                        }
                        turns_since_pass = 0;
                    }
                    continue;
                }
                Next::Message(i) => i,
            };
            let session = incoming.session.clone();
            let text = self.run_turn(incoming).await?;
            turns_since_pass += 1;
            channel
                .send(&session, &text)
                .await
                .map_err(|e| EngineError::Channel(e.to_string()))?;
            summary_due = Some(session);
        }
    }
}

/// What the wait for the next message produced.
enum Next {
    Message(Incoming),
    /// `idle_after` elapsed with the channel quiet.
    Idle,
    Closed,
}

/// One wait on the channel, with the idle timeout folded in. A free function
/// so it borrows only the channel, leaving the engine free for the summary.
async fn next_message(
    channel: &mut dyn Channel,
    idle_after: Option<std::time::Duration>,
) -> Result<Next, EngineError> {
    let received = match idle_after {
        Some(d) => tokio::time::timeout(d, channel.recv()).await,
        None => Ok(channel.recv().await),
    };
    match received {
        Ok(Ok(i)) => Ok(Next::Message(i)),
        Ok(Err(ChannelError::Closed)) => Ok(Next::Closed),
        Ok(Err(e)) => Err(EngineError::Channel(e.to_string())),
        Err(_elapsed) => Ok(Next::Idle),
    }
}

/// Stands in for the real channel while `run` holds it as a local. Never
/// polled — `run` puts the real one back before returning.
struct DetachedChannel;

#[async_trait::async_trait]
impl Channel for DetachedChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(
        &mut self,
        _session: &nscore::SessionId,
        _text: &str,
    ) -> Result<(), ChannelError> {
        Ok(())
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

/// One human-readable line per this-turn event, each carrying the id of the
/// event it came from where that id is a *handle* — that is, for tool
/// results, which are the only lines a model can ask to see more of.
///
/// Split out from `turn_trace` so the clip and `inspect_result` name the same
/// event. A handle the model cannot resolve is worse than no handle.
fn trace_entries(events: &[nscore::Event], turn: u32) -> Vec<(Option<nscore::EventId>, String)> {
    events
        .iter()
        .filter(|e| e.turn == turn)
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => {
                Some((None, format!("Proposed({})", proposal.action)))
            }
            EventKind::Rejected { reason, .. } => Some((
                None,
                match reason {
                    RejectReason::Malformed { detail } => format!("Rejected(malformed: {detail})"),
                    // Named as an endpoint problem, because this line is fed back
                    // to the emitter as context: telling it three times a turn
                    // that it produced bad output, while the endpoint was down,
                    // is teaching it the wrong lesson about its own behaviour.
                    RejectReason::ProviderUnavailable { status, detail } => {
                        format!("(provider unavailable: HTTP {status}: {detail})")
                    }
                    RejectReason::IllegalAction { action } => {
                        format!("Rejected(illegal action: {action})")
                    }
                    RejectReason::GuardDenied { guard, reason } => {
                        format!("Rejected(guard {guard}: {reason})")
                    }
                },
            )),
            EventKind::ToolReturned { outcome, .. } => Some(match outcome {
                ToolOutcome::Ok { output } => {
                    (Some(e.id), format!("ToolReturned(ok: {})", output.summary))
                }
                ToolOutcome::Err { kind, detail } => {
                    (None, format!("ToolReturned(err {kind}: {detail})"))
                }
            }),
            _ => None,
        })
        .collect()
}

/// One human-readable line per this-turn event: outcomes AND refusal reasons.
/// Public because `ns-app echo` reconstructs, from a stored log, the material
/// a reply was shown — and a second rendering of it would drift.
pub fn turn_trace(events: &[nscore::Event], turn: u32) -> String {
    trace_entries(events, turn)
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Longest a single trace line may be when it goes into a prompt.
///
/// The trace is re-sent to the emitter on *every* iteration of the turn, so a
/// large tool result is not paid for once but once per remaining step. On a
/// desktop task that is the dominant cost: `pointer_ui_read` renders the
/// visible control tree to one line — around 800 nodes on a browser page —
/// and fifteen iterations after it, the same text has been sent fifteen more
/// times.
///
/// 1200 characters keeps what a model actually needs from a result: whether it
/// worked, and the first screenful of what came back. What is dropped is
/// counted rather than silently cut, so a model that needs the rest knows to
/// narrow its query — `pointer_ui_find` over `pointer_ui_read` — instead of
/// concluding the screen is empty.
const TRACE_LINE_MAX_CHARS: usize = 1200;

/// Clip one trace line, on a character boundary, saying what was dropped and
/// — for a tool result — how to get it.
///
/// The dropped text is not gone: the whole result is in the log, inside the
/// `ToolReturned` event this line came from. Naming that event turns the cap
/// from a loss into a page boundary, which is the difference between an
/// agent that narrows its query and one that concludes the screen is empty.
/// Without a handle the only recovery is running the tool again, and on a
/// desktop that is neither free nor guaranteed to return the same thing.
///
/// Char boundaries rather than bytes: these lines carry window titles and
/// control names, which on this machine are Czech, and slicing a UTF-8
/// sequence in half would panic.
fn clip_trace_line(line: &str, max: usize, handle: Option<nscore::EventId>) -> String {
    let total = line.chars().count();
    if total <= max {
        return line.to_string();
    }
    let head: String = line.chars().take(max).collect();
    match handle {
        Some(id) => format!(
            "{head}… [{}: {total} chars, {max} shown — {INSPECT_RESULT} to see more]",
            result_handle(id)
        ),
        None => format!("{head}… [{} more characters]", total - max),
    }
}

/// How a tool result is named in a prompt: `r42` for event 42.
///
/// Short because it is repeated in every clipped line, and prefixed because a
/// bare number in a trace reads as data from the tool rather than as an
/// address.
fn result_handle(id: nscore::EventId) -> String {
    format!("r{}", id.0)
}

/// The event id behind a handle, accepting `r42` and `42` alike. A small
/// model that echoes the number without the prefix has still identified the
/// result it means.
fn parse_result_handle(raw: &str) -> Option<nscore::EventId> {
    raw.trim()
        .trim_start_matches(['r', 'R'])
        .parse::<u64>()
        .ok()
        .map(nscore::EventId)
}

/// Unqueried inspections of `id` already made this turn — the page number of
/// the next one. Read out of this turn's own events, so a replay pages
/// identically.
fn inspect_page(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> usize {
    events
        .iter()
        .filter(|e| e.turn == turn)
        .filter_map(|e| match &e.kind {
            EventKind::ToolCalled { action, args } if action == INSPECT_RESULT => Some(args),
            _ => None,
        })
        .filter(|args| {
            let arg = |name: &str| {
                args.iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, tv)| tv.value.clone())
            };
            let same = arg("id")
                .as_ref()
                .and_then(|v| v.as_str())
                .and_then(parse_result_handle)
                == Some(id);
            let unqueried = arg("query")
                .as_ref()
                .and_then(|v| v.as_str())
                .map(|q| q.trim().is_empty())
                .unwrap_or(true);
            same && unqueried
        })
        .count()
}

/// The trace as the models should see it: every line clipped, and the
/// characters the cap removed.
///
/// The second number is what the budget report is about. It is the
/// difference between the text a tool produced and the text the model was
/// shown, and until it is recorded there is no way to tell a cap that is
/// saving a turn from one that is hiding the answer.
fn trace_for_prompt(events: &[nscore::Event], turn: u32) -> (Vec<String>, usize) {
    let mut dropped = 0;
    let lines = trace_entries(events, turn)
        .into_iter()
        .map(|(handle, line)| {
            dropped += line.chars().count().saturating_sub(TRACE_LINE_MAX_CHARS);
            clip_trace_line(&line, TRACE_LINE_MAX_CHARS, handle)
        })
        .collect();
    (lines, dropped)
}

/// The full text of a tool result recorded this turn, by handle.
///
/// Only `Ok` outcomes: an error's detail is already short and is never
/// clipped, so there is nothing behind it to page through.
fn result_text(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> Option<String> {
    events
        .iter()
        .find(|e| e.id == id && e.turn == turn)
        .and_then(|e| match &e.kind {
            EventKind::ToolReturned {
                outcome: ToolOutcome::Ok { output },
                ..
            } => Some(output.summary.clone()),
            _ => None,
        })
}

/// The trust of the result behind a handle, so an inspected window carries
/// the trust of the tool that produced it rather than `System` by default.
fn result_trust(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> nscore::Trust {
    events
        .iter()
        .find(|e| e.id == id && e.turn == turn)
        .and_then(|e| match &e.kind {
            EventKind::ToolReturned {
                outcome: ToolOutcome::Ok { output },
                ..
            } => Some(output.trust),
            _ => None,
        })
        .unwrap_or(nscore::Trust::System)
}

/// Handles of this turn's results that the cap actually shortened — the set
/// `inspect_result` is legal over.
///
/// Derived from this turn's own events, never from the store: legality that
/// depends on stored state makes replay from a fresh store diverge, which is
/// the rule M6 §15 records after `forget_all` was written that way once.
fn clipped_results(events: &[nscore::Event], turn: u32) -> Vec<nscore::EventId> {
    trace_entries(events, turn)
        .into_iter()
        .filter(|(handle, line)| handle.is_some() && line.chars().count() > TRACE_LINE_MAX_CHARS)
        .filter_map(|(handle, _)| handle)
        .collect()
}

/// The inclusive turn range of a verbatim window; `None` when it is empty.
fn window_range(window: &[nscore::TurnRecord]) -> Option<(u32, u32)> {
    match (window.first(), window.last()) {
        (Some(first), Some(last)) => Some((first.turn, last.turn)),
        _ => None,
    }
}

/// What the emitter was shown, in keys (M7 T0.1). Built from the context
/// immediately before it is moved into the call, so the two cannot drift.
fn emitter_manifest(
    ctx: &nscore::EmitterContext,
    tools: usize,
    clipped_chars: usize,
) -> nscore::ContextManifest {
    nscore::ContextManifest {
        fact_keys: ctx.facts.iter().map(|f| f.key.clone()).collect(),
        summary_through: ctx.summary.as_ref().map(|s| s.through_turn),
        window: window_range(&ctx.window),
        trace_lines: ctx.trace_so_far.len(),
        trace_chars: ctx.trace_so_far.iter().map(|l| l.chars().count()).sum(),
        clipped_chars,
        tools,
        guidance: ctx.guidance.len(),
    }
}

/// What the replier was shown. `tools` is zero: the reply model is given no
/// action schema at all, which is half of why it is the cheaper of the two.
fn reply_manifest(ctx: &nscore::ReplyContext, clipped_chars: usize) -> nscore::ContextManifest {
    nscore::ContextManifest {
        fact_keys: ctx.facts.iter().map(|f| f.key.clone()).collect(),
        summary_through: ctx.summary.as_ref().map(|s| s.through_turn),
        window: window_range(&ctx.window),
        trace_lines: ctx.turn_trace.lines().count(),
        trace_chars: ctx.turn_trace.chars().count(),
        clipped_chars,
        tools: 0,
        guidance: ctx.guidance.len(),
    }
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

    /// A result that fits is passed through untouched: most of them do, and a
    /// trace full of "[0 more characters]" would be noise.
    #[test]
    fn a_short_trace_line_is_left_alone() {
        let line = "ToolReturned(ok: moved to (960, 540))";
        let handle = Some(nscore::EventId(7));
        assert_eq!(clip_trace_line(line, TRACE_LINE_MAX_CHARS, handle), line);
        assert_eq!(clip_trace_line("exactly ten", 11, handle), "exactly ten");
    }

    /// The case the cap exists for: a `pointer_ui_read` rendered to one line.
    /// The head survives, and what was dropped is counted rather than silently
    /// cut, so a model can tell the difference between an empty screen and a
    /// result it should have narrowed.
    #[test]
    fn a_long_trace_line_keeps_its_head_and_says_what_was_dropped() {
        let line = format!("ToolReturned(ok: {})", "node ".repeat(500));
        let clipped = clip_trace_line(&line, 100, Some(nscore::EventId(42)));
        assert!(
            clipped.starts_with("ToolReturned(ok: node node"),
            "{clipped}"
        );
        // The drop is visible, and addressable: the handle names the event
        // the rest is still sitting in.
        assert!(
            clipped.contains(&format!(
                "[r42: {} chars, 100 shown — inspect_result to see more]",
                line.chars().count()
            )),
            "{clipped}"
        );
        // 100 kept, plus the note.
        assert_eq!(clipped.chars().take(100).count(), 100);
        assert!(clipped.chars().count() < line.chars().count());
        // A line with no result behind it — a guard denial — still says what
        // it dropped, but promises nothing that can be fetched.
        let denial = clip_trace_line(&line, 100, None);
        assert!(denial.contains("more characters]"), "{denial}");
        assert!(!denial.contains(INSPECT_RESULT), "{denial}");
    }

    #[test]
    fn a_handle_round_trips_and_tolerates_a_bare_number() {
        let id = nscore::EventId(42);
        assert_eq!(result_handle(id), "r42");
        assert_eq!(parse_result_handle("r42"), Some(id));
        assert_eq!(parse_result_handle(" R42 "), Some(id));
        assert_eq!(parse_result_handle("42"), Some(id), "a bare number is one");
        assert_eq!(parse_result_handle("rubbish"), None);
    }

    /// Paging is by character window, and the window is anchored a little
    /// before a match so the match arrives with the context that makes it
    /// usable — in a control tree, the role and name that precede a point.
    #[test]
    fn a_result_window_pages_and_anchors_on_a_query() {
        let text: String = (0..50)
            .map(|i| format!("button \"item{i}\" ({i},{i}) | "))
            .collect();
        let total = text.chars().count();

        let (first, start, end) = result_window(&text, None, 0, 100);
        assert_eq!((start, end), (0, 100));
        assert!(first.starts_with("button \"item0\""));
        let (second, start, _) = result_window(&text, None, 1, 100);
        assert_eq!(start, 100);
        assert_ne!(first, second, "page 1 is not page 0");

        // Past the end is empty rather than an error: "no more" is an answer.
        assert!(result_window(&text, None, 999, 100).0.is_empty());

        let (hit, start, _) = result_window(&text, Some("item47"), 0, 100);
        assert!(hit.contains("item47"), "{hit}");
        assert!(start > 0, "anchored at the match, not at the top");
        assert!(
            hit.contains("button"),
            "the match arrives with its context: {hit}"
        );
        assert!(result_window(&text, Some("nothing here"), 0, 100)
            .0
            .is_empty());
        assert_eq!(total, text.chars().count());
    }

    /// The window counts characters, because these results carry Czech
    /// control names and an emoji round-trips through the clipboard test.
    #[test]
    fn a_result_window_never_splits_a_character() {
        let text = "Průzkumník souborů 🐎 Zrušit Tlačítko Systémové hodiny".repeat(4);
        for max in 1..40 {
            let (window, _, _) = result_window(&text, None, 0, max);
            assert_eq!(window.chars().count(), max);
        }
        let (window, _, _) = result_window(&text, Some("Zrušit"), 0, 20);
        assert!(window.contains("Zrušit"), "{window}");
    }

    /// Window titles and control names on this machine are Czech, and the
    /// clipboard round-trip test types an emoji. Slicing a UTF-8 sequence in
    /// half would panic, so the cap counts characters.
    #[test]
    fn clipping_never_splits_a_character() {
        let line = "Průzkumník souborů 🐎 Zrušit Tlačítko Systémové hodiny";
        for max in 1..line.chars().count() {
            let clipped = clip_trace_line(line, max, Some(nscore::EventId(1)));
            assert!(clipped.chars().count() >= max, "max {max}: {clipped}");
        }
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
