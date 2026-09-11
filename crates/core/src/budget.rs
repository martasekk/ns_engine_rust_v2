//! A token budget the engine can hold by construction (M7 plan Phase 2).
//!
//! M6 bounded every *block* of a context — six turn records, ten facts, an
//! 800-character summary — but never their sum, and never against a number.
//! A budget over the sum is what makes the whole context a governed object
//! rather than four independently capped ones, and it is the difference
//! between "the window is six turns" and "this prompt is 2.1k of 6k tokens".
//!
//! Two rules keep it honest.
//!
//! **It reports before it enforces.** [`BudgetMode::Report`] computes exactly
//! what it *would* drop, records that in the `ModelCall` manifest, and drops
//! nothing. Self-GC (findings 06) reports 85% of prunes leaving future
//! continuations unaffected; until this engine's own no-impact rate is known
//! from real sessions, a dropping context manager is an untested theory about
//! which context does not matter. Enforcing is one config word away, and
//! should be turned on by evidence rather than by intent.
//!
//! **What it measures is what this crate renders.** The estimate covers the
//! blocks the engine composes — facts, summary, window, the current turn —
//! using the same renderers the prompt is built from. It does not cover the
//! system prompt, which is fixed, or the tool schemas, which are measured
//! separately and exactly by [`crate::Usage::tools_tokens`]. A budget that
//! silently included a number it could not control would be unfalsifiable.
use crate::usage::estimate_tokens;
use serde::{Deserialize, Serialize};

/// Whether the fit acts on what it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetMode {
    /// Measure and record; change nothing. The default, deliberately.
    #[default]
    Report,
    /// Apply the drops.
    Enforce,
}

impl BudgetMode {
    pub fn parse(s: &str) -> Option<BudgetMode> {
        match s {
            "report" => Some(BudgetMode::Report),
            "enforce" => Some(BudgetMode::Enforce),
            _ => None,
        }
    }
}

/// One thing the fit removed, or would have removed under `Report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    /// `window`, `obligations`, `facts`, `summary` or `guidance`.
    pub block: String,
    /// Which item, in the terms the log already uses: a turn number, a fact
    /// key. Enough to ask afterwards whether the turns that went wrong were
    /// the turns something was dropped from.
    pub detail: String,
    pub tokens: u32,
}

/// What one context cost, and what the budget did or would do about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReport {
    pub limit: u32,
    /// Estimated tokens of the composed blocks before the fit.
    pub before: u32,
    /// After — equal to `before` under [`BudgetMode::Report`], which is the
    /// point: the pair says what enforcing would have cost.
    pub after: u32,
    pub mode: BudgetMode,
    #[serde(default)]
    pub dropped: Vec<Dropped>,
}

impl BudgetReport {
    pub fn over(&self) -> bool {
        self.before > self.limit
    }

    /// One line the emitter may be shown (`show_budget_line`), in the terms
    /// VISTA found moved a Flash-class model from 22.7% to 50.7%: its own
    /// size against its own ceiling, and the lever that would shrink it.
    pub fn line(&self, clipped: &[String]) -> String {
        let mut s = format!(
            "Context: ~{} of {} tokens",
            thousands(self.after),
            thousands(self.limit)
        );
        if !clipped.is_empty() {
            s.push_str(&format!(
                " · {} result{} clipped ({})",
                clipped.len(),
                if clipped.len() == 1 { "" } else { "s" },
                clipped.join(", ")
            ));
            s.push_str(" — narrow the query, or inspect_result for the rest");
        }
        s
    }
}

