//! The paraphrase arm: does `recall` still find a line when the question is
//! not asked in the line's own words? (M8 Phase 1.)
//!
//! M6 §12.8 deferred vector search behind a number: *stay lexical; add
//! embeddings when the Phase 7 suite shows `recall` miss rate above 20% on
//! paraphrased queries.* The suite arrived with M7 and passes 9/9, and it has
//! never been able to answer that, because every one of its six memory
//! abilities asks in the words the fact was stated in. A lexical retriever
//! handed the same words back cannot miss. The trigger has been sitting
//! behind a measurement nobody could take.
//!
//! This module takes it. A corpus of lines, each with two questions — one
//! reusing the line's own vocabulary, one asking the same thing in different
//! words — run through `MemoryStore::search_turns` and scored on whether the
//! target line comes back in the top `k`.
//!
//! **It measures the retriever, not the engine.** The whole corpus goes
//! through the `MemoryStore` trait, so the same cases run against whichever
//! store is handed in. That matters more than it looks: the ability suite
//! runs on `InMemoryStore`, whose `search_turns` counts token hits, while
//! what ships is `SqliteStore`'s FTS5 `bm25`. Those are two different
//! retrievers, and a trigger measured on the one that does not ship would be
//! measuring the wrong thing.
//!
//! **What it is not.** Not a judgement about whether embeddings would help —
//! that is Phase 3, and it only happens if this arm says so. Not a model of
//! how often real users paraphrase; the corpus is deliberately adversarial,
//! so a miss rate here is an upper bound on the failure, and the threshold
//! was written against exactly that kind of set.

use nscore::{EventKind, EventLog, MemoryStore, SessionId, Timestamp};

/// One line to find, the distractors it has to be found among, and the two
/// ways of asking for it.
pub struct RecallCase {
    pub id: &'static str,
    /// "cs" or "en". Both, deliberately: this engine's traffic is Czech, and
    /// Czech inflects the words a lexical index keys on — "objednávka" and
    /// "objednávek" are different tokens for the same thing, so a Czech
    /// paraphrase loses vocabulary overlap it never meant to lose. An
    /// English-only corpus would report a friendlier number than this
    /// deployment will ever see.
    pub lang: &'static str,
    /// The line the query is supposed to find.
    pub said: &'static str,
    /// Other lines in the same conversation. Present so a hit means the
    /// retriever *chose* the target, not that the target was the only thing
    /// in the store.
    pub distractors: &'static [&'static str],
    /// Asked in the line's own words — the shape the ability suite already
    /// covers, here as the control.
    pub verbatim: &'static str,
    /// The same question, asked the way somebody who did not memorise their
    /// own phrasing would ask it.
    pub paraphrase: &'static str,
}

