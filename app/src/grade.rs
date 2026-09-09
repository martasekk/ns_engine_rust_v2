//! `ns-app grade [--local] [--split dev|held|all]` — what an evaluator is
//! worth, measured (M8 T2.2, T2.3, T2.7).
//!
//! The corpus is `nstestkit::grading`: a hundred labelled turns, half Czech,
//! half English, split development and held-out. This command runs an
//! evaluator over it and prints three things, in this order, because they are
//! only readable in this order:
//!
//! 1. **What each labelled kind cost.** A κ says how much two raters agree; it
//!    does not say that the disagreement is all one class. Here it usually is.
//! 2. **Agreement against the labels**, with the contingency, both positive
//!    rates, Gwet's AC1 and PABAK beside κ — never κ alone
//!    (`nsevolution::kappa`).
//! 3. **Agreement between the evaluators**, which is the number T2.7's gate is
//!    written in, and which means nothing without the two above: a local
//!    scorer that disagrees with the symbolic proxies might be worse than
//!    them, or better where they are blind, and only the labels separate those.
//!
//! **The split is the discipline, not a formality.** `--split dev` is where a
//! cut-off may be chosen; `--split held` is the only number worth quoting. The
//! 2026 cross-dataset audit
//! (`docs/research/2026-09-09-local-evaluator-findings.md` §1) measured 0.172
//! AUROC of regret for metrics picked without exactly this separation.
//!
//! No key, no network for the symbolic arm; `--local` dials nsmodels on
//! loopback and degrades to `unavailable` rows if it is not there.

use nsevolution::evaluate::SymbolicEvaluator;
use nsevolution::kappa::Agreement;
use nsevolution::local::RawScores;
use nsevolution::local::{LocalConfig, LocalEvaluator};
use nstestkit::grading::{corpus, run, GradeRun, Split};

pub struct Args {
    pub local: bool,
    pub split: Option<Split>,
    /// Choose the local scorer's two cuts, on the development half only.
    pub sweep: bool,
}

pub fn parse_args(rest: &[String]) -> Result<Args, String> {
    let mut out = Args {
        local: false,
        split: Some(Split::Held),
        sweep: false,
    };
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--local" => out.local = true,
            // A sweep fits, so it is pinned to the development half and
            // cannot be pointed anywhere else. `--split held --sweep` is not
            // a mistake worth supporting; it is the mistake the split exists
            // to prevent.
            "--sweep" => {
                out.sweep = true;
                out.local = true;
                out.split = Some(Split::Dev);
            }
            "--split" => {
                i += 1;
                out.split = match rest.get(i).map(String::as_str) {
                    Some("dev") => Some(Split::Dev),
                    Some("held") => Some(Split::Held),
                    Some("all") => None,
                    other => {
                        return Err(format!(
                            "usage: ns-app grade [--local] [--sweep] [--split dev|held|all] (got {other:?})"
                        ))
                    }
                };
            }
            other => {
                return Err(format!(
                    "usage: ns-app grade [--local] [--sweep] [--split dev|held|all] (got {other:?})"
                ))
            }
        }
        i += 1;
    }
    Ok(out)
}

/// The head-to-head block. Printed only when there are two runs to compare.
fn render_pair(local: &GradeRun, symbolic: &GradeRun) -> String {
    let mut out = String::new();
    let a = local.against(symbolic);
    out.push_str(&format!("\nlocal vs symbolic: {}\n", a.scores()));
    // The cases that moved are the argument; the summary is only the headline.
    let mut better = Vec::new();
    let mut worse = Vec::new();
    for (c, i) in local.answered() {
        if let Some((_, j)) = symbolic.answered().find(|(o, _)| o.id == c.id) {
            let want = c.gold.is_problem();
            match (i.is_problem() == want, j.is_problem() == want) {
                (true, false) => better.push(c.id),
                (false, true) => worse.push(c.id),
                _ => {}
            }
        }
    }
    out.push_str(&format!(
        "local right where symbolic was wrong ({}):\n",
        better.len()
    ));
    for id in &better {
        out.push_str(&format!("  + {id}\n"));
    }
    out.push_str(&format!(
        "local wrong where symbolic was right ({}):\n",
        worse.len()
    ));
    for id in &worse {
        out.push_str(&format!("  - {id}\n"));
    }
    out
}

