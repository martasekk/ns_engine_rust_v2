//! T5.1, memory half: the six M6 Phase 7 memory abilities as a fixed,
//! model-free task set (M7 plan §9).
//!
//! "Don't Blame the LLM" (2607.03691) is the whole design of this file: fix
//! the model, vary the harness, measure. Every model here is a scripted
//! double, so a number that moves between two runs moved because the harness
//! changed. There is no sampling temperature to blame it on, and no provider
//! to be down.
//!
//! **What is graded is the `ReplyContext` the engine built, never the string
//! a double handed back.** A double can be made to say "Martin" with the
//! memory ripped out entirely, because the string lives in the test. Only the
//! context proves the harness remembered, so every fixture captures the
//! context with the probe-replier pattern `turn_loop.rs` uses and grades what
//! is in it.
//!
//! Each ability returns an [`Ability`] row instead of asserting inline. A
//! regression has to say *which* of the six broke and *by how much* — that is
//! the row `ns-app eval` (T5.2) will append to the ledger and diff against
//! the previous one.
//!
//! Each fixture names the recorded failure it stands for. They are not
//! hypotheticals: M6 spec §2 lists F1–F9 from the analysis of the 2026-09-02
//! session, and §4.6 maps turns 76, 85, 93, 104 and 114 of that session to
//! what should happen now instead.

use nscore::*;
use nsengine::script::*;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig, ASK_CLARIFICATION, FORGET_FACT, RECALL, REMEMBER_FACT};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct NullChannel;
#[async_trait::async_trait]
impl Channel for NullChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// What the reply model was shown
// ---------------------------------------------------------------------------

/// Everything the reply model was shown, composed from the blocks
/// `CloudReplier` composes and in M6 §4.3's order: facts, summary, the
/// verbatim window, the user's message, this turn's trace.
///
/// The test cannot call the real replier — `ns-engine` does not depend on
/// `ns-llm`, and an integration test sees public API only — so it composes
/// the same blocks from the same `nscore` renderers the real prompt is built
/// from. That is what makes "prompt characters" a number about this harness
/// rather than about a provider's tokenizer: it counts what the engine put in
/// front of the model, in the engine's own rendering.
fn render_prompt(ctx: &ReplyContext) -> String {
    let mut s = String::new();
    if !ctx.persona.is_empty() {
        s.push_str(&ctx.persona);
        s.push('\n');
    }
    for f in &ctx.facts {
        s.push_str(&format!("- {}\n", render_fact(f)));
    }
    if let Some(summary) = &ctx.summary {
        s.push_str(&render_summary(summary));
        s.push('\n');
    }
    if !ctx.window.is_empty() {
        s.push_str(&render_window(&ctx.window, ctx.window.len(), &ctx.caps));
        s.push('\n');
    }
    s.push_str(&ctx.user_text);
    s.push('\n');
    s.push_str(&ctx.turn_trace);
    for g in &ctx.guidance {
        s.push_str(&format!("\n- {g}"));
    }
    s
}

/// One reply context, kept as strings so a fixture can ask what the model
/// could read.
#[derive(Clone)]
struct Shown {
    /// One `render_fact` line per fact, exactly as the prompt carries it —
    /// including the `(was "Martin" until 17:35 UTC)` marker, which is how a
    /// superseded value reaches the model at all.
    facts: Vec<String>,
    fact_keys: Vec<String>,
    window: String,
    trace: String,
    do_not_state: Vec<String>,
    prompt: String,
}

impl Shown {
    fn capture(ctx: &ReplyContext) -> Self {
        Self {
            facts: ctx.facts.iter().map(render_fact).collect(),
            fact_keys: ctx.facts.iter().map(|f| f.key.clone()).collect(),
            window: render_window(&ctx.window, ctx.window.len(), &ctx.caps),
            trace: ctx.turn_trace.clone(),
            do_not_state: ctx.do_not_state.clone(),
            prompt: render_prompt(ctx),
        }
    }

    fn chars(&self) -> usize {
        self.prompt.chars().count()
    }