/// Twelve cases, six Czech and six English, drawn from what this harness
/// actually carries: desktop instructions, budgets, preferences, names and
/// times.
///
/// Small on purpose. The number this produces is a rate over a set anyone can
/// read in one sitting and disagree with case by case; a thousand generated
/// pairs would give a tighter number that nobody could audit, and the
/// decision it feeds is "build a vector index or do not".
pub fn corpus() -> &'static [RecallCase] {
    &[
        RecallCase {
            id: "en/budget",
            lang: "en",
            said: "our budget for the whole campaign is 2000 crowns",
            distractors: &[
                "can you open the browser",
                "what time is it",
                "the deadline is friday",
            ],
            verbatim: "what is our budget for the campaign",
            paraphrase: "how much money are we allowed to spend",
        },
        RecallCase {
            id: "en/name",
            lang: "en",
            said: "my name is Martin and I work at a print shop",
            distractors: &[
                "never mind that",
                "the printer is out of paper",
                "good morning",
            ],
            verbatim: "what is my name",
            paraphrase: "who am I, remind me",
        },
        RecallCase {
            id: "en/time-format",
            lang: "en",
            said: "I prefer 24-hour times, never am and pm",
            distractors: &["set an alarm for later", "the meeting moved", "thanks"],
            verbatim: "what times do I prefer",
            paraphrase: "how should you write clock values for me",
        },
        RecallCase {
            id: "en/desktop",
            lang: "en",
            said: "the invoice window is the one titled Faktury on the second screen",
            distractors: &[
                "close the other tab",
                "scroll down a bit",
                "that is the wrong one",
            ],
            verbatim: "which window is the invoice one",
            paraphrase: "where do I find billing documents on my desktop",
        },
        RecallCase {
            id: "en/refusal",
            lang: "en",
            said: "do not send anything to clients without showing me first",
            distractors: &[
                "the draft looks fine",
                "add a signature",
                "send it tomorrow",
            ],
            verbatim: "what did I say about clients without showing me first",
            paraphrase: "what am I not allowed to do on my own",
        },
        RecallCase {
            id: "en/deadline",
            lang: "en",
            said: "everything has to be finished before the fair on the twelfth",
            distractors: &[
                "the stand is booked",
                "print two hundred flyers",
                "call the supplier",
            ],
            verbatim: "when does everything have to be finished",
            paraphrase: "what is the last possible day for this work",
        },
        RecallCase {
            id: "cs/budget",
            lang: "cs",
            said: "náš rozpočet na celou kampaň je 2000 korun",
            distractors: &[
                "otevři prosím prohlížeč",
                "kolik je hodin",
                "termín je v pátek",
            ],
            verbatim: "jaký je náš rozpočet na kampaň",
            paraphrase: "kolik peněz můžeme utratit",
        },
        RecallCase {
            id: "cs/name",
            lang: "cs",
            said: "jmenuji se Martin a dělám v tiskárně",
            distractors: &["to nech být", "došel papír", "dobré ráno"],
            verbatim: "jak se jmenuji",
            paraphrase: "kdo jsem, připomeň mi to",
        },
        RecallCase {
            id: "cs/orders",
            lang: "cs",
            said: "přehled objednávek se otevírá přes zelené tlačítko vlevo dole",
            distractors: &[
                "zavři tu druhou záložku",
                "posuň to kousek dolů",
                "to není ono",
            ],
            verbatim: "jak se otevírá přehled objednávek",
            paraphrase: "kde najdu seznam toho, co si lidé koupili",
        },
        RecallCase {
            id: "cs/refusal",
            lang: "cs",
            said: "nikdy nic neposílej klientům bez toho, abys mi to ukázal",
            distractors: &["ten návrh je dobrý", "přidej podpis", "pošli to zítra"],
            verbatim: "říkal jsem, že nikdy nic neposílej klientům?",
            paraphrase: "co nesmíš udělat sám",
        },
        RecallCase {
            id: "cs/deadline",
            lang: "cs",
            said: "všechno musí být hotové před veletrhem dvanáctého",
            distractors: &[
                "stánek je zamluvený",
                "vytiskni dvě stě letáků",
                "zavolej dodavateli",
            ],
            verbatim: "kdy musí být všechno hotové",
            paraphrase: "dokdy nejpozději to má být udělané",
        },
        RecallCase {
            id: "cs/preference",
            lang: "cs",
            said: "piš mi časy ve dvacetičtyřhodinovém formátu, ne dopoledne odpoledne",
            distractors: &["nastav budík", "schůzka se posunula", "díky"],
            verbatim: "v jakém formátu mám psát časy",
            paraphrase: "jak mám zapisovat hodiny",
        },
    ]
}

/// The share of a query's matchable tokens that also appear in the line it is
/// meant to find.
///
/// In `query_tokens`' terms rather than a notion of "word" invented here: the
/// point of the number is what the *retriever* can key on, and the retriever
/// keys on those tokens. A paraphrase whose overlap is high is not a
/// paraphrase, it is the verbatim query with the articles moved around, and
/// the corpus test below fails on it — a lazy paraphrase has to break the
/// build, not quietly report a friendly miss rate.
pub fn overlap(query: &str, said: &str) -> f64 {
    let tokens = nscore::query_tokens(query);
    if tokens.is_empty() {
        return 0.0;
    }
    let hay = nscore::query_tokens(said);
    let shared = tokens.iter().filter(|t| hay.contains(t)).count();
    shared as f64 / tokens.len() as f64
}

/// One arm's result over the whole corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    /// "verbatim" or "paraphrase".
    pub arm: &'static str,
    pub hits: usize,
    pub total: usize,
    /// Case ids the retriever did not return the target for, so a miss rate
    /// can be argued with rather than only reported.
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

/// Both arms against one retriever.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// What was measured — "in-memory (token hits)" or "sqlite (fts5 bm25)".
    /// Named in the output because the two are different retrievers and a
    /// number without its retriever is not a number.
    pub retriever: String,
    pub verbatim: Arm,
    pub paraphrase: Arm,
    /// Whether this arm had vectors to search — i.e. whether the backfill
    /// wrote any (M10 P3).
    ///
    /// It changes what the closing verdict may say. M6 §12.8's sentence is
    /// "stay lexical *unless* the miss rate clears 20%", and reading that
    /// back at an arm which is not lexical would report the phase's success
    /// as a reason not to have built it.
    pub vectors: bool,
}

