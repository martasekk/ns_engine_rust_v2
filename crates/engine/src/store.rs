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
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
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
        hits.truncate(k);
        Ok(hits)
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
        Ok(nscore::lexical_rank(&current, query, k))
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