    /// Whether a value was anywhere the model could read it. Case-insensitive
    /// substring, which is the same test the grounding interceptor applies in
    /// `ground::Material::contains` — a fixture must not hold the harness to a
    /// stricter standard than the harness holds a reply to.
    fn shows(&self, value: &str) -> bool {
        self.prompt.to_lowercase().contains(&value.to_lowercase())
    }

    fn fact_line(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}: ");
        self.facts
            .iter()
            .find(|l| l.starts_with(&prefix))
            .map(String::as_str)
    }

    fn rows_for(&self, key: &str) -> usize {
        self.fact_keys.iter().filter(|k| *k == key).count()
    }
}

/// The reply double: captures the context, then says nothing that could be
/// mistaken for an answer.
///
/// `first_draft`, when set, makes the first draft state a value out of
/// nowhere, so the abstention fixture can grade the grounding interceptor
/// (M6 §4.5) — which is harness, not model — instead of the double's manners.
/// The regeneration falls back to the inert marker.
struct Probe {
    shown: Arc<Mutex<Vec<Shown>>>,
    first_draft: Option<&'static str>,
    drafts: AtomicU32,
}

/// Deliberately claim-free: no digits, no mid-sentence capitals, no quotes,
/// so the grounding interceptor has nothing to find and `flags` counts only
/// what a fixture asked for.
const INERT: &str = "(scripted reply)";

#[async_trait::async_trait]
impl Replier for Probe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        let n = self.drafts.fetch_add(1, Ordering::SeqCst);
        self.shown.lock().unwrap().push(Shown::capture(&ctx));
        match self.first_draft {
            Some(draft) if n == 0 => Ok(draft.into()),
            _ => Ok(INERT.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// The harness under test
// ---------------------------------------------------------------------------

/// A clock that advances one second per read, shared by every engine a
/// fixture builds.
///
/// Not the frozen `Timestamp(42)` of `turn_loop.rs`: two of these fixtures
/// supersede a fact, and `render_fact` prints the superseded value's
/// `valid_to` as wall-clock time. Under a frozen clock every "what was it
/// before" marker reads `until 00:00 UTC`, which is exactly the rendering a
/// small model has to make sense of, so it should be real.
fn clock(ticks: Arc<AtomicU64>) -> Box<dyn Fn() -> Timestamp + Send + Sync> {
    const BASE_MS: u64 = 1_788_370_524_628;
    Box::new(move || Timestamp(BASE_MS + ticks.fetch_add(1, Ordering::SeqCst) * 1_000))
}

struct Harness {
    store: Arc<InMemoryStore>,
    shown: Arc<Mutex<Vec<Shown>>>,
    sessions: Mutex<Vec<SessionId>>,
    ticks: Arc<AtomicU64>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(InMemoryStore::new()),
            shown: Arc::new(Mutex::new(Vec::new())),
            sessions: Mutex::new(Vec::new()),
            ticks: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn turn(&self, session: &SessionId, text: &str, script: Vec<Proposal>) -> Vec<Shown> {
        self.turn_drafting(session, text, script, None).await
    }

    /// Runs one turn on a freshly built engine over the shared store.
    ///
    /// A fresh engine per turn is `turn_loop.rs`'s idiom whenever a fixture
    /// needs a different script on each turn (`facts5`, `forget`), and it is
    /// safe because `run_turn` reloads the log from the store and folds it:
    /// nothing a turn learned lives in the engine. It is also what these
    /// fixtures need — `ScriptedEmitter` pops one queue for its whole life, so
    /// one engine could not both remember on turn 1 and stay quiet on turn 2.
    async fn turn_drafting(
        &self,
        session: &SessionId,
        text: &str,
        script: Vec<Proposal>,
        first_draft: Option<&'static str>,
    ) -> Vec<Shown> {
        {
            let mut seen = self.sessions.lock().unwrap();
            if !seen.contains(session) {
                seen.push(session.clone());
            }
        }
        let before = self.shown.lock().unwrap().len();
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(script)));
        b.set_replier(Box::new(Probe {
            shown: self.shown.clone(),
            first_draft,
            drafts: AtomicU32::new(0),
        }));
        b.set_memory(self.store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut engine = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig {
                // The double replies with a fixed marker rather than prose,
                // and that marker is verbatim in the window from turn 2 on,
                // so the echo monitor would fire on every turn and measure
                // nothing. `turn_loop.rs` turns it off on its own doubles for
                // the same reason. The grounding check stays on: it is the
                // interceptor the abstention fixture grades.
                max_echo_ratio: 1.1,
                ..EngineConfig::default()
            },
            clock(self.ticks.clone()),
        );
        engine
            .run_turn(Incoming {
                session: session.clone(),
                text: text.into(),
            })
            .await
            .unwrap();
        self.shown.lock().unwrap()[before..].to_vec()
    }

    /// Reply-model calls over the whole fixture: one per captured context, so
    /// a regeneration counts, which is the point of counting it.
    fn reply_calls(&self) -> usize {
        self.shown.lock().unwrap().len()
    }

    /// The largest reply prompt the fixture ever built — the plan's "peak
    /// prompt", metric 2.
    fn peak_chars(&self) -> usize {
        self.shown
            .lock()
            .unwrap()
            .iter()
            .map(Shown::chars)
            .max()
            .unwrap_or(0)
    }

    /// The plan's per-run numbers, read back off the event log of every
    /// session the fixture touched. Read back rather than counted by hand: a
    /// number a fixture computes for itself drifts from what the harness did,
    /// and then the regression set is measuring the test.
    async fn counters(&self) -> Counters {
        let sessions = self.sessions.lock().unwrap().clone();
        let mut c = Counters::default();
        for sid in sessions {
            let events = self.store.load(&sid).await.unwrap();
            let recall_calls: HashSet<u64> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::ToolCalled { action, .. } if action == RECALL => Some(e.id.0),
                    _ => None,
                })
                .collect();
            for e in &events {
                match &e.kind {
                    EventKind::UserSaid { .. } => c.turns += 1,
                    EventKind::Proposed { proposal } => {
                        c.emitter_calls += 1;
                        if proposal.action == ASK_CLARIFICATION {
                            c.clarifications += 1;
                        }
                    }
                    EventKind::ToolCalled { .. } => c.tool_calls += 1,
                    EventKind::ReplyFlagged { .. } => c.flags += 1,
                    EventKind::ToolReturned {
                        call,
                        outcome: ToolOutcome::Ok { output },
                    } if recall_calls.contains(&call.0) => {
                        // `recall` joins its hits with "; " and says "no
                        // matches" when it found none (M6 §7). Zero is the
                        // number abstention is graded on, so it has to be
                        // read off the result the model was shown, not
                        // inferred from the call having happened.
                        c.recall_hits += if output.summary == "no matches" {
                            0
                        } else {
                            output.summary.split("; ").count()
                        };
                    }
                    _ => {}
                }
            }
            c.recall_calls += recall_calls.len();
        }
        c
    }
}

