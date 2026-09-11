//! M9 T0.4 — what one context block is worth, measured by removing it.
//!
//! "Ablation as the arbiter" (plan §7.4 S8): a block earns its tokens only if
//! the suite is worse without it. So the ability set runs twice over the same
//! scripted doubles — once whole, once with one block blanked on every model
//! call — and the difference is the block's marginal effect.
//!
//! Modelled on [`crate::paraphrase`]'s two arms, and for the same reason: a
//! single number is not a measurement. The arm that was not ablated is
//! printed beside the one that was, so a run that was already failing cannot
//! be read as a block doing nothing.
//!
//! **This is a report, not a gate.** `ns-app eval --ablate facts` exits 0
//! whatever the delta is. A block with no measurable effect is a finding —
//! permission to drop it, or a sign the scripted suite cannot see it — and a
//! red exit code would make the measurement something to avoid taking.

use crate::eval::{run_all_for, Ability, Run};
use crate::fixtures::{self, Fixture};
use nscore::Ablate;

/// One arm's result over the whole ability set.
///
/// The counts are read off the [`Ability`] rows rather than recomputed here:
/// `passed` is the suite's own verdict and the arm must not hold the
/// fixtures to a second standard.
#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    /// "full" or the blanked block's name.
    pub arm: String,
    pub passed: usize,
    pub total: usize,
    /// Abilities this arm failed, so a delta can be argued with rather than
    /// only reported.
    pub failed: Vec<String>,
}

impl Arm {
    pub fn of(arm: impl Into<String>, rows: &[Ability]) -> Self {
        Self {
            arm: arm.into(),
            passed: rows.iter().filter(|r| r.passed).count(),
            total: rows.len(),
            failed: rows
                .iter()
                .filter(|r| !r.passed)
                .map(|r| r.ability.to_string())
                .collect(),
        }
    }

    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.passed as f64 / self.total as f64
    }
}

/// Both arms for one block.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The block that was blanked. Named in the output because "the delta"
    /// without its block is not a number.
    pub block: Ablate,
    pub full: Vec<Ability>,
    pub ablated: Vec<Ability>,
    /// M10 T5.1: the same two arms over the scripted sessions, which is the
    /// half of this report that can see `summary` and `guidance` at all.
    ///
    /// M9 §Results T0.4 read 9/9 → 9/9 for both blocks and could not say
    /// whether that was a block doing nothing or a suite carrying nothing.
    /// These rows carry a summary and two rendered notes each and print both
    /// counts, so the next reader is never left with that question.
    pub full_fixtures: Vec<Fixture>,
    pub ablated_fixtures: Vec<Fixture>,
}

impl Report {
    pub fn full_arm(&self) -> Arm {
        Arm::of("full", &self.full)
    }

    pub fn ablated_arm(&self) -> Arm {
        Arm::of(block_name(self.block), &self.ablated)
    }

    /// Abilities the ablation cost, i.e. passing whole and failing blanked.
    ///
    /// Positional, not by name: [`run_all_for`] returns a fixed order
    /// and `crates/testkit/src/eval.rs` has a test that guards it, so the two
    /// arms line up row by row.
    pub fn lost(&self) -> Vec<&'static str> {
        self.full
            .iter()
            .zip(&self.ablated)
            .filter(|(f, a)| f.passed && !a.passed)
            .map(|(f, _)| f.ability)
            .collect()
    }

    /// Passes lost overall: negative when the block was carrying the suite,
    /// zero when blanking it changed nothing.
    pub fn delta(&self) -> i64 {
        self.ablated_arm().passed as i64 - self.full_arm().passed as i64
    }

    /// The same number over the scripted sessions' answerable arm (M10 T5.1).
    pub fn fixture_delta(&self) -> i64 {
        let passed = |rows: &[Fixture]| rows.iter().filter(|r| r.passed).count() as i64;
        passed(&self.ablated_fixtures) - passed(&self.full_fixtures)
    }

    /// And over the abstention arm, kept separate for 2606.09376's reason:
    /// folded into one rate, a block that made the harness decline more would
    /// look like a block that made it answer better.
    pub fn abstention_delta(&self) -> i64 {
        let declined = |rows: &[Fixture]| rows.iter().filter(|r| r.declined).count() as i64;
        declined(&self.ablated_fixtures) - declined(&self.full_fixtures)
    }

    /// Whether the suite could have seen this block at all: the sessions the
    /// **full** arm ran carried one.
    ///
    /// This is the question M9 could not answer. A zero delta with this true
    /// is a measured zero; a zero delta with it false is blindness.
    pub fn fixtures_carry_the_block(&self) -> bool {
        match self.block {
            Ablate::Summary => self.full_fixtures.iter().all(|r| r.summary_shown > 0),
            Ablate::Guidance => self.full_fixtures.iter().all(|r| r.notes_shown >= 2),
            // `facts` was never blind: M9 measured 9/9 → 4/9 with it.
            Ablate::Facts => !self.full_fixtures.is_empty(),
        }
    }
}

