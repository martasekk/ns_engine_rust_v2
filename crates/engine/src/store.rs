use async_trait::async_trait;
use nscore::{
    ArtifactId, Consolidator, Event, Fact, FactState, MemoryStore, SessionId, StoreError, Timestamp,
};
use std::collections::HashMap;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct InMemoryStore {
    events: Mutex<HashMap<SessionId, Vec<Event>>>,
    /// Every fact version, in insertion order (M6 §6.1: never overwritten).
    facts: Mutex<Vec<Fact>>,
    artifacts: Mutex<HashMap<ArtifactId, Vec<u8>>>,
    /// One digest per session, replaced on rewrite (M7 §8). Keyed the way
    /// the trait says writes are idempotent, so the map cannot hold the
    /// duplicate a re-digested session would otherwise create.
    digests: Mutex<HashMap<SessionId, nscore::SessionDigest>>,
    /// M9 T3.1/T3.2: the activation prior's knobs, off by default.
    ///
    /// On the struct rather than on the trait method because the trait has
    /// four implementors and two of them are test doubles that have no
    /// opinion about ranking. A builder keeps every existing `new()` call
    /// site — and both conformance suites — exactly as it was.
    activation: nscore::Activation,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// M9 T3.1/T3.2: rank with the activation prior at this weight.
    ///
    /// `half_life_days` decays the fact term; the turn term's half-life is
    /// [`nscore::RECENCY_HALF_LIFE_TURNS`], a constant, because turns are not
    /// days. `weight = 0.0` is the default and is today's behaviour exactly.
    pub fn with_activation(mut self, weight: f32, half_life_days: f32) -> Self {
        self.activation.weight = weight;
        self.activation.half_life_days = half_life_days;
        self
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<(), StoreError> {
        let mut map = self.events.lock().await;
        let stored = map.entry(session.clone()).or_default();
        let last_id = stored.last().map(|e| e.id.0).unwrap_or(0);
        for e in events {
            if e.id.0 > last_id {
                stored.push(e.clone());
            }
        }
        Ok(())
    }

    async fn load(&self, session: &SessionId) -> Result<Vec<Event>, StoreError> {
        Ok(self
            .events
            .lock()
            .await
            .get(session)
            .cloned()
            .unwrap_or_default())
    }

    /// Token-count ranking over UserSaid/Replied text; ties newest first.
    async fn search_turns(
        &self,
        session: &SessionId,
        query: &str,
        k: usize,
    ) -> Result<Vec<nscore::TurnHit>, StoreError> {
        let tokens = nscore::query_tokens(query);
        if tokens.is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let events = self.load(session).await?;
        let mut hits: Vec<nscore::TurnHit> = events
            .iter()
            .filter_map(|e| match &e.kind {
                nscore::EventKind::UserSaid { text } => Some((e.turn, "user", text)),
                nscore::EventKind::Replied { text } => Some((e.turn, "bot", text)),
                _ => None,
            })
            .map(|(turn, speaker, text)| {
                let hay = text.to_lowercase();
                let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count() as f64;
                nscore::TurnHit {
                    session: session.clone(),
                    turn,
                    speaker,
                    text: text.clone(),
                    score,
                }
            })
            .filter(|h| h.score > 0.0)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.turn.cmp(&a.turn))
        });
        // M9 T3.2: the lexical top candidates, rescored by recency, then cut
        // to `k`. At weight 0 `rescore_by_recency` only truncates, so this is
        // the same list in the same order it has always been.
        hits.truncate(nscore::recency_candidates(k));
        nscore::rescore_by_recency(&mut hits, self.activation.weight, k);
        Ok(hits)
    }

    async fn put_session_digest(&self, digest: &nscore::SessionDigest) -> Result<(), StoreError> {
        self.digests
            .lock()
            .await
            .insert(digest.session.clone(), digest.clone());
        Ok(())
    }

    async fn session_digests(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<nscore::SessionDigest>, StoreError> {
        let all = self.digests.lock().await;
        let mut out: Vec<nscore::SessionDigest> =
            all.values().filter(|d| d.scope == scope).cloned().collect();
        // HashMap iteration order is not stable across runs, so the session
        // id is the tiebreak: two digests written in the same millisecond
        // must still come out in one order, or a replay diff moves.
        out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.session.0.cmp(&a.session.0)));
        out.truncate(limit);
        Ok(out)
    }

    /// Substring match over the digest's own text, scored like
    /// `search_turns`: this twin is what the engine's tests search, so it
    /// has to return the same shape of answer as FTS5 does, not nothing.
    async fn search_digests(
        &self,
        scope: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<nscore::SessionDigest>, StoreError> {
        let tokens = nscore::query_tokens(query);
        if tokens.is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let mut scored: Vec<(usize, nscore::SessionDigest)> = self
            .session_digests(scope, usize::MAX)
            .await?
            .into_iter()
            .map(|d| {
                let hay = format!(
                    "{} {} {}",
                    d.summary.topic,
                    d.summary.established.join(" "),
                    d.summary.open.join(" ")
                )
                .to_lowercase();
                let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
                (score, d)
            })
            .filter(|(score, _)| *score > 0)
            .collect();
        // `sort_by_key` is stable and the list is already in `at` DESC,
        // session id DESC order, so equal scores keep that order instead of
        // coming out in whatever order the HashMap iterated: a recall that
        // is replayed has to return the same digests in the same places.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        Ok(scored.into_iter().take(k).map(|(_, d)| d).collect())
    }

    async fn facts(&self, scope: &str, key_prefix: &str) -> Result<Vec<Fact>, StoreError> {
        let all = self.facts.lock().await;
        let mut out: Vec<Fact> = all
            .iter()
            .filter(|f| f.scope == scope && matches!(f.state, FactState::Current | FactState::Cold))
            .filter(|f| f.key.starts_with(key_prefix))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn fact_history(&self, scope: &str, key: &str) -> Result<Vec<Fact>, StoreError> {
        let all = self.facts.lock().await;
        let mut out: Vec<Fact> = all
            .iter()
            .filter(|f| f.scope == scope && f.key == key)
            .cloned()
            .collect();
        out.sort_by_key(|f| std::cmp::Reverse(f.valid_from));
        Ok(out)
    }

    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError> {
        let mut all = self.facts.lock().await;
        if let Some(row) = all
            .iter_mut()
            .find(|f| f.scope == fact.scope && f.key == fact.key && f.valid_from == fact.valid_from)
        {
            *row = fact;
            return Ok(());
        }
        for row in all.iter_mut().filter(|f| {
            f.scope == fact.scope
                && f.key == fact.key
                && matches!(f.state, FactState::Current | FactState::Cold)
        }) {
            row.state = FactState::Superseded;
            row.valid_to = Some(fact.valid_from);
        }
        all.push(fact);
        Ok(())
    }

    /// M9 T4.3: the two derived columns on the current version, in place.
    /// Overridden rather than left to the trait's default only so the
    /// in-memory store cannot drift from the SQLite one — it is the store
    /// every engine test ranks against.
    async fn set_fact_fitness(
        &self,
        scope: &str,
        key: &str,
        exposures: u32,
        credits: u32,
    ) -> Result<(), StoreError> {
        let mut all = self.facts.lock().await;
        for row in all.iter_mut().filter(|f| {
            f.scope == scope
                && f.key == key
                && matches!(f.state, FactState::Current | FactState::Cold)
        }) {
            row.exposures = exposures;
            row.credits = credits;
        }
        Ok(())
    }

    async fn forget_fact(&self, scope: &str, key: &str, at: Timestamp) -> Result<bool, StoreError> {
        let mut all = self.facts.lock().await;
        let mut hit = false;
        for row in all
            .iter_mut()
            .filter(|f| f.scope == scope && f.key == key && f.state != FactState::Forgotten)
            .filter(|f| f.state != FactState::Superseded)
        {
            row.state = FactState::Forgotten;
            row.valid_to = Some(at);
            hit = true;
        }
        Ok(hit)
    }

    async fn purge_facts(&self, scope: &str) -> Result<usize, StoreError> {
        let mut all = self.facts.lock().await;
        let before = all.len();
        all.retain(|f| f.scope != scope);
        Ok(before - all.len())
    }

    async fn search_facts(
        &self,
        scope: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Fact>, StoreError> {
        let current = self.facts(scope, "").await?;
        Ok(nscore::lexical_rank(
            &current,
            query,
            k,
            nscore::Activation {
                now: Timestamp(nscore::now_ms()),
                ..self.activation
            },
        ))
    }

    async fn scopes(&self) -> Result<Vec<String>, StoreError> {
        let all = self.facts.lock().await;
        let mut out: Vec<String> = all.iter().map(|f| f.scope.clone()).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, StoreError> {
        self.artifacts
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or(StoreError::NotFound)
    }

    async fn put_artifact(&self, content: Vec<u8>) -> Result<ArtifactId, StoreError> {
        let id = ArtifactId::for_content(&content);
        self.artifacts.lock().await.insert(id, content);
        Ok(id)
    }

    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        let map = self.events.lock().await;
        let mut v: Vec<(u64, SessionId)> = map
            .iter()
            .filter(|(_, evs)| !evs.is_empty())
            .map(|(sid, evs)| (evs.iter().map(|e| e.at.0).max().unwrap_or(0), sid.clone()))
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1 .0.cmp(&a.1 .0)));
        Ok(v.into_iter().map(|(_, s)| s).collect())
    }
}