/// Collect the raw scores once, then score every candidate cut by arithmetic.
///
/// **Development half only**, and the flag enforces it. Choosing a threshold
/// on the data a number is then quoted from is the exact failure the 2026
/// transfer audit measured at 0.172 AUROC of regret; the whole value of the
/// split is that it makes that impossible rather than discouraged.
async fn sweep(local: &LocalEvaluator) -> i32 {
    let cases: Vec<_> = corpus().iter().filter(|c| c.split == Split::Dev).collect();
    let mut raw: Vec<(&'static str, bool, RawScores)> = Vec::new();
    for c in &cases {
        let view = nsevolution::evaluate::TurnView {
            user: c.user,
            shown: c.shown,
            acted: c.acted,
            task_tier: c.task_tier,
            reply: c.reply,
            next_user: c.next_user,
        };
        match local.raw(&view).await {
            Ok(r) => raw.push((c.id, c.gold.is_problem(), r)),
            Err(e) => {
                eprintln!("sweep: {} could not be scored ({e}); aborting", c.id);
                return 0;
            }
        }
    }

    // What the scores actually look like, before any cut. A grid whose best
    // point sits at an edge is a grid that was drawn in the wrong place, and
    // this is how that becomes visible.
    let mut cos: Vec<f32> = raw
        .iter()
        .filter_map(|(_, _, r)| r.follow_up_cosine)
        .collect();
    let mut rel: Vec<f32> = raw.iter().map(|(_, _, r)| r.relevance).collect();
    cos.sort_by(|a, b| a.partial_cmp(b).unwrap());
    rel.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |v: &[f32], q: f32| -> f32 {
        if v.is_empty() {
            return f32::NAN;
        }
        v[((v.len() - 1) as f32 * q).round() as usize]
    };
    println!(
        "follow-up cosine   min {:.3}  p25 {:.3}  median {:.3}  p75 {:.3}  max {:.3}  (n={})",
        pct(&cos, 0.0),
        pct(&cos, 0.25),
        pct(&cos, 0.5),
        pct(&cos, 0.75),
        pct(&cos, 1.0),
        cos.len()
    );
    println!(
        "relevance          min {:.3}  p25 {:.3}  median {:.3}  p75 {:.3}  max {:.3}  (n={})",
        pct(&rel, 0.0),
        pct(&rel, 0.25),
        pct(&rel, 0.5),
        pct(&rel, 0.75),
        pct(&rel, 1.0),
        rel.len()
    );

    // The candidate cuts are the observed values themselves, not a grid
    // somebody drew.
    //
    // The first version of this used fixed grids — cosine 0.60–0.99 and
    // relevance −12…8, the latter written on the assumption that a
    // cross-encoder emits logits. It emits probabilities: the observed
    // relevance range is 0.000 to 0.999, so every point on that grid switched
    // the signal off, and the sweep dutifully reported its best result at the
    // grid's edge. A threshold sweep has exactly one correct candidate set —
    // the values that actually occur, plus one below all of them — and using
    // it removes the whole class of mistake.
    let cut_candidates = |mut v: Vec<f32>| -> Vec<f32> {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.dedup();
        // One cut below everything, so "never fires" stays reachable, and the
        // observed values themselves, each of which is the smallest cut that
        // includes it.
        let mut out = vec![v.first().copied().unwrap_or(0.0) - 1.0];
        out.extend(v);
        out
    };
    let cos_grid = cut_candidates(cos.clone());
    let rel_grid = cut_candidates(rel.clone());
    let mut best: Option<(f64, f32, f32)> = None;
    for &rc in &cos_grid {
        for &rv in &rel_grid {
            let got: Vec<bool> = raw
                .iter()
                .map(|(_, _, r)| r.issue_at(rc, rv).is_problem())
                .collect();
            let gold: Vec<bool> = raw.iter().map(|(_, g, _)| *g).collect();
            let k = Agreement::tally(&got, &gold).scores().kappa;
            if best.is_none_or(|(bk, _, _)| k > bk) {
                best = Some((k, rc, rv));
            }
        }
    }
    match best {
        Some((k, rc, rv)) => {
            let kappa_at = |rc: f32, rv: f32| -> f64 {
                let got: Vec<bool> = raw
                    .iter()
                    .map(|(_, _, r)| r.issue_at(rc, rv).is_problem())
                    .collect();
                let gold: Vec<bool> = raw.iter().map(|(_, g, _)| *g).collect();
                Agreement::tally(&got, &gold).scores().kappa
            };
            // Sentinels outside any observed value, so a signal can be turned
            // off without pretending some cut does it.
            let (cos_off, rel_off) = (2.0_f32, -1.0_f32);
            let fires_cos = raw
                .iter()
                .filter(|(_, _, r)| !r.lexical_reask && r.follow_up_cosine.is_some_and(|c| c >= rc))
                .count();
            let fires_rel = raw.iter().filter(|(_, _, r)| r.relevance < rv).count();

            // Printed at full precision, not rounded: a cross-encoder score can be
            // 4e-4 and a cut of "0.000" would be a different setting than the one
            // that was measured.
            println!("\nbest on dev: kappa={k:.3} at reask_cosine={rc:e} relevance_cut={rv:e}");
            println!(
                "  the embedder adds {fires_cos} re-ask(s) the lexical band missed; \
                 the cross-encoder calls {fires_rel} repl(ies) off-topic"
            );

            // Does each signal earn its place? Turn one off at a time and see.
            // A signal whose removal does not move kappa is a dependency being
            // paid for with nothing bought — and on a service that has to be
            // running, that is a real cost.
            println!("ablation, at the same cuts:");
            println!("  both signals            kappa={:.3}", kappa_at(rc, rv));
            println!(
                "  embedder only           kappa={:.3}",
                kappa_at(rc, rel_off)
            );
            println!(
                "  cross-encoder only      kappa={:.3}",
                kappa_at(cos_off, rv)
            );
            println!(
                "  neither (lexical only)  kappa={:.3}",
                kappa_at(cos_off, rel_off)
            );
            println!(
                "\nput the cuts worth keeping in [models] and re-run \
                 `ns-app grade --local --split held` for the number worth quoting"
            );
        }
        None => println!("nothing to sweep"),
    }
    0
}

pub async fn run_cmd(
    args: &Args,
    base_url: &str,
    timeout_ms: u64,
    reask_cosine: f32,
    relevance_cut: f32,
) -> i32 {
    let symbolic = SymbolicEvaluator::default();
    let sym_run = run(&symbolic, args.split).await;
    println!("{sym_run}\n");

    if !args.local {
        return 0;
    }

    let local = LocalEvaluator::new(
        LocalConfig {
            base_url: base_url.into(),
            timeout_ms,
            reask_cosine,
            relevance_cut,
        },
        SymbolicEvaluator::default(),
    );
    if args.sweep {
        return sweep(&local).await;
    }
    let local_run = run(&local, args.split).await;
    println!("{local_run}");
    if local_run.unavailable() == local_run.cases.len() {
        eprintln!(
            "\nthe local scorer answered nothing — is nsmodels serving on {base_url}? \
             (`nsmodels serve --model quality --rerank`)"
        );
        // Not a failure of the harness. A lane that degrades is the designed
        // behaviour (M8 §2), and exiting non-zero here would make an absent
        // optional service break a build.
        return 0;
    }
    print!("{}", render_pair(&local_run, &sym_run));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_held_out_half_and_the_symbolic_arm() {
        let a = parse_args(&[]).unwrap();
        assert!(!a.local);
        assert!(!a.sweep);
        assert_eq!(a.split, Some(Split::Held));
    }

    /// A sweep fits, so it cannot be aimed at the half the number is quoted
    /// from — not by accident and not on purpose.
    #[test]
    fn a_sweep_is_pinned_to_the_development_half() {
        let a = parse_args(&["--sweep".into()]).unwrap();
        assert!(a.sweep && a.local);
        assert_eq!(a.split, Some(Split::Dev));
        let b = parse_args(&["--split".into(), "held".into(), "--sweep".into()]).unwrap();
        assert_eq!(
            b.split,
            Some(Split::Dev),
            "--sweep overrides a later target"
        );
    }

    #[test]
    fn splits_parse_and_nonsense_does_not() {
        assert_eq!(
            parse_args(&["--split".into(), "dev".into()]).unwrap().split,
            Some(Split::Dev)
        );
        assert_eq!(
            parse_args(&["--split".into(), "all".into()]).unwrap().split,
            None
        );
        assert!(parse_args(&["--split".into(), "train".into()]).is_err());
        assert!(parse_args(&["--everything".into()]).is_err());
    }

    /// The symbolic arm needs nothing, so this is a real run rather than a
    /// smoke test: it is the corpus's own regression against the checks.
    #[tokio::test]
    async fn the_symbolic_arm_answers_every_case_offline() {
        let r = run(&SymbolicEvaluator::default(), None).await;
        assert_eq!(r.cases.len(), 100);
        assert_eq!(r.unavailable(), 0);
        assert_eq!(r.invalid(), 0);
    }
}