/// The CLI spelling of a block, and the column heading. One function so the
/// argument parser, the heading and the ledger cannot drift apart.
pub fn block_name(block: Ablate) -> &'static str {
    match block {
        Ablate::Facts => "facts",
        Ablate::Summary => "summary",
        Ablate::Guidance => "guidance",
    }
}

/// `--ablate <block>`'s argument, or `None` if it names no block.
pub fn parse_block(name: &str) -> Option<Ablate> {
    match name {
        "facts" => Some(Ablate::Facts),
        "summary" => Some(Ablate::Summary),
        "guidance" => Some(Ablate::Guidance),
        _ => None,
    }
}

/// Run the ability set twice: whole, then with `block` blanked.
///
/// Sequential, and for [`run_all_for`]'s own reason — every fixture
/// builds its own store and engines, but `requests` and `peak_chars` are the
/// columns a release is compared on and a concurrent run would let the
/// scheduler into them.
pub async fn measure(block: Ablate, activation_weight: f32) -> Report {
    let arm = |ablate| Run {
        ablate,
        activation_weight,
        ..Run::default()
    };
    let full = run_all_for(arm(None)).await;
    let ablated = run_all_for(arm(Some(block))).await;
    // M10 T5.1, in the same two arms and the same order, so the fixture
    // section of the table is read the way the ability section is.
    let full_fixtures = fixtures::run_all_for(arm(None)).await;
    let ablated_fixtures = fixtures::run_all_for(arm(Some(block))).await;
    Report {
        block,
        full,
        ablated,
        full_fixtures,
        ablated_fixtures,
    }
}

fn row_line(cells: [&str; 4]) -> String {
    format!(
        "  {:<24}  {:<8}  {:<8}  {:>5}\n",
        cells[0], cells[1], cells[2], cells[3]
    )
}

/// The marginal-effect table for one block.
pub fn render(r: &Report) -> String {
    let block = block_name(r.block);
    let mut out = format!("\nM9 T0.4 — marginal effect of the `{block}` block\n\n");
    out.push_str(&row_line([
        "ability",
        "full",
        &format!("no {block}"),
        "delta",
    ]));
    out.push_str(&row_line([
        "------------------------",
        "--------",
        "--------",
        "-----",
    ]));
    for (f, a) in r.full.iter().zip(&r.ablated) {
        let cell = |p: bool| if p { "1/1 ok" } else { "0/1 FAIL" }.to_string();
        let d = a.passed as i64 - f.passed as i64;
        out.push_str(&row_line([
            f.ability,
            &cell(f.passed),
            &cell(a.passed),
            &format!("{d:+}"),
        ]));
    }
    let (full, abl) = (r.full_arm(), r.ablated_arm());
    out.push_str(&format!(
        "\n  {}/{} → {}/{} abilities pass without `{block}` ({:+}).\n",
        full.passed,
        full.total,
        abl.passed,
        abl.total,
        r.delta()
    ));
    let lost = r.lost();
    if lost.is_empty() {
        out.push_str(&format!(
            "  no ability changed state: on this scripted suite `{block}` has no \
             measurable marginal effect.\n"
        ));
    } else {
        out.push_str(&format!("  lost without `{block}`: {}\n", lost.join(", ")));
    }
    for a in r.ablated.iter().filter(|a| !a.passed) {
        out.push_str(&format!("  {} FAILED: {}\n", a.ability, a.detail));
    }
    out.push_str(&render_fixtures(r));
    out
}

