//! M10 T5.2 — the tie-heavy recall corpus: the arm `activation_weight` was
//! waiting for.
//!
//! M9 T3.1 built the activation prior — `score = hits + w · ln(1 + credits) ·
//! exp(−Δdays / half_life)` — shipped it at `w = 0.0`, and then T3.3 tried to
//! decide the knob and could not: *"suites insensitive: at `w ∈ {0, 0.5, 1.0}`
//! … identical lists at every weight."* The reason is in M9's own finding
//! under T3.2: in `InMemoryStore` a lexical score is a **whole token count**,
//! so a fractional prior can only move a fact *past a fact it is tied with*.
//! Every existing fixture asks for something nothing else in the store
//! matches. Nothing ties, so nothing moves, at any weight.
//!
//! So this corpus is nothing but ties. Each case is five facts whose keys
//! carry exactly the same query tokens — the retriever scores them all
//! identically and has to break the tie on something else. At `w = 0` that
//! something else is `last_validated`, and the corpus is built so the target
//! is the *oldest* of the five and loses. At `w = 1` it is
//! `ln(1 + credits) · exp(−Δdays / half_life)`, and the target is the one
//! fact that earned credits (M9 P4: credits, not `uses` — "how often a fact
//! *helped*, not how often it was shown") and was used today.
//!
//! **The cut is where a tie is decided.** Five candidates and
//! [`TIE_TOP_K`]` = 2`: a retriever handed a `k` as wide as its candidate set
//! ranks nothing, it only returns, and an arm measured that way would score
//! 5/5 at every weight — the exact failure `paraphrase.rs` records talking
//! itself out of. So the target has to be in the two slots the fact block
//! actually has room for.
//!
//! **In-memory only, and that is the point rather than a shortcut.** The
//! integer scores that make M9's prior invisible on the shipping suite are
//! what make an *exact* tie constructible here; under FTS5's fractional
//! `bm25` two facts almost never tie to the last bit, so a corpus built on it
//! would be measuring rounding. The seeds go in through the
//! [`nscore::MemoryStore`] trait, so the day a store wants to answer the same
//! question it can be handed to [`measure_on`].

use crate::eval::{Harness, Run};
use nscore::{Fact, FactState, MemoryStore, Provenance, Timestamp, Trust};

/// How many facts the arm lets through. Two of five: the slot count a fact
/// block really has, and narrow enough that a tie has to be broken rather
/// than sidestepped.
pub const TIE_TOP_K: usize = 2;

/// One candidate fact: what it says, how often it has helped, and how long
/// ago it was last shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// Every candidate of a case carries the same query tokens in its key,
    /// so `lexical_rank` scores them all the same and the tie is exact.
    pub key: &'static str,
    pub value: &'static str,
    /// M9 P4's frequency column.
    pub credits: u32,
    /// Days since the fact was last shown to a model — the prior's decay
    /// input.
    pub used_days_ago: u64,
    /// Days since the value was confirmed. This is the tiebreak at `w = 0`,
    /// and the corpus makes the target the oldest so that today's ranking
    /// puts it last.
    pub validated_days_ago: u64,
}

/// One tie: a query, and the facts that all match it equally.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TieCase {
    pub id: &'static str,
    pub lang: &'static str,
    /// Both of its tokens appear in every candidate's key.
    pub query: &'static str,
    /// The one the arm is asking for — the fact that earned credits.
    pub target: &'static str,
    pub candidates: &'static [Candidate; 5],
}

/// One weight's result over the whole corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    pub weight: f32,
    pub hits: usize,
    pub total: usize,
    /// Case ids whose target did not make the cut, so a number can be argued
    /// with rather than only reported.
    pub misses: Vec<String>,
}

impl Arm {
    pub fn miss_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        (self.total - self.hits) as f64 / self.total as f64
    }
}

/// One arm against one retriever.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub retriever: String,
    pub arm: Arm,
}

/// Seed one case's facts into `store` under `scope`.
///
/// Through `put_fact` rather than by handing `lexical_rank` a slice: the
/// question T5.2 asks is whether the *store* ranks differently at two
/// weights, and a corpus that bypassed the store would answer a question
/// about a pure function instead.
pub async fn seed(store: &dyn MemoryStore, scope: &str, case: &TieCase) {
    const DAY: u64 = 86_400_000;
    let now = nscore::now_ms();
    for c in case.candidates {
        let _ = store
            .put_fact(Fact {
                key: c.key.into(),
                value: serde_json::json!(c.value),
                confidence: 1.0,
                uses: 0,
                last_validated: Timestamp(now.saturating_sub(c.validated_days_ago * DAY)),
                prov: Provenance::Constant,
                scope: scope.into(),
                trust: Trust::User,
                valid_from: Timestamp(now.saturating_sub(c.validated_days_ago * DAY)),
                valid_to: None,
                state: FactState::Current,
                last_used: Timestamp(now.saturating_sub(c.used_days_ago * DAY)),
                exposures: c.credits,
                credits: c.credits,
            })
            .await;
    }
}