impl Report {
    /// Whether M6 §12.8's trigger has fired for this retriever.
    pub fn trigger_fired(&self) -> bool {
        self.paraphrase.miss_rate() > 0.20
    }
}

/// The threshold M6 §12.8 wrote down, kept here so the report and the
/// decision cannot drift apart.
pub const TRIGGER_MISS_RATE: f64 = 0.20;

/// Every line of every case, in corpus order, as one pool.
///
/// The lines a query has to pick its target out of. Exposed because whatever
/// is compared against this arm later has to face the same pool: a retriever
/// measured against four candidates and one measured against forty-eight are
/// not measured against the same thing.
pub fn pool() -> Vec<String> {
    let mut out = Vec::new();
    for case in corpus() {
        let (before, after) = case.distractors.split_at(case.distractors.len() / 2);
        for text in before
            .iter()
            .chain(std::iter::once(&case.said))
            .chain(after)
        {
            out.push((*text).to_string());
        }
    }
    out
}

/// Run the corpus through one store.
///
/// **One session, every line.** The first cut of this gave each case its own
/// session, which made the target one of four candidates — and with
/// `recall_top_k` at 5, a retriever that returned everything would have
/// scored a perfect 12/12 without ranking anything. The number survived only
/// because a lexical search drops zero-score lines and so returns nothing at
/// all on a paraphrase. That is a real failure mode, but resting the
/// measurement on it would have made this arm useless for the comparison it
/// exists to enable: anything measured against it later must have to *rank*,
/// not merely to return.
///
/// So all twelve cases share one session and every query is asked against all
/// forty-eight lines. A hit means the target came back inside the top `k` of
/// the whole pool.
pub async fn measure(store: &dyn MemoryStore, retriever: &str, k: usize) -> Report {
    let mut verbatim = Arm {
        arm: "verbatim",
        hits: 0,
        total: 0,
        misses: Vec::new(),
    };
    let mut paraphrase = Arm {
        arm: "paraphrase",
        hits: 0,
        total: 0,
        misses: Vec::new(),
    };

    let sid = SessionId("paraphrase-pool".into());
    // Built through `EventLog` rather than by hand: the chain is part of
    // what a store may check on the way in, and a corpus that only
    // measured stores lenient about it would be measuring leniency.
    let mut log = EventLog::new(sid.clone());
    for (i, text) in pool().into_iter().enumerate() {
        log.append(
            i as u32 + 1,
            Timestamp(i as u64 + 1),
            EventKind::UserSaid { text },
        );
    }
    if let Err(e) = store.append(&sid, log.events()).await {
        // A store that cannot take the corpus cannot be measured, and saying
        // so beats reporting a 100% miss rate as if it were the retriever's
        // fault.
        return Report {
            retriever: format!("{retriever} — NOT MEASURED: {e}"),
            verbatim,
            paraphrase,
            vectors: false,
        };
    }

    // M8 T3.1: the vectors the hybrid arm reads have to exist before it is
    // measured, and they are written exactly where the idle pass writes
    // them — through `backfill_embeddings`, in bounded batches, before the
    // first query. A store without an encoder returns 0 on the first call
    // and this loop costs one method call.
    //
    // The bound is here rather than trusted: an arm that hung on a service
    // would look like a slow test rather than a misconfiguration.
    let mut vectors = false;
    for _ in 0..64 {
        match store.backfill_embeddings(64).await {
            Ok(0) | Err(_) => break,
            Ok(_) => vectors = true,
        }
    }

    for case in corpus() {
        for (arm, query) in [
            (&mut verbatim, case.verbatim),
            (&mut paraphrase, case.paraphrase),
        ] {
            arm.total += 1;
            // `search_turns_hybrid`, whose default *is* `search_turns_in`,
            // which for one session is `search_turns`: the in-memory and
            // bm25 arms measure exactly what they measured before this
            // existed, and the third arm measures the path that ships.
            let found = store
                .search_turns_hybrid(std::slice::from_ref(&sid), query, k)
                .await
                .unwrap_or_default()
                .iter()
                .any(|hit| hit.text == case.said);
            if found {
                arm.hits += 1;
            } else {
                arm.misses.push(case.id.to_string());
            }
        }
    }

    Report {
        retriever: retriever.to_string(),
        verbatim,
        paraphrase,
        vectors,
    }
}