/// The scripted-session half of the same report (M10 T5.1).
///
/// Counts rather than rows: thirty fixtures times two arms is not a table
/// anybody reads, and the two numbers that matter are the delta and whether
/// the full arm carried the block at all.
pub fn render_fixtures(r: &Report) -> String {
    let block = block_name(r.block);
    let (full, ablated) = (&r.full_fixtures, &r.ablated_fixtures);
    if full.is_empty() {
        return String::new();
    }
    let passed = |rows: &[Fixture]| rows.iter().filter(|x| x.passed).count();
    let declined = |rows: &[Fixture]| rows.iter().filter(|x| x.declined).count();
    let summaries: usize = full.iter().filter(|x| x.summary_chars > 0).count();
    let notes: usize = full.iter().filter(|x| x.notes_held >= 2).count();
    let mut out = format!(
        "\n  M10 T5.1 — the same block over {} scripted sessions from hand-authored seeds\n\n\
         \x20 answerable   {}/{} → {}/{} ({:+})\n\
         \x20 abstention   {}/{} → {}/{} ({:+})\n\
         \x20 carried      {summaries}/{} sessions hold a summary in the log, \
         {notes}/{} hand the engine two reply notes\n",
        full.len(),
        passed(full),
        full.len(),
        passed(ablated),
        ablated.len(),
        r.fixture_delta(),
        declined(full),
        full.len(),
        declined(ablated),
        ablated.len(),
        r.abstention_delta(),
        full.len(),
        full.len(),
    );
    if r.fixture_delta() == 0 {
        out.push_str(&format!(
            "  a zero delta here is {}: the full arm's sessions {} carry `{block}`.\n",
            if r.fixtures_carry_the_block() {
                "a measurement"
            } else {
                "BLINDNESS, not a finding"
            },
            if r.fixtures_carry_the_block() {
                "do"
            } else {
                "do NOT"
            },
        ));
    } else {
        let mut lost: Vec<&str> = full
            .iter()
            .zip(ablated)
            .filter(|(f, a)| f.passed && !a.passed)
            .map(|(f, _)| f.id)
            .collect();
        lost.truncate(6);
        out.push_str(&format!(
            "  lost without `{block}` (first {}): {}\n",
            lost.len(),
            lost.join(", ")
        ));
        if let Some(one) = ablated.iter().find(|a| !a.passed) {
            out.push_str(&format!("  {} FAILED: {}\n", one.id, one.detail));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The finding this task exists to produce: blanking `facts` costs the
    /// suite exactly the abilities whose pass condition is a fact in front of
    /// the model, and costs the others nothing.
    ///
    /// Named abilities rather than a bare "the delta is negative": a drop
    /// that moved the desktop rows would mean the blanking reached machinery
    /// it has no business in, and a report that only counted passes could not
    /// tell the two apart.
    #[tokio::test]
    async fn blanking_facts_lowers_the_fact_dependent_abilities_only() {
        let r = measure(Ablate::Facts, 0.0).await;
        assert_eq!(
            r.full_arm().failed,
            Vec::<String>::new(),
            "the full arm must be green or the delta means nothing:\n{}",
            render(&r)
        );
        assert_eq!(
            r.lost(),
            FACT_DEPENDENT,
            "blanking facts must cost exactly the fact-dependent abilities:\n{}",
            render(&r)
        );
        assert_eq!(r.delta(), -(FACT_DEPENDENT.len() as i64));
    }

    /// The abilities whose graded condition is a fact line in the reply
    /// context. Written down rather than derived, so the day one of them
    /// stops depending on facts the test says so instead of quietly agreeing.
    ///
    /// Five of the six memory fixtures. `abstention` is the sixth and holds:
    /// its pass condition is that nothing was invented about a fact the
    /// harness never had, and blanking the block cannot make that less true.
    /// The three desktop tasks are graded on the emitter's trace lines, which
    /// carry no facts, and hold for that reason.
    const FACT_DEPENDENT: [&str; 5] = [
        "information extraction",
        "multi-session reasoning",
        "temporal reasoning",
        "knowledge updates",
        "selective forgetting",
    ];

    /// Every row of both arms, one delta column, and the totals — the
    /// deliverable is the table, so the table is graded.
    #[test]
    fn the_table_has_a_row_per_ability_and_a_delta_column() {
        let row = |ability, passed| Ability {
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
            clipped_chars: 0,
            inspections: 0,
            budget_drops: 0,
            escalations: 0,
            target_action: "remember_fact",
            target_proposed: passed,
            legal_size: 8,
            bits: crate::eval::bits_over_random(passed, 8),
            detail: if passed {
                String::new()
            } else {
                "user.name absent from the context".into()
            },
        };
        let r = Report {
            block: Ablate::Facts,
            full: vec![row("information extraction", true), row("abstention", true)],
            ablated: vec![
                row("information extraction", false),
                row("abstention", true),
            ],
            full_fixtures: vec![],
            ablated_fixtures: vec![],
        };
        let t = render(&r);
        assert!(
            t.contains("ability                   full      no facts"),
            "{t}"
        );
        assert!(
            t.contains("information extraction    1/1 ok    0/1 FAIL     -1"),
            "{t}"
        );
        assert!(
            t.contains("abstention                1/1 ok    1/1 ok       +0"),
            "{t}"
        );
        assert!(
            t.contains("2/2 → 1/2 abilities pass without `facts` (-1)."),
            "{t}"
        );
        assert!(
            t.contains("lost without `facts`: information extraction"),
            "{t}"
        );
        assert!(
            t.contains("information extraction FAILED: user.name absent from the context"),
            "a lost ability must name its condition:\n{t}"
        );
        assert_eq!(r.delta(), -1);
        assert_eq!(r.lost(), vec!["information extraction"]);
    }

    /// **The finding M9 could not take** (M10 T5.1, and P5's exit criterion).
    ///
    /// M9 §Results T0.4: *"`summary` and `guidance`: 9/9 → 9/9, not
    /// measurable — the scripted suite carries no summary and no notes."*
    /// Both halves are asserted here, because either alone would let the
    /// blindness back: the full arm's sessions have to **carry** the block,
    /// and blanking it has to **cost** them.
    #[tokio::test]
    async fn the_summary_ablation_arm_is_no_longer_blind() {
        for block in [Ablate::Summary, Ablate::Guidance] {
            let r = measure(block, 0.0).await;
            let table = render(&r);
            assert!(
                r.fixtures_carry_the_block(),
                "the full arm does not carry `{}` — this is M9's blindness again:\n{table}",
                block_name(block)
            );
            assert_eq!(
                r.full_fixtures.iter().filter(|f| f.passed).count(),
                r.full_fixtures.len(),
                "the full arm must be green or the delta means nothing:\n{table}"
            );
            assert!(
                r.fixture_delta() < 0,
                "blanking `{}` cost the scripted sessions nothing:\n{table}",
                block_name(block)
            );
            // Abstention is graded on its own arm, and neither block is what
            // makes a reply decline: a delta there would mean the blanking
            // reached the grounding interceptor, which is not its business.
            assert_eq!(r.abstention_delta(), 0, "{table}");
            assert!(table.contains("M10 T5.1 —"), "{table}");
            assert!(table.contains("answerable"), "{table}");
        }
    }

    #[test]
    fn only_the_three_blocks_parse() {
        assert_eq!(parse_block("facts"), Some(Ablate::Facts));
        assert_eq!(parse_block("summary"), Some(Ablate::Summary));
        assert_eq!(parse_block("guidance"), Some(Ablate::Guidance));
        assert_eq!(parse_block("obligations"), None);
        assert_eq!(parse_block(""), None);
        for b in [Ablate::Facts, Ablate::Summary, Ablate::Guidance] {
            assert_eq!(parse_block(block_name(b)), Some(b));
        }
    }
}
