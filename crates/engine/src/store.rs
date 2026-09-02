use async_trait::async_trait;
use nscore::{ArtifactId, Consolidator, Event, Fact, MemoryStore, SessionId, StoreError};
use std::collections::HashMap;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct InMemoryStore {
    events: Mutex<HashMap<SessionId, Vec<Event>>>,
    facts: Mutex<HashMap<String, Fact>>,
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

    async fn facts(&self, key_prefix: &str) -> Result<Vec<Fact>, StoreError> {
        let map = self.facts.lock().await;
        let mut out: Vec<Fact> = map
            .values()
            .filter(|f| f.key.starts_with(key_prefix))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError> {
        self.facts.lock().await.insert(fact.key.clone(), fact);
        Ok(())
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
            confidence: 1.0,
            uses: 0,
            last_validated: Timestamp(1),
            prov: Provenance::Constant,
        };
        store.put_fact(f.clone()).await.unwrap();
        assert_eq!(store.facts("user.prefs").await.unwrap(), vec![f]);
        assert_eq!(store.facts("orders").await.unwrap(), vec![]);
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