/// The arm as a person has to read it to make the §12.8 decision: both
/// retrievers, both arms, and what the number means stated rather than left
/// to be looked up.
pub fn render(reports: &[Report]) -> String {
    let mut out = String::from(
        "\n  paraphrased recall (M6 §12.8 trigger: miss rate > 20% on the paraphrase arm)\n\n",
    );
    out.push_str(&format!(
        "  {:<44} {:>10} {:>8} {:>10} {:>8}\n",
        "retriever", "verbatim", "miss", "paraphrase", "miss"
    ));
    out.push_str(&format!("  {}\n", "-".repeat(84)));
    for r in reports {
        out.push_str(&format!(
            "  {:<44} {:>6}/{:<3} {:>7.0}% {:>6}/{:<3} {:>7.0}%\n",
            r.retriever,
            r.verbatim.hits,
            r.verbatim.total,
            r.verbatim.miss_rate() * 100.0,
            r.paraphrase.hits,
            r.paraphrase.total,
            r.paraphrase.miss_rate() * 100.0,
        ));
    }
    out.push('\n');
    for r in reports {
        if !r.paraphrase.misses.is_empty() {
            out.push_str(&format!(
                "  {} missed: {}\n",
                r.retriever,
                r.paraphrase.misses.join(", ")
            ));
        }
    }
    out.push('\n');
    for r in reports {
        if r.trigger_fired() {
            out.push_str(&format!(
                "  {}: TRIGGER FIRED — {:.0}% > 20%. M6 §12.8 permits embeddings here.\n",
                r.retriever,
                r.paraphrase.miss_rate() * 100.0
            ));
        } else if r.vectors {
            // Not a §12.8 verdict: this arm is what §12.8 permitted. The
            // sentence it has to answer is M8 §6's exit line instead.
            out.push_str(&format!(
                "  {}: {:.0}% ≤ 20% — M8 §6's exit criterion met on this arm.\n",
                r.retriever,
                r.paraphrase.miss_rate() * 100.0
            ));
        } else {
            out.push_str(&format!(
                "  {}: trigger not fired — {:.0}% ≤ 20%. M6 §12.8 says stay lexical.\n",
                r.retriever,
                r.paraphrase.miss_rate() * 100.0
            ));
        }
    }
    out.push_str(
        "\n  The corpus is adversarial by construction, so this is an upper bound on the\n  \
         failure, not a rate over real traffic.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsengine::store::InMemoryStore;

    /// The corpus's own honesty check. A "paraphrase" that reuses the line's
    /// vocabulary would make the arm pass by construction and the trigger
    /// unreachable, so it fails the build instead.
    #[test]
    fn every_paraphrase_actually_paraphrases() {
        for case in corpus() {
            let p = overlap(case.paraphrase, case.said);
            assert!(
                p <= 0.30,
                "{}: paraphrase shares {:.0}% of its tokens with the line — that is a \
                 rewording, not a paraphrase",
                case.id,
                p * 100.0
            );
            // And the control has to be a control: a "verbatim" query with no
            // overlap would make both arms the same measurement.
            let v = overlap(case.verbatim, case.said);
            assert!(
                v >= 0.40,
                "{}: verbatim query shares only {:.0}% — it is not the control it claims \
                 to be",
                case.id,
                v * 100.0
            );
        }
    }

    /// Both languages present, and the target never the only line in its
    /// session — the two things that would quietly make the number easier.
    #[test]
    fn the_corpus_is_shaped_the_way_the_measurement_needs() {
        let cs = corpus().iter().filter(|c| c.lang == "cs").count();
        let en = corpus().iter().filter(|c| c.lang == "en").count();
        assert!(cs >= 5 && en >= 5, "cs={cs} en={en}");
        for case in corpus() {
            assert!(
                case.distractors.len() >= 3,
                "{}: too few distractors to prove anything",
                case.id
            );
        }
    }

    /// The control arm has to land: if the in-memory retriever cannot find a
    /// line asked for in its own words, the paraphrase number means nothing
    /// and the harness is what is broken.
    #[tokio::test]
    async fn the_verbatim_arm_is_a_control_that_passes() {
        let store = InMemoryStore::new();
        let report = measure(&store, "in-memory (token hits)", 5).await;
        assert_eq!(
            report.verbatim.misses,
            Vec::<String>::new(),
            "the control arm missed"
        );
    }

    #[test]
    fn miss_rate_is_a_share_of_the_arm() {
        let arm = Arm {
            arm: "paraphrase",
            hits: 9,
            total: 12,
            misses: vec!["a".into(), "b".into(), "c".into()],
        };
        assert!((arm.miss_rate() - 0.25).abs() < 1e-9);
        assert!(
            Arm {
                arm: "verbatim",
                hits: 0,
                total: 0,
                misses: vec![],
            }
            .miss_rate()
            .abs()
                < 1e-9
        );
    }
}