/// Run the corpus through one store, which must already be configured at the
/// weight being measured.
///
/// A scope per case, so five candidates are five candidates and not forty.
pub async fn measure_on(store: &dyn MemoryStore, retriever: &str, weight: f32) -> Report {
    let mut arm = Arm {
        weight,
        hits: 0,
        total: 0,
        misses: Vec::new(),
    };
    for case in corpus() {
        let scope = format!("tie-{}", case.id.replace('/', "-"));
        seed(store, &scope, case).await;
        arm.total += 1;
        let top = store
            .search_facts(&scope, case.query, TIE_TOP_K)
            .await
            .unwrap_or_default();
        if top.iter().any(|f| f.key == case.target) {
            arm.hits += 1;
        } else {
            arm.misses.push(case.id.to_string());
        }
    }
    Report {
        retriever: retriever.to_string(),
        arm,
    }
}

/// The arm as `ns-app eval --activation <w>` runs it: the store the harness
/// builds, at the weight the run was given.
pub async fn measure(weight: f32) -> Report {
    let h = Harness::for_run(Run {
        activation_weight: weight,
        ..Run::default()
    });
    measure_on(&*h.store(), "in-memory (token hits)", weight).await
}

/// The table, and what a reader is meant to do with it.
pub fn render(r: &Report) -> String {
    let mut out = format!(
        "\n  M10 T5.2 — tie-heavy recall (5 facts tie on hits; only recency or credits \
         separate them)\n\n  {:<26} {:>8} {:>8} {:>8}\n",
        "retriever", "weight", "top-2", "miss"
    );
    out.push_str(&format!("  {}\n", "-".repeat(56)));
    out.push_str(&format!(
        "  {:<26} {:>8.2} {:>4}/{:<3} {:>7.0}%\n",
        r.retriever,
        r.arm.weight,
        r.arm.hits,
        r.arm.total,
        r.arm.miss_rate() * 100.0
    ));
    if !r.arm.misses.is_empty() {
        out.push_str(&format!("\n  missed: {}\n", r.arm.misses.join(", ")));
    }
    out.push_str(&format!(
        "\n  At `activation_weight = 0` the tiebreak is `last_validated` and the target is \
         the oldest\n  of its five, so it loses. Run this at 0 and at 1 and compare: two \
         different numbers\n  are the precondition M9 T3.3 could not meet, and they are what \
         lets the knob be decided.\n  k = {TIE_TOP_K}; {} cases; nothing here spends a \
         request.\n",
        corpus().len()
    ));
    out
}

/// Eight ties, four Czech and four English.
///
/// Small on purpose, `paraphrase.rs`'s argument: the decision this feeds is
/// "move `activation_weight` off zero or do not", and a rate over a set
/// nobody can audit case by case is not evidence for it.
///
/// Every case has the same shape, and it is the shape that carries the claim:
/// five keys holding the same two query tokens, one target that is the
/// **oldest validated** (so `w = 0` ranks it last) and the only one with
/// credits and a recent `last_used` (so `w = 1` ranks it first).
pub fn corpus() -> &'static [TieCase] {
    &CORPUS
}

/// A decoy: no credits worth the name, used long ago, validated recently.
const fn decoy(
    key: &'static str,
    value: &'static str,
    credits: u32,
    used_days_ago: u64,
    validated_days_ago: u64,
) -> Candidate {
    Candidate {
        key,
        value,
        credits,
        used_days_ago,
        validated_days_ago,
    }
}

/// The one that earned its place: credits from turns that went well, shown
/// today, and confirmed longest ago.
const fn target(key: &'static str, value: &'static str) -> Candidate {
    Candidate {
        key,
        value,
        credits: 40,
        used_days_ago: 0,
        validated_days_ago: 90,
    }
}

