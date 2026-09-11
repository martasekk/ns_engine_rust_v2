//! T5.1/T5.2: the nine abilities of the fixed, model-free task set (M7 plan
//! §9) — the six M6 Phase 7 memory abilities, then the three desktop tasks.
//!
//! "Don't Blame the LLM" (2607.03691) is the whole design of this module: fix
//! the model, vary the harness, measure. Every model here is a scripted
//! double, so a number that moves between two runs moved because the harness
//! changed. There is no sampling temperature to blame it on, and no provider
//! to be down.
//!
//! **What is graded is the [`nscore::ReplyContext`] the engine built, never
//! the string a double handed back.** A double can be made to say "Martin"
//! with the memory ripped out entirely, because the string lives in the
//! fixture. Only the context proves the harness remembered, so every fixture
//! captures the context with the probe-replier pattern `turn_loop.rs` uses
//! and grades what is in it.
//!
//! Each ability returns an [`Ability`] row instead of asserting inline. A
//! regression has to say *which* of the six broke and *by how much* — that is
//! the row `ns-app eval` appends to the eval ledger and diffs against the
//! previous one.
//!
//! Each fixture names the recorded failure it stands for. They are not
//! hypotheticals: M6 spec §2 lists F1–F9 from the analysis of the 2026-09-02
//! session, and §4.6 maps turns 76, 85, 93, 104 and 114 of that session to
//! what should happen now instead.
//!
//! **The two halves exercise different machinery, and the columns say so.**
//! The memory fixtures never clip a result, never page one, never route a
//! turn and never go over the budget: their four context columns are zeros,
//! and those zeros are the evidence that the desktop half is measuring
//! something the memory half cannot reach. The desktop tasks stand for the
//! 2026-09-07 session, where two `pointer_ui_read` results were 14,425 and
//! 10,025 characters against a median tool result of 24, and the harness
//! re-sent the larger of them on every remaining iteration of the turn it
//! landed in — under `ns-run`'s twelve-iteration budget, up to eleven more
//! times, at roughly 3.6k tokens each (plan §2). They are graded on what the
//! harness put in front of the model — the trace lines the emitter was
//! shown, iteration by iteration — and never on what a double said about
//! them.
//!
//! **Why this is library code and not a test.** It began as an integration
//! test, which meant `cargo test` was the only thing that could run it and
//! the numbers existed only in captured stdout. The plan's exit criterion is
//! a *release gate* — "every harness release runs it once; that is the whole
//! procedure" (§9) — and a gate has to be runnable by whatever runs a
//! release. So the set lives here, [`run_all`] is the entry point, and
//! `crates/engine/tests/harness_eval.rs` is now one caller of it among two.
//! The doubles it needs (`script`, `store`) were already public for the same
//! reason.

use nscore::*;
use nsengine::router::KeywordRouter;
use nsengine::script::*;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{
    Engine, EngineConfig, ASK_CLARIFICATION, FORGET_FACT, INSPECT_RESULT, RECALL, REMEMBER_FACT,
};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct NullChannel;
#[async_trait::async_trait]
impl Channel for NullChannel {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
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
/// The fixture cannot call the real replier — `ns-engine` does not depend on
/// `ns-llm` — so it composes the same blocks from the same `nscore`
/// renderers the real prompt is built from. That is what makes "prompt
/// characters" a number about this harness rather than about a provider's
/// tokenizer: it counts what the engine put in front of the model, in the
/// engine's own rendering.
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
///
/// `Default` is the empty one, and it is not decoration: a turn that ran out
/// of iterations settles on the fallback text without ever calling the reply
/// model, so there is no context to grade. The fixture still owes the run a
/// row — a missing row would read as "not run" rather than "failed" — so it
/// grades the empty one and says why.
#[derive(Clone, Default)]
pub(crate) struct Shown {
    /// One `render_fact` line per fact, exactly as the prompt carries it —
    /// including the `(was "Martin" until 17:35 UTC)` marker, which is how a
    /// superseded value reaches the model at all.
    pub(crate) facts: Vec<String>,
    pub(crate) fact_keys: Vec<String>,
    pub(crate) window: String,
    pub(crate) trace: String,
    pub(crate) do_not_state: Vec<String>,
    pub(crate) prompt: String,
    /// The rolling summary block exactly as the prompt carries it, or `None`
    /// when the engine had no summary to show (M10 T5.1).
    ///
    /// Captured as its own field rather than inferred from `prompt`: the
    /// point of the fixtures module is to be able to say *this session
    /// carried a summary of n characters and the graded turn still failed*,
    /// and a substring search over the prompt cannot tell an absent block
    /// apart from a present one the question did not need.
    pub(crate) summary: Option<String>,
    /// The guidance notes rendered into this context, one per note (M10 T5.1).
    pub(crate) guidance: Vec<String>,
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
            summary: ctx.summary.as_ref().map(render_summary),
            guidance: ctx.guidance.clone(),
        }
    }

    pub(crate) fn chars(&self) -> usize {
        self.prompt.chars().count()
    }

    /// Whether a value was anywhere the model could read it. Case-insensitive
    /// substring, which is the same test the grounding interceptor applies in
    /// `ground::Material::contains` — a fixture must not hold the harness to a
    /// stricter standard than the harness holds a reply to.
    pub(crate) fn shows(&self, value: &str) -> bool {
        self.prompt.to_lowercase().contains(&value.to_lowercase())
    }

    pub(crate) fn fact_line(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}: ");
        self.facts
            .iter()
            .find(|l| l.starts_with(&prefix))
            .map(String::as_str)
    }

    pub(crate) fn rows_for(&self, key: &str) -> usize {
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
pub(crate) struct Probe {
    pub(crate) shown: Arc<Mutex<Vec<Shown>>>,
    pub(crate) first_draft: Option<&'static str>,
    pub(crate) drafts: AtomicU32,
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
        if let Some(sink) = &ctx.usage {
            sink.record(spent("replier"));
        }
        match self.first_draft {
            Some(draft) if n == 0 => Ok(draft.into()),
            _ => Ok(INERT.into()),
        }
    }
}

/// The emitter side of the same thing: the scripted double wrapped in what a
/// provider client does at the end of a call.
///
/// It exists for one reason. The engine appends a `ModelCall` — the event
/// carrying the [`ContextManifest`] of what that call was shown — once per
/// `Usage` it drains from the sink it handed the call in its context, so a
/// harness whose doubles leave nothing there records no manifests, and the
/// four context columns would then have to be recomputed here from the log.
/// A number a fixture computes for itself drifts from what the harness did,
/// and then the set measures the test; the same argument
/// [`Harness::counters`] is built on.
pub(crate) struct MeteredEmitter {
    pub(crate) inner: Box<dyn Emitter>,
}

#[async_trait::async_trait]
impl Emitter for MeteredEmitter {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        let sink = ctx.usage.clone();
        let proposed = self.inner.propose(ctx, legal).await;
        if let Some(sink) = sink {
            sink.record(spent("emitter"));
        }
        proposed
    }
}

/// What one call by a scripted double cost.
///
/// Zero tokens, marked estimated. There is no provider in this set, and a
/// fabricated prompt size would sit in the eval ledger beside measured
/// numbers with nothing to tell them apart — the objection `Usage::estimated`
/// exists to answer. What this carries is the manifest attached to it by the
/// engine; the prompt columns come from `estimate_tokens` over the context
/// the fixture rendered itself.
fn spent(role: &'static str) -> Usage {
    Usage {
        role: role.into(),
        model: "scripted".into(),
        prompt_tokens: 0,
        completion_tokens: 0,
        estimated: true,
        attempts: 1,
        latency_ms: 0,
        tools_tokens: 0,
        cached_tokens: 0,
    }
}

// ---------------------------------------------------------------------------
// The desktop the three task fixtures run against
// ---------------------------------------------------------------------------

/// The size of the larger of the two `pointer_ui_read` results in the
/// 2026-09-07 session (plan §2). The median tool result in that session was
/// 24 characters; one action produced 98% of the tool text.
const RECORDED_CHARS: usize = 14_425;

/// The control `find-and-click` reaches, and the first of the three
/// `open-and-search` clicks. In the modal, so inside the first 1,200
/// characters — the part the cap keeps.
const HEAD_CONTROL: &str = "Uložit jako…";
/// The other two, also inside the shown head.
const SEARCH_CONTROL: &str = "Najít v listu";
const RESULT_CONTROL: &str = "První výsledek";
/// The control the third task reaches: the last line of the tree, thirteen
/// thousand characters past the cap. Nothing but `inspect_result` can get to
/// it, which is the point — if the third task can pass without following the
/// handle, the fixture is measuring nothing.
const TAIL_CONTROL: &str = "Sloučit buňky";
/// Its click point. Unique in the tree, and the thing the emitter has to
/// come away with: the name is in the user's message, the point is only ever
/// in the tool's output.
const TAIL_POINT: (i64, i64) = (1704, 928);

/// The control the hard-query task reaches (M10 T0.3). **Not in
/// [`control_tree`] at all** — it is below the fold, so no `pointer_ui_read`
/// carries it and no `inspect_result` can page to it either. The only thing
/// on this desktop that brings it into view is `pointer_scroll`.
///
/// That is the whole point of the fixture. Tool-loading §5.3: a narrower
/// legal set looks like a pure win on every existing task, because none of
/// them needs a tool a narrow tier would withhold. This one does, so
/// withholding it reads as a failed ability rather than as a silent
/// capability loss.
const DEEP_CONTROL: &str = "Zmrazit příčky";
/// Its click point, off the filler grid (`x` is 1704, and the grid's
/// columns stop at 1560) and past the tree's own tail row, so `control_at`
/// cannot resolve a click to some other control.
const DEEP_POINT: (i64, i64) = (1704, 2104);
/// Notches, the unit `pointer_scroll` takes ("Positive dy scrolls down",
/// `components-std/src/pointer_tool.rs`).
const SCROLL_NOTCHES: i64 = 12;
/// The one action the hard query has, under the name the real tool uses.
const POINTER_SCROLL: &str = "pointer_scroll";

