//! Agreement between two binary raters, reported so it cannot be misread
//! (M6 §8.5 **[R2]**, M8 T2.7).
//!
//! The gate is `evaluator_min_kappa`: below it an evaluator's signatures yield
//! observations and no candidates. That makes κ a number a decision hangs on,
//! and κ has two well-documented ways of being the wrong number.
//!
//! **Raw agreement overstates.** M6 §8.5 already refuses it — the 2026 audit
//! it cites puts judge reliability 33–41 points higher on raw agreement than
//! on a chance-corrected coefficient. That is why κ is here at all.
//!
//! **κ understates when positives are rare.** The prevalence paradox: when one
//! class dominates the marginals, observed agreement can be high while κ
//! collapses toward zero or goes negative, and two κ values computed at
//! different prevalences are not comparable to each other
//! (`docs/research/2026-09-09-symbolic-evaluation-findings.md` §3). The
//! evaluation lane's proxies are rare by construction — a handful of turns in
//! a session of a hundred and fifty — so this is not a corner case here, it is
//! the expected operating point.
//!
//! So [`Agreement`] carries the whole picture and never κ alone: the 2×2
//! table, both raters' positive rates, raw agreement, κ with a confidence
//! interval, Gwet's AC1 as the paradox-resistant companion, and PABAK. A
//! reader who sees κ = 0.18 next to prevalence 0.04 and AC1 = 0.91 knows what
//! happened; a reader who sees κ = 0.18 does not.
//!
//! And when there is not enough signal to estimate anything, it says so rather
//! than printing a number — see [`Agreement::decisive`].

/// The 2×2 table and every coefficient computed from it.
///
/// "a" is the rater under test, "b" the reference it is being compared with.
#[derive(Debug, Clone, PartialEq)]
pub struct Agreement {
    /// Both said yes.
    pub both: u32,
    /// Under test said yes, reference said no.
    pub a_only: u32,
    /// Reference said yes, under test said no.
    pub b_only: u32,
    /// Both said no.
    pub neither: u32,
}

/// What the coefficients come out as, and whether they may be believed.
#[derive(Debug, Clone, PartialEq)]
pub struct Scores {
    pub n: u32,
    /// Raw agreement. Reported for the reader, never for the gate.
    pub observed: f64,
    /// Cohen's κ.
    pub kappa: f64,
    /// 95% interval on κ, asymptotic.
    pub kappa_lo: f64,
    pub kappa_hi: f64,
    /// Gwet's AC1 — chance-corrected like κ, but with a chance term that does
    /// not collapse when one class is rare.
    pub ac1: f64,
    /// Prevalence-adjusted bias-adjusted κ: `2 * observed - 1`. The value κ
    /// *would* have at balanced marginals, which is what makes the gap between
    /// it and κ a direct read-out of how much prevalence is doing.
    pub pabak: f64,
    /// Share of cases the rater under test called positive.
    pub a_rate: f64,
    /// Share the reference called positive — the prevalence that drives the
    /// paradox.
    pub b_rate: f64,
    /// Positives seen by either rater. Small values make every coefficient
    /// above unstable; see [`Agreement::decisive`].
    pub positives: u32,
}

impl Agreement {
    /// Tally two aligned label streams. Panics only on a length mismatch,
    /// which is a programming error rather than a datum.
    pub fn tally(under_test: &[bool], reference: &[bool]) -> Agreement {
        assert_eq!(
            under_test.len(),
            reference.len(),
            "agreement needs one reference label per case"
        );
        let mut t = Agreement {
            both: 0,
            a_only: 0,
            b_only: 0,
            neither: 0,
        };
        for (&a, &b) in under_test.iter().zip(reference) {
            match (a, b) {
                (true, true) => t.both += 1,
                (true, false) => t.a_only += 1,
                (false, true) => t.b_only += 1,
                (false, false) => t.neither += 1,
            }
        }
        t
    }