/// `2.1k` past a thousand, plain below it: this string goes into a prompt a
/// small model reads, and `2137` invites arithmetic nobody wants it doing.
fn thousands(n: u32) -> String {
    if n < 1000 {
        return n.to_string();
    }
    format!("{:.1}k", f64::from(n) / 1000.0)
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// The rendered size of each stable block, in characters. Public because
/// the engine records them in the manifest after the fit, and a second
/// measurement written there would be the one that drifts (M9 T0.5).
pub fn facts_chars(facts: &[crate::memory::FactView]) -> usize {
    facts
        .iter()
        .map(|f| chars(&crate::memory::render_fact(f)) + 3)
        .sum()
}

pub fn summary_chars(summary: Option<&crate::memory::SessionSummary>) -> usize {
    summary
        .map(|s| chars(&crate::memory::render_summary(s)))
        .unwrap_or(0)
}

pub fn window_chars(window: &[crate::memory::TurnRecord], caps: &crate::memory::Caps) -> usize {
    chars(&crate::memory::render_window(window, window.len(), caps))
}

fn lines_chars(lines: &[String]) -> usize {
    lines.iter().map(|l| chars(l) + 3).sum()
}

/// The blocks of an emitter context, in characters.
fn emitter_chars(ctx: &crate::traits::EmitterContext) -> usize {
    facts_chars(&ctx.facts)
        + summary_chars(ctx.summary.as_ref())
        + window_chars(&ctx.window, &ctx.caps)
        + chars(&ctx.user_text)
        + lines_chars(&ctx.obligations)
        + lines_chars(&ctx.trace_so_far)
        + lines_chars(&ctx.rejections_this_turn)
        + lines_chars(&ctx.guidance)
}

/// Cut `guidance` to `max`, reporting each note that went (M9 T2.2).
///
/// From the end: file order is the priority order today — `learned.toml` is
/// written newest-last by the evolution pass and read in order — so a prefix
/// is the notes that earned their place first. Dropping from the end is also
/// what keeps `ContextManifest.note_hashes` truthful: the engine cuts the
/// hash list to `ctx.guidance.len()`, which is only the right list if nothing
/// was taken from the middle.
///
/// Unlike every other stage this is a hard cap rather than a budget
/// decision, so it runs whether or not the context is over the limit — but
/// like every other stage it only *reports* under [`BudgetMode::Report`].
fn clamp_guidance(guidance: &mut Vec<String>, max: usize, report: &mut BudgetReport) {
    while guidance.len() > max {
        let note = guidance.pop().expect("len > max >= 0");
        report.dropped.push(Dropped {
            block: "guidance".into(),
            detail: note.chars().take(40).collect(),
            tokens: estimate_tokens(chars(&note) + 3),
        });
    }
}

/// The blocks of a reply context, in characters.
fn reply_chars(ctx: &crate::traits::ReplyContext) -> usize {
    chars(&ctx.persona)
        + facts_chars(&ctx.facts)
        + summary_chars(ctx.summary.as_ref())
        + window_chars(&ctx.window, &ctx.caps)
        + chars(&ctx.user_text)
        + lines_chars(&ctx.obligations)
        + chars(&ctx.turn_trace)
        + lines_chars(&ctx.guidance)
}

/// Whether a fact is one of the pinned core (M6 §6.5) and so out of reach of
/// the fit. Prefix rather than a flag on `FactView` because the prefixes are
/// configuration and the view is a projection; duplicating the decision into
/// the projection is how the two drift.
fn is_pinned(key: &str, pinned_prefixes: &[String]) -> bool {
    pinned_prefixes.iter().any(|p| key.starts_with(p))
}

/// Bring an emitter context inside `limit`, or report what would.
///
/// Order, first to go (plan §6, T2.1): oldest window records, then
/// obligations from the end (M9 T2.1), then non-pinned relevant facts
/// newest-rank-last, then the summary clamped. Guidance is clamped to
/// `guidance_max` before any of it (M9 T2.2) and is no longer exempt from
/// the count.
/// Never the user's message, the rejections, the pending-confirmation line,
/// or this turn's trace — the trace is already bounded deterministically by
/// the fold (T1.3), which keeps refusals, and a second budget-driven trimmer
/// over the same lines could drop one.
///
/// One deviation from the plan's literal order, recorded: the window is
/// never emptied. The last record is the immediately preceding turn, and
/// losing it is exactly M6's F3 — "no memory beyond ~3 exchanges" — which
/// the window exists to fix. Dropping it to buy room for a fact ranked
/// fifth by lexical overlap would trade the finding for the workaround.
pub fn fit_emitter(
    ctx: &mut crate::traits::EmitterContext,
    limit: u32,
    mode: BudgetMode,
    pinned_prefixes: &[String],
    guidance_max: usize,
) -> BudgetReport {
    let before = estimate_tokens(emitter_chars(ctx));
    let mut report = BudgetReport {
        limit,
        before,
        after: before,
        mode,
        dropped: Vec::new(),
    };
    // Work on a copy under Report, so the caller's context is untouched and
    // the numbers are still the ones enforcing would have produced.
    let mut facts = ctx.facts.clone();
    let mut window = ctx.window.clone();
    let mut summary = ctx.summary.clone();
    let mut obligations = ctx.obligations.clone();
    let mut guidance = ctx.guidance.clone();
    // The hard cap first (M9 T2.2): it is not a budget decision, so it does
    // not wait for the context to be over, and the stages below then see the
    // notes that are actually going to be sent.
    clamp_guidance(&mut guidance, guidance_max, &mut report);
    let fixed = chars(&ctx.user_text)
        + lines_chars(&ctx.trace_so_far)
        + lines_chars(&ctx.rejections_this_turn)
        + lines_chars(&guidance);
    let total = |facts: &[crate::memory::FactView],
                 window: &[crate::memory::TurnRecord],
                 summary: Option<&crate::memory::SessionSummary>,
                 obligations: &[String]| {
        estimate_tokens(
            fixed
                + facts_chars(facts)
                + window_chars(window, &ctx.caps)
                + summary_chars(summary)
                + lines_chars(obligations),
        )
    };

    if limit > 0 {
        while total(&facts, &window, summary.as_ref(), &obligations) > limit && window.len() > 1 {
            let record = window.remove(0);
            report.dropped.push(Dropped {
                block: "window".into(),
                detail: format!("t{}", record.turn),
                tokens: estimate_tokens(chars(&crate::memory::render_record(&record, &ctx.caps))),
            });
        }
        // Obligations sit between the window and the facts (M9 T2.1): they
        // are worth more than a fact ranked fifth by lexical overlap and
        // less than the immediately preceding turn.
        while total(&facts, &window, summary.as_ref(), &obligations) > limit
            && !obligations.is_empty()
        {
            let o = obligations.pop().expect("not empty");
            report.dropped.push(Dropped {
                block: "obligations".into(),
                detail: o.chars().take(40).collect(),
                tokens: estimate_tokens(chars(&o) + 3),
            });
        }
        while total(&facts, &window, summary.as_ref(), &obligations) > limit {
            let Some(at) = facts
                .iter()
                .rposition(|f| !is_pinned(&f.key, pinned_prefixes))
            else {
                break;
            };
            let fact = facts.remove(at);
            report.dropped.push(Dropped {
                block: "facts".into(),
                detail: fact.key.clone(),
                tokens: estimate_tokens(chars(&crate::memory::render_fact(&fact))),
            });
        }
        if total(&facts, &window, summary.as_ref(), &obligations) > limit {
            if let Some(s) = summary.as_mut() {
                let over = total(&facts, &window, Some(s), &obligations).saturating_sub(limit);
                let was = summary_chars(Some(s));
                // Four characters to the token, so trimming `over` tokens
                // means trimming four times as many characters.
                let target = was.saturating_sub(over as usize * 4);
                s.clamp(target);
                let now = summary_chars(Some(s));
                if now < was {
                    report.dropped.push(Dropped {
                        block: "summary".into(),
                        detail: format!("clamped to {target} chars"),
                        tokens: estimate_tokens(was - now),
                    });
                }
            }
        }
    }
    report.after = total(&facts, &window, summary.as_ref(), &obligations);
    if mode == BudgetMode::Enforce {
        ctx.facts = facts;
        ctx.window = window;
        ctx.summary = summary;
        ctx.obligations = obligations;
        ctx.guidance = guidance;
    }
    report
}

/// The reply context's fit. Same order and the same exemptions, minus the
/// trace: the replier's `turn_trace` is the material it narrates from, and
/// the grounding interceptor flags a reply for stating anything absent from
/// it — trimming it would manufacture the fabrications the interceptor then
/// catches.
pub fn fit_reply(
    ctx: &mut crate::traits::ReplyContext,
    limit: u32,
    mode: BudgetMode,
    pinned_prefixes: &[String],
    guidance_max: usize,
) -> BudgetReport {
    let before = estimate_tokens(reply_chars(ctx));
    let mut report = BudgetReport {
        limit,
        before,
        after: before,
        mode,
        dropped: Vec::new(),
    };
    let mut facts = ctx.facts.clone();
    let mut window = ctx.window.clone();
    let summary = ctx.summary.clone();
    let mut obligations = ctx.obligations.clone();
    let mut guidance = ctx.guidance.clone();
    clamp_guidance(&mut guidance, guidance_max, &mut report);
    let fixed = chars(&ctx.persona)
        + chars(&ctx.user_text)
        + chars(&ctx.turn_trace)
        + lines_chars(&guidance);
    let total = |facts: &[crate::memory::FactView],
                 window: &[crate::memory::TurnRecord],
                 obligations: &[String]| {
        estimate_tokens(
            fixed
                + facts_chars(facts)
                + window_chars(window, &ctx.caps)
                + summary_chars(summary.as_ref())
                + lines_chars(obligations),
        )
    };
    if limit > 0 {
        while total(&facts, &window, &obligations) > limit && window.len() > 1 {
            let record = window.remove(0);
            report.dropped.push(Dropped {
                block: "window".into(),
                detail: format!("t{}", record.turn),
                tokens: estimate_tokens(chars(&crate::memory::render_record(&record, &ctx.caps))),
            });
        }
        while total(&facts, &window, &obligations) > limit && !obligations.is_empty() {
            let o = obligations.pop().expect("not empty");
            report.dropped.push(Dropped {
                block: "obligations".into(),
                detail: o.chars().take(40).collect(),
                tokens: estimate_tokens(chars(&o) + 3),
            });
        }
        while total(&facts, &window, &obligations) > limit {
            let Some(at) = facts
                .iter()
                .rposition(|f| !is_pinned(&f.key, pinned_prefixes))
            else {
                break;
            };
            let fact = facts.remove(at);
            report.dropped.push(Dropped {
                block: "facts".into(),
                detail: fact.key.clone(),
                tokens: estimate_tokens(chars(&crate::memory::render_fact(&fact))),
            });
        }
    }
    report.after = total(&facts, &window, &obligations);
    if mode == BudgetMode::Enforce {
        ctx.facts = facts;
        ctx.window = window;
        ctx.obligations = obligations;
        ctx.guidance = guidance;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Caps, FactView, SessionSummary, TurnRecord};
    use crate::traits::EmitterContext;
    use crate::value::Trust;

    fn fact(key: &str, value: &str) -> FactView {
        crate::action::Fact {
            key: key.into(),
            value: serde_json::json!(value),
            ..Default::default()
        }
        .into()
    }

    fn record(turn: u32, text: &str) -> TurnRecord {
        TurnRecord {
            turn,
            user: text.into(),
            did: vec![],
            reply: text.into(),
            trust: Trust::User,
        }
    }

    fn ctx() -> EmitterContext {
        EmitterContext {
            usage: None,
            facts: vec![
                fact("user.name", "Martin"),
                fact("user.city", "Brno"),
                fact("order.42.status", "shipped"),
                fact("order.43.status", "packing"),
            ],
            summary: Some(SessionSummary {
                through_turn: 4,
                topic: "x".repeat(400),
                established: vec!["y".repeat(200)],
                open: vec![],
                trust: Trust::User,
                rebuilt_from: 1,
            }),
            window: (1..=6).map(|t| record(t, &"w".repeat(200))).collect(),
            caps: Caps::default(),
            user_text: "what now".into(),
            obligations: vec![],
            trace_so_far: vec!["ToolReturned(ok: done)".into()],
            pending_confirmation: false,
            rejections_this_turn: vec!["guard g: no".into()],
            guidance: vec![],
            budget_line: None,
        }
    }

    fn pinned() -> Vec<String> {
        vec!["user.".to_string()]
    }

    /// Report is the default and it is not a dry run of nothing: it computes
    /// the same drops enforcing would make, and leaves the context alone.
    /// Until the no-impact rate is known from real sessions, a context
    /// manager that drops is an untested theory about which context does not
    /// matter.
    #[test]
    fn report_mode_measures_the_drops_without_making_them() {
        let mut c = ctx();
        let facts_before = c.facts.len();
        let window_before = c.window.len();
        let r = fit_emitter(&mut c, 100, BudgetMode::Report, &pinned(), 6);
        assert!(r.over(), "the fixture is deliberately over: {}", r.before);
        assert!(!r.dropped.is_empty(), "it says what it would drop");
        assert!(r.after < r.before, "and what that would save");
        assert_eq!(c.facts.len(), facts_before, "but drops nothing");
        assert_eq!(c.window.len(), window_before);
        assert_eq!(r.mode, BudgetMode::Report);
    }

    #[test]
    fn enforce_applies_the_same_drops_in_the_planned_order() {
        let mut c = ctx();
        let r = fit_emitter(&mut c, 100, BudgetMode::Enforce, &pinned(), 6);
        let blocks: Vec<&str> = r.dropped.iter().map(|d| d.block.as_str()).collect();
        assert_eq!(
            blocks.iter().position(|b| *b == "window"),
            Some(0),
            "oldest window records go first: {blocks:?}"
        );
        // The window is trimmed oldest-first and never emptied.
        assert!(!c.window.is_empty(), "the previous turn always survives");
        assert_eq!(
            c.window.last().map(|w| w.turn),
            Some(6),
            "and it is the most recent one"
        );
        assert!(r.after <= r.before);
    }

    /// The pinned core is what the emitter needs to stop re-asking for the
    /// user's name (M6 F2). A budget that could evict it would reintroduce
    /// the failure the facts block exists to fix.
    #[test]
    fn pinned_facts_are_never_dropped_however_tight_the_budget() {
        let mut c = ctx();
        let r = fit_emitter(&mut c, 1, BudgetMode::Enforce, &pinned(), 6);
        let keys: Vec<&str> = c.facts.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&"user.name"), "{keys:?}");
        assert!(keys.contains(&"user.city"), "{keys:?}");
        assert!(
            !keys.contains(&"order.43.status"),
            "the relevant slice goes: {keys:?}"
        );
        assert!(
            r.dropped.iter().any(|d| d.block == "facts"),
            "and it is recorded as a fact drop"
        );
        assert!(
            !c.user_text.is_empty(),
            "the message being answered is never a candidate"
        );
        assert_eq!(c.rejections_this_turn.len(), 1, "nor are the refusals");
        assert_eq!(c.trace_so_far.len(), 1, "nor this turn's trace");
    }

    /// A context already inside the budget is not touched at all, and says
    /// so with an empty drop list rather than an absent report.
    #[test]
    fn a_context_within_budget_is_left_alone() {
        let mut c = ctx();
        let r = fit_emitter(&mut c, 100_000, BudgetMode::Enforce, &pinned(), 6);
        assert!(!r.over());
        assert_eq!(r.before, r.after);
        assert!(r.dropped.is_empty());
        assert_eq!(c.window.len(), 6);
        assert_eq!(c.facts.len(), 4);
    }

    /// 0 is off, not "drop everything" — the same convention
    /// `summary_every_turns = 0` already uses for the layer it governs.
    #[test]
    fn a_zero_limit_disables_the_fit() {
        let mut c = ctx();
        let r = fit_emitter(&mut c, 0, BudgetMode::Enforce, &pinned(), 6);
        assert!(r.dropped.is_empty());
        assert_eq!(c.window.len(), 6);
    }

    /// M9 T2.2. Guidance used to be exempt from the count entirely, so
    /// twelve notes could push a context past its ceiling and the fit would
    /// answer by clamping the summary. The cap runs first and is enough on
    /// its own here: the summary comes out untouched.
    #[test]
    fn guidance_is_clamped_before_the_summary_is() {
        let mut c = ctx();
        c.window.clear();
        c.summary = Some(SessionSummary {
            through_turn: 4,
            topic: "t".repeat(200),
            established: vec![],
            open: vec![],
            trust: Trust::User,
            rebuilt_from: 1,
        });
        c.guidance = (1..=12)
            .map(|n| format!("note {n}: {}", "g".repeat(60)))
            .collect();
        let summary_before = summary_chars(c.summary.as_ref());
        // A ceiling the twelve notes breach and the six do not.
        let limit = estimate_tokens(
            summary_before
                + chars(&c.user_text)
                + lines_chars(&c.trace_so_far)
                + lines_chars(&c.rejections_this_turn)
                + facts_chars(&c.facts)
                + lines_chars(&c.guidance[..8]),
        );
        let r = fit_emitter(&mut c, limit, BudgetMode::Enforce, &pinned(), 6);

        assert_eq!(c.guidance.len(), 6, "six notes are rendered");
        assert_eq!(c.guidance[0], "note 1: ".to_string() + &"g".repeat(60));
        let guidance_drops: Vec<&Dropped> =
            r.dropped.iter().filter(|d| d.block == "guidance").collect();
        assert_eq!(guidance_drops.len(), 6, "{:?}", r.dropped);
        assert!(
            guidance_drops[0].detail.starts_with("note 12"),
            "the tail goes first: {:?}",
            guidance_drops[0]
        );
        assert_eq!(
            summary_chars(c.summary.as_ref()),
            summary_before,
            "the clamp alone brought it under, so the summary stands"
        );
        assert!(
            !r.dropped.iter().any(|d| d.block == "summary"),
            "{:?}",
            r.dropped
        );
        assert!(r.after <= limit, "after {} limit {limit}", r.after);
    }

    #[test]
    fn the_budget_line_reads_as_a_size_against_a_ceiling() {
        let r = BudgetReport {
            limit: 6000,
            before: 2137,
            after: 2137,
            mode: BudgetMode::Report,
            dropped: vec![],
        };
        assert_eq!(r.line(&[]), "Context: ~2.1k of 6.0k tokens");
        let with = r.line(&["r42".to_string()]);
        assert!(with.contains("1 result clipped (r42)"), "{with}");
        assert!(with.contains("inspect_result"), "{with}");
    }

    #[test]
    fn mode_parses_and_round_trips() {
        assert_eq!(BudgetMode::parse("report"), Some(BudgetMode::Report));
        assert_eq!(BudgetMode::parse("enforce"), Some(BudgetMode::Enforce));
        assert_eq!(BudgetMode::parse("maybe"), None);
        let json = serde_json::to_string(&BudgetReport::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<BudgetReport>(&json).unwrap(),
            BudgetReport::default()
        );
    }
}