const CORPUS: [TieCase; 8] = [
    TieCase {
        id: "en/print-order",
        lang: "en",
        query: "order printer",
        target: "shop.order.printer.kessler",
        candidates: &[
            target(
                "shop.order.printer.kessler",
                "the run we reprint every month",
            ),
            decoy("shop.order.printer.halden", "cancelled in March", 0, 60, 1),
            decoy("shop.order.printer.vogel", "one flyer job, paid", 1, 45, 2),
            decoy(
                "shop.order.printer.ashby",
                "quote only, never placed",
                0,
                80,
                3,
            ),
            decoy("shop.order.printer.quint", "archived last year", 2, 70, 4),
        ],
    },
    TieCase {
        id: "en/invoice-window",
        lang: "en",
        query: "invoice window",
        target: "desktop.invoice.window.faktury",
        candidates: &[
            target(
                "desktop.invoice.window.faktury",
                "the one on the second screen",
            ),
            decoy("desktop.invoice.window.old", "closed and gone", 0, 55, 1),
            decoy("desktop.invoice.window.demo", "the trial copy", 1, 40, 2),
            decoy(
                "desktop.invoice.window.backup",
                "read-only mirror",
                0,
                75,
                3,
            ),
            decoy(
                "desktop.invoice.window.test",
                "left over from setup",
                2,
                65,
                4,
            ),
        ],
    },
    TieCase {
        id: "en/paper-supplier",
        lang: "en",
        query: "paper supplier",
        target: "shop.paper.supplier.papirna",
        candidates: &[
            target(
                "shop.paper.supplier.papirna",
                "the one we actually order from",
            ),
            decoy(
                "shop.paper.supplier.grafix",
                "a sample book, nothing since",
                0,
                50,
                1,
            ),
            decoy(
                "shop.paper.supplier.kolding",
                "too slow last winter",
                1,
                48,
                2,
            ),
            decoy(
                "shop.paper.supplier.wolseley",
                "out of stock again",
                0,
                85,
                3,
            ),
            decoy("shop.paper.supplier.ravensburg", "reprints only", 2, 72, 4),
        ],
    },
    TieCase {
        id: "en/delivery-address",
        lang: "en",
        query: "delivery address",
        target: "shop.delivery.address.havlickova",
        candidates: &[
            target(
                "shop.delivery.address.havlickova",
                "where the courier actually goes",
            ),
            decoy("shop.delivery.address.krizova", "the old yard", 0, 58, 1),
            decoy(
                "shop.delivery.address.depot",
                "pickup point, unused",
                1,
                44,
                2,
            ),
            decoy(
                "shop.delivery.address.home",
                "personal, not for jobs",
                0,
                78,
                3,
            ),
            decoy(
                "shop.delivery.address.fair",
                "the stand, one week a year",
                2,
                68,
                4,
            ),
        ],
    },
    TieCase {
        id: "cs/objednavka-tiskarna",
        lang: "cs",
        query: "objednavka tiskarna",
        target: "dilna.objednavka.tiskarna.kessler",
        candidates: &[
            target(
                "dilna.objednavka.tiskarna.kessler",
                "zakázka, kterou tiskneme každý měsíc",
            ),
            decoy(
                "dilna.objednavka.tiskarna.halden",
                "zrušená v březnu",
                0,
                62,
                1,
            ),
            decoy(
                "dilna.objednavka.tiskarna.vogel",
                "jednorázové letáky",
                1,
                46,
                2,
            ),
            decoy("dilna.objednavka.tiskarna.ashby", "jen nabídka", 0, 82, 3),
            decoy(
                "dilna.objednavka.tiskarna.quint",
                "loni archivováno",
                2,
                74,
                4,
            ),
        ],
    },
    TieCase {
        id: "cs/faktura-okno",
        lang: "cs",
        query: "faktura okno",
        target: "plocha.faktura.okno.faktury",
        candidates: &[
            target("plocha.faktura.okno.faktury", "to na druhé obrazovce"),
            decoy("plocha.faktura.okno.stare", "zavřené a pryč", 0, 57, 1),
            decoy("plocha.faktura.okno.demo", "zkušební kopie", 1, 41, 2),
            decoy("plocha.faktura.okno.zaloha", "jen pro čtení", 0, 76, 3),
            decoy("plocha.faktura.okno.test", "zbylo z instalace", 2, 66, 4),
        ],
    },
    TieCase {
        id: "cs/papir-dodavatel",
        lang: "cs",
        query: "papir dodavatel",
        target: "dilna.papir.dodavatel.papirna",
        candidates: &[
            target("dilna.papir.dodavatel.papirna", "od koho opravdu bereme"),
            decoy("dilna.papir.dodavatel.grafix", "jen vzorník", 0, 51, 1),
            decoy(
                "dilna.papir.dodavatel.kolding",
                "loni v zimě pomalý",
                1,
                49,
                2,
            ),
            decoy(
                "dilna.papir.dodavatel.wolseley",
                "zase není skladem",
                0,
                86,
                3,
            ),
            decoy("dilna.papir.dodavatel.ravensburg", "jen dotisky", 2, 73, 4),
        ],
    },
    TieCase {
        id: "cs/rozvoz-adresa",
        lang: "cs",
        query: "rozvoz adresa",
        target: "dilna.rozvoz.adresa.havlickova",
        candidates: &[
            target("dilna.rozvoz.adresa.havlickova", "kam kurýr skutečně jezdí"),
            decoy("dilna.rozvoz.adresa.krizova", "starý dvůr", 0, 59, 1),
            decoy(
                "dilna.rozvoz.adresa.depo",
                "výdejní místo, nepoužívá se",
                1,
                43,
                2,
            ),
            decoy(
                "dilna.rozvoz.adresa.domu",
                "soukromá, ne na zakázky",
                0,
                79,
                3,
            ),
            decoy(
                "dilna.rozvoz.adresa.veletrh",
                "stánek, týden v roce",
                2,
                69,
                4,
            ),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus's own honesty check: if the candidates did not really tie,
    /// the arm would be measuring relevance and the weight would be
    /// incidental.
    #[test]
    fn every_candidate_of_a_case_scores_exactly_the_same_lexically() {
        for case in corpus() {
            let tokens = nscore::query_tokens(case.query);
            assert!(
                tokens.len() >= 2,
                "{}: a one-token query is not a tie worth constructing",
                case.id
            );
            let hits: Vec<usize> = case
                .candidates
                .iter()
                .map(|c| {
                    let hay = format!(
                        "{} {}",
                        c.key.replace(['.', '_', '-'], " ").to_lowercase(),
                        c.value.to_lowercase()
                    );
                    tokens.iter().filter(|t| hay.contains(t.as_str())).count()
                })
                .collect();
            assert!(
                hits.iter().all(|h| *h == tokens.len()),
                "{}: candidates score {hits:?}, and a tie has to be exact",
                case.id
            );
            // Three is the plan's floor; five is what the corpus carries.
            assert!(case.candidates.len() >= 3);
            assert_eq!(case.candidates[0].key, case.target);
            // The target is the oldest of its five, which is what makes it
            // lose at weight zero.
            assert!(
                case.candidates[1..]
                    .iter()
                    .all(|d| d.validated_days_ago < case.candidates[0].validated_days_ago),
                "{}: the target is not the oldest, so weight zero has no reason to drop it",
                case.id
            );
        }
        let cs = corpus().iter().filter(|c| c.lang == "cs").count();
        assert!(cs >= 3 && corpus().len() - cs >= 3, "cs={cs}");
    }

    /// **The exit criterion** (M10 T5.2): the target is reachable at
    /// `activation_weight = 1` and not at `0`.
    ///
    /// Both halves are the assertion. An arm that only hit would say nothing
    /// about the knob, and one that only missed would be a broken corpus —
    /// the same shape `the_hard_query_ability_fails_when_its_tool_is_withheld`
    /// takes, and for the same reason.
    #[tokio::test]
    async fn the_tie_corpus_separates_weight_zero_from_weight_one() {
        let off = measure(0.0).await;
        let on = measure(1.0).await;
        println!("{}{}", render(&off), render(&on));
        assert_eq!(
            off.arm.hits, 0,
            "at weight zero the tie is broken by `last_validated` and every target is the \
             oldest of its five: {:?}",
            off.arm.misses
        );
        assert_eq!(
            on.arm.hits, on.arm.total,
            "at weight one credits and recency have to carry every target into the top \
             {TIE_TOP_K}: {:?}",
            on.arm.misses
        );
        assert_ne!(
            off.arm.hits, on.arm.hits,
            "`ns-app eval --activation 0` and `--activation 1` must return different numbers"
        );
        // Half a weight is still a weight: the knob is continuous and the arm
        // has to move with it rather than flip on a boolean.
        let half = measure(0.5).await;
        assert_eq!(half.arm.hits, half.arm.total, "{:?}", half.arm.misses);
    }

    #[test]
    fn miss_rate_is_a_share_of_the_arm() {
        let arm = Arm {
            weight: 0.0,
            hits: 6,
            total: 8,
            misses: vec!["a".into(), "b".into()],
        };
        assert!((arm.miss_rate() - 0.25).abs() < 1e-9);
    }
}