    pub fn n(&self) -> u32 {
        self.both + self.a_only + self.b_only + self.neither
    }

    /// Positives on either side. The count that decides whether any of this
    /// is estimable.
    pub fn positives(&self) -> u32 {
        self.both + self.a_only + self.b_only
    }

    /// Whether the table carries enough signal for its coefficients to be
    /// worth reading.
    ///
    /// Two conditions, and both are about the same thing. `n` must reach the
    /// size a ±0.2 interval on κ needs — ~96 by the Donner–Eliasziw treatment
    /// (`docs/research/2026-09-09-local-evaluator-findings.md` §2) — and there
    /// must be enough positives that the estimate is not resting on three
    /// cases.
    ///
    /// **An indecisive κ is treated as below threshold**, never as absent:
    /// the conservative direction is observations and no candidates, which is
    /// what an evaluator that has not proved itself should get.
    pub fn decisive(&self, min_n: u32, min_positives: u32) -> bool {
        self.n() >= min_n && self.positives() >= min_positives
    }

    pub fn scores(&self) -> Scores {
        let n = self.n();
        let nf = n as f64;
        if n == 0 {
            return Scores {
                n: 0,
                observed: 0.0,
                kappa: 0.0,
                kappa_lo: 0.0,
                kappa_hi: 0.0,
                ac1: 0.0,
                pabak: -1.0,
                a_rate: 0.0,
                b_rate: 0.0,
                positives: 0,
            };
        }
        let (both, a_only, b_only, neither) = (
            self.both as f64,
            self.a_only as f64,
            self.b_only as f64,
            self.neither as f64,
        );
        let observed = (both + neither) / nf;
        let a_rate = (both + a_only) / nf;
        let b_rate = (both + b_only) / nf;

        // Cohen: chance agreement from the product of the marginals.
        let pe = a_rate * b_rate + (1.0 - a_rate) * (1.0 - b_rate);
        // Perfect agreement with pe == 1 means both raters were constant and
        // identical. κ is 0/0 there; 1.0 is the reading that does not
        // manufacture disagreement out of an undefined ratio, and `decisive`
        // is what stops it from being believed.
        let kappa = if (1.0 - pe).abs() < f64::EPSILON {
            1.0
        } else {
            (observed - pe) / (1.0 - pe)
        };

        // Asymptotic standard error, the usual approximation for a 2×2 table.
        let se = if (1.0 - pe).abs() < f64::EPSILON {
            0.0
        } else {
            (observed * (1.0 - observed) / (nf * (1.0 - pe) * (1.0 - pe))).sqrt()
        };

        // Gwet: chance agreement from a single overall positive propensity,
        // which is what stops it collapsing when one class is rare.
        let pi = (a_rate + b_rate) / 2.0;
        let pe_gwet = 2.0 * pi * (1.0 - pi);
        let ac1 = if (1.0 - pe_gwet).abs() < f64::EPSILON {
            1.0
        } else {
            (observed - pe_gwet) / (1.0 - pe_gwet)
        };

        Scores {
            n,
            observed,
            kappa,
            kappa_lo: kappa - 1.96 * se,
            kappa_hi: kappa + 1.96 * se,
            ac1,
            pabak: 2.0 * observed - 1.0,
            a_rate,
            b_rate,
            positives: self.positives(),
        }
    }
}

