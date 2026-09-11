//! M10 T5.4 — the two M9 knobs the ablation arms could not reach.
//!
//! `--ablate` measures a context *block*; `--activation` measures a ranking
//! *weight*. Two M9 knobs are neither: `obligation_check` (M9 T2.1) changes
//! what a turn does **after** the draft comes back, and `summary_guidelines`
//! (M9 T5.2) changes what the summarizer is **told**. Both ship off. The M9
//! rule for moving one is the same rule: *a knob moves off its default only
//! on a non-regressing verbatim arm* — so both arms print on and off side by
//! side over the thirty scripted sessions and the ten abilities, and neither
//! decides anything.
//!
//! **This is a report, not a gate.** `ns-app eval --obligations` and
//! `--guidelines` exit 0 whatever the numbers are, for [`crate::ablate`]'s
//! reason: a red exit code makes a measurement something to avoid taking.

use crate::eval::{run_all_for, summarizer_honours_guidelines, Ability, Run};
use crate::fixtures::{self, Fixture};

/// Three guidelines written from what the fixtures' summaries actually get
/// wrong, not from what a summarizer might get wrong in general (M10 T5.4,
/// arm 4).
///
/// Read off `ScriptedSummarizer`'s output on these seeds. It emits
/// `topic = "scripted summary of turns {first}-{last}"`, `established` =
/// every user turn copied verbatim, `open = []`. So:
///
/// 1. every knowledge-update seed restates one key with a new value
///    (`user.name` Martin → Marek), and both turns land in `established`
///    unordered — the summary does not say which one is current;
/// 2. the asking turns are verbatim user text (`"remind me who I am"`), so
///    the entity the session is about never appears in its own summary;
/// 3. `open` is always empty, so an unanswered question — which is what the
///    abstention arm's turn is — leaves no trace in the summary at all.
///
/// Written down here rather than in the config so the arm is reproducible;
/// `[memory] summary_guidelines` is where a decision would land.
pub const GUIDELINES: [&str; 3] = [
    "When a fact is restated with a new value, keep only the newest value and say when it changed.",
    "Name entities in full; never carry a pronoun or a bare \"me\" out of the user's words.",
    "List every question left unanswered, verbatim, under open.",
];

/// One knob's two arms, over both suites.
#[derive(Debug, Clone)]
pub struct Knob {
    /// The knob, spelled as its config key.
    pub knob: &'static str,
    pub off_label: String,
    pub on_label: String,
    pub off: Vec<Ability>,
    pub on: Vec<Ability>,
    pub off_fixtures: Vec<Fixture>,
    pub on_fixtures: Vec<Fixture>,
    /// `Some(reason)` when the offline substrate cannot see this knob at all.
    ///
    /// The distinction [`crate::ablate::Report::fixtures_carry_the_block`]
    /// draws, at the knob level: a zero delta from a suite that could have
    /// seen the knob is a measurement, and a zero delta from one that could
    /// not is nothing, and reporting them the same way is how a default gets
    /// confirmed by an instrument that was switched off.
    pub not_measurable: Option<&'static str>,
}

impl Knob {
    fn passed(rows: &[Ability]) -> usize {
        rows.iter().filter(|r| r.passed).count()
    }

    /// Abilities gained (positive) or lost (negative) by turning it on.
    pub fn ability_delta(&self) -> i64 {
        Self::passed(&self.on) as i64 - Self::passed(&self.off) as i64
    }

    pub fn fixture_delta(&self) -> i64 {
        let p = |r: &[Fixture]| r.iter().filter(|x| x.passed).count() as i64;
        p(&self.on_fixtures) - p(&self.off_fixtures)
    }

    pub fn abstention_delta(&self) -> i64 {
        let d = |r: &[Fixture]| r.iter().filter(|x| x.declined).count() as i64;
        d(&self.on_fixtures) - d(&self.off_fixtures)
    }

    /// The regeneration cost, in replier calls over the thirty fixtures.
    pub fn replier_delta(&self) -> i64 {
        fixtures::replier_requests(&self.on_fixtures).0 as i64
            - fixtures::replier_requests(&self.off_fixtures).0 as i64
    }

