//! The backward arrow's join (M9 T4.3): what was in a prompt × how that turn
//! went → per-fact and per-note fitness.
//!
//! Three event kinds meet here, all of them already in the log and all of them
//! joined on one number, the turn:
//!
//! * `ModelCall.manifest.fact_keys` / `note_hashes` — what the call was shown;
//! * `Graded { turn, grade, by }` — what an evaluator made of the turn;
//! * `ReplyCited { sources }` — what the reply actually drew on.
//!
//! **Counters are derived, never incremented.** Every pass recomputes both
//! numbers for every fact from the whole store and *sets* them. That is what
//! makes a second pass idempotent, makes any number auditable against the
//! events that produced it, and makes a pre-M9 session — whose manifests carry
//! no keys and are never backfilled, because events are hash-chained over
//! their JSON — contribute exactly zero rather than a guess.
//!
//! Recorded caveat (plan T4.3): the counters are set on the fact version
//! *current at pass time*. A version superseded between the call that showed
//! it and the pass that scores it is not credited — its exposures were real,
//! but they were exposures of a value the store no longer serves.
use crate::symbolic::Recorded;
use nscore::{EventKind, MemoryStore, StoreError};
use std::collections::{BTreeMap, BTreeSet};

/// Per-note counters, the ledger's copy of what facts keep in their columns
/// (M9, decision 3: they live beside `entries`, not inside a `LedgerEntry`,
/// because a hand-written note has no entry).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NoteFitness {
    #[serde(default)]
    pub exposures: u32,
    #[serde(default)]
    pub credits: u32,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FitnessReport {
    /// Every current fact in the store, with its derived numbers:
    /// `(scope, key, exposures, credits)`. Facts nothing ever showed are in
    /// here at 0/0 — "never queried" and "queried and useless" are different
    /// findings, and only the first number tells them apart.
    pub facts: Vec<(String, String, u32, u32)>,
    pub notes: BTreeMap<String, NoteFitness>,
    /// `lift` per note hash, filled by the pass from the notes library: the
    /// number a note was accepted on, printed beside the ones it has earned
    /// since.
    pub note_lifts: BTreeMap<String, f64>,
    /// Sessions with at least one `Graded` turn whose manifests name no fact
    /// at all. The alarm the risk register asks for: an off-by-one between
    /// `ModelCall.turn` and `Graded.turn` would zero every credit silently,
    /// and with demotion on it would demote everything.
    pub graded_sessions_with_zero_exposures: Vec<String>,
}