impl std::fmt::Display for Scores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} pos={} raw={:.3} kappa={:.3} [{:.3}, {:.3}] ac1={:.3} pabak={:.3} \
             rate(test)={:.2} rate(ref)={:.2}",
            self.n,
            self.positives,
            self.observed,
            self.kappa,
            self.kappa_lo,
            self.kappa_hi,
            self.ac1,
            self.pabak,
            self.a_rate,
            self.b_rate
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 5e-3
    }

    #[test]
    fn perfect_agreement_on_a_balanced_table_is_one() {
        let t = Agreement::tally(&[true, true, false, false], &[true, true, false, false]);
        let s = t.scores();
        assert!(close(s.kappa, 1.0), "{s}");
        assert!(close(s.ac1, 1.0), "{s}");
        assert!(close(s.observed, 1.0));
    }

    #[test]
    fn independent_raters_land_near_zero() {
        // 25 in each cell: agreement exactly what chance predicts.
        let t = Agreement {
            both: 25,
            a_only: 25,
            b_only: 25,
            neither: 25,
        };
        let s = t.scores();
        assert!(close(s.kappa, 0.0), "{s}");
        assert!(close(s.observed, 0.5));
    }

    /// The reason this module exists. 95 agreed negatives, 3 agreed positives,
    /// one disagreement each way: raw agreement 98%, and κ falls to a number
    /// that reads like a broken evaluator.
    #[test]
    fn the_prevalence_paradox_is_visible_rather_than_hidden() {
        let t = Agreement {
            both: 3,
            a_only: 1,
            b_only: 1,
            neither: 95,
        };
        let s = t.scores();
        assert!(close(s.observed, 0.98), "{s}");
        assert!(
            s.kappa < 0.76,
            "kappa should be dragged well under raw: {s}"
        );
        // AC1 and PABAK both stay near the raw number, which is the whole
        // point of reporting them beside it.
        assert!(s.ac1 > 0.95, "{s}");
        assert!(s.pabak > 0.95, "{s}");
        assert!(s.b_rate < 0.05, "the reference is rare: {s}");
    }

    /// κ and AC1 both punish a rater that says yes to everything, and the
    /// table says which failure it was.
    #[test]
    fn a_rater_that_always_says_yes_scores_nothing() {
        let t = Agreement {
            both: 20,
            a_only: 80,
            b_only: 0,
            neither: 0,
        };
        let s = t.scores();
        assert!(close(s.kappa, 0.0), "{s}");
        assert!(close(s.a_rate, 1.0));
        assert!(s.ac1 < 0.3, "{s}");
    }

    #[test]
    fn the_interval_narrows_as_n_grows() {
        let small = Agreement {
            both: 5,
            a_only: 2,
            b_only: 2,
            neither: 5,
        }
        .scores();
        let large = Agreement {
            both: 50,
            a_only: 20,
            b_only: 20,
            neither: 50,
        }
        .scores();
        assert!(close(small.kappa, large.kappa), "{small} vs {large}");
        let small_w = small.kappa_hi - small.kappa_lo;
        let large_w = large.kappa_hi - large.kappa_lo;
        assert!(large_w < small_w * 0.5, "{small_w} vs {large_w}");
    }

    /// The corpus was sized so this passes at n = 100; the recorded session,
    /// at n = 20, is what it is there to refuse.
    #[test]
    fn decisiveness_needs_both_size_and_positives() {
        let twenty = Agreement {
            both: 3,
            a_only: 1,
            b_only: 1,
            neither: 15,
        };
        assert!(!twenty.decisive(96, 10), "n = 20 cannot resolve the gate");
        let hundred_but_flat = Agreement {
            both: 1,
            a_only: 1,
            b_only: 1,
            neither: 97,
        };
        assert!(
            !hundred_but_flat.decisive(96, 10),
            "three positives is not an estimate"
        );
        let hundred = Agreement {
            both: 30,
            a_only: 8,
            b_only: 7,
            neither: 55,
        };
        assert!(hundred.decisive(96, 10));
    }

    #[test]
    fn tallying_counts_each_cell() {
        let t = Agreement::tally(
            &[true, true, false, false, true],
            &[true, false, true, false, false],
        );
        assert_eq!(
            t,
            Agreement {
                both: 1,
                a_only: 2,
                b_only: 1,
                neither: 1
            }
        );
        assert_eq!(t.n(), 5);
        assert_eq!(t.positives(), 4);
    }
}
