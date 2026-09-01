use async_trait::async_trait;
use nscore::{ArtifactId, Event, EventId, Fact, MemoryStore, SessionId, StoreError, Timestamp};
use rusqlite::Connection;
use std::path::Path;
use tokio::sync::Mutex;

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

fn io_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Io(e.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Result<[u8; 32], StoreError> {
    if s.len() != 64 {
        return Err(StoreError::Io("hash must be 64 hex chars".into()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(io_err)?;
    }
    Ok(out)
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(io_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(io_err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 id         INTEGER NOT NULL,
                 parent     INTEGER,
                 prev_hash  TEXT NOT NULL,
                 turn       INTEGER NOT NULL,
                 at         INTEGER NOT NULL,
                 kind_json  TEXT NOT NULL,
                 PRIMARY KEY (session_id, id)
             );
             CREATE TABLE IF NOT EXISTS facts (
                 key            TEXT PRIMARY KEY,
                 value_json     TEXT NOT NULL,
                 confidence     REAL NOT NULL,
                 uses           INTEGER NOT NULL,
                 last_validated INTEGER NOT NULL,
                 prov_json      TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS artifacts (
                 id      TEXT PRIMARY KEY,
                 content BLOB NOT NULL
             );",
        )
        .map_err(io_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}

#[async_trait]
impl MemoryStore for SqliteStore {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        let max_id: Option<u64> = conn
            .query_row(
                "SELECT MAX(id) FROM events WHERE session_id = ?1",
                [&session.0],
                |r| r.get(0),
            )
            .map_err(io_err)?;
        let floor = max_id.unwrap_or(0);
        for e in events.iter().filter(|e| e.id.0 > floor) {
            conn.execute(
                "INSERT INTO events (session_id, id, parent, prev_hash, turn, at, kind_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    session.0,
                    e.id.0,
                    e.parent.map(|p| p.0),
                    hex(&e.prev_hash),
                    e.turn,
                    e.at.0,
                    serde_json::to_string(&e.kind).map_err(io_err)?,
                ],
            )
            .map_err(io_err)?;
        }
        Ok(())
    }

    async fn load(&self, session: &SessionId) -> Result<Vec<Event>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, parent, prev_hash, turn, at, kind_json
                 FROM events WHERE session_id = ?1 ORDER BY id",
            )
            .map_err(io_err)?;
        let rows = stmt
            .query_map([&session.0], |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, Option<u64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, u64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, parent, prev_hash, turn, at, kind_json) = row.map_err(io_err)?;
            out.push(Event {
                id: EventId(id),
                parent: parent.map(EventId),
                prev_hash: unhex32(&prev_hash)?,
                turn,
                at: Timestamp(at),
                kind: serde_json::from_str(&kind_json).map_err(io_err)?,
            });
        }
        Ok(out)
    }

    async fn facts(&self, key_prefix: &str) -> Result<Vec<Fact>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT key, value_json, confidence, uses, last_validated, prov_json
                 FROM facts WHERE key >= ?1 AND key < ?1 || x'7F' ORDER BY key",
            )
            .map_err(io_err)?;
        let rows = stmt
            .query_map([key_prefix], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, u64>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value_json, confidence, uses, last_validated, prov_json) =
                row.map_err(io_err)?;
            out.push(Fact {
                key,
                value: serde_json::from_str(&value_json).map_err(io_err)?,
                confidence: confidence as f32,
                uses,
                last_validated: Timestamp(last_validated),
                prov: serde_json::from_str(&prov_json).map_err(io_err)?,
            });
        }
        Ok(out)
    }

    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO facts (key, value_json, confidence, uses, last_validated, prov_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET
                 value_json = excluded.value_json, confidence = excluded.confidence,
                 uses = excluded.uses, last_validated = excluded.last_validated,
                 prov_json = excluded.prov_json",
            rusqlite::params![
                fact.key,
                serde_json::to_string(&fact.value).map_err(io_err)?,
                fact.confidence as f64,
                fact.uses,
                fact.last_validated.0,
                serde_json::to_string(&fact.prov).map_err(io_err)?,
            ],
        )
        .map_err(io_err)?;
        Ok(())
    }

    async fn artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, StoreError> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT content FROM artifacts WHERE id = ?1",
            [hex(&id.0)],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound,
            other => io_err(other),
        })
    }

    async fn put_artifact(&self, content: Vec<u8>) -> Result<ArtifactId, StoreError> {
        let id = ArtifactId::for_content(&content);
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO artifacts (id, content) VALUES (?1, ?2)",
            rusqlite::params![hex(&id.0), content],
        )
        .map_err(io_err)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn tmp_store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("t.sqlite")).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn append_load_round_trips_and_chain_verifies() {
        let (_d, store) = tmp_store();
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        log.append(1, Timestamp(2), EventKind::Replied { text: "hello".into() });
        store.append(&sid, log.events()).await.unwrap();

        let loaded = store.load(&sid).await.unwrap();
        assert_eq!(loaded, log.events().to_vec());
        assert!(EventLog::from_events(sid.clone(), loaded).verify_chain().is_ok());
        assert_eq!(store.load(&SessionId("other".into())).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn append_is_idempotent_by_event_id() {
        let (_d, store) = tmp_store();
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "a".into() });
        store.append(&sid, log.events()).await.unwrap();
        log.append(1, Timestamp(2), EventKind::Replied { text: "b".into() });
        // re-append the WHOLE log: event 1 must be skipped, event 2 stored
        store.append(&sid, log.events()).await.unwrap();
        assert_eq!(store.load(&sid).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        let sid = SessionId("s".into());
        {
            let store = SqliteStore::open(&path).unwrap();
            let mut log = EventLog::new(sid.clone());
            log.append(1, Timestamp(1), EventKind::UserSaid { text: "persist me".into() });
            store.append(&sid, log.events()).await.unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let loaded = store.load(&sid).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(&loaded[0].kind, EventKind::UserSaid { text } if text == "persist me"));
    }

    #[tokio::test]
    async fn facts_upsert_and_prefix_query() {
        let (_d, store) = tmp_store();
        let mut f = Fact {
            key: "user.prefs.lang".into(),
            value: serde_json::json!("cs"),
            confidence: 1.0,
            uses: 0,
            last_validated: Timestamp(1),
            prov: Provenance::Constant,
        };
        store.put_fact(f.clone()).await.unwrap();
        f.value = serde_json::json!("en");
        store.put_fact(f.clone()).await.unwrap(); // upsert same key
        assert_eq!(store.facts("user.prefs").await.unwrap(), vec![f]);
        assert_eq!(store.facts("orders").await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn artifacts_are_content_addressed() {
        let (_d, store) = tmp_store();
        let id = store.put_artifact(b"payload".to_vec()).await.unwrap();
        assert_eq!(id, ArtifactId::for_content(b"payload"));
        assert_eq!(store.artifact(&id).await.unwrap(), b"payload".to_vec());
        assert!(matches!(store.artifact(&ArtifactId([9u8; 32])).await, Err(StoreError::NotFound)));
    }
}