impl FitnessReport {
    pub fn with_exposures(&self) -> usize {
        self.facts.iter().filter(|(_, _, e, _)| *e > 0).count()
    }
    pub fn zero_credit(&self) -> usize {
        self.facts
            .iter()
            .filter(|(_, _, e, c)| *e > 0 && *c == 0)
            .count()
    }
    /// Zero-credit facts, most exposed first, then by key — a total order, so
    /// two runs print the same ten.
    pub fn top_zero_credit(&self, n: usize) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = self
            .facts
            .iter()
            .filter(|(_, _, e, c)| *e > 0 && *c == 0)
            .map(|(_, k, e, _)| (k.clone(), *e))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

/// One turn's outcome, as the join sees it.
#[derive(Default)]
struct TurnOutcome {
    /// The authoritative evaluator graded it, and said ok.
    good: bool,
    /// Anything graded it at all — what the alarm counts sessions by.
    graded: bool,
    cited: BTreeSet<String>,
}

/// Derive both counter pairs from the whole log.
///
/// Pure over `sessions`: it reads the store only for the list of current
/// facts to report against, and it writes nothing. The caller decides whether
/// the numbers reach the store (`set_fact_fitness`) and the ledger, so a dry
/// run computes exactly what a real run would and applies none of it.
pub async fn derive(
    store: &dyn MemoryStore,
    sessions: &[Recorded],
    authoritative: &str,
) -> Result<FitnessReport, StoreError> {
    let mut fact_exposures: BTreeMap<String, u32> = BTreeMap::new();
    let mut fact_credits: BTreeMap<String, u32> = BTreeMap::new();
    let mut notes: BTreeMap<String, NoteFitness> = BTreeMap::new();
    let mut alarms: Vec<String> = Vec::new();

    for (sid, events) in sessions {
        // Pass one: what happened to each turn.
        let mut outcomes: BTreeMap<u32, TurnOutcome> = BTreeMap::new();
        for e in events {
            match &e.kind {
                EventKind::Graded {
                    turn, grade, by, ..
                } => {
                    let o = outcomes.entry(*turn).or_default();
                    o.graded = true;
                    // One named evaluator decides, never a merge: two scorers
                    // that disagree are a κ measurement, not an average.
                    if by == authoritative && grade.ok {
                        o.good = true;
                    }
                }
                EventKind::ReplyCited { sources } => {
                    outcomes
                        .entry(e.turn)
                        .or_default()
                        .cited
                        .extend(sources.iter().cloned());
                }
                _ => {}
            }
        }
        // Pass two: what each call was shown, credited by its turn's outcome.
        let mut session_exposures: u64 = 0;
        let mut session_graded = false;
        for o in outcomes.values() {
            session_graded |= o.graded;
        }
        for e in events {
            let EventKind::ModelCall { manifest, .. } = &e.kind else {
                continue;
            };
            let none = TurnOutcome::default();
            let o = outcomes.get(&e.turn).unwrap_or(&none);
            for key in &manifest.fact_keys {
                session_exposures += 1;
                *fact_exposures.entry(key.clone()).or_insert(0) += 1;
                if o.good || o.cited.contains(&format!("fact:{key}")) {
                    *fact_credits.entry(key.clone()).or_insert(0) += 1;
                }
            }
            for hash in &manifest.note_hashes {
                let n = notes.entry(hash.clone()).or_default();
                n.exposures += 1;
                if o.good || o.cited.contains(&format!("guidance:{hash}")) {
                    n.credits += 1;
                }
            }
        }
        if session_graded && session_exposures == 0 {
            alarms.push(sid.0.clone());
        }
    }

    // Report against the facts that exist now, not against the keys the log
    // happens to name: a key whose fact was forgotten has no version to score,
    // and a fact nothing ever showed is the more interesting of the two rows.
    let mut facts: Vec<(String, String, u32, u32)> = Vec::new();
    for scope in store.scopes().await? {
        for f in store.facts(&scope, "").await? {
            facts.push((
                scope.clone(),
                f.key.clone(),
                fact_exposures.get(&f.key).copied().unwrap_or(0),
                fact_credits.get(&f.key).copied().unwrap_or(0),
            ));
        }
    }
    facts.sort();
    Ok(FitnessReport {
        facts,
        notes,
        note_lifts: BTreeMap::new(),
        graded_sessions_with_zero_exposures: alarms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{ContextManifest, Event, EventLog, Fact, Grade, SessionId, Timestamp, Usage};
    use nsengine::store::InMemoryStore;

    fn usage() -> Usage {
        Usage {
            role: "emitter".into(),
            model: "m".into(),
            prompt_tokens: 100,
            completion_tokens: 10,
            estimated: false,
            attempts: 1,
            latency_ms: 5,
            tools_tokens: 0,
            cached_tokens: 0,
        }
    }

    fn manifest(keys: &[&str]) -> ContextManifest {
        ContextManifest {
            fact_keys: keys.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A session built event by event, so every turn's shape is visible in the
    /// test rather than hidden behind a harness.
    struct Fixture {
        log: EventLog,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            Self {
                log: EventLog::new(SessionId(name.into())),
            }
        }
        fn turn(&mut self, turn: u32, keys: &[&str]) -> &mut Self {
            self.log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::UserSaid {
                    text: "what is my name".into(),
                },
            );
            self.log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::ModelCall {
                    usage: usage(),
                    manifest: manifest(keys),
                },
            );
            self
        }
        fn graded(&mut self, turn: u32, ok: bool, by: &str) -> &mut Self {
            self.log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::Graded {
                    turn,
                    grade: Grade { ok, issues: vec![] },
                    by: by.into(),
                    revision: "r1".into(),
                },
            );
            self
        }
        fn cited(&mut self, turn: u32, sources: &[&str]) -> &mut Self {
            self.log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::ReplyCited {
                    sources: sources.iter().map(|s| s.to_string()).collect(),
                },
            );
            self
        }
        fn recorded(&self) -> Recorded {
            (self.log.session().clone(), self.log.events().to_vec())
        }
    }