pub struct NoopConsolidator;

#[async_trait]
impl Consolidator for NoopConsolidator {
    async fn run(&self, _store: &dyn MemoryStore) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Shared conformance tests for `MemoryStore` fact semantics (M6 §6.1–6.2),
/// run by every store implementation.
pub async fn fact_conformance(store: &dyn MemoryStore) {
    let f = |key: &str, value: &str, at: u64| Fact {
        key: key.into(),
        value: serde_json::json!(value),
        confidence: 1.0,
        uses: 0,
        last_validated: Timestamp(at),
        prov: nscore::Provenance::Constant,
        valid_from: Timestamp(at),
        ..Default::default()
    };
    // first version
    store.put_fact(f("user.name", "Martin", 10)).await.unwrap();
    // same valid_from: in-place update (a restatement / a uses bump)
    let mut bumped = f("user.name", "Martin", 10);
    bumped.uses = 3;
    store.put_fact(bumped).await.unwrap();
    let cur = store.facts("global", "user").await.unwrap();
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].uses, 3);
    // new value: supersedes, never overwrites
    store.put_fact(f("user.name", "Peter", 20)).await.unwrap();
    let cur = store.facts("global", "user.name").await.unwrap();
    assert_eq!(cur.len(), 1, "one current version");
    assert_eq!(cur[0].value, serde_json::json!("Peter"));
    let hist = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(hist.len(), 2, "history keeps both");
    assert_eq!(hist[0].value, serde_json::json!("Peter"), "newest first");
    assert_eq!(hist[1].state, FactState::Superseded);
    assert_eq!(hist[1].valid_to, Some(Timestamp(20)));
    // scope isolation
    let mut other = f("user.name", "Jana", 30);
    other.scope = "chat42".into();
    store.put_fact(other).await.unwrap();
    assert_eq!(store.facts("global", "").await.unwrap().len(), 1);
    assert_eq!(
        store.facts("chat42", "").await.unwrap()[0].value,
        serde_json::json!("Jana")
    );
    let mut scopes = store.scopes().await.unwrap();
    scopes.sort();
    assert_eq!(scopes, vec!["chat42".to_string(), "global".to_string()]);
    // lexical search over current facts only
    store.put_fact(f("user.city", "Brno", 40)).await.unwrap();
    let hits = store.search_facts("global", "which city", 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, "user.city");
    assert!(
        store
            .search_facts("global", "martin", 5)
            .await
            .unwrap()
            .is_empty(),
        "superseded values are not searched"
    );
    // forget: soft, keeps history, Ok(false) when nothing is current
    assert!(store
        .forget_fact("global", "user.name", Timestamp(50))
        .await
        .unwrap());
    assert!(store.facts("global", "user.name").await.unwrap().is_empty());
    let hist = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(hist[0].state, FactState::Forgotten);
    assert_eq!(hist[0].valid_to, Some(Timestamp(50)));
    assert!(!store
        .forget_fact("global", "user.name", Timestamp(51))
        .await
        .unwrap());
    assert!(!store
        .forget_fact("global", "nope", Timestamp(51))
        .await
        .unwrap());
    // re-remember after forgetting: a fresh current version
    store.put_fact(f("user.name", "Martin", 60)).await.unwrap();
    assert_eq!(store.facts("global", "user.name").await.unwrap().len(), 1);
    assert_eq!(
        store
            .fact_history("global", "user.name")
            .await
            .unwrap()
            .len(),
        3
    );
    // purge: hard delete of one scope only
    let removed = store.purge_facts("global").await.unwrap();
    assert_eq!(removed, 4, "3 name versions + city");
    assert!(store.facts("global", "").await.unwrap().is_empty());
    assert!(store
        .fact_history("global", "user.name")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(store.facts("chat42", "").await.unwrap().len(), 1);
}