/// The recorded control tree, as the `pointer_ui_read` fixture.
///
/// **Not `NullPlatform`, which the plan names (§9).** `ns-engine` does not
/// depend on `ns-pointer` and should not start: the eval set would then pull
/// a desktop backend — and its transport, its screen model and its platform
/// code — into every crate that links the engine, to produce one string that
/// this file can produce itself. What the plan actually asks of the fixture
/// is its shape and its size, and both are recorded.
///
/// Shape from `UiView::render`: `MODAL (handle this before anything behind
/// it):` first, because that is where the real tool puts whatever is
/// blocking the screen, then one `role "name" (x,y)` line per control. Size
/// from the session: [`RECORDED_CHARS`].
fn control_tree() -> String {
    // Names that never collide with the four the tasks reach, so a control
    // is found because the harness carried the line it is on and not because
    // some filler row happened to spell it.
    const FILLER: &[(&str, &str)] = &[
        ("menuitem", "Formát"),
        ("menuitem", "Vložit"),
        ("button", "Tučné"),
        ("cell", "Řádek"),
        ("text", "Sloupec"),
        ("checkbox", "Zamknout"),
        ("button", "Filtr"),
        ("tab", "Sešit"),
    ];
    let mut s = String::from("MODAL (handle this before anything behind it):\n");
    s.push_str("  dialog \"Uložit změny v sešitu?\" (960,472)\n");
    s.push_str(&format!("  button \"{HEAD_CONTROL}\" (872,604)\n"));
    s.push_str("  button \"Zahodit změny\" (976,604)\n");
    s.push_str("  button \"Zrušit\" (1080,604)\n");
    s.push_str(&format!("button \"{SEARCH_CONTROL}\" (1240,96)\n"));
    s.push_str(&format!("button \"{RESULT_CONTROL}\" (1360,96)\n"));
    let tail = format!(
        "button \"{TAIL_CONTROL}\" ({},{})\n",
        TAIL_POINT.0, TAIL_POINT.1
    );
    let mut len = s.chars().count();
    let mut i = 0usize;
    while len + tail.chars().count() < RECORDED_CHARS {
        let (role, name) = FILLER[i % FILLER.len()];
        // Coordinates on a grid that never lands on any of the four points
        // above, so `control_at` cannot resolve a click to the wrong row.
        let row = format!(
            "{role} \"{name} {i}\" ({},{})\n",
            120 + (i % 9) * 180,
            96 + (i / 9) * 24
        );
        len += row.chars().count();
        s.push_str(&row);
        i += 1;
    }
    s.push_str(&tail);
    s
}

/// The control at a point, in the tool's own rendering — or `None`, which is
/// a click that hit nothing.
///
/// This is the world answering, not a double: a click is graded on what the
/// screen says was under it, so a fixture that reached the wrong control
/// fails loudly instead of passing on the fact that a click happened.
fn control_at(x: i64, y: i64) -> Option<String> {
    // Below the fold, and answered separately because it is deliberately not
    // in the rendered tree: a screen read never prints it, so the only way
    // an emitter can arrive at this point is out of a `pointer_scroll`
    // result. The click still lands — the control is real, it was just off
    // the visible page.
    if (x, y) == DEEP_POINT {
        return Some(format!("button {DEEP_CONTROL:?}"));
    }
    let point = format!(" ({x},{y})");
    control_tree()
        .lines()
        .find(|l| l.ends_with(&point))
        .map(|l| l.trim().trim_end_matches(&point).to_string())
}