    /// The M9 rule, as a predicate: a knob may move off its default only if
    /// no graded arm regressed. It does not say the knob *should* move —
    /// that is the parent's call, and a zero-gain arm clears this too.
    pub fn non_regressing(&self) -> bool {
        self.ability_delta() >= 0 && self.fixture_delta() >= 0 && self.abstention_delta() >= 0
    }
}

/// Both arms of `obligation_check` (M9 T2.1): off, then on.
///
/// Sequential for [`run_all_for`]'s reason — `requests` is a reported column
/// and this arm's whole finding is a request count.
pub async fn measure_obligations() -> Knob {
    let arm = |on| Run {
        obligation_check: on,
        ..Run::default()
    };
    Knob {
        knob: "obligation_check",
        off_label: "off".into(),
        on_label: "on".into(),
        off: run_all_for(arm(false)).await,
        on: run_all_for(arm(true)).await,
        off_fixtures: fixtures::run_all_for(arm(false)).await,
        on_fixtures: fixtures::run_all_for(arm(true)).await,
        not_measurable: None,
    }
}

/// Both arms of `summary_guidelines` (M9 T5.2): empty, then [`GUIDELINES`].
///
/// The plumbing runs for real — the guidelines reach
/// `crate::eval::summarizer_double`, which is the one line a live summarizer
/// is swapped into — and the arm still reports *not measurable*, because the
/// double the fixtures run discards them before they can reach a prompt. The
/// numbers are printed anyway so the claim can be checked rather than taken:
/// an arm that claimed blindness and quietly moved would be worse than one
/// that moved.
pub async fn measure_guidelines() -> Knob {
    let arm = |on: bool| Run {
        summary_guidelines: if on { &GUIDELINES } else { &[] },
        ..Run::default()
    };
    Knob {
        knob: "summary_guidelines",
        off_label: "empty".into(),
        on_label: "3 lines".into(),
        off: run_all_for(arm(false)).await,
        on: run_all_for(arm(true)).await,
        off_fixtures: fixtures::run_all_for(arm(false)).await,
        on_fixtures: fixtures::run_all_for(arm(true)).await,
        not_measurable: (!summarizer_honours_guidelines()).then_some(
            "the fixtures run nsengine::script::ScriptedSummarizer, which builds its draft \
             from the records alone and never renders a system prompt; guidelines reach a \
             summarizer only through LlmSummarizer::with_guidelines, so both arms summarize \
             byte-identically by construction. Measurable on a live summarizer, not offline.",
        ),
    }
}

fn ability_row(cells: [&str; 4]) -> String {
    format!(
        "  {:<24}  {:<10}  {:<10}  {:>5}\n",
        cells[0], cells[1], cells[2], cells[3]
    )
}