    async fn store_with(keys: &[&str]) -> InMemoryStore {
        let store = InMemoryStore::new();
        for key in keys {
            store
                .put_fact(Fact {
                    key: (*key).into(),
                    value: serde_json::json!("x"),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        store
    }

    fn numbers(r: &FitnessReport, key: &str) -> (u32, u32) {
        r.facts
            .iter()
            .find(|(_, k, _, _)| k == key)
            .map(|(_, _, e, c)| (*e, *c))
            .unwrap_or((0, 0))
    }

    #[tokio::test]
    async fn a_fact_in_two_graded_good_turns_gets_two_credits_and_two_exposures() {
        let store = store_with(&["user.name", "user.city"]).await;
        let mut f = Fixture::new("s");
        f.turn(1, &["user.name", "user.city"])
            .graded(1, true, "symbolic");
        f.turn(2, &["user.name"]).graded(2, true, "symbolic");
        let r = derive(&store, &[f.recorded()], "symbolic").await.unwrap();
        assert_eq!(numbers(&r, "user.name"), (2, 2));
        assert_eq!(numbers(&r, "user.city"), (1, 1));
        assert!(r.graded_sessions_with_zero_exposures.is_empty());
        assert_eq!(r.with_exposures(), 2);
        assert_eq!(r.zero_credit(), 0);
    }

    /// The citation boost, and the reason it is not redundant with the grade:
    /// a turn can go wrong for a reason that has nothing to do with the fact
    /// it correctly quoted. Crediting only good turns would punish the fact
    /// for the turn's other failure.
    #[tokio::test]
    async fn a_cited_fact_in_a_bad_turn_still_gets_a_credit() {
        let store = store_with(&["user.name", "user.city"]).await;
        let mut f = Fixture::new("s");
        f.turn(1, &["user.name", "user.city"])
            .graded(1, false, "symbolic")
            .cited(1, &["fact:user.name"]);
        let r = derive(&store, &[f.recorded()], "symbolic").await.unwrap();
        assert_eq!(
            numbers(&r, "user.name"),
            (1, 1),
            "cited despite the verdict"
        );
        assert_eq!(
            numbers(&r, "user.city"),
            (1, 0),
            "shown, unused, ungraded good"
        );
        assert_eq!(r.zero_credit(), 1);
    }

    /// An ungraded turn is not evidence either way. It still cost the prompt
    /// space, so it counts as an exposure; it says nothing about usefulness,
    /// so it earns no credit. A scheme that credited ungraded turns would make
    /// "the evaluator was down" look like success.
    #[tokio::test]
    async fn an_ungraded_turn_moves_exposures_but_not_credits() {
        let store = store_with(&["user.name"]).await;
        let mut f = Fixture::new("s");
        f.turn(1, &["user.name"]);
        let r = derive(&store, &[f.recorded()], "symbolic").await.unwrap();
        assert_eq!(numbers(&r, "user.name"), (1, 0));
        assert!(
            r.graded_sessions_with_zero_exposures.is_empty(),
            "nothing graded, so nothing to be alarmed about"
        );
        // And a grade by somebody else is not the authoritative one.
        let mut f = Fixture::new("s");
        f.turn(1, &["user.name"]).graded(1, true, "local");
        let r = derive(&store, &[f.recorded()], "symbolic").await.unwrap();
        assert_eq!(numbers(&r, "user.name"), (1, 0));
    }

    /// The property the whole design rests on: the counters are set from the
    /// log, so running the join twice over the same log produces the same
    /// numbers. An incrementing scheme would double them here.
    #[tokio::test]
    async fn a_second_derivation_yields_the_same_numbers() {
        let store = store_with(&["user.name", "user.city"]).await;
        let mut f = Fixture::new("s");
        f.turn(1, &["user.name", "user.city"])
            .graded(1, true, "symbolic");
        f.turn(2, &["user.name"]).cited(2, &["fact:user.name"]);
        let sessions = vec![f.recorded()];
        let first = derive(&store, &sessions, "symbolic").await.unwrap();
        // Apply, exactly as the pass does, and derive again over the same log.
        for (scope, key, e, c) in &first.facts {
            store.set_fact_fitness(scope, key, *e, *c).await.unwrap();
        }
        let second = derive(&store, &sessions, "symbolic").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(numbers(&second, "user.name"), (2, 2));
        // And the store carries what was derived.
        let stored = store.facts("global", "user.name").await.unwrap();
        assert_eq!((stored[0].exposures, stored[0].credits), (2, 2));
    }

    /// The join matching nothing is the failure this whole phase is most
    /// exposed to — an off-by-one between `ModelCall.turn` and `Graded.turn`
    /// would zero every credit, and with demotion on it would demote the
    /// store. A session that was graded and exposed no fact is that failure's
    /// only visible symptom, so it is printed rather than counted.
    #[tokio::test]
    async fn a_graded_session_whose_manifests_name_no_fact_raises_the_alarm() {
        let store = store_with(&["user.name"]).await;
        let mut f = Fixture::new("blind");
        f.turn(1, &[]).graded(1, true, "symbolic");
        let r = derive(&store, &[f.recorded()], "symbolic").await.unwrap();
        assert_eq!(r.graded_sessions_with_zero_exposures, vec!["blind"]);
        assert_eq!(numbers(&r, "user.name"), (0, 0));
        assert_eq!(r.with_exposures(), 0, "the number the alarm explains");
    }

    /// Notes join on the same turn by hash, from `manifest.note_hashes`.
    #[tokio::test]
    async fn notes_are_credited_by_hash_on_the_same_join() {
        let store = store_with(&[]).await;
        let hash = nscore::Note::hash_of("global", "prefer the shortest action");
        let mut log = EventLog::new(SessionId("n".into()));
        for (turn, ok) in [(1u32, true), (2, false)] {
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::ModelCall {
                    usage: usage(),
                    manifest: ContextManifest {
                        guidance: 1,
                        note_hashes: vec![hash.clone()],
                        ..Default::default()
                    },
                },
            );
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::Graded {
                    turn,
                    grade: Grade { ok, issues: vec![] },
                    by: "symbolic".into(),
                    revision: "r1".into(),
                },
            );
        }
        let sessions = vec![(log.session().clone(), log.events().to_vec())];
        let r = derive(&store, &sessions, "symbolic").await.unwrap();
        assert_eq!(
            r.notes.get(&hash).copied(),
            Some(NoteFitness {
                exposures: 2,
                credits: 1
            })
        );
        // A graded session that showed a note but no fact is still the alarm:
        // the demotion signal reads facts, and it saw none.
        assert_eq!(r.graded_sessions_with_zero_exposures, vec!["n"]);
    }

    #[allow(dead_code)]
    fn _event_type_is_used(_: Event) {}
}