/// M9 T4.1/T4.3: the fitness counters round-trip through a write, and setting
/// them moves *only* them — no new version, no changed value.
///
/// The second half is the property the whole design rests on. The evolution
/// pass rescores every fact on every run; if that went through `put_fact`'s
/// value path it would append a version per pass, and `fact_history` — the
/// record M6 §6.1 exists for — would fill with rows that differ in nothing
/// but a counter.
pub async fn fitness_conformance(store: &dyn MemoryStore) {
    let f = |key: &str, value: &str, at: u64| Fact {
        key: key.into(),
        value: serde_json::json!(value),
        last_validated: Timestamp(at),
        valid_from: Timestamp(at),
        prov: nscore::Provenance::Constant,
        ..Default::default()
    };
    // A write carries the counters it was given.
    let mut born = f("user.name", "Martin", 10);
    born.exposures = 4;
    born.credits = 1;
    store.put_fact(born).await.unwrap();
    let cur = store.facts("global", "user.name").await.unwrap();
    assert_eq!((cur[0].exposures, cur[0].credits), (4, 1));
    // A new value supersedes, and the new version starts at 0/0.
    store.put_fact(f("user.name", "Peter", 20)).await.unwrap();
    let before = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(before.len(), 2);
    assert_eq!((before[0].exposures, before[0].credits), (0, 0));

    store
        .set_fact_fitness("global", "user.name", 9, 3)
        .await
        .unwrap();
    let after = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(after.len(), 2, "no version was added");
    let values: Vec<&serde_json::Value> = after.iter().map(|f| &f.value).collect();
    let was: Vec<&serde_json::Value> = before.iter().map(|f| &f.value).collect();
    assert_eq!(values, was, "values unchanged");
    let states: Vec<_> = after.iter().map(|f| f.state).collect();
    assert_eq!(
        states,
        before.iter().map(|f| f.state).collect::<Vec<_>>(),
        "states unchanged"
    );
    assert_eq!(
        after.iter().map(|f| f.valid_from).collect::<Vec<_>>(),
        before.iter().map(|f| f.valid_from).collect::<Vec<_>>(),
        "identities unchanged"
    );
    assert_eq!(
        (after[0].exposures, after[0].credits),
        (9, 3),
        "counters set"
    );
    assert_eq!(
        (after[1].exposures, after[1].credits),
        (4, 1),
        "the superseded version keeps the numbers it had"
    );
    // Setting is a set, not an add: a second call with smaller numbers wins.
    store
        .set_fact_fitness("global", "user.name", 2, 0)
        .await
        .unwrap();
    let cur = store.facts("global", "user.name").await.unwrap();
    assert_eq!((cur[0].exposures, cur[0].credits), (2, 0));
    // A key with no current version is a no-op, not an error.
    store
        .set_fact_fitness("global", "user.nothing", 5, 5)
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    #[tokio::test]
    async fn append_then_load_round_trips() {
        let store = InMemoryStore::new();
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "x".into() });
        store.append(&sid, log.events()).await.unwrap();
        let loaded = store.load(&sid).await.unwrap();
        assert_eq!(loaded, log.events().to_vec());
        assert_eq!(
            store.load(&SessionId("other".into())).await.unwrap(),
            vec![]
        );
    }

    /// The corpus of [`search_turns_ranks_by_token_hits_newest_first`], in a
    /// store built however `build` says.
    async fn turn_corpus(
        build: impl Fn(InMemoryStore) -> InMemoryStore,
    ) -> (InMemoryStore, SessionId) {
        let store = build(InMemoryStore::new());
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        for (turn, user, bot) in [
            (1u32, "what time is it", "It is noon."),
            (2, "remember my name is Martin", "Got it."),
            (3, "and the time again?", "Still noon."),
        ] {
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::UserSaid { text: user.into() },
            );
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::Replied { text: bot.into() },
            );
        }
        store.append(&sid, log.events()).await.unwrap();
        (store, sid)
    }

    /// M9 T3.2, the twin of the SQLite test: at weight 0 the rescoring pass
    /// is not a pass — same hits, same order, same scores as a store built
    /// without the builder at all.
    #[tokio::test]
    async fn search_turns_order_is_unchanged_at_weight_zero() {
        let (plain, sid) = turn_corpus(|s| s).await;
        let (zero, _) = turn_corpus(|s| s.with_activation(0.0, 7.0)).await;
        for q in ["noon", "time", "what time", "martin"] {
            for k in [1usize, 5] {
                let a = plain.search_turns(&sid, q, k).await.unwrap();
                let b = zero.search_turns(&sid, q, k).await.unwrap();
                assert_eq!(a, b, "{q:?} k={k}");
            }
        }
    }

    /// Turn 1 and turn 3 each say "time" once, so the token-hit score ties
    /// and only the newest-first tiebreak separates them.
    ///
    /// Which is why this arm reads the *score*, not just the order: this
    /// store's scores are whole token counts, so a fractional recency term
    /// can never outrank a real extra hit here — it can only confirm a tie
    /// that already resolved the same way. The term bites where scores are
    /// fractional, i.e. against bm25 (`memory-sqlite`, where the twin of this
    /// test does reverse an order). Recorded rather than hidden: the
    /// in-memory retriever is a test double, and T3.3's numbers come from
    /// both.
    #[tokio::test]
    async fn a_recent_turn_outranks_an_older_equal_match_at_weight_one() {
        let (off, sid) = turn_corpus(|s| s).await;
        let (on, _) = turn_corpus(|s| s.with_activation(1.0, 7.0)).await;

        let tied = off.search_turns(&sid, "time", 5).await.unwrap();
        assert_eq!(tied.len(), 2);
        assert_eq!(tied[0].score, tied[1].score, "a true tie on token hits");

        let hits = on.search_turns(&sid, "time", 5).await.unwrap();
        assert_eq!(
            hits.iter().map(|h| h.turn).collect::<Vec<_>>(),
            vec![3, 1],
            "the newer line first"
        );
        assert!(
            hits[0].score > hits[1].score,
            "and now by score, not by tiebreak: {hits:?}"
        );
        // Reorders, never admits.
        assert!(on
            .search_turns(&sid, "invoice", 5)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn search_turns_ranks_by_token_hits_newest_first() {
        let store = InMemoryStore::new();
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        for (turn, user, bot) in [
            (1, "what time is it", "It is noon."),
            (2, "remember my name is Martin", "Got it."),
            (3, "and the time again?", "Still noon."),
        ] {
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::UserSaid { text: user.into() },
            );
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::Replied { text: bot.into() },
            );
        }
        store.append(&sid, log.events()).await.unwrap();
        let hits = store.search_turns(&sid, "what time", 5).await.unwrap();
        assert_eq!(hits[0].turn, 1, "both tokens hit turn 1's user text");
        assert_eq!(hits[0].speaker, "user");
        assert_eq!(hits[1].turn, 3, "one token, newer first");
        assert!(store.search_turns(&sid, "hi", 5).await.unwrap().is_empty());
        assert!(store
            .search_turns(&SessionId("other".into()), "time", 5)
            .await
            .unwrap()
            .is_empty());
    }

    fn digest(session: &str, scope: &str, topic: &str, at: u64) -> SessionDigest {
        SessionDigest {
            session: SessionId(session.into()),
            scope: scope.into(),
            summary: SessionSummary {
                through_turn: 9,
                topic: topic.into(),
                established: vec!["the invoice was sent".into()],
                open: vec![],
                trust: Trust::External,
                rebuilt_from: 1,
            },
            last_turn: 11,
            at: Timestamp(at),
        }
    }

    /// The twin is a real store, not a stub: a re-digested session replaces
    /// its digest instead of adding a second one, and scope is a wall. Both
    /// are what the engine's tests would otherwise prove nothing about.
    #[tokio::test]
    async fn session_digests_replace_by_session_and_stay_in_scope() {
        let store = InMemoryStore::new();
        store
            .put_session_digest(&digest("s1", "global", "renewing the domain", 100))
            .await
            .unwrap();
        store
            .put_session_digest(&digest("s1", "global", "booking the flight", 200))
            .await
            .unwrap();
        store
            .put_session_digest(&digest("s2", "chat42", "renewing the domain", 300))
            .await
            .unwrap();
        let global = store.session_digests("global", 5).await.unwrap();
        assert_eq!(global.len(), 1, "one digest per session");
        assert_eq!(global[0].summary.topic, "booking the flight");
        assert_eq!(global[0].summary.trust, Trust::External, "trust survives");
        assert_eq!(store.session_digests("chat42", 5).await.unwrap().len(), 1);
        assert!(store.session_digests("other", 5).await.unwrap().is_empty());
    }

    /// A digest is found by a word of its own text, never by a word from a
    /// digest of another scope.
    #[tokio::test]
    async fn search_digests_matches_summary_text_within_one_scope() {
        let store = InMemoryStore::new();
        store
            .put_session_digest(&digest("s1", "global", "renewing the domain", 100))
            .await
            .unwrap();
        store
            .put_session_digest(&digest("s2", "chat42", "renewing the domain", 200))
            .await
            .unwrap();
        let hits = store.search_digests("global", "domain", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, SessionId("s1".into()));
        // `established` is searched too, not just the topic.
        assert_eq!(
            store
                .search_digests("global", "invoice", 5)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .search_digests("global", "unrelated", 5)
            .await
            .unwrap()
            .is_empty());
    }

    /// The trait's default `search_turns_in` — no store overrides it here —
    /// must merge the sessions into one ranking, not concatenate them.
    #[tokio::test]
    async fn search_turns_in_merges_two_sessions_by_score() {
        let store = InMemoryStore::new();
        for (name, user) in [
            ("s1", "where is the blue invoice folder"),
            ("s2", "print the invoice"),
            ("s3", "the blue invoice is late"),
        ] {
            let sid = SessionId(name.into());
            let mut log = EventLog::new(sid.clone());
            log.append(1, Timestamp(1), EventKind::UserSaid { text: user.into() });
            store.append(&sid, log.events()).await.unwrap();
        }
        let sessions = [SessionId("s2".into()), SessionId("s1".into())];
        let hits = store
            .search_turns_in(&sessions, "blue invoice", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].session,
            SessionId("s1".into()),
            "two tokens beat one, whatever order the sessions came in: {hits:?}"
        );
        assert_eq!(hits[1].session, SessionId("s2".into()));
        assert!(!hits.iter().any(|h| h.session.0 == "s3"));
        // k caps the merged list, not each session.
        assert_eq!(
            store
                .search_turns_in(&sessions, "blue invoice", 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn artifacts_are_content_addressed() {
        let store = InMemoryStore::new();
        let id = store.put_artifact(b"payload".to_vec()).await.unwrap();
        assert_eq!(id, ArtifactId::for_content(b"payload"));
        assert_eq!(store.artifact(&id).await.unwrap(), b"payload".to_vec());
        assert!(matches!(
            store.artifact(&ArtifactId([9u8; 32])).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn facts_prefix_query() {
        let store = InMemoryStore::new();
        let f = Fact {
            key: "user.prefs.lang".into(),
            value: serde_json::json!("cs"),
            ..Default::default()
        };
        store.put_fact(f.clone()).await.unwrap();
        assert_eq!(store.facts("global", "user.prefs").await.unwrap(), vec![f]);
        assert_eq!(store.facts("global", "orders").await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn fact_versions_scopes_search_forget_and_purge() {
        fact_conformance(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn setting_fitness_updates_the_current_version_in_place_without_superseding() {
        fitness_conformance(&InMemoryStore::new()).await;
    }

    #[tokio::test]
    async fn sessions_lists_newest_first() {
        let store = InMemoryStore::new();
        for (name, at) in [("old", 1u64), ("new", 9), ("mid", 5)] {
            let sid = SessionId(name.into());
            let mut log = EventLog::new(sid.clone());
            log.append(1, Timestamp(at), EventKind::UserSaid { text: "x".into() });
            store.append(&sid, log.events()).await.unwrap();
        }
        let names: Vec<String> = store
            .sessions()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.0)
            .collect();
        assert_eq!(names, vec!["new", "mid", "old"]);
    }
}
