//! Rendering a turn's events into the text a model is shown.
//!
//! Split out of `turn.rs` because none of it ever belonged to `Engine`:
//! every function here is free, takes `&[Event]` plus scalars, and reads no
//! engine state. That is why this was already the testable part of the turn
//! loop, and why `ns-app budget` and `ns-app echo` call `trace_for_prompt`
//! and `turn_trace` directly instead of reimplementing them — a
//! reimplementation would make the saving they report a fiction.
//!
//! The operations, in the order a turn applies them: build entries from
//! events (`trace_entries`), fold this turn's older steps into a counted
//! line (`fold_older_steps`), clip what is left to a character budget while
//! leaving a handle so the dropped part stays addressable (`clip_trace_line`,
//! `result_handle`), and page into it when asked (`result_window`).

use crate::turn::INSPECT_RESULT;
use nscore::{EventKind, RejectReason, ToolOutcome};

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
pub(crate) fn result_window(
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

/// One human-readable line per this-turn event, each carrying the id of the
/// event it came from where that id is a *handle* — that is, for tool
/// results, which are the only lines a model can ask to see more of.
///
/// Split out from `turn_trace` so the clip and `inspect_result` name the same
/// event. A handle the model cannot resolve is worse than no handle.
fn trace_entries(events: &[nscore::Event], turn: u32) -> Vec<TraceEntry> {
    // `ToolReturned` names the call it answers, not the action; the action is
    // on the `ToolCalled` it points at. The same resolution the fold in
    // state.rs does, for the same reason: a count of "pointer_move ×3" needs
    // a name, and `call 17` is not one.
    let mut action_of: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for e in events.iter().filter(|e| e.turn == turn) {
        if let EventKind::ToolCalled { action, .. } = &e.kind {
            action_of.insert(e.id.0, action.clone());
        }
    }
    events
        .iter()
        .filter(|e| e.turn == turn)
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => Some(TraceEntry {
                handle: None,
                action: Some(proposal.action.clone()),
                rejection: false,
                foldable: false,
                line: format!("Proposed({})", proposal.action),
            }),
            EventKind::Rejected { reason, .. } => Some(TraceEntry {
                handle: None,
                action: None,
                rejection: true,
                foldable: false,
                line: match reason {
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
            }),
            EventKind::ToolReturned { outcome, call } => {
                let action = action_of.get(&call.0).cloned();
                Some(match outcome {
                    ToolOutcome::Ok { output } => TraceEntry {
                        handle: Some(e.id),
                        action,
                        rejection: false,
                        foldable: true,
                        line: format!("ToolReturned(ok: {})", output.summary),
                    },
                    ToolOutcome::Err { kind, detail } => TraceEntry {
                        handle: None,
                        action,
                        rejection: false,
                        foldable: true,
                        line: format!("ToolReturned(err {kind}: {detail})"),
                    },
                })
            }
            _ => None,
        })
        .collect()
}

/// One line of this turn's trace, with what the prompt renderer needs to
/// decide how much of it to send.
struct TraceEntry {
    /// The result this line can be paged through with `inspect_result`.
    handle: Option<nscore::EventId>,
    /// The action behind the line, for the fold's counts.
    action: Option<String>,
    /// A refusal. Never folded: refusals are what steer the next proposal,
    /// and a model that cannot see why it was refused proposes it again —
    /// which is the failure the repeat gate exists to stop and the schema
    /// narrowing exists to prevent recurring.
    rejection: bool,
    /// Whether the fold may summarize this line into a count. Outcomes may;
    /// `Proposed` lines are dropped by the fold instead, since the outcome
    /// line beneath them already names the action.
    foldable: bool,
    line: String,
}