/// The two-arm table for one knob: ten abilities, then thirty fixtures.
pub fn render(k: &Knob) -> String {
    let mut out = format!(
        "\nM10 T5.4 — `{}` on vs off (M9 default: {})\n\n",
        k.knob, k.off_label
    );
    out.push_str(&ability_row([
        "ability",
        &k.off_label,
        &k.on_label,
        "delta",
    ]));
    out.push_str(&ability_row([
        "------------------------",
        "----------",
        "----------",
        "-----",
    ]));
    for (f, a) in k.off.iter().zip(&k.on) {
        let cell = |p: bool| if p { "1/1 ok" } else { "0/1 FAIL" }.to_string();
        out.push_str(&ability_row([
            f.ability,
            &cell(f.passed),
            &cell(a.passed),
            &format!("{:+}", a.passed as i64 - f.passed as i64),
        ]));
    }
    out.push_str(&format!(
        "\n  {}/{} → {}/{} abilities ({:+}).\n",
        Knob::passed(&k.off),
        k.off.len(),
        Knob::passed(&k.on),
        k.on.len(),
        k.ability_delta()
    ));
    out.push_str(&format!(
        "\n  over {} scripted sessions:\n\n",
        k.off_fixtures.len()
    ));
    out.push_str(&fixtures::render_arms(
        &k.off_label,
        &k.off_fixtures,
        &k.on_label,
        &k.on_fixtures,
    ));
    match k.not_measurable {
        Some(reason) => out.push_str(&format!(
            "\n  NOT MEASURABLE OFFLINE — {reason}\n  \
             The plumbing stays: `Run.summary_guidelines` is honoured by the harness's \
             summarizer choice, so the arm reads itself the day a live summarizer is \
             wired in. The numbers above are the double's, and mean nothing about the knob.\n"
        )),
        None => out.push_str(&format!(
            "\n  verdict: {} — abilities {:+}, answerable {:+}, abstention {:+}, \
             replier requests {:+}. {}\n",
            if k.non_regressing() {
                "non-regressing"
            } else {
                "REGRESSES"
            },
            k.ability_delta(),
            k.fixture_delta(),
            k.abstention_delta(),
            k.replier_delta(),
            if k.replier_delta() == 0 && k.fixture_delta() == 0 && k.ability_delta() == 0 {
                "The two arms read identically: on this substrate the scripted replier \
                 leaves no obligation unaddressed that the interceptor can find, so the \
                 regeneration never fires and the knob costs and buys nothing here."
            } else {
                "Reported, not decided: the M9 rule leaves the move to the caller."
            }
        )),
    }
    out.push_str(
        "  both arms are the same fixtures against the same scripted doubles; \
                  nothing here spends a request.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arm 3's finding, whichever way it falls: the obligations knob is
    /// measured on both suites, and the replier count — which *is* the knob's
    /// cost — is reported per fixture.
    ///
    /// The assertion is the M9 rule, not a number: if the scripted replier
    /// ever starts leaving a clause unaddressed the deltas move, and the test
    /// still passes as long as nothing regressed. A regression is what the
    /// rule forbids, and is the one outcome worth failing on.
    #[tokio::test]
    async fn the_obligations_arm_reports_both_suites_and_the_replier_cost() {
        let k = measure_obligations().await;
        let t = render(&k);
        assert_eq!(k.off_fixtures.len(), 30, "{t}");
        assert_eq!(Knob::passed(&k.off), k.off.len(), "off arm not green:\n{t}");
        assert!(
            k.non_regressing(),
            "turning obligation_check on regressed a graded arm:\n{t}"
        );
        assert!(t.contains("`obligation_check` on vs off"), "{t}");
        assert!(t.contains("replier requests:"), "{t}");
        assert!(t.contains("verdict:"), "{t}");
    }

    /// Arm 4's finding: the substrate cannot see this knob, and the report
    /// says so instead of printing a zero delta as if it were a measurement.
    ///
    /// Both halves, as [`crate::ablate`]'s blindness test does it: the double
    /// really is guideline-blind, **and** the renderer really does refuse to
    /// call the arm a result.
    #[tokio::test]
    async fn the_guidelines_arm_is_reported_not_measurable_rather_than_zero() {
        let k = measure_guidelines().await;
        let t = render(&k);
        assert!(
            !summarizer_honours_guidelines(),
            "the double now honours guidelines — this arm is measurable and the \
             report must stop saying it is not"
        );
        assert!(k.not_measurable.is_some(), "{t}");
        assert!(t.contains("NOT MEASURABLE OFFLINE"), "{t}");
        assert!(
            !t.contains("verdict:"),
            "a blind arm must not print a verdict:\n{t}"
        );
        // And the blindness is real, not asserted: identical arms end to end.
        assert_eq!(k.fixture_delta(), 0, "{t}");
        assert_eq!(k.ability_delta(), 0, "{t}");
        assert_eq!(
            k.off_fixtures
                .iter()
                .map(|f| f.summary_chars)
                .collect::<Vec<_>>(),
            k.on_fixtures
                .iter()
                .map(|f| f.summary_chars)
                .collect::<Vec<_>>(),
            "the summaries differ between the arms — the double is not blind after all:\n{t}"
        );
    }

    /// Three guidelines, each one a sentence about a fault these seeds have.
    #[test]
    fn the_guidelines_are_three_hand_written_lines() {
        assert_eq!(GUIDELINES.len(), 3);
        for g in GUIDELINES {
            assert!(g.ends_with('.'), "{g}");
            assert!(g.len() > 30, "{g}");
        }
    }
}