#[derive(Default)]
struct Counters {
    turns: usize,
    emitter_calls: usize,
    tool_calls: usize,
    recall_calls: usize,
    recall_hits: usize,
    flags: usize,
    clarifications: usize,
}

fn remember(key: &str, value: &str) -> Proposal {
    Proposal {
        rationale: "durable".into(),
        action: REMEMBER_FACT.into(),
        args: serde_json::json!({ "key": key, "value": value }),
    }
}

fn forget(key: &str) -> Proposal {
    Proposal {
        rationale: "the user asked".into(),
        action: FORGET_FACT.into(),
        args: serde_json::json!({ "key": key }),
    }
}

/// The query is a span of the user's own words, so provenance classifies it
/// `UserInput` rather than `Residual` — a recall query the model invented is
/// a different failure and would muddy this one.
fn recall_for(query: &str) -> Proposal {
    Proposal {
        rationale: "not in the window".into(),
        action: RECALL.into(),
        args: serde_json::json!({ "query": query }),
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// One ability, graded, with the numbers the plan asks each run to report
/// (§9): completion, requests, prompt size, peak prompt, tool calls, recall
/// hits. `inspect_result` calls, fit drops and tier are the desktop half's
/// and the later phases' columns; nothing in the memory half clips a result
/// or routes a turn, so they would be six zeroes.
struct Ability {
    ability: &'static str,
    passed: bool,
    turns: usize,
    /// Emitter iterations plus reply calls — model requests, which is the
    /// scarce resource on the free tier (fifty a day), not tokens.
    requests: usize,
    /// The reply prompt on the turn the ability is decided on.
    prompt_chars: usize,
    /// Four characters to the token, `nscore`'s own offline estimate, so this
    /// column and the engine's budget report cannot disagree.
    prompt_tokens: u32,
    peak_chars: usize,
    tool_calls: usize,
    recall_fired: bool,
    recall_hits: usize,
    /// Grounding-interceptor flags (M6 §4.5).
    flags: usize,
    /// Empty on a pass; on a failure, every condition that failed and what
    /// the harness showed instead.
    detail: String,
}

impl Ability {
    fn build(
        ability: &'static str,
        graded: &Shown,
        h: &Harness,
        c: &Counters,
        fails: Vec<String>,
    ) -> Self {
        Self {
            ability,
            passed: fails.is_empty(),
            turns: c.turns,
            requests: c.emitter_calls + h.reply_calls(),
            prompt_chars: graded.chars(),
            prompt_tokens: estimate_tokens(graded.chars()),
            peak_chars: h.peak_chars(),
            tool_calls: c.tool_calls,
            recall_fired: c.recall_calls > 0,
            recall_hits: c.recall_hits,
            flags: c.flags,
            detail: fails.join("; "),
        }
    }
}

/// One graded condition. Collected rather than asserted: a run has to report
/// every condition that failed, across all six abilities, or the first broken
/// one hides the rest and the next run rediscovers them one at a time.
fn require(fails: &mut Vec<String>, ok: bool, why: impl FnOnce() -> String) {
    if !ok {
        fails.push(why());
    }
}

/// Header, rule and every row through one set of widths, so a column cannot
/// drift out of line with its heading when a number grows.
fn row_line(cells: [&str; 11]) -> String {
    format!(
        "  {:<24}  {:<4}  {:>5}  {:>4}  {:>6}  {:>4}  {:>4}  {:>5}  {:<6}  {:>4}  {:>5}\n",
        cells[0],
        cells[1],
        cells[2],
        cells[3],
        cells[4],
        cells[5],
        cells[6],
        cells[7],
        cells[8],
        cells[9],
        cells[10],
    )
}

fn render_table(rows: &[Ability]) -> String {
    let mut out = String::from("\nM7 T5.1 — memory abilities, scripted model\n\n");
    out.push_str(&row_line([
        "ability", "pass", "turns", "reqs", "prompt", "~tok", "peak", "tools", "recall", "hits",
        "flags",
    ]));
    out.push_str(&row_line([
        "------------------------",
        "----",
        "-----",
        "----",
        "------",
        "----",
        "----",
        "-----",
        "------",
        "----",
        "-----",
    ]));
    for r in rows {
        out.push_str(&row_line([
            r.ability,
            if r.passed { "ok" } else { "FAIL" },
            &r.turns.to_string(),
            &r.requests.to_string(),
            &r.prompt_chars.to_string(),
            &r.prompt_tokens.to_string(),
            &r.peak_chars.to_string(),
            &r.tool_calls.to_string(),
            if r.recall_fired { "yes" } else { "no" },
            &r.recall_hits.to_string(),
            &r.flags.to_string(),
        ]));
    }
    out.push_str(&format!(
        "\n  {}/{} abilities pass.\n",
        rows.iter().filter(|r| r.passed).count(),
        rows.len()
    ));
    for r in rows.iter().filter(|r| !r.passed) {
        out.push_str(&format!("  {} FAILED: {}\n", r.ability, r.detail));
    }
    out
}

// ---------------------------------------------------------------------------
// The six abilities (M6 spec §11, Phase 7)
// ---------------------------------------------------------------------------

/// **Information extraction.** Name, age and a preference stated over eight
/// turns, all three asked for on turn 9.
///
/// F1 and F2, together. F1: the replier never saw the user's message —
/// `turn_trace` carried Proposed/Rejected/ToolReturned lines and
/// `state_summary` was "turn N, M messages", so on a plain `respond_directly`
/// turn the reply model got a counter. F2: the emitter never saw facts, so on
/// recorded turn 85 ("whats my name") it asked the user for a name the
/// replier already held; §4.6 says that turn should now settle on
/// `respond_directly` with `user.name` in front of both models.
///
/// The evidence that makes this non-trivial: turn 1 and turn 2 have fallen
/// out of the six-turn window by turn 9, so the graded check is that the
/// window does *not* carry "Martin" while the context does. The values are
/// there because they were pinned, which is why no `recall` is needed — the
/// pass condition in the Phase 7 table.
async fn information_extraction() -> Ability {
    let h = Harness::new();
    let sid = SessionId("eval-extraction".into());
    h.turn(
        &sid,
        "my name is Martin",
        vec![remember("user.name", "Martin")],
    )
    .await;
    h.turn(&sid, "and i am 17", vec![remember("user.age", "17")])
        .await;
    h.turn(
        &sid,
        "i prefer 24-hour times",
        vec![remember("user.preference.time_format", "24-hour")],
    )
    .await;
    for text in [
        "what else can you do",
        "never mind that",
        "tell me something",
        "still there?",
        "good",
    ] {
        h.turn(&sid, text, vec![]).await;
    }
    let shown = h
        .turn(&sid, "remind me who i am and how i like times", vec![])
        .await;
    let graded = &shown[0];

    let mut fails = Vec::new();
    for (key, value) in [
        ("user.name", "Martin"),
        ("user.age", "17"),
        ("user.preference.time_format", "24-hour"),
    ] {
        match graded.fact_line(key) {
            Some(line) => require(&mut fails, line.contains(value), || {
                format!("{key} shown as {line:?}, not {value:?}")
            }),
            None => fails.push(format!("{key} was not in the reply context")),
        }
    }
    require(&mut fails, !graded.window.contains("Martin"), || {
        "turn 1 is still in the window, so this proves nothing about facts".into()
    });
    let c = h.counters().await;
    require(&mut fails, c.recall_calls == 0, || {
        format!(
            "{} recall calls; pinned facts should need none",
            c.recall_calls
        )
    });
    Ability::build("information extraction", graded, &h, &c, fails)
}

/// **Multi-session reasoning.** A fact stated in one session, needed in
/// another, under one scope.
///
/// F3 — "no memory beyond ~3 exchanges" — at its extreme: across sessions
/// there was none at all. M6 §6.6 makes the fact scope, not the session, the
/// unit of memory, which is also F9's answer (facts were global; correct for
/// a single-user CLI, a leak for the Telegram target).
///
/// The second session's window is empty, and that is the evidence: nothing
/// crossed through the transcript, because `search_turns` is per session by
/// construction. Only the scoped fact crossed. The failure this excludes is
/// the recorded one — asking the user again for something already stored.
async fn multi_session_reasoning() -> Ability {
    let h = Harness::new();
    let first = SessionId("eval-multi-a".into());
    let second = SessionId("eval-multi-b".into());
    h.turn(
        &first,
        "our budget is 2000 crowns",
        vec![remember("budget.total", "2000 crowns")],
    )
    .await;
    let shown = h.turn(&second, "what was the budget again?", vec![]).await;
    let graded = &shown[0];

    let mut fails = Vec::new();
    require(&mut fails, graded.window.is_empty(), || {
        format!("session two already had a window: {:?}", graded.window)
    });
    let c = h.counters().await;
    let from_memory = graded
        .fact_line("budget.total")
        .map(|l| l.contains("2000 crowns"))
        .unwrap_or(false)
        || c.recall_hits > 0;
    require(&mut fails, from_memory, || {
        format!(
            "neither facts nor recall carried the budget into session two; facts were {:?}",
            graded.facts
        )
    });
    require(&mut fails, c.clarifications == 0, || {
        format!(
            "{} clarification(s) asked instead of answering",
            c.clarifications
        )
    });
    Ability::build("multi-session reasoning", graded, &h, &c, fails)
}

/// **Temporal reasoning.** The name changes on turn 6; "what was it before"
/// on turn 12.
///
/// F4, and §4.6's recorded turn 104: "what was my name before" got "Peter,
/// your message was not delivered", because the rewrite had overwritten the
/// old value and there was nothing left to answer from. Versioned facts
/// (§6.1: supersede, never overwrite) put both values in one rendered line,
/// `user.name: "Peter" (was "Martin" until 17:35 UTC)`.
///
/// Turn 6 is inside the window at turn 12 and turn 1 is not, so "Peter" has
/// two sources and "Martin" has exactly one: the superseded marker. The graded
/// check is that the window is clean of "Martin" — otherwise the fixture would
/// pass on the transcript and say nothing about fact versioning.
async fn temporal_reasoning() -> Ability {
    let h = Harness::new();
    let sid = SessionId("eval-temporal".into());
    h.turn(
        &sid,
        "my name is Martin",
        vec![remember("user.name", "Martin")],
    )
    .await;
    for text in ["what can you do", "ok", "go on", "sure"] {
        h.turn(&sid, text, vec![]).await;
    }
    h.turn(
        &sid,
        "call me Peter from now on",
        vec![remember("user.name", "Peter")],
    )
    .await;
    for text in ["thanks", "fine", "carry on", "right", "one more thing"] {
        h.turn(&sid, text, vec![]).await;
    }
    let shown = h.turn(&sid, "what was my name before?", vec![]).await;
    let graded = &shown[0];

    let mut fails = Vec::new();
    match graded.fact_line("user.name") {
        Some(line) => require(
            &mut fails,
            line.starts_with("user.name: \"Peter\" (was \"Martin\" until "),
            || format!("superseded value not rendered: {line:?}"),
        ),
        None => fails.push("user.name was not in the reply context".into()),
    }
    require(&mut fails, !graded.window.contains("Martin"), || {
        "turn 1 is still in the window; the marker is not carrying the old value".into()
    });
    require(
        &mut fails,
        graded.shows("Peter") && graded.shows("Martin"),
        || "both values must be in the context for the answer to name both".into(),
    );
    let c = h.counters().await;
    Ability::build("temporal reasoning", graded, &h, &c, fails)
}

/// **Knowledge updates.** The age is restated with a new value.
///
/// F4: duplicates were written every turn and rewrites reset `uses`, so the
/// store held several rows for one key and the context could show two ages at
/// once — the ghost mixing §3 lists as the failure versioned facts exist to
/// prevent. §6.1: one `current` row, the one it replaced `superseded` with a
/// `valid_to`.
///
/// Graded on both sides, because either alone passes with the bug in place:
/// the store must hold exactly one current row, and the context must show
/// exactly one `user.age` line whose *value* is the new age. The old age is
/// legitimate material, but only behind the `(was …)` marker, which is why
/// the check splits the line there instead of searching the whole of it.
async fn knowledge_updates() -> Ability {
    let h = Harness::new();
    let sid = SessionId("eval-updates".into());
    h.turn(&sid, "i am 17", vec![remember("user.age", "17")])
        .await;
    h.turn(
        &sid,
        "wait, actually i am 18 now",
        vec![remember("user.age", "18")],
    )
    .await;
    let shown = h.turn(&sid, "how old am i?", vec![]).await;
    let graded = &shown[0];

    let mut fails = Vec::new();
    let history = h.store.fact_history("global", "user.age").await.unwrap();
    let current: Vec<&Fact> = history
        .iter()
        .filter(|f| f.state == FactState::Current)
        .collect();
    require(&mut fails, current.len() == 1, || {
        format!("{} current rows for user.age, want 1", current.len())
    });
    require(
        &mut fails,
        current.first().map(|f| f.value == serde_json::json!("18")) == Some(true),
        || {
            format!(
                "current value is {:?}, want \"18\"",
                current.first().map(|f| &f.value)
            )
        },
    );
    let superseded: Vec<&Fact> = history
        .iter()
        .filter(|f| f.state == FactState::Superseded)
        .collect();
    require(
        &mut fails,
        superseded.len() == 1 && superseded[0].valid_to.is_some(),
        || {
            format!(
                "{} superseded rows with a valid_to, want 1",
                superseded.len()
            )
        },
    );
    require(&mut fails, graded.rows_for("user.age") == 1, || {
        format!(
            "{} user.age lines in the context — ghost mixing: {:?}",
            graded.rows_for("user.age"),
            graded.facts
        )
    });
    match graded.fact_line("user.age") {
        Some(line) => {
            let value = line.split(" (was ").next().unwrap_or(line);
            require(
                &mut fails,
                value.contains("18") && !value.contains("17"),
                || format!("the current value reads {value:?}, want just the new age"),
            );
        }
        None => fails.push("user.age was not in the reply context".into()),
    }
    let c = h.counters().await;
    Ability::build("knowledge updates", graded, &h, &c, fails)
}

/// **Abstention.** A question about a fact that was never stated.
///
/// Two harness properties, because a scripted double cannot demonstrate the
/// third. The harness must (1) put no value there — `recall` searches and
/// returns "no matches", and no fact in the context answers the question —
/// and (2) catch a value stated anyway. Whether the *model* then abstains is
/// the model's business, and a double that politely said "I don't know" would
/// prove nothing about this harness at all.
///
/// So the double is made to invent one, which is the recorded failure this
/// stands for: turn 93 replied "the session has reached 185 messages…", a
/// number from nowhere. §4.6 records that the interceptor (§4.5) would have
/// flagged it. Here it does: `ReplyFlagged` is logged and the single
/// regeneration is handed the span by name in `do_not_state`. That
/// regeneration is a second billed call, which is why this row spends three
/// reply calls over two turns where every other fixture spends one per turn —
/// the cost of the check, measured rather than assumed.
///
/// The memory is not empty: `user.name` is stored first, so what is being
/// graded is a missing key, not a missing store (F4's junk keys with values
/// the user never said are the same failure from the other side).
async fn abstention() -> Ability {
    const INVENTED: &str = "Ostrava";
    const INVENTING_DRAFT: &str = "You live in Ostrava.";

    let h = Harness::new();
    let sid = SessionId("eval-abstention".into());
    h.turn(
        &sid,
        "my name is Martin",
        vec![remember("user.name", "Martin")],
    )
    .await;
    let shown = h
        .turn_drafting(
            &sid,
            "which city do i live in?",
            vec![recall_for("city do i live in")],
            Some(INVENTING_DRAFT),
        )
        .await;
    let graded = &shown[0];

    let mut fails = Vec::new();
    let c = h.counters().await;
    require(&mut fails, c.recall_calls == 1, || {
        format!("{} recall calls, want 1", c.recall_calls)
    });
    require(&mut fails, c.recall_hits == 0, || {
        format!(
            "recall returned {} hits for a fact never stated",
            c.recall_hits
        )
    });
    require(&mut fails, graded.trace.contains("no matches"), || {
        format!(
            "the reply model was not told the search found nothing: {:?}",
            graded.trace
        )
    });
    require(&mut fails, !graded.shows(INVENTED), || {
        "the context already carried a city; abstention is untestable here".into()
    });
    require(
        &mut fails,
        !graded.fact_keys.iter().any(|k| k.starts_with("user.city")),
        || format!("a city fact reached the context: {:?}", graded.fact_keys),
    );
    require(&mut fails, c.flags == 1, || {
        format!(
            "{} interceptor flags, want 1 for the invented value",
            c.flags
        )
    });
    require(&mut fails, shown.len() == 2, || {
        format!(
            "{} drafts; the flagged reply was not regenerated",
            shown.len()
        )
    });
    if let Some(second) = shown.get(1) {
        require(
            &mut fails,
            second.do_not_state.iter().any(|s| s == INVENTED),
            || {
                format!(
                    "the regeneration was not told what to drop: {:?}",
                    second.do_not_state
                )
            },
        );
    }
    Ability::build("abstention", graded, &h, &c, fails)
}

/// **Selective forgetting** (MemoryAgentBench, per the Phase 7 table).
/// `forget_fact`, then the same question again.
///
/// F4 listed "no way to forget" among the broken lifecycle; §4.6's turn 114
/// is what that cost — "can you reset the memory" was answered by *storing a
/// fact* called `memory_reset_requested = true`. §6.2 makes forgetting four
/// real mechanisms, of which this grades the soft delete.
///
/// The question is asked twice, on turn 8 and turn 10, and both prompts are
/// graded. Before: the value is in the context, so the harness could have
/// shown it. After: it is nowhere in the prompt — not in the facts, not in
/// the window, not in the trace. The stating turn is turn 1 and has fallen
/// out of the six-turn window by turn 8, which is what makes the "after"
/// check mean anything; the turn-9 trace line is `forget_fact -> ok: forgot
/// user.name`, which names the key and never the value.
async fn selective_forgetting() -> Ability {
    let h = Harness::new();
    let sid = SessionId("eval-forgetting".into());
    h.turn(
        &sid,
        "my name is Martin",
        vec![remember("user.name", "Martin")],
    )
    .await;
    for text in [
        "what can you do",
        "ok",
        "go on",
        "sure",
        "right",
        "one more thing",
    ] {
        h.turn(&sid, text, vec![]).await;
    }
    let before = h.turn(&sid, "what is my name?", vec![]).await;
    h.turn(&sid, "forget my name", vec![forget("user.name")])
        .await;
    let after = h.turn(&sid, "what is my name?", vec![]).await;
    let graded = &after[0];

    let mut fails = Vec::new();
    require(&mut fails, before[0].shows("Martin"), || {
        "the value was already absent before the forget; nothing was forgotten".into()
    });
    require(&mut fails, !graded.shows("Martin"), || {
        format!(
            "the forgotten value is still in the prompt:\n{}",
            graded.prompt
        )
    });
    require(&mut fails, graded.fact_line("user.name").is_none(), || {
        format!(
            "user.name is still a fact in the context: {:?}",
            graded.facts
        )
    });
    let history = h.store.fact_history("global", "user.name").await.unwrap();
    require(
        &mut fails,
        history.first().map(|f| f.state) == Some(FactState::Forgotten),
        || {
            format!(
                "the row was not soft-deleted: {:?}",
                history.first().map(|f| f.state)
            )
        },
    );
    let c = h.counters().await;
    Ability::build("selective forgetting", graded, &h, &c, fails)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_six_memory_abilities_pass_against_a_fixed_model() {
    let rows = vec![
        information_extraction().await,
        multi_session_reasoning().await,
        temporal_reasoning().await,
        knowledge_updates().await,
        abstention().await,
        selective_forgetting().await,
    ];
    let table = render_table(&rows);
    // Printed on every run, not only on a failure: `cargo test -- --nocapture`
    // is how the numbers are read off a passing harness, and comparing two
    // passing runs is the point of the set (plan §9, "harness release runs it
    // once"). On a failure the same table is the panic message.
    println!("{table}");
    assert!(
        rows.iter().all(|r| r.passed),
        "the memory task set regressed:\n{table}"
    );
}

/// The table is the deliverable, so it is graded too: a run that failed must
/// name the ability and the condition, or the ledger row says "6/6 → 5/6" and
/// nothing else.
#[test]
fn the_report_names_the_ability_that_broke_and_its_numbers() {
    let row = |ability, passed, detail: &str| Ability {
        ability,
        passed,
        turns: 9,
        requests: 19,
        prompt_chars: 512,
        prompt_tokens: 128,
        peak_chars: 700,
        tool_calls: 3,
        recall_fired: false,
        recall_hits: 0,
        flags: 0,
        detail: detail.into(),
    };
    let table = render_table(&[
        row("information extraction", true, ""),
        row("temporal reasoning", false, "superseded value not rendered"),
    ]);
    assert!(table.contains("ability                   pass"), "{table}");
    assert!(table.contains("information extraction    ok"), "{table}");
    assert!(table.contains("1/2 abilities pass."), "{table}");
    assert!(
        table.contains("temporal reasoning FAILED: superseded value not rendered"),
        "a failing row must name the condition, not just the ability:\n{table}"
    );
    // The numbers are the row, so every one of them has to be in it.
    for n in ["9", "19", "512", "128", "700"] {
        assert!(table.contains(n), "{n} missing from:\n{table}");
    }
}