/// `pointer_move ×3 ok` — how one folded outcome is counted.
fn fold_descriptor(entry: &TraceEntry, max_chars: usize) -> String {
    let action = entry.action.as_deref().unwrap_or("action");
    let outcome = if entry.line.starts_with("ToolReturned(err") {
        "err"
    } else {
        "ok"
    };
    match entry
        .handle
        .filter(|_| entry.line.chars().count() > max_chars)
    {
        Some(id) => format!("{action} {outcome} ({}, clipped)", result_handle(id)),
        None => format!("{action} {outcome}"),
    }
}

/// Collapse the turn's older steps into one line, keeping the last
/// `verbatim` of them and every refusal in full.
///
/// The measured arm of "Less Context, Better Agents" (2606.10209): last-N
/// tool spans plus a summary of the rest took completion from 71% to 92%
/// while cutting tokens to a third. This is the within-a-turn half of that
/// shape — the cross-turn half is M6's window and rolling summary, and is
/// untouched.
///
/// It is emphatically *not* the consolidated window that the 2026-09-04
/// entrainment plan §9 rules out: nothing here is a model's paraphrase.
/// The fold is a count, produced deterministically, and the full trace is
/// still in the log for `ns-app echo` to measure against.
fn fold_older_steps(entries: Vec<TraceEntry>, verbatim: usize, max_chars: usize) -> Vec<TraceEntry> {
    // The budget counts *outcomes*, not lines. Counting lines put five
    // `Proposed`/`Rejected` lines of churn in the verbatim window and folded
    // the turn's one real result away — and the result is what the replier
    // narrates from, so the reply then stated something its own prompt no
    // longer contained. A trace is mostly bookkeeping; the outcomes are the
    // part with content in them.
    let outcomes: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.foldable)
        .map(|(i, _)| i)
        .collect();
    if verbatim == 0 || outcomes.len() <= verbatim {
        return entries;
    }
    let keep_from = outcomes[outcomes.len() - verbatim];
    let mut folded: Vec<String> = Vec::new();
    let mut folded_steps = 0usize;
    let mut kept: Vec<TraceEntry> = Vec::new();
    for (i, entry) in entries.into_iter().enumerate() {
        if i >= keep_from || entry.rejection {
            kept.push(entry);
            continue;
        }
        folded_steps += 1;
        if entry.foldable {
            folded.push(fold_descriptor(&entry, max_chars));
        }
    }
    if folded_steps == 0 {
        return kept;
    }
    // Equal descriptors are counted rather than repeated: eight moves in a
    // row is one fact about the turn, not eight.
    let mut counts: Vec<(String, usize)> = Vec::new();
    for d in folded {
        match counts.iter_mut().find(|(seen, _)| *seen == d) {
            Some((_, n)) => *n += 1,
            None => counts.push((d, 1)),
        }
    }
    let summary = counts
        .into_iter()
        .map(|(d, n)| if n > 1 { format!("{d} ×{n}") } else { d })
        .collect::<Vec<_>>()
        .join(" · ");
    let line = if summary.is_empty() {
        format!("earlier this turn: {folded_steps} steps")
    } else {
        format!("earlier this turn ({folded_steps} steps): {summary}")
    };
    let mut out = vec![TraceEntry {
        handle: None,
        action: None,
        rejection: false,
        foldable: false,
        line,
    }];
    out.extend(kept);
    out
}