fn tool_spec(
    name: &str,
    description: &str,
    side_effect: SideEffect,
    args: &[(&str, &str)],
) -> ActionSpec {
    let properties: serde_json::Map<String, serde_json::Value> = args
        .iter()
        .map(|(k, ty)| ((*k).to_string(), serde_json::json!({ "type": ty })))
        .collect();
    ActionSpec {
        name: name.into(),
        description: description.into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": args.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        }),
        side_effect,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// The three pointer actions a desktop turn is made of, under the names the
/// recorded session used.
///
/// `Irreversible` on the click and the keystroke is the real spec
/// (`components-std/src/pointer_tool.rs`): whatever is under the pointer
/// will be activated, and typed text goes to whatever has focus. It is why
/// the desktop harness runs with `confirm_irreversible: false` — see
/// [`Harness::desktop`].
enum Desk {
    Read,
    Click,
    Type,
    /// `Reversible`, as the real spec has it: scrolling back undoes it. It
    /// is also the only action on this desktop that reaches what is below
    /// the fold — see [`DEEP_CONTROL`].
    Scroll,
}

struct DeskTool {
    act: Desk,
    spec: ActionSpec,
}

impl DeskTool {
    fn read() -> Self {
        Self {
            act: Desk::Read,
            spec: tool_spec(
                "pointer_ui_read",
                "The remote machine's controls as text — role, name and a clickable point \
                 each. Anything blocking the screen is listed first under MODAL.",
                SideEffect::Pure,
                &[("query", "string")],
            ),
        }
    }

    fn click() -> Self {
        Self {
            act: Desk::Click,
            spec: tool_spec(
                "pointer_click",
                "Click on the remote machine. Irreversible: whatever is under the pointer \
                 will be activated.",
                SideEffect::Irreversible,
                &[("x", "integer"), ("y", "integer")],
            ),
        }
    }

    fn typing() -> Self {
        Self {
            act: Desk::Type,
            spec: tool_spec(
                "pointer_type",
                "Type text on the remote machine. Irreversible: it goes to whatever has focus.",
                SideEffect::Irreversible,
                &[("text", "string")],
            ),
        }
    }

    /// The tool the hard query needs and nothing else can stand in for
    /// (M10 T0.3). Description and side-effect class from the real spec.
    fn scroll() -> Self {
        Self {
            act: Desk::Scroll,
            spec: tool_spec(
                POINTER_SCROLL,
                "Scroll the remote machine in notches. Positive dy scrolls down.",
                SideEffect::Reversible,
                &[("dx", "integer"), ("dy", "integer")],
            ),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DeskTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(&self, args: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let text = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or_default();
        let n = |k: &str| args.get(k).and_then(serde_json::Value::as_i64);
        let summary = match self.act {
            // The query steers the real tool's text windows and never
            // filters, "so a bad query cannot hide the control that was
            // needed" — so the whole tree comes back whatever is asked for,
            // which is exactly the cost this phase exists to bound.
            Desk::Read => control_tree(),
            Desk::Click => {
                let (Some(x), Some(y)) = (n("x"), n("y")) else {
                    return Err(ToolError::Failed {
                        kind: "args".into(),
                        detail: "needs numeric x and y".into(),
                    });
                };
                match control_at(x, y) {
                    Some(control) => format!("clicked {control} at ({x},{y})"),
                    None => {
                        return Err(ToolError::Failed {
                            kind: "miss".into(),
                            detail: format!("no control at ({x},{y})"),
                        })
                    }
                }
            }
            Desk::Type => format!("typed {:?}", text("text")),
            // The result is the only place the below-the-fold control's
            // point ever appears, in the same `role "name" (x,y)` rendering
            // a screen read uses — so the double reads it out of what it was
            // shown, exactly as it reads a point out of `pointer_ui_read`.
            Desk::Scroll => {
                let Some(dy) = n("dy") else {
                    return Err(ToolError::Failed {
                        kind: "args".into(),
                        detail: "needs a numeric dy".into(),
                    });
                };
                format!(
                    "scrolled {dy} notches; below the fold: button \"{DEEP_CONTROL}\" ({},{})",
                    DEEP_POINT.0, DEEP_POINT.1
                )
            }
        };
        Ok(ToolOutput {
            summary,
            artifact: None,
            // What the remote machine reports, not what the user said — the
            // trust the real pointer tools return, and the reason a point
            // copied out of a screen read does not trip the taint gate.
            trust: Trust::System,
        })
    }
}

fn desktop_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(DeskTool::read()),
        Arc::new(DeskTool::click()),
        Arc::new(DeskTool::typing()),
        Arc::new(DeskTool::scroll()),
    ]
}

/// One thing the desktop double is trying to do.
///
/// The *intent* is scripted; every argument it sends is read out of the
/// trace the harness showed it. That split is the whole design: a double
/// that carried the coordinates would pass with the screen read deleted.
#[derive(Clone, Copy)]
enum Step {
    /// `pointer_ui_read` with this query. The queries differ between reads
    /// because the repeat gate refuses an identical (action, args) call
    /// twice in a turn, and because the real tool takes one.
    Read(&'static str),
    /// Get to this control and click it. Nothing here knows where it is: if
    /// the trace carries its point, click it; if the trace says a result was
    /// clipped, follow the handle first and look again.
    Reach(&'static str),
    Type(&'static str),
    /// `pointer_scroll` by this many notches. The step the hard query is
    /// built on: with the tool out of the legal set there is no other action
    /// that does it, and the double says so by settling rather than by
    /// spending twelve iterations proposing something illegal.
    Scroll(i64),
}

/// The click point the trace shows for a control, or `None` when the trace
/// does not carry it — because the read has not happened, or because the cap
/// clipped that part away.
///
/// Parsed out of the tool's own `role "name" (x,y)` rendering. A line the cap
/// cut mid-coordinate has no closing bracket to find, so it reads as absent
/// rather than as a wrong point.
fn point_of(trace: &str, name: &str) -> Option<(i64, i64)> {
    let opener = format!("\"{name}\" (");
    let at = trace.find(&opener)?;
    let rest = &trace[at + opener.len()..];
    let close = rest.find(')')?;
    let (x, y) = rest[..close].split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// The handle in a clipped line — `…[r42: 14425 chars, 1200 shown — …]` —
/// found the way an emitter reading its own prompt has to find it.
fn handle_in(trace: &str) -> Option<String> {
    let at = trace.find("[r")?;
    let handle: String = trace[at + 1..]
        .chars()
        .take_while(|c| *c == 'r' || c.is_ascii_digit())
        .collect();
    (handle.len() > 1).then_some(handle)
}

/// The outcome lines of a trace, the fold's counted line included — since
/// that line is what an older outcome becomes.
///
/// Compared between iterations to tell "my last proposal ran" from "my last
/// proposal never happened". The second is what a misroute looks like from
/// inside the emitter: the engine widens the tier and asks again without
/// recording a refusal, and the double has to ask for the same thing rather
/// than skipping a step that nothing carried out.
fn outcomes_in(trace: &[String]) -> String {
    trace
        .iter()
        .filter(|l| l.contains("ToolReturned(") || l.starts_with("earlier this turn"))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether the trace shows the click that finishes a [`Step::Reach`]: the
/// tool's own report of what was under the pointer, on one line. Both halves
/// have to be on the same line — in the third task the control's name is
/// also in an `inspect_result` window a few lines above.
fn reached(outcomes: &str, name: &str) -> bool {
    outcomes
        .lines()
        .any(|l| l.contains("ok: clicked ") && l.contains(name))
}

/// The desktop emitter double: it reads the trace it was shown and acts on
/// what is in it.
///
/// The counterpart of `turn_loop.rs`'s `HandleFollowingEmitter`, with a plan
/// behind it. Everything it needs — a control's point, a clipped result's
/// handle — it takes out of `trace_so_far`, so every one of these three
/// tasks fails if the harness stops carrying that material, which is the
/// only reason a pass is worth recording.
struct DesktopEmitter {
    traces: Arc<Mutex<Vec<Vec<String>>>>,
    plan: Mutex<Plan>,
}

struct Plan {
    steps: VecDeque<Step>,
    current: Option<Step>,
    /// The outcomes visible when `current` was taken up.
    seen: String,
}

impl DesktopEmitter {
    fn new(traces: Arc<Mutex<Vec<Vec<String>>>>, steps: Vec<Step>) -> Self {
        Self {
            traces,
            plan: Mutex::new(Plan {
                steps: steps.into(),
                current: None,
                seen: String::new(),
            }),
        }
    }
}

fn proposal(action: &str, args: serde_json::Value) -> Proposal {
    Proposal {
        rationale: "desktop task".into(),
        action: action.into(),
        args,
    }
}

fn settle() -> Proposal {
    proposal("respond_directly", serde_json::json!({}))
}

#[async_trait::async_trait]
impl Emitter for DesktopEmitter {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        self.traces.lock().unwrap().push(ctx.trace_so_far.clone());
        let trace = ctx.trace_so_far.join("\n");
        let outcomes = outcomes_in(&ctx.trace_so_far);
        let mut plan = self.plan.lock().unwrap();
        // A step is finished when its effect is in the trace, and only then.
        // `Reach` is finished by the click, never by the inspection it took
        // to find where to click — otherwise the third task would step past
        // its own goal the moment paging worked.
        let finished = match plan.current {
            None => true,
            Some(Step::Reach(name)) => reached(&outcomes, name),
            Some(_) => plan.seen != outcomes,
        };
        if finished {
            plan.current = plan.steps.pop_front();
            plan.seen = outcomes;
        }
        let Some(step) = plan.current else {
            return Ok(settle());
        };
        Ok(match step {
            Step::Read(query) => proposal("pointer_ui_read", serde_json::json!({"query": query})),
            Step::Type(text) => proposal("pointer_type", serde_json::json!({"text": text})),
            // Withheld: nothing else on this desktop brings the control into
            // view, so the turn ends here and the ability fails on the click
            // that never happened — the honest failure, and the one the
            // fixture exists to produce (M10 T0.3).
            Step::Scroll(dy) if !legal.contains(POINTER_SCROLL) => {
                plan.steps.clear();
                plan.current = None;
                let _ = dy;
                settle()
            }
            Step::Scroll(dy) => proposal(POINTER_SCROLL, serde_json::json!({"dx": 0, "dy": dy})),
            Step::Reach(name) => match point_of(&trace, name) {
                Some((x, y)) => proposal("pointer_click", serde_json::json!({"x": x, "y": y})),
                None => match handle_in(&trace).filter(|_| legal.contains(INSPECT_RESULT)) {
                    Some(id) => {
                        proposal(INSPECT_RESULT, serde_json::json!({"id": id, "query": name}))
                    }
                    // Not in the trace and nothing left to page: the control
                    // is out of reach. Saying so ends the turn, rather than
                    // spending the remaining iterations rediscovering it —
                    // and the fixture then fails on the click that never
                    // happened, which is the honest failure.
                    None => {
                        plan.steps.clear();
                        plan.current = None;
                        settle()
                    }
                },
            },
        })
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
pub(crate) fn clock(ticks: Arc<AtomicU64>) -> Box<dyn Fn() -> Timestamp + Send + Sync> {
    const BASE_MS: u64 = 1_788_370_524_628;
    Box::new(move || Timestamp(BASE_MS + ticks.fetch_add(1, Ordering::SeqCst) * 1_000))
}

pub(crate) struct Harness {
    store: Arc<InMemoryStore>,
    shown: Arc<Mutex<Vec<Shown>>>,
    /// What the *emitter* was shown, one entry per iteration: this turn's
    /// trace lines exactly as `trace_for_prompt` produced them, clip and fold
    /// applied. The reply context is captured by [`Probe`]; this is the other
    /// half, and it is the half the desktop tasks are graded on — the trace
    /// is the block that is re-sent on every iteration, so it is where a wide
    /// result is paid for again and again.
    traces: Arc<Mutex<Vec<Vec<String>>>>,
    sessions: Mutex<Vec<SessionId>>,
    ticks: Arc<AtomicU64>,
    /// Whether this fixture is a desktop one. Set by [`Harness::desktop`];
    /// decides the tools, the router and the iteration budget below.
    desktop: bool,
    /// M10 T1.4: the verbatim window this fixture runs with, when it needs a
    /// narrower one than the default six.
    ///
    /// Applicability pruning makes `recall` legal only once there is
    /// something out of sight — a turn older than the window, or an earlier
    /// session. The `abstention` fixture asks its question on turn 2 of a
    /// fresh store, so under the default window there is nothing out of sight
    /// and `recall` is (correctly) not offered. A one-turn window puts turn 1
    /// out of sight and restores the ability's own shape: the search runs,
    /// finds nothing, and the reply has to decline.
    ///
    /// M12 T5.1 starts it from [`Run::window_turns`], so a context-profile
    /// arm reaches every fixture through this one field. A fixture that
    /// overrides it keeps its own window under every arm, which is the point
    /// of the override: `abstention` grades what one turn out of sight does,
    /// not what six do.
    window_turns: Option<usize>,
    /// M10 T5.1: the learned rules this fixture hands the engine, which is
    /// where the guidance block comes from. Empty on the ten abilities, so
    /// their rows are the numbers they have always been.
    learned: std::sync::Arc<arc_swap::ArcSwap<LearnedRules>>,
    /// M10 T5.1: the persona the seed authored, in front of the reply model
    /// exactly as `[persona]` would be on the live path. Empty on the ten
    /// abilities.
    persona: String,
    /// M10 T5.1: run the engine's own summarizer path at every turn
    /// boundary, so `state.summary` is non-empty when a graded turn runs.
    ///
    /// Off on the ten abilities for the same reason: `maybe_summarize` is a
    /// model call, and turning it on for them would change `requests` on
    /// every row of the ledger.
    summaries: bool,
    /// What this arm does differently: the blanked block (M9 T0.4) and the
    /// activation weight (M9 T3.3). Passed straight into every
    /// `EngineConfig` the fixture builds, so every turn of an arm runs with
    /// it — an arm that ablated only the graded turn would measure a harness
    /// nobody runs.
    run: Run,
}

/// What one arm of the suite is configured to do differently (M9 T0.4, T3.3).
///
/// One struct rather than two parameters threaded through nine fixtures: the
/// list will grow — every M9 knob that ships off has to be measurable at the
/// value it might ship on — and a fixture signature is not the place to keep
/// that list.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Run {
    /// The context block this arm blanks, or `None` for the full arm.
    pub ablate: Option<Ablate>,
    /// `[memory] activation_weight` for this arm (M9 T3.1/T3.2), applied to
    /// the store the fixtures search and mirrored into `EngineConfig`.
    /// `0.0` is the default and is pre-M9 ranking.
    pub activation_weight: f32,
    /// Tool names this arm does **not** register, so the engine never puts
    /// them in the legal set (M10 T0.3).
    ///
    /// The narrowing P2 will do lives in the router; this is the same effect
    /// reached from outside the engine, which is what lets the hard-query
    /// fixture prove it guards before any of that exists. A `&'static`
    /// slice rather than a `Vec` so [`Run`] stays `Copy` — it is passed by
    /// value into every fixture, and the names are literals at every call
    /// site anyway.
    pub withhold: &'static [&'static str],
    /// `[memory] window_turns` for this arm (M12 T5.1), or `None` for the
    /// engine's own default — which is what every run before this field was
    /// measured at, so `None` and `Some(6)` must render the same table.
    ///
    /// It reaches the engine through [`Harness::window_turns`], the
    /// per-fixture override M10 T1.4 added, rather than through a second
    /// path: a fixture that needs a particular window needs it under every
    /// arm (`abstention` asks its question with exactly one turn out of
    /// sight), so the fixture's own value wins where it sets one.
    pub window_turns: Option<usize>,
    /// `[memory] facts_in_context` for this arm (M12 T5.1), or `None` for
    /// the engine's own default. No fixture overrides this one, so it goes
    /// straight into every `EngineConfig` the harness builds.
    pub facts_in_context: Option<usize>,
    /// `[router] depth` for this arm (M10 T2.1/T2.3). `Full` is the default
    /// and is the engine's behaviour before adaptive depth existed.
    ///
    /// Only the desktop fixtures feel it: the memory abilities register one
    /// tool and run with no router, and an arm that changed their numbers
    /// would be reporting a narrowing nothing narrowed.
    pub depth: nscore::Depth,
    /// `[memory] obligation_check` for this arm (M9 T2.1, read by M10 T5.4).
    ///
    /// `false` is the default and is the engine's behaviour before the
    /// obligation interceptor existed; `true` lets one unaddressed clause
    /// cost the turn a second replier call.
    pub obligation_check: bool,
    /// `[memory] summary_guidelines` for this arm (M9 T5.2, read by M10 T5.4).
    ///
    /// Plumbed to the one place the harness builds a summarizer, so a live
    /// summarizer dropped in there is measured by this arm without a second
    /// change. The double the fixtures run today ignores them — see
    /// [`summarizer_honours_guidelines`], which is this arm's finding, not an
    /// oversight. A `&'static` slice for [`Run`]'s `Copy`, as `withhold`.
    pub summary_guidelines: &'static [&'static str],
}

/// Whether the summarizer the fixtures run can see `summary_guidelines` at
/// all (M10 T5.4, arm 4).
///
/// `false`, and it is a property of the double rather than of the knob:
/// `nsengine::script::ScriptedSummarizer` returns `"scripted summary of turns
/// {first}-{last}"` with `established` copied verbatim off the records and
/// `open` empty — computed from the records alone. Guidelines reach a real
/// summarizer through `nsllm::summarizer::LlmSummarizer::with_guidelines`,
/// which renders them into a **system prompt**: a prompt the double never
/// builds and never sends. So the on-arm and off-arm summaries are
/// byte-identical by construction, and the honest reading of this arm offline
/// is *not measurable*, not *zero effect*.
pub fn summarizer_honours_guidelines() -> bool {
    false
}

/// The one place the harness chooses a summarizer (M10 T5.4).
///
/// It takes the guidelines even though today's double discards them: this is
/// the line a live summarizer is swapped into — `LlmSummarizer::new(..)
/// .with_guidelines(guidelines.iter().map(|g| g.to_string()).collect())` —
/// and keeping the argument here makes that swap one line rather than a
/// re-plumbing.
fn summarizer_double(_guidelines: &'static [&'static str]) -> Box<dyn nscore::Summarizer> {
    Box::new(ScriptedSummarizer::default())
}

impl Harness {
    /// The full arm passes `Run::default()`; `ns-app eval --ablate <block>`
    /// and `--activation <w>` fill it in, and every engine and every store
    /// this fixture builds is configured from it.
    pub(crate) fn for_run(run: Run) -> Self {
        Self {
            store: Arc::new(
                InMemoryStore::new().with_activation(run.activation_weight, ACTIVATION_HALF_LIFE),
            ),
            shown: Arc::new(Mutex::new(Vec::new())),
            traces: Arc::new(Mutex::new(Vec::new())),
            sessions: Mutex::new(Vec::new()),
            ticks: Arc::new(AtomicU64::new(0)),
            desktop: false,
            // M12 T5.1. The arm's cap is the fixture's starting value; a
            // fixture that needs its own window says so afterwards, the way
            // `abstention` does.
            window_turns: run.window_turns,
            learned: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default())),
            summaries: false,
            persona: String::new(),
            run,
        }
    }

    /// M10 T5.1: the same harness with notes in front of both models and the
    /// summarizer running between turns.
    ///
    /// Two flags rather than two harnesses because the fixtures module grades
    /// what the ten abilities grade — the [`ReplyContext`] the engine built —
    /// and a second harness would be a second definition of that.
    pub(crate) fn for_fixture(run: Run, rules: LearnedRules, persona: &str) -> Self {
        Self {
            learned: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(rules)),
            summaries: true,
            persona: persona.to_string(),
            ..Self::for_run(run)
        }
    }

    /// The same harness with a desktop wired into it.
    fn desktop_for_run(run: Run) -> Self {
        Self {
            desktop: true,
            ..Self::for_run(run)
        }
    }

    pub(crate) async fn turn(
        &self,
        session: &SessionId,
        text: &str,
        script: Vec<Proposal>,
    ) -> Vec<Shown> {
        self.turn_drafting(session, text, script, None).await
    }

    pub(crate) async fn turn_drafting(
        &self,
        session: &SessionId,
        text: &str,
        script: Vec<Proposal>,
        first_draft: Option<&'static str>,
    ) -> Vec<Shown> {
        self.run(
            session,
            text,
            Box::new(ScriptedEmitter::new(script)),
            first_draft,
        )
        .await
    }

    /// One desktop turn, driven by a plan of intents whose arguments the
    /// emitter has to find in what it was shown ([`DesktopEmitter`]).
    async fn desktop_turn(&self, session: &SessionId, text: &str, plan: Vec<Step>) -> Vec<Shown> {
        self.run(
            session,
            text,
            Box::new(DesktopEmitter::new(self.traces.clone(), plan)),
            None,
        )
        .await
    }

    /// Runs one turn on a freshly built engine over the shared store.
    ///
    /// A fresh engine per turn is `turn_loop.rs`'s idiom whenever a fixture
    /// needs a different script on each turn (`facts5`, `forget`), and it is
    /// safe because `run_turn` reloads the log from the store and folds it:
    /// nothing a turn learned lives in the engine. It is also what these
    /// fixtures need — `ScriptedEmitter` pops one queue for its whole life, so
    /// one engine could not both remember on turn 1 and stay quiet on turn 2.
    async fn run(
        &self,
        session: &SessionId,
        text: &str,
        emitter: Box<dyn Emitter>,
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
        b.set_emitter(Box::new(MeteredEmitter { inner: emitter }));
        b.set_replier(Box::new(Probe {
            shown: self.shown.clone(),
            first_draft,
            drafts: AtomicU32::new(0),
        }));
        b.set_memory(self.store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        // The double replies with a fixed marker rather than prose, and that
        // marker is verbatim in the window from turn 2 on, so the echo
        // monitor would fire on every turn and measure nothing.
        // `turn_loop.rs` turns it off on its own doubles for the same reason.
        // The grounding check stays on: it is the interceptor the abstention
        // fixture grades.
        if self.summaries {
            b.set_summarizer(summarizer_double(self.run.summary_guidelines));
        }
        let common = EngineConfig {
            max_echo_ratio: 1.1,
            persona: self.persona.clone(),
            // M10 T5.1. The default is an empty rule set, so the ten
            // abilities render no guidance and their rows do not move.
            learned: self.learned.clone(),
            // M9 T0.4. `None` on every existing run, so the suite's numbers
            // are the ones they have always been.
            ablate: self.run.ablate,
            // M9 T3.3. The store is where the prior is applied; these two
            // are the config mirror, so a fixture that reads the engine's
            // configuration sees the weight the arm is actually running at.
            activation_weight: self.run.activation_weight,
            activation_half_life_days: ACTIVATION_HALF_LIFE,
            // M10 T5.4. `false` on every existing run, so the ten abilities
            // and the thirty fixtures keep the numbers they have.
            obligation_check: self.run.obligation_check,
            window_turns: self
                .window_turns
                .unwrap_or(EngineConfig::default().window_turns),
            // M12 T5.1. `None` on every existing run, so the ten abilities
            // and the thirty fixtures keep the numbers they have.
            facts_in_context: self
                .run
                .facts_in_context
                .unwrap_or(EngineConfig::default().facts_in_context),
            ..EngineConfig::default()
        };
        let cfg = if self.desktop {
            for t in desktop_tools() {
                // M10 T0.3. Removing rather than masking, which is the rule
                // this engine narrows by: an unregistered tool is out of
                // `legal.actions` and out of the schemas, so the arm costs
                // what a narrowed arm would cost.
                if self.run.withhold.contains(&t.spec().name.as_str()) {
                    continue;
                }
                b.add_tool(t);
            }
            EngineConfig {
                // `ns-run`'s own numbers (app/src/config.rs), because these
                // three tasks stand for turns that machine actually ran: a
                // desktop task is ten to fifteen actions over as many
                // iterations, and five verbatim outcomes is what that
                // default was chosen for. Stated rather than inherited —
                // both are what the mutation check moves.
                max_iterations: 12,
                trace_verbatim_lines: 5,
                // Unattended. `pointer_click` and `pointer_type` are
                // `Irreversible`, so with the gate on every desktop turn
                // would stage its first click and end there, and these
                // fixtures would grade the confirmation flow — which is M6's
                // and is graded in `turn_loop.rs` — instead of what twelve
                // iterations put in front of the model.
                confirm_irreversible: false,
                // The real router, not a stub. `tier` is one of the columns,
                // and a fixture that decided the tier itself would report a
                // routing nothing routed.
                router: Some(Arc::new(KeywordRouter {
                    // M10 T2.1. `Full` on every existing run, so the ten
                    // abilities' rows are the ones they have always been.
                    depth: self.run.depth,
                    ..KeywordRouter::default()
                })),
                ..common
            }
        } else {
            b.add_tool(Arc::new(EchoTool::new()));
            common
        };
        let engine = Engine::with_clock(b.build().unwrap(), cfg, clock(self.ticks.clone()));
        engine
            .run_turn(Incoming {
                session: session.clone(),
                text: text.into(),
            })
            .await
            .unwrap();
        // M10 T5.1. The dispatcher, not `run_turn`, is what calls this on the
        // live path (`dispatch.rs`), so a fixture that wants a summary has to
        // call it too — at the same place, after the reply is sent.
        if self.summaries {
            engine.maybe_summarize(session).await.unwrap();
        }
        self.shown.lock().unwrap()[before..].to_vec()
    }

    /// Reply-model calls over the whole fixture: one per captured context, so
    /// a regeneration counts, which is the point of counting it.
    pub(crate) fn reply_calls(&self) -> usize {
        self.shown.lock().unwrap().len()
    }

    /// The largest trace any emitter iteration was shown. The plan's metric
    /// for phase 1: the trace is rebuilt and re-sent on every iteration, so
    /// this number, not the tool's output, is what a wide result costs.
    fn peak_trace_chars(&self) -> usize {
        self.traces
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.iter().map(|l| l.chars().count()).sum())
            .max()
            .unwrap_or(0)
    }

    /// Every emitter trace, in the order the iterations happened.
    fn traces(&self) -> Vec<Vec<String>> {
        self.traces.lock().unwrap().clone()
    }

    /// Every successful tool outcome, as the model was shown it, in order.
    ///
    /// Read back off the log rather than remembered by the tool double: a
    /// click is graded on what the harness recorded happening, not on what a
    /// fixture believes it asked for.
    async fn outcomes(&self) -> Vec<String> {
        let sessions = self.sessions.lock().unwrap().clone();
        let mut out = Vec::new();
        for sid in sessions {
            for e in self.store.load(&sid).await.unwrap() {
                if let EventKind::ToolReturned {
                    outcome: ToolOutcome::Ok { output },
                    ..
                } = &e.kind
                {
                    out.push(output.summary.clone());
                }
            }
        }
        out
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

    /// The store this fixture built, so a caller can seed and read facts
    /// through the same rows the engine ranks (M10 T5.2).
    pub(crate) fn store(&self) -> Arc<InMemoryStore> {
        self.store.clone()
    }

    /// The plan's per-run numbers, read back off the event log of every
    /// session the fixture touched. Read back rather than counted by hand: a
    /// number a fixture computes for itself drifts from what the harness did,
    /// and then the regression set is measuring the test.
    pub(crate) async fn counters(&self) -> Counters {
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
            // The tier the previous model call of *this* turn ran at. Tier
            // only ever rises, and only within a turn — the router decides
            // afresh at every turn boundary — so a rise between two adjacent
            // calls of one turn is an escalation and nothing else is.
            let mut previous_tier: Option<(u32, Tier)> = None;
            // M10 T0.4: the legal-set size the *last* model call was given.
            // `record_model_calls` appends the emitter's `ModelCall`
            // immediately before the `Proposed` it produced (`turn.rs`), so
            // the last one seen is that proposal's own call and the pairing
            // cannot drift.
            let mut legal_on_last_call = 0usize;
            for e in &events {
                match &e.kind {
                    EventKind::UserSaid { .. } => c.turns += 1,
                    EventKind::Proposed { proposal } => {
                        c.emitter_calls += 1;
                        c.proposals
                            .push((proposal.action.clone(), legal_on_last_call));
                        if proposal.action == ASK_CLARIFICATION {
                            c.clarifications += 1;
                        }
                    }
                    EventKind::ToolCalled { action, .. } => {
                        c.tool_calls += 1;
                        if action == INSPECT_RESULT {
                            c.inspections += 1;
                        }
                    }
                    EventKind::ReplyFlagged { .. } => c.flags += 1,
                    // What each model call was shown (M7 T0.1). Read off the
                    // manifest rather than recomputed here: the manifest is
                    // built from the context immediately before it is moved
                    // into the call, so it cannot drift from what was sent.
                    EventKind::ModelCall { usage, manifest } => {
                        legal_on_last_call = manifest.tools;
                        // M12 T5.3. The stable prefix as the emitter was
                        // sent it: the three blocks that do not change
                        // between the iterations of a turn, which is what a
                        // cache breakpoint would sit behind. Emitter only —
                        // the replier's window is the turn it is answering,
                        // so folding it in would measure a different prefix.
                        if usage.role == "emitter" {
                            let stable = manifest.facts_chars
                                + manifest.summary_chars
                                + manifest.window_chars;
                            c.context_chars += stable;
                            c.emitter_prefix.push(estimate_tokens(stable));
                        }
                        c.legal_max = c.legal_max.max(manifest.tools);
                        c.clipped_chars += manifest.clipped_chars;
                        c.budget_drops += manifest
                            .budget
                            .as_ref()
                            .map(|b| b.dropped.len())
                            .unwrap_or(0);
                        if let Some(tier) = manifest.tier {
                            if tier == Tier::Chat {
                                c.chat_calls += 1;
                            }
                            if let Some((turn, was)) = previous_tier {
                                if turn == e.turn && tier > was {
                                    c.escalations += 1;
                                }
                            }
                            previous_tier = Some((e.turn, tier));
                        }
                    }
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
pub(crate) struct Counters {
    pub(crate) turns: usize,
    pub(crate) emitter_calls: usize,
    pub(crate) tool_calls: usize,
    pub(crate) recall_calls: usize,
    pub(crate) recall_hits: usize,
    pub(crate) flags: usize,
    pub(crate) clarifications: usize,
    /// The three stable blocks as sent to the *emitter*, summed over the
    /// fixture's emitter calls (M12 T5.3).
    context_chars: usize,
    /// The same sum in estimated tokens, one entry per emitter call rather
    /// than a total: the number a cache breakpoint is judged against is a
    /// level per call, and a sum over twelve iterations clears a floor no
    /// single call does. A call whose three blocks were all empty is a zero
    /// entry and stays in — an emitter that was sent nothing stable is what
    /// this arm is measuring, not a record that went missing.
    emitter_prefix: Vec<u32>,
    /// The four M7 numbers, summed over every model call the fixture made.
    clipped_chars: usize,
    inspections: usize,
    budget_drops: usize,
    escalations: usize,
    /// Model calls made at `Tier::Chat`. Not a reported column — one number
    /// per row is enough — but the two find-and-click tasks are graded on it
    /// being zero: a desktop instruction that routed to `Chat` would spend an
    /// iteration discovering its tools, and `escalations == 0` alone cannot
    /// tell that apart from a turn that never proposed one.
    chat_calls: usize,
    /// Every proposal the fixture made, in order, with the legal-set size
    /// the call that produced it was given (M10 T0.4). The raw material for
    /// Bits-over-Random, kept as the pair rather than as a computed number
    /// so a run can be re-scored against a different target without rerunning
    /// the fixture.
    proposals: Vec<(String, usize)>,
    /// The widest legal set any model call of the fixture saw. What a miss is
    /// scored against: without it a fixture whose target was never proposed
    /// would have no `n` to report at all.
    legal_max: usize,
}

impl Counters {
    /// The middle emitter prefix of the fixture, in estimated tokens, or `0`
    /// when no emitter call carried one (M12 T5.3).
    ///
    /// The median rather than the mean, and the same upper-median convention
    /// `ns-app budget`'s `prefix_summary` uses, so the harness column and the
    /// live report cannot disagree about what a "median prefix" is.
    fn emitter_prefix_tokens(&self) -> u32 {
        if self.emitter_prefix.is_empty() {
            return 0;
        }
        let mut sorted = self.emitter_prefix.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// The deciding call for `target`: the first proposal of that action and
    /// the legal-set size it was chosen from, or the widest set seen and a
    /// miss (M10 T0.4).
    ///
    /// *First*, not best: the question BoR answers is whether the right tool
    /// was reachable when it was needed, and a fixture that proposed it on
    /// iteration nine after eight wrong ones did not choose it from that set
    /// the way iteration one would have.
    fn decision(&self, target: &str) -> (usize, bool) {
        match self.proposals.iter().find(|(a, _)| a == target) {
            Some((_, n)) => (*n, true),
            None => (self.legal_max, false),
        }
    }
}

/// **Bits-over-Random** (tool-loading §5.2, arXiv:2605.24660).
///
/// `BoR = log₂(P_obs / P_rand)`. One fixture is one observation, so
/// `P_obs = 1` on a hit and `0` on a miss; a uniform guess over `n` legal
/// actions has `P_rand = 1/n`. A hit is therefore `log₂(n)` bits and a miss
/// is scored `0` rather than `−∞`, which is the convention that lets the
/// column be summed and averaged.
///
/// The chance correction is the whole reason to report it: raw hit rate
/// *rises* as the legal set shrinks, because guessing gets easier, so a
/// narrowing that cost accuracy could still read as a win. BoR charges the
/// narrowing for the ease it bought — `n = 1` is worth zero bits, however
/// certainly the one action was chosen.
pub fn bits_over_random(chosen: bool, legal_set_size: usize) -> f64 {
    if chosen && legal_set_size > 1 {
        (legal_set_size as f64).log2()
    } else {
        0.0
    }
}

pub(crate) fn remember(key: &str, value: &str) -> Proposal {
    Proposal {
        rationale: "durable".into(),
        action: REMEMBER_FACT.into(),
        args: serde_json::json!({ "key": key, "value": value }),
    }
}

pub(crate) fn forget(key: &str) -> Proposal {
    Proposal {
        rationale: "the user asked".into(),
        action: FORGET_FACT.into(),
        args: serde_json::json!({ "key": key }),
    }
}

/// The query is a span of the user's own words, so provenance classifies it
/// `UserInput` rather than `Residual` — a recall query the model invented is
/// a different failure and would muddy this one.
pub(crate) fn recall_for(query: &str) -> Proposal {
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
/// hits, clipped chars, `inspect_result` calls, fit drops and tier.
///
/// The last four are the desktop half's, and they are zero on all six memory
/// rows. That is not padding — it is the statement that the two halves
/// exercise different machinery: a memory fixture that started clipping
/// results, or a desktop fixture that stopped, would show it in a column
/// rather than in a failure nobody could locate.
///
/// Every field is public because the eval ledger row is these numbers and
/// nothing else: a row that carried a re-derived copy of them would be a
/// second implementation to keep in step with this one.
#[derive(Debug, Clone, PartialEq)]
pub struct Ability {
    pub ability: &'static str,
    pub passed: bool,
    pub turns: usize,
    /// Emitter iterations plus reply calls — model requests, which is the
    /// scarce resource on the free tier (fifty a day), not tokens.
    pub requests: usize,
    /// The reply prompt on the turn the ability is decided on.
    pub prompt_chars: usize,
    /// Four characters to the token, `nscore`'s own offline estimate, so this
    /// column and the engine's budget report cannot disagree.
    pub prompt_tokens: u32,
    /// The three stable blocks — facts, summary and verbatim window — as the
    /// *emitter* was sent them, summed over the ability's emitter calls
    /// (M12 T5.3, column `ctx chars`).
    ///
    /// Off `ContextManifest`, not off the fixture's own rendering: the
    /// manifest is built from the context immediately before it is moved
    /// into the call, so this is the size that was sent. `prompt_chars`
    /// beside it is the fixture's rendering of the graded *reply* context,
    /// which is a different prompt for a different model — the pair is what
    /// makes a context-profile arm readable.
    pub context_chars: usize,
    /// The median of `estimate_tokens(facts + summary + window)` over the
    /// ability's emitter calls (M12 T5.3, column `prefix tok`), or `0` when
    /// it made none.
    ///
    /// A level, not a total: this is the run of blocks a provider cache
    /// breakpoint would sit behind, and it buys nothing until it clears the
    /// 1,024-token floor. A sum over a turn's iterations would clear that
    /// floor without any single call doing so.
    pub emitter_prefix_tokens: u32,
    pub peak_chars: usize,
    pub tool_calls: usize,
    pub recall_fired: bool,
    pub recall_hits: usize,
    /// Grounding-interceptor flags (M6 §4.5).
    pub flags: usize,
    /// Characters the per-line cap kept out of a prompt, summed over every
    /// model call of the fixture (`ContextManifest::clipped_chars`). Summed
    /// rather than maxed because that is what the cap actually saved: a wide
    /// result is re-sent on every remaining iteration, so one 14,425-char
    /// screen read costs its clip again at each of them.
    pub clipped_chars: usize,
    /// `inspect_result` calls — the other half of the cap. Zero is right
    /// when everything needed was in the shown head; on the task whose
    /// target is only past the cap, zero is the failure.
    pub inspections: usize,
    /// Blocks the budget dropped, or would have dropped under `report` mode
    /// (`ContextManifest::budget.dropped`), summed over the fixture's calls.
    ///
    /// Zero on all nine, and measured rather than assumed: every call carries
    /// a budget report, and every one of them came in under the ceiling. That
    /// is the ceiling working as `ns-run` sized it — 6,000 tokens "chosen so
    /// … a desktop turn's trace sit inside it with room, and so going over is
    /// a signal rather than a routine event". The clip is what keeps it
    /// there: uncapped, the two screen reads the fold leaves verbatim in
    /// open-and-search are 28,850 characters — 7,212 tokens — on their own,
    /// before a fact or a window record. A fixture rigged to force a drop
    /// would be reporting a ceiling nothing runs at.
    pub budget_drops: usize,
    /// Times the tier rose inside a turn: a message routed to `Chat` whose
    /// emitter then proposed a real tool. One iteration is the price of that
    /// guess, and it is a request, which is the resource the free tier
    /// meters — so it is counted rather than assumed harmless.
    pub escalations: usize,
    /// The action this ability is about — the one a narrowing would have to
    /// keep for the ability to remain possible (M10 T0.4).
    pub target_action: &'static str,
    /// Whether it was proposed, and the legal-set size on the call that
    /// proposed it (or the widest set seen, on a miss).
    pub target_proposed: bool,
    pub legal_size: usize,
    /// [`bits_over_random`] of the two above. Zero at chance — a miss, or a
    /// legal set of one — and `log₂(n)` when the target was chosen out of
    /// `n`. Today's number is the baseline P2's narrowing is compared to.
    pub bits: f64,
    /// Empty on a pass; on a failure, every condition that failed and what
    /// the harness showed instead.
    pub detail: String,
}

impl Ability {
    fn build(
        ability: &'static str,
        target_action: &'static str,
        graded: &Shown,
        h: &Harness,
        c: &Counters,
        fails: Vec<String>,
    ) -> Self {
        let (legal_size, target_proposed) = c.decision(target_action);
        Self {
            ability,
            target_action,
            target_proposed,
            legal_size,
            bits: bits_over_random(target_proposed, legal_size),
            passed: fails.is_empty(),
            turns: c.turns,
            requests: c.emitter_calls + h.reply_calls(),
            prompt_chars: graded.chars(),
            prompt_tokens: estimate_tokens(graded.chars()),
            context_chars: c.context_chars,
            emitter_prefix_tokens: c.emitter_prefix_tokens(),
            peak_chars: h.peak_chars(),
            tool_calls: c.tool_calls,
            recall_fired: c.recall_calls > 0,
            recall_hits: c.recall_hits,
            flags: c.flags,
            clipped_chars: c.clipped_chars,
            inspections: c.inspections,
            budget_drops: c.budget_drops,
            escalations: c.escalations,
            detail: fails.join("; "),
        }
    }
}

/// One graded condition. Collected rather than asserted: a run has to report
/// every condition that failed, across all six abilities, or the first broken
/// one hides the rest and the next run rediscovers them one at a time.
pub(crate) fn require(fails: &mut Vec<String>, ok: bool, why: impl FnOnce() -> String) {
    if !ok {
        fails.push(why());
    }
}

/// Header, rule and every row through one set of widths, so a column cannot
/// drift out of line with its heading when a number grows.
fn row_line(cells: [&str; 19]) -> String {
    format!(
        "  {:<24}  {:<4}  {:>5}  {:>4}  {:>6}  {:>4}  {:>9}  {:>10}  {:>4}  {:>5}  {:<6}  \
         {:>4}  {:>5}  {:>7}  {:>5}  {:>5}  {:>4}  {:>5}  {:>5}\n",
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
        cells[11],
        cells[12],
        cells[13],
        cells[14],
        cells[15],
        cells[16],
        cells[17],
        cells[18],
    )
}

pub fn render_table(rows: &[Ability]) -> String {
    let mut out = String::from("\nM7 T5.1 — memory and desktop abilities, scripted model\n\n");
    out.push_str(&row_line([
        "ability",
        "pass",
        "turns",
        "reqs",
        "prompt",
        "~tok",
        "ctx chars",
        "prefix tok",
        "peak",
        "tools",
        "recall",
        "hits",
        "flags",
        "clipped",
        "insp",
        "drops",
        "esc",
        "legal",
        "BoR",
    ]));
    out.push_str(&row_line([
        "------------------------",
        "----",
        "-----",
        "----",
        "------",
        "----",
        "---------",
        "----------",
        "----",
        "-----",
        "------",
        "----",
        "-----",
        "-------",
        "-----",
        "-----",
        "----",
        "-----",
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
            &r.context_chars.to_string(),
            &r.emitter_prefix_tokens.to_string(),
            &r.peak_chars.to_string(),
            &r.tool_calls.to_string(),
            if r.recall_fired { "yes" } else { "no" },
            &r.recall_hits.to_string(),
            &r.flags.to_string(),
            &r.clipped_chars.to_string(),
            &r.inspections.to_string(),
            &r.budget_drops.to_string(),
            &r.escalations.to_string(),
            // A miss prints its `n` too — "0.00 out of 17" and "0.00 out of
            // 3" are different findings, and a bare zero hides which.
            &if r.target_proposed {
                r.legal_size.to_string()
            } else {
                format!("{}!", r.legal_size)
            },
            &format!("{:.2}", r.bits),
        ]));
    }
    let bits: f64 = rows.iter().map(|r| r.bits).sum();
    out.push_str(&format!(
        "\n  {}/{} abilities pass.\n",
        rows.iter().filter(|r| r.passed).count(),
        rows.len()
    ));
    // M10 T0.4. Printed beside the pass counts because it is the number P2's
    // narrowing is judged on: a narrower legal set that keeps every ability
    // passing but drops BoR bought its pass rate from an easier guess.
    out.push_str(&format!(
        "  Bits-over-Random: {bits:.2} bits over {} abilities ({:.2} mean); \
         `n!` marks a target that was never proposed.\n",
        rows.len(),
        if rows.is_empty() {
            0.0
        } else {
            bits / rows.len() as f64
        },
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
async fn information_extraction(run: Run) -> Ability {
    let h = Harness::for_run(run);
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
    Ability::build(
        "information extraction",
        REMEMBER_FACT,
        graded,
        &h,
        &c,
        fails,
    )
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
async fn multi_session_reasoning(run: Run) -> Ability {
    let h = Harness::for_run(run);
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
    Ability::build(
        "multi-session reasoning",
        REMEMBER_FACT,
        graded,
        &h,
        &c,
        fails,
    )
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
async fn temporal_reasoning(run: Run) -> Ability {
    let h = Harness::for_run(run);
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
    Ability::build("temporal reasoning", REMEMBER_FACT, graded, &h, &c, fails)
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
async fn knowledge_updates(run: Run) -> Ability {
    let h = Harness::for_run(run);
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
    Ability::build("knowledge updates", REMEMBER_FACT, graded, &h, &c, fails)
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
async fn abstention(run: Run) -> Ability {
    const INVENTED: &str = "Ostrava";
    const INVENTING_DRAFT: &str = "You live in Ostrava.";

    // One turn of verbatim window rather than six (M10 T1.4): the question
    // is asked on turn 2, and `recall` is offered only once a turn has left
    // the window. What the ability grades is unchanged — the search still
    // runs, still finds nothing, and the reply still has to decline — but
    // now turn 1 is genuinely out of sight, which is the situation a model
    // reaches for `recall` in.
    let h = Harness {
        window_turns: Some(1),
        ..Harness::for_run(run)
    };
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
    Ability::build("abstention", RECALL, graded, &h, &c, fails)
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
async fn selective_forgetting(run: Run) -> Ability {
    let h = Harness::for_run(run);
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
    Ability::build("selective forgetting", FORGET_FACT, graded, &h, &c, fails)
}

// ---------------------------------------------------------------------------
// The three desktop tasks (M7 plan §9, T5.1)
// ---------------------------------------------------------------------------

/// What this turn's trace may cost, in characters.
///
/// `trace_verbatim_lines` (5) outcomes at the 1,200-character cap, plus the
/// fold's counted line and the short bookkeeping lines around them. It is the
/// bound the two mechanisms promise together, and it does not grow with the
/// turn: a task that reads the screen three times pays for two of them, not
/// three, and would pay 43,275 characters for three without them.
const TRACE_CEILING: usize = 6_500;

/// **Open and search.** One desktop turn: read the screen, act on what came
/// back, read it again, act again — eight actions over ten iterations.
///
/// The recorded turn this stands for is the 2026-09-07 one that carried a
/// 14,425-character `pointer_ui_read` in its trace: on `main` that result was
/// rebuilt into `trace_so_far` on every remaining iteration, uncapped, at
/// roughly 3.6k tokens each — up to eleven more times under `ns-run`'s
/// twelve-iteration budget (plan §2). What is graded is exactly that — not
/// the answer, which a double controls anyway, but the size of the trace the
/// emitter was handed on each of the ten iterations.
///
/// Two mechanisms have to hold for it to pass and they fail differently: the
/// clip keeps any one line inside 1,200 characters, and the fold keeps only
/// the last five outcomes verbatim and counts the rest into one line. Remove
/// the clip and the ceiling breaks; remove the fold and the counted line is
/// missing while the ceiling still holds. Both are checked.
///
/// The message is conversational, so the router sends it to `Chat` and the
/// first proposal is a misroute — the tier widens, no refusal is recorded,
/// and one iteration is the price. That path is a request on a fifty-request
/// day, so it is counted rather than assumed free.
async fn desktop_open_and_search(run: Run) -> Ability {
    let h = Harness::desktop_for_run(run);
    let sid = SessionId("eval-desktop-open".into());
    let shown = h
        .desktop_turn(
            &sid,
            // No task cue and no tool name anywhere in it: `KeywordRouter`
            // has nothing to go on and routes to `Chat`.
            "let's get that invoice sorted out, i want the totals right",
            vec![
                Step::Read("uložit"),
                Step::Reach(HEAD_CONTROL),
                Step::Type("faktura 2026-09"),
                Step::Read("najít"),
                Step::Reach(SEARCH_CONTROL),
                Step::Type("celkem"),
                Step::Read("výsledek"),
                Step::Reach(RESULT_CONTROL),
            ],
        )
        .await;
    let graded = shown.first().cloned().unwrap_or_default();

    let mut fails = Vec::new();
    let c = h.counters().await;
    require(&mut fails, !shown.is_empty(), || {
        "the turn never reached the reply model — it ran out of iterations".into()
    });
    // The actions completed. Read off the log, and off the tool's own report
    // of what was under the pointer: three clicks that landed somewhere else
    // would otherwise pass as three clicks.
    let outcomes = h.outcomes().await;
    for control in [HEAD_CONTROL, SEARCH_CONTROL, RESULT_CONTROL] {
        require(
            &mut fails,
            outcomes
                .iter()
                .any(|o| o.starts_with("clicked") && o.contains(control)),
            || format!("{control:?} was never clicked; the turn did: {outcomes:?}"),
        );
    }
    require(&mut fails, c.tool_calls == 8, || {
        format!(
            "{} tool calls, want the 8 the task is made of",
            c.tool_calls
        )
    });
    // The clip.
    require(&mut fails, h.peak_trace_chars() <= TRACE_CEILING, || {
        format!(
            "the emitter was shown {} characters of trace, over the {TRACE_CEILING} the clip \
             and the fold bound it to; one raw screen read is {}",
            h.peak_trace_chars(),
            control_tree().chars().count()
        )
    });
    // The fold. Its counted line is the only evidence that the older steps
    // were summarized rather than re-sent, and with the fold off the ceiling
    // above still holds — so this condition is what separates them.
    require(
        &mut fails,
        h.traces()
            .iter()
            .any(|t| t.iter().any(|l| l.starts_with("earlier this turn ("))),
        || "no folded line in any trace: every step was re-sent verbatim".into(),
    );
    require(&mut fails, c.clipped_chars > 0, || {
        "nothing was clipped, so this turn measures nothing about the cap".into()
    });
    // Everything the task needed was in the head the cap keeps, so paging
    // would have been a request that bought nothing.
    require(&mut fails, c.inspections == 0, || {
        format!(
            "{} inspections for controls already in the shown head",
            c.inspections
        )
    });
    // The misroute, once, and paid for once.
    require(&mut fails, c.escalations == 1, || {
        format!(
            "{} escalations; a conversational opener should cost exactly one",
            c.escalations
        )
    });
    require(&mut fails, c.chat_calls == 1, || {
        format!(
            "{} calls at the Chat tier, want the single misrouted one",
            c.chat_calls
        )
    });
    Ability::build(
        "desktop open-and-search",
        "pointer_click",
        &graded,
        &h,
        &c,
        fails,
    )
}

/// **Find and click.** Reach a control by name, from one screen read.
///
/// The control is in the modal, and the modal is what the real tool prints
/// first — so its line is inside the 1,200 characters the cap keeps, and the
/// emitter can take the point straight out of the trace it was shown. What
/// is graded is that it got there *and* that the rest of the tree never
/// entered a prompt: the largest prompt this fixture ever built is smaller
/// than one raw `pointer_ui_read`, and the tree's last control — thirteen
/// thousand characters in — is in none of them.
///
/// The recorded failure is the reason the task file already tells the model
/// to avoid `ui_read`: one action produced 98% of the tool text in that
/// session, and the advice was to not look at the screen. The point of the
/// cap is that looking at the screen stops costing that.
async fn desktop_find_and_click(run: Run) -> Ability {
    let h = Harness::desktop_for_run(run);
    let sid = SessionId("eval-desktop-find".into());
    let shown = h
        .desktop_turn(
            &sid,
            // "klikni" is a task cue, so this routes to `Task` from the
            // message and no iteration is spent discovering it.
            "klikni na Uložit jako v tom dialogu na obrazovce",
            vec![Step::Read("uložit"), Step::Reach(HEAD_CONTROL)],
        )
        .await;
    let graded = shown.first().cloned().unwrap_or_default();

    let mut fails = Vec::new();
    let c = h.counters().await;
    let outcomes = h.outcomes().await;
    require(
        &mut fails,
        outcomes
            .iter()
            .any(|o| o.starts_with("clicked") && o.contains(HEAD_CONTROL)),
        || format!("{HEAD_CONTROL:?} was never clicked; the turn did: {outcomes:?}"),
    );
    require(&mut fails, c.tool_calls == 2, || {
        format!("{} tool calls, want a read and a click", c.tool_calls)
    });
    // The whole tree never entered a prompt. Two independent readings of
    // that: no prompt is even as large as one screen read, and the control
    // at the far end of the tree is in none of them. The first could be
    // satisfied by a prompt that carried the tree and nothing else; the
    // second could not.
    let tree = control_tree().chars().count();
    require(&mut fails, h.peak_chars() < tree, || {
        format!(
            "the largest prompt was {} characters against a {tree}-character screen read",
            h.peak_chars()
        )
    });
    require(&mut fails, !graded.shows(TAIL_CONTROL), || {
        format!("the end of the tree reached the reply prompt: {TAIL_CONTROL:?}")
    });
    require(
        &mut fails,
        !h.traces()
            .iter()
            .any(|t| t.join("\n").contains(TAIL_CONTROL)),
        || "the end of the tree reached an emitter prompt".into(),
    );
    require(&mut fails, c.clipped_chars > 0, || {
        "nothing was clipped, so nothing here is about the cap".into()
    });
    // The control was in the shown head, so the handle was not needed.
    require(&mut fails, c.inspections == 0, || {
        format!("{} inspections for a control the cap kept", c.inspections)
    });
    require(&mut fails, c.escalations + c.chat_calls == 0, || {
        format!(
            "a desktop instruction routed to Chat: {} calls there, {} escalations",
            c.chat_calls, c.escalations
        )
    });
    Ability::build(
        "desktop find-and-click",
        "pointer_click",
        &graded,
        &h,
        &c,
        fails,
    )
}

/// **The clipped tail.** The control exists only past the cap.
///
/// This is the one that proves the handle is not decoration. `Sloučit buňky`
/// is the last line of the 14,425-character tree, thirteen thousand
/// characters past what the cap shows, so no prompt in this turn can carry
/// its point: the only route to it is the handle the clipped line names —
/// `[r42: 14425 chars, 1200 shown — inspect_result to see more]` — and the
/// `inspect_result` action, which the engine offers only while something is
/// actually clipped.
///
/// The failure it stands for is the one the branch commit that added the cap
/// left open: capping the trace fixed the cost and made the dropped text
/// unreachable from the prompt, so the only recovery was running the tool
/// again — which on a desktop is neither free nor guaranteed to return the
/// same screen (plan §2).
///
/// Graded on the inspection having happened, not only on the click landing.
/// If this task can pass with no `inspect_result` call then the target was
/// not past the cap and the fixture is measuring nothing — which is exactly
/// what raising the cap does to it.
async fn desktop_clipped_tail(run: Run) -> Ability {
    let h = Harness::desktop_for_run(run);
    let sid = SessionId("eval-desktop-tail".into());
    let shown = h
        .desktop_turn(
            &sid,
            "klikni na Sloučit buňky v panelu formátování",
            vec![Step::Read("sloučit"), Step::Reach(TAIL_CONTROL)],
        )
        .await;
    let graded = shown.first().cloned().unwrap_or_default();

    let mut fails = Vec::new();
    let c = h.counters().await;
    let outcomes = h.outcomes().await;
    require(&mut fails, c.inspections >= 1, || {
        "the tail was reached without inspect_result — the target is not past the cap".into()
    });
    require(
        &mut fails,
        outcomes
            .iter()
            .any(|o| o.starts_with("clicked") && o.contains(TAIL_CONTROL)),
        || format!("{TAIL_CONTROL:?} was never clicked; the turn did: {outcomes:?}"),
    );
    require(&mut fails, c.tool_calls == 3, || {
        format!(
            "{} tool calls, want a read, an inspection and a click",
            c.tool_calls
        )
    });
    // And the point arrived through the handle and nothing else. The first
    // trace that carries it must be the one carrying the inspection window:
    // an `inspect_result` outcome is the only tool summary here that begins
    // with a handle, so `ok: r` identifies it and no screen read or click
    // can be mistaken for one.
    let point = format!("({},{})", TAIL_POINT.0, TAIL_POINT.1);
    let traces = h.traces();
    match traces.iter().position(|t| t.join("\n").contains(&point)) {
        Some(at) => require(
            &mut fails,
            traces[at].join("\n").contains("ToolReturned(ok: r"),
            || {
                format!(
                    "the control's point reached a prompt some other way than the handle:\n{}",
                    traces[at].join("\n")
                )
            },
        ),
        None => fails.push("no prompt ever carried the control's point".into()),
    }
    let tree = control_tree().chars().count();
    require(&mut fails, h.peak_chars() < tree, || {
        format!(
            "the largest prompt was {} characters against a {tree}-character screen read",
            h.peak_chars()
        )
    });
    require(&mut fails, c.escalations + c.chat_calls == 0, || {
        format!(
            "a desktop instruction routed to Chat: {} calls there, {} escalations",
            c.chat_calls, c.escalations
        )
    });
    Ability::build(
        "desktop clipped tail",
        INSPECT_RESULT,
        &graded,
        &h,
        &c,
        fails,
    )
}

/// **The hard query.** The control is below the fold, and one tool reaches it.
///
/// M10 T0.3, and the risk it guards is written down: *"adaptive depth
/// withholds the tool a hard task needs"*. Tool-loading §1.3 measured what
/// that looks like — a fixed depth of five scores 64.7% on ToolBench on
/// average and **0% on hard queries** — and §5.3 says why it has to be a
/// fixture: without one, a narrower legal set "will look like a pure win on
/// every existing fixture, because none of them needs a tool the router
/// would withhold".
///
/// So this one does. [`DEEP_CONTROL`] is not in [`control_tree`] at all: no
/// `pointer_ui_read` prints it and no `inspect_result` can page to it,
/// because there is nothing clipped that holds it. `pointer_scroll` is the
/// only action on this desktop whose result carries its point — and it is
/// one of the two pointer tools the recorded log never called, which is
/// exactly the kind a cue-driven selection would drop first.
///
/// The proof that it guards is the other half, in
/// `the_hard_query_ability_fails_when_its_tool_is_withheld`: the same
/// fixture under `Run { withhold: &["pointer_scroll"], .. }` must fail. An
/// ability that passed both ways would be measuring nothing about depth.
async fn desktop_hard_query(run: Run) -> Ability {
    let h = Harness::desktop_for_run(run);
    let sid = SessionId("eval-desktop-hard".into());
    let shown = h
        .desktop_turn(
            &sid,
            // "klikni" is the task cue, so the tier is `Task` from the
            // message — and it stays the cue whether or not `pointer_scroll`
            // is registered, so the withheld arm differs in one thing only.
            "sjeď dolů v panelu a klikni na Zmrazit příčky",
            vec![
                Step::Read("zmrazit"),
                Step::Scroll(SCROLL_NOTCHES),
                Step::Reach(DEEP_CONTROL),
            ],
        )
        .await;
    let graded = shown.first().cloned().unwrap_or_default();

    let mut fails = Vec::new();
    let c = h.counters().await;
    let outcomes = h.outcomes().await;
    // The one action the task has. Read off the log, so a turn that talked
    // about scrolling without scrolling fails here.
    require(
        &mut fails,
        outcomes.iter().any(|o| o.starts_with("scrolled ")),
        || {
            format!(
                "pointer_scroll never ran, so nothing brought {DEEP_CONTROL:?} into view; \
                 the turn did: {outcomes:?}"
            )
        },
    );
    require(
        &mut fails,
        outcomes
            .iter()
            .any(|o| o.starts_with("clicked") && o.contains(DEEP_CONTROL)),
        || format!("{DEEP_CONTROL:?} was never clicked; the turn did: {outcomes:?}"),
    );
    require(&mut fails, c.tool_calls == 3, || {
        format!(
            "{} tool calls, want a read, a scroll and a click",
            c.tool_calls
        )
    });
    // And the point arrived through the scroll and nothing else. The screen
    // read cannot have carried it — the control is not in the tree — so the
    // first trace holding the point must be the one holding the scroll's
    // own outcome. If that ever stops being true, the tree grew the control
    // and the fixture is no longer hard.
    let point = format!("({},{})", DEEP_POINT.0, DEEP_POINT.1);
    let traces = h.traces();
    match traces.iter().position(|t| t.join("\n").contains(&point)) {
        Some(at) => require(
            &mut fails,
            traces[at].join("\n").contains("ToolReturned(ok: scrolled "),
            || {
                format!(
                    "the control's point reached a prompt some other way than the scroll:\n{}",
                    traces[at].join("\n")
                )
            },
        ),
        None => fails.push("no prompt ever carried the control's point".into()),
    }
    // Not a paging problem. The handle leads into the screen read, and the
    // screen read does not have this control, so an inspection here would be
    // a request that could not have bought the answer.
    require(&mut fails, c.inspections == 0, || {
        format!(
            "{} inspections for a control no screen read carries",
            c.inspections
        )
    });
    require(&mut fails, c.clipped_chars > 0, || {
        "nothing was clipped, so this is not the recorded desktop".into()
    });
    require(&mut fails, c.escalations + c.chat_calls == 0, || {
        format!(
            "a desktop instruction routed to Chat: {} calls there, {} escalations",
            c.chat_calls, c.escalations
        )
    });
    Ability::build("desktop hard query", POINTER_SCROLL, &graded, &h, &c, fails)
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// The whole set, in a fixed order, so two ledger rows line up ability by
/// ability without being matched by name.
///
/// Sequential rather than joined: every fixture builds its own store and its
/// own engines, so they are independent, but `requests` and `peak_chars` are
/// the numbers a release is compared on and a concurrent run would let the
/// scheduler into them. It takes under a second; there is nothing to buy.
pub async fn run_all() -> Vec<Ability> {
    run_all_for(Run::default()).await
}

/// `ns-app eval`'s own default: the plan's default half-life, so an arm run
/// at a non-zero weight decays the way the shipped config would.
const ACTIVATION_HALF_LIFE: f32 = 7.0;

/// The same set with one context block blanked on every model call of every
/// fixture (M9 T0.4), or the full set when `ablate` is `None`.
///
/// A parameter rather than a switch the fixtures read off a global: the two
/// arms are built one after the other in the same process, and `cargo test`
/// runs this file's own tests beside them.
pub async fn run_all_ablating(ablate: Option<Ablate>) -> Vec<Ability> {
    run_all_for(Run {
        ablate,
        ..Run::default()
    })
    .await
}

/// The set under one arm's configuration (M9 T3.3).
pub async fn run_all_for(run: Run) -> Vec<Ability> {
    vec![
        information_extraction(run).await,
        multi_session_reasoning(run).await,
        temporal_reasoning(run).await,
        knowledge_updates(run).await,
        abstention(run).await,
        selective_forgetting(run).await,
        desktop_open_and_search(run).await,
        desktop_find_and_click(run).await,
        desktop_clipped_tail(run).await,
        desktop_hard_query(run).await,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the deliverable, so it is graded too: a run that failed
    /// must name the ability and the condition, or the ledger row says
    /// "6/6 → 5/6" and nothing else.
    #[test]
    fn the_report_names_the_ability_that_broke_and_its_numbers() {
        let row = |ability, passed, detail: &str| Ability {
            ability,
            passed,
            turns: 9,
            requests: 19,
            prompt_chars: 512,
            prompt_tokens: 128,
            context_chars: 300,
            emitter_prefix_tokens: 75,
            peak_chars: 700,
            tool_calls: 3,
            recall_fired: false,
            recall_hits: 0,
            flags: 0,
            clipped_chars: 13_225,
            inspections: 2,
            budget_drops: 1,
            escalations: 1,
            target_action: REMEMBER_FACT,
            target_proposed: true,
            legal_size: 8,
            bits: 3.0,
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
        for n in ["9", "19", "512", "128", "700", "13225"] {
            assert!(table.contains(n), "{n} missing from:\n{table}");
        }
        // Including the four the desktop half is about, by heading as well as
        // by value: a column nobody can name in the printed table is a column
        // nobody reads.
        for heading in ["clipped", "insp", "drops", "esc"] {
            assert!(table.contains(heading), "{heading} missing from:\n{table}");
        }
        // M10 T0.4: the column and the summed line, both.
        assert!(table.contains("BoR"), "{table}");
        assert!(table.contains("3.00"), "log2(8) per row:\n{table}");
        assert!(
            table.contains("Bits-over-Random: 6.00 bits over 2 abilities (3.00 mean)"),
            "the summed line is what a narrowing is judged on:\n{table}"
        );
    }

    /// A miss and a one-action legal set both read zero, and the miss says
    /// which `n` it missed out of — the two ways BoR refuses to flatter.
    #[test]
    fn a_missed_target_prints_its_legal_set_size_and_zero_bits() {
        let mut r = Ability {
            ability: "desktop hard query",
            passed: false,
            turns: 1,
            requests: 2,
            prompt_chars: 0,
            prompt_tokens: 0,
            context_chars: 0,
            emitter_prefix_tokens: 0,
            peak_chars: 0,
            tool_calls: 1,
            recall_fired: false,
            recall_hits: 0,
            flags: 0,
            clipped_chars: 1,
            inspections: 0,
            budget_drops: 0,
            escalations: 0,
            target_action: POINTER_SCROLL,
            target_proposed: false,
            legal_size: 16,
            bits: 0.0,
            detail: "pointer_scroll never ran".into(),
        };
        let table = render_table(std::slice::from_ref(&r));
        assert!(table.contains("16!"), "a miss names its n:\n{table}");
        assert!(table.contains("0.00"), "{table}");
        // And a legal set of one is worth nothing however certain the pick.
        r.target_proposed = true;
        r.legal_size = 1;
        assert_eq!(bits_over_random(r.target_proposed, r.legal_size), 0.0);
    }

    /// `ns-app eval` diffs two ledger rows position by position and the test
    /// harness asserts the whole set. Both break silently if the set ever
    /// returns a different number of rows or reorders them. Memory first,
    /// desktop after: the order is the file's and the ledger's.
    #[tokio::test]
    async fn run_all_returns_the_ten_abilities_in_a_fixed_order() {
        let names: Vec<&str> = run_all().await.iter().map(|r| r.ability).collect();
        assert_eq!(
            names,
            vec![
                "information extraction",
                "multi-session reasoning",
                "temporal reasoning",
                "knowledge updates",
                "abstention",
                "selective forgetting",
                "desktop open-and-search",
                "desktop find-and-click",
                "desktop clipped tail",
                "desktop hard query",
            ]
        );
    }

    /// **The point of the hard-query fixture** (M10 T0.3).
    ///
    /// It passes on today's full legal set and fails with `pointer_scroll`
    /// out of it. Both halves are the assertion: a fixture that only passed
    /// would prove nothing about depth, and one that only failed would be
    /// broken. This is what lets P2's narrowing be judged rather than
    /// assumed — tool-loading §1.3's "0% on hard queries" shows up here as a
    /// red row instead of as a capability that quietly stopped existing.
    #[tokio::test]
    async fn the_hard_query_ability_fails_when_its_tool_is_withheld() {
        let full = desktop_hard_query(Run::default()).await;
        assert!(
            full.passed,
            "the hard query must pass on the full set: {}",
            full.detail
        );
        // It is the *tool* that carries it, so the full arm's BoR has to be
        // a real choice out of a real legal set rather than a set of one.
        assert!(full.target_proposed && full.legal_size > 1, "{full:?}");
        assert!(full.bits > 0.0, "{full:?}");

        let narrowed = desktop_hard_query(Run {
            withhold: &[POINTER_SCROLL],
            ..Run::default()
        })
        .await;
        assert!(
            !narrowed.passed,
            "withholding pointer_scroll must fail the ability, or it guards nothing: {narrowed:?}"
        );
        assert!(
            narrowed.detail.contains("pointer_scroll never ran"),
            "and it must fail on the missing action, not on some side effect: {}",
            narrowed.detail
        );
        // Chance-corrected, the withheld arm scores nothing: the target was
        // never proposed because it was never offered.
        assert!(!narrowed.target_proposed, "{narrowed:?}");
        assert_eq!(narrowed.bits, 0.0, "{narrowed:?}");
        assert!(
            narrowed.legal_size < full.legal_size,
            "the narrowed arm really was narrower: {} vs {}",
            narrowed.legal_size,
            full.legal_size
        );
    }

    /// The claim the four new columns exist to make: the two halves exercise
    /// different machinery.
    ///
    /// Nothing in the memory half clips a result, pages one, goes over the
    /// budget or routes a turn, so all four read zero there — and a memory
    /// fixture that started clipping, or a desktop one that stopped, would
    /// otherwise be a fixture quietly measuring something other than what its
    /// name says. `harness_eval.rs` grades whether the nine pass; this grades
    /// whether they are still about what they claim to be about.
    #[tokio::test]
    async fn the_two_halves_move_different_columns() {
        let rows = run_all().await;
        // The columns are the subject of this assertion, so a failure has to
        // print them.
        println!("{}", render_table(&rows));
        let (memory, desktop) = rows.split_at(6);
        for r in memory {
            assert_eq!(
                (
                    r.clipped_chars,
                    r.inspections,
                    r.budget_drops,
                    r.escalations
                ),
                (0, 0, 0, 0),
                "{} is a memory fixture and reaches none of the M7 machinery",
                r.ability
            );
        }
        assert!(
            desktop.iter().all(|r| r.clipped_chars > 0),
            "every desktop task clips the screen read it is built on"
        );
        assert!(
            desktop.iter().any(|r| r.inspections > 0),
            "one of them has to page past the cap, or the handle is untested"
        );
        assert!(
            desktop.iter().any(|r| r.escalations > 0),
            "one of them has to start conversational, or the misroute path is untested"
        );
    }

    /// **M12 T5.1: `None` is today's defaults.**
    ///
    /// The caps are an arm, not a change: a run that leaves them unset and a
    /// run that names the engine's own numbers have to render the same table,
    /// byte for byte, or every ledger diff taken after this task reads as a
    /// harness change.
    #[tokio::test]
    async fn a_run_at_the_default_profile_is_byte_identical_to_todays() {
        let today = render_table(&run_all().await);
        assert_eq!(
            render_table(&run_all_for(Run::default()).await),
            today,
            "the default arm is `Run::default()` and nothing else"
        );
        let named = Run {
            window_turns: Some(EngineConfig::default().window_turns),
            facts_in_context: Some(EngineConfig::default().facts_in_context),
            ..Run::default()
        };
        assert_eq!(
            render_table(&run_all_for(named).await),
            today,
            "an arm that names the defaults must be the default arm"
        );
    }

    /// **M12 T5.3: the two context columns come off the manifest.**
    ///
    /// `prompt_chars` is the fixture's own rendering of the graded reply
    /// context; `context_chars` is what the engine recorded having sent the
    /// *emitter*, summed over that ability's calls. They are different
    /// numbers from different places, and the point of the new columns is
    /// that the second is measured rather than re-derived from the first.
    ///
    /// The two are compared at the level they are comparable at: a *call's*
    /// prefix against the graded prompt. `context_chars` is a sum over every
    /// emitter call of the fixture and `prompt_chars` is one prompt, so
    /// `information extraction` spends 3,840 context characters over nine
    /// turns against a 550-character graded prompt — a sum being larger than
    /// one of its neighbours' single readings says nothing about either.
    /// `emitter_prefix_tokens` is the per-call level, and that is what has to
    /// sit under the prompt the same fixture rendered.
    #[tokio::test]
    async fn the_prompt_column_sums_the_manifest_not_the_fixture() {
        let rows = run_all_for(Run::default()).await;
        println!("{}", render_table(&rows));
        for r in rows.iter().filter(|r| r.context_chars > 0) {
            assert!(
                r.emitter_prefix_tokens <= r.prompt_tokens,
                "{}: a {}-token median emitter prefix against a {}-token graded prompt",
                r.ability,
                r.emitter_prefix_tokens,
                r.prompt_tokens
            );
            assert!(
                r.emitter_prefix_tokens > 0,
                "{}: stable blocks were sent and none of them was counted",
                r.ability
            );
        }
        // The ability whose scripted turns pin three facts and then ask for
        // them: its emitter calls carry a facts block, so its prefix is not
        // zero. Without this the loop above would pass on a column that was
        // zero everywhere.
        let extraction = rows
            .iter()
            .find(|r| r.ability == "information extraction")
            .expect("the set opens with information extraction");
        assert!(
            extraction.context_chars > 0 && extraction.emitter_prefix_tokens > 0,
            "{extraction:?}"
        );
    }

    /// The desktop fixture has to be the recorded size and the wrong shape
    /// would be invisible in a passing row.
    ///
    /// Three properties, each of which a task depends on: the tree is the
    /// size of the larger recorded `pointer_ui_read`; the control the second
    /// task reaches is inside the 1,200 characters the cap shows; and the
    /// control the third task reaches is far outside them. Let the third one
    /// drift inside the cap and that task passes with `inspect_result` never
    /// called and nothing measured — which is precisely what the mutation
    /// check does to it on purpose.
    #[test]
    fn the_control_tree_is_the_recorded_size_and_its_tail_is_past_the_cap() {
        let tree = control_tree();
        let chars = tree.chars().count();
        // To within one control line: the tree is filled a whole row at a
        // time, so it lands within about forty characters of the record.
        assert!(
            (RECORDED_CHARS - 60..=RECORDED_CHARS + 60).contains(&chars),
            "{chars} characters, want about the recorded {RECORDED_CHARS}"
        );
        let head: String = tree.chars().take(1_200).collect();
        assert!(
            head.starts_with("MODAL"),
            "the real tool puts what is blocking the screen first:\n{head}"
        );
        assert!(
            head.contains(HEAD_CONTROL) && head.contains(SEARCH_CONTROL),
            "find-and-click reaches its controls out of the shown head"
        );
        assert!(
            !head.contains(TAIL_CONTROL),
            "the third task's control must not be in the part the cap keeps"
        );
        let at = tree
            .find(TAIL_CONTROL)
            .expect("the tail control is in the tree");
        assert!(
            tree[..at].chars().count() > 13_000,
            "the tail sits {} characters in; it has to be far past the cap",
            tree[..at].chars().count()
        );
        assert_eq!(
            tree.matches(TAIL_CONTROL).count(),
            1,
            "and only once, or an inspect_result query would anchor on the wrong one"
        );
        // The world's answer to a click, which is what a task is graded on.
        assert_eq!(
            control_at(TAIL_POINT.0, TAIL_POINT.1).as_deref(),
            Some("button \"Sloučit buňky\"")
        );
        assert_eq!(control_at(1, 1), None, "a click that hit nothing says so");
    }

    /// The double reads the point out of the trace and nothing else. Given a
    /// trace with the line in it, it finds the point; given the clipped line,
    /// it finds the handle instead — the two halves of what makes a desktop
    /// pass mean something about the harness.
    #[test]
    fn the_desktop_double_reads_points_and_handles_out_of_what_it_was_shown() {
        let shown = "ToolReturned(ok: MODAL (handle this before anything behind it):\n  \
                     button \"Uložit jako…\" (872,604)";
        assert_eq!(point_of(shown, HEAD_CONTROL), Some((872, 604)));
        assert_eq!(point_of(shown, TAIL_CONTROL), None);
        let clipped = "ToolReturned(ok: MODAL…) [r42: 14425 chars, 1200 shown — \
                       inspect_result to see more]";
        assert_eq!(handle_in(clipped).as_deref(), Some("r42"));
        assert_eq!(handle_in(shown), None, "nothing was clipped");
        // A line the cap cut mid-coordinate reads as absent, not as a wrong
        // point: a click on a half-read coordinate would land somewhere.
        assert_eq!(point_of("button \"Uložit jako…\" (872", HEAD_CONTROL), None);
    }
}