/// One human-readable line per this-turn event: outcomes AND refusal reasons.
/// Public because `ns-app echo` reconstructs, from a stored log, the material
/// a reply was shown — and a second rendering of it would drift.
pub fn turn_trace(events: &[nscore::Event], turn: u32) -> String {
    trace_entries(events, turn)
        .into_iter()
        .map(|entry| entry.line)
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
/// The default for `EngineConfig::tool_result_max_chars`, and the value every
/// scripted double and replay runs at.
///
/// A constant until M8 T0.2: the cap was the one number in the clip that
/// could not be moved without a rebuild, which made "is this cap saving a
/// turn or hiding the answer" an unaskable question on a real machine.
pub const DEFAULT_TOOL_RESULT_MAX_CHARS: usize = 1200;

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
pub(crate) fn result_handle(id: nscore::EventId) -> String {
    format!("r{}", id.0)
}

/// The event id behind a handle, accepting `r42` and `42` alike. A small
/// model that echoes the number without the prefix has still identified the
/// result it means.
pub(crate) fn parse_result_handle(raw: &str) -> Option<nscore::EventId> {
    raw.trim()
        .trim_start_matches(['r', 'R'])
        .parse::<u64>()
        .ok()
        .map(nscore::EventId)
}

/// Unqueried inspections of `id` already made this turn — the page number of
/// the next one. Read out of this turn's own events, so a replay pages
/// identically.
pub(crate) fn inspect_page(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> usize {
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
/// Public alongside `turn_trace` and for the same reason: `ns-app budget`
/// reports the raw trace against the one actually sent, and a second
/// implementation of the clip and the fold would drift from this one, which
/// would make the saving it reports a fiction.
pub fn trace_for_prompt(
    events: &[nscore::Event],
    turn: u32,
    verbatim_lines: usize,
    max_chars: usize,
) -> (Vec<String>, usize) {
    let mut dropped = 0;
    let lines = fold_older_steps(trace_entries(events, turn), verbatim_lines, max_chars)
        .into_iter()
        .map(|entry| {
            dropped += entry.line.chars().count().saturating_sub(max_chars);
            clip_trace_line(&entry.line, max_chars, entry.handle)
        })
        .collect();
    (lines, dropped)
}

/// The full text of a tool result recorded this turn, by handle.
///
/// Only `Ok` outcomes: an error's detail is already short and is never
/// clipped, so there is nothing behind it to page through.
pub(crate) fn result_text(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> Option<String> {
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
pub(crate) fn result_trust(events: &[nscore::Event], turn: u32, id: nscore::EventId) -> nscore::Trust {
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
pub(crate) fn clipped_results(events: &[nscore::Event], turn: u32, max_chars: usize) -> Vec<nscore::EventId> {
    trace_entries(events, turn)
        .into_iter()
        .filter(|entry| entry.line.chars().count() > max_chars)
        .filter_map(|entry| entry.handle)
        .collect()
}

/// The inclusive turn range of a verbatim window; `None` when it is empty.
pub(crate) fn window_range(window: &[nscore::TurnRecord]) -> Option<(u32, u32)> {
    match (window.first(), window.last()) {
        (Some(first), Some(last)) => Some((first.turn, last.turn)),
        _ => None,
    }
}

/// What the emitter was shown, in keys (M7 T0.1). Built from the context
/// immediately before it is moved into the call, so the two cannot drift.
pub(crate) fn emitter_manifest(
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
        tier: None,
        route_cues: Vec::new(),
        // Filled in by the caller, which is the only place that knows what
        // the budget did to this context.
        budget: None,
    }
}

/// What the replier was shown. `tools` is zero: the reply model is given no
/// action schema at all, which is half of why it is the cheaper of the two.
pub(crate) fn reply_manifest(ctx: &nscore::ReplyContext, clipped_chars: usize) -> nscore::ContextManifest {
    nscore::ContextManifest {
        fact_keys: ctx.facts.iter().map(|f| f.key.clone()).collect(),
        summary_through: ctx.summary.as_ref().map(|s| s.through_turn),
        window: window_range(&ctx.window),
        trace_lines: ctx.turn_trace.lines().count(),
        trace_chars: ctx.turn_trace.chars().count(),
        clipped_chars,
        tools: 0,
        guidance: ctx.guidance.len(),
        tier: None,
        route_cues: Vec::new(),
        budget: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result that fits is passed through untouched: most of them do, and a
    /// trace full of "[0 more characters]" would be noise.
    #[test]
    fn a_short_trace_line_is_left_alone() {
        let line = "ToolReturned(ok: moved to (960, 540))";
        let handle = Some(nscore::EventId(7));
        assert_eq!(clip_trace_line(line, DEFAULT_TOOL_RESULT_MAX_CHARS, handle), line);
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

    fn outcome(action: &str, id: u64, text: &str) -> TraceEntry {
        TraceEntry {
            handle: Some(nscore::EventId(id)),
            action: Some(action.into()),
            rejection: false,
            foldable: true,
            line: format!("ToolReturned(ok: {text})"),
        }
    }

    fn proposed(action: &str) -> TraceEntry {
        TraceEntry {
            handle: None,
            action: Some(action.into()),
            rejection: false,
            foldable: false,
            line: format!("Proposed({action})"),
        }
    }

    fn refusal(text: &str) -> TraceEntry {
        TraceEntry {
            handle: None,
            action: None,
            rejection: true,
            foldable: false,
            line: format!("Rejected({text})"),
        }
    }

    fn folded(entries: Vec<TraceEntry>, verbatim: usize) -> Vec<String> {
        fold_older_steps(entries, verbatim, DEFAULT_TOOL_RESULT_MAX_CHARS)
            .into_iter()
            .map(|e| e.line)
            .collect()
    }

    /// The regression that produced this rule. Counting *lines* let five
    /// `Proposed`/`Rejected` lines of churn fill the verbatim window and
    /// folded the turn's one real result away — after which the reply model
    /// narrated a result its own prompt no longer contained. The budget
    /// counts outcomes, so the only outcome always survives.
    #[test]
    fn the_fold_never_swallows_the_only_outcome() {
        let entries = vec![
            proposed("echo"),
            outcome("echo", 4, "echo: hi"),
            proposed("echo"),
            refusal("guard repeat_gate: identical call to 'echo'"),
            proposed("echo"),
            refusal("illegal action: echo"),
        ];
        let lines = folded(entries, 5);
        assert!(
            lines.iter().any(|l| l.contains("echo: hi")),
            "the result must survive: {lines:?}"
        );
    }

    #[test]
    fn the_fold_keeps_recent_outcomes_and_every_refusal_and_counts_the_rest() {
        let mut entries = vec![refusal("guard taint_policy: external")];
        for i in 0..4 {
            entries.push(proposed("pointer_move"));
            entries.push(outcome("pointer_move", 10 + i, "moved"));
        }
        entries.push(outcome("pointer_click", 20, "clicked (389, 1056)"));
        let lines = folded(entries, 2);

        assert!(
            lines[0].starts_with("earlier this turn ("),
            "the fold leads: {lines:?}"
        );
        assert!(
            lines[0].contains("pointer_move ok ×3"),
            "equal steps are counted, not repeated: {}",
            lines[0]
        );
        assert!(
            lines.iter().any(|l| l.contains("taint_policy")),
            "a refusal is never folded — it is what steers the next proposal: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("clicked (389, 1056)")),
            "the last outcomes stay verbatim: {lines:?}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("moved")).count(),
            1,
            "one of the four moves is recent enough to keep: {lines:?}"
        );
    }

    /// A folded result keeps its handle in the summary, so the page is still
    /// reachable after the line that carried it is gone.
    #[test]
    fn a_folded_clipped_result_still_names_its_handle() {
        let big = outcome("pointer_ui_read", 42, &"node ".repeat(400));
        let entries = vec![big, outcome("a", 1, "x"), outcome("b", 2, "y")];
        let lines = folded(entries, 2);
        assert!(
            lines[0].contains("pointer_ui_read ok (r42, clipped)"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn the_fold_is_off_below_the_budget_and_at_zero() {
        let entries = || vec![outcome("a", 1, "x"), outcome("b", 2, "y")];
        assert_eq!(folded(entries(), 5).len(), 2, "nothing to fold");
        let many = || {
            vec![
                outcome("a", 1, "x"),
                outcome("b", 2, "y"),
                outcome("c", 3, "z"),
            ]
        };
        assert_eq!(folded(many(), 0).len(), 3, "0 disables the fold");
        assert_eq!(folded(many(), 1).len(), 2, "fold line plus the last one");
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
}
