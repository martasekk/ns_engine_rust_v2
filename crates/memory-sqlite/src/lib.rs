use async_trait::async_trait;
use nscore::{
    ArtifactId, Event, EventId, Fact, FactState, MemoryStore, SessionId, StoreError, Timestamp,
    Trust,
};
use rusqlite::{Connection, OptionalExtension};
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

fn trust_str(t: Trust) -> &'static str {
    match t {
        Trust::External => "External",
        Trust::System => "System",
        Trust::User => "User",
    }
}

fn parse_trust(s: &str) -> Trust {
    match s {
        "External" => Trust::External,
        "User" => Trust::User,
        _ => Trust::System,
    }
}

const FACT_COLUMNS: &str =
    "scope, key, valid_from, valid_to, state, value_json, confidence, uses, \
                            last_validated, prov_json, trust, last_used";

fn row_to_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    let value_json: String = r.get(5)?;
    let prov_json: String = r.get(9)?;
    let state: String = r.get(4)?;
    let trust: String = r.get(10)?;
    Ok(Fact {
        scope: r.get(0)?,
        key: r.get(1)?,
        valid_from: Timestamp(r.get::<_, u64>(2)?),
        valid_to: r.get::<_, Option<u64>>(3)?.map(Timestamp),
        state: FactState::parse(&state).unwrap_or_default(),
        value: serde_json::from_str(&value_json).unwrap_or(serde_json::Value::Null),
        confidence: r.get::<_, f64>(6)? as f32,
        uses: r.get(7)?,
        last_validated: Timestamp(r.get::<_, u64>(8)?),
        prov: serde_json::from_str(&prov_json).unwrap_or(nscore::Provenance::Residual),
        trust: parse_trust(&trust),
        last_used: Timestamp(r.get::<_, u64>(11)?),
    })
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(io_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(io_err)?;
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
             CREATE TABLE IF NOT EXISTS artifacts (
                 id      TEXT PRIMARY KEY,
                 content BLOB NOT NULL
             );",
        )
        .map_err(io_err)?;
        Self::migrate_facts(&conn)?;
        Self::ensure_events_fts(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// M6 §7: full-text index over the event log (external-content FTS5 on
    /// `events.kind_json`, kept in sync by an insert trigger; events are
    /// append-only). Built once for a pre-existing log.
    fn ensure_events_fts(conn: &Connection) -> Result<(), StoreError> {
        let existed: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'events_fts'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(io_err)?
            > 0;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                 kind_json, content='events', content_rowid='rowid'
             );
             CREATE TRIGGER IF NOT EXISTS events_fts_ai AFTER INSERT ON events BEGIN
                 INSERT INTO events_fts(rowid, kind_json) VALUES (new.rowid, new.kind_json);
             END;",
        )
        .map_err(io_err)?;
        if !existed {
            conn.execute_batch("INSERT INTO events_fts(events_fts) VALUES('rebuild')")
                .map_err(io_err)?;
        }
        Ok(())
    }

    /// M6 §6.1: facts are versioned by `(scope, key, valid_from)`. A pre-M6
    /// `facts(key PRIMARY KEY, …)` table is carried over as one `current`
    /// version per key in the `global` scope, `valid_from = last_validated`.
    fn migrate_facts(conn: &Connection) -> Result<(), StoreError> {
        let has_facts: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'facts'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(io_err)?
            > 0;
        let versioned = if has_facts {
            let mut stmt = conn.prepare("PRAGMA table_info(facts)").map_err(io_err)?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(io_err)?
                .collect::<Result<_, _>>()
                .map_err(io_err)?;
            cols.iter().any(|c| c == "scope")
        } else {
            false
        };
        if has_facts && !versioned {
            conn.execute_batch("ALTER TABLE facts RENAME TO facts_v1")
                .map_err(io_err)?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS facts (
                 scope          TEXT NOT NULL DEFAULT 'global',
                 key            TEXT NOT NULL,
                 valid_from     INTEGER NOT NULL,
                 valid_to       INTEGER,
                 state          TEXT NOT NULL DEFAULT 'current',
                 value_json     TEXT NOT NULL,
                 confidence     REAL NOT NULL,
                 uses           INTEGER NOT NULL,
                 last_validated INTEGER NOT NULL,
                 prov_json      TEXT NOT NULL,
                 trust          TEXT NOT NULL DEFAULT 'System',
                 last_used      INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (scope, key, valid_from)
             );
             CREATE INDEX IF NOT EXISTS facts_current ON facts(scope, state, key);",
        )
        .map_err(io_err)?;
        if has_facts && !versioned {
            conn.execute_batch(
                "INSERT INTO facts (scope, key, valid_from, valid_to, state, value_json, confidence,
                                    uses, last_validated, prov_json, trust, last_used)
                 SELECT 'global', key, last_validated, NULL, 'current', value_json, confidence,
                        uses, last_validated, prov_json, 'System', last_validated
                 FROM facts_v1;
                 DROP TABLE facts_v1;",
            )
            .map_err(io_err)?;
        }
        Ok(())
    }
}

/// FTS5 MATCH expression: each 3+ char token quoted, OR-joined. None when
/// nothing is worth matching.
fn fts_query(query: &str) -> Option<String> {
    let tokens = nscore::query_tokens(query);
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
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

    async fn search_turns(
        &self,
        session: &SessionId,
        query: &str,
        k: usize,
    ) -> Result<Vec<nscore::TurnHit>, StoreError> {
        let Some(expr) = fts_query(query) else {
            return Ok(vec![]);
        };
        if k == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT e.turn, e.kind_json, bm25(events_fts) AS score
                 FROM events_fts JOIN events e ON e.rowid = events_fts.rowid
                 WHERE events_fts MATCH ?1 AND e.session_id = ?2
                   AND (e.kind_json LIKE '{\"type\":\"UserSaid\"%'
                        OR e.kind_json LIKE '{\"type\":\"Replied\"%')
                 ORDER BY score, e.turn DESC LIMIT ?3",
            )
            .map_err(io_err)?;
        let rows = stmt
            .query_map(rusqlite::params![expr, session.0, k as i64], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (turn, kind_json, score) = row.map_err(io_err)?;
            let kind: nscore::EventKind = serde_json::from_str(&kind_json).map_err(io_err)?;
            let (speaker, text) = match kind {
                nscore::EventKind::UserSaid { text } => ("user", text),
                nscore::EventKind::Replied { text } => ("bot", text),
                _ => continue,
            };
            out.push(nscore::TurnHit {
                turn,
                speaker,
                text,
                // bm25 is negative, lower = better; flip so higher is better.
                score: -score,
            });
        }
        Ok(out)
    }

    async fn facts(&self, scope: &str, key_prefix: &str) -> Result<Vec<Fact>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {FACT_COLUMNS} FROM facts
                 WHERE scope = ?1 AND state IN ('current', 'cold')
                   AND key >= ?2 AND key < ?2 || x'7F'
                 ORDER BY key"
            ))
            .map_err(io_err)?;
        let rows = stmt
            .query_map(rusqlite::params![scope, key_prefix], row_to_fact)
            .map_err(io_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_err)
    }

    async fn fact_history(&self, scope: &str, key: &str) -> Result<Vec<Fact>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {FACT_COLUMNS} FROM facts WHERE scope = ?1 AND key = ?2
                 ORDER BY valid_from DESC"
            ))
            .map_err(io_err)?;
        let rows = stmt
            .query_map(rusqlite::params![scope, key], row_to_fact)
            .map_err(io_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_err)
    }

    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM facts WHERE scope = ?1 AND key = ?2 AND valid_from = ?3",
                rusqlite::params![fact.scope, fact.key, fact.valid_from.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(io_err)?;
        let value_json = serde_json::to_string(&fact.value).map_err(io_err)?;
        let prov_json = serde_json::to_string(&fact.prov).map_err(io_err)?;
        if exists.is_some() {
            conn.execute(
                "UPDATE facts SET valid_to = ?4, state = ?5, value_json = ?6, confidence = ?7,
                     uses = ?8, last_validated = ?9, prov_json = ?10, trust = ?11, last_used = ?12
                 WHERE scope = ?1 AND key = ?2 AND valid_from = ?3",
                rusqlite::params![
                    fact.scope,
                    fact.key,
                    fact.valid_from.0,
                    fact.valid_to.map(|t| t.0),
                    fact.state.as_str(),
                    value_json,
                    fact.confidence as f64,
                    fact.uses,
                    fact.last_validated.0,
                    prov_json,
                    trust_str(fact.trust),
                    fact.last_used.0,
                ],
            )
            .map_err(io_err)?;
            return Ok(());
        }
        conn.execute(
            "UPDATE facts SET state = 'superseded', valid_to = ?3
             WHERE scope = ?1 AND key = ?2 AND state IN ('current', 'cold')",
            rusqlite::params![fact.scope, fact.key, fact.valid_from.0],
        )
        .map_err(io_err)?;
        conn.execute(
            &format!(
                "INSERT INTO facts ({FACT_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
            ),
            rusqlite::params![
                fact.scope,
                fact.key,
                fact.valid_from.0,
                fact.valid_to.map(|t| t.0),
                fact.state.as_str(),
                value_json,
                fact.confidence as f64,
                fact.uses,
                fact.last_validated.0,
                prov_json,
                trust_str(fact.trust),
                fact.last_used.0,
            ],
        )
        .map_err(io_err)?;
        Ok(())
    }

    async fn forget_fact(&self, scope: &str, key: &str, at: Timestamp) -> Result<bool, StoreError> {
        let conn = self.conn.lock().await;
        let n = conn
            .execute(
                "UPDATE facts SET state = 'forgotten', valid_to = ?3
                 WHERE scope = ?1 AND key = ?2 AND state IN ('current', 'cold')",
                rusqlite::params![scope, key, at.0],
            )
            .map_err(io_err)?;
        Ok(n > 0)
    }

    async fn purge_facts(&self, scope: &str) -> Result<usize, StoreError> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM facts WHERE scope = ?1", [scope])
            .map_err(io_err)
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
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare("SELECT DISTINCT scope FROM facts ORDER BY scope")
            .map_err(io_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(io_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_err)
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

    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, MAX(at) AS last FROM events
                 GROUP BY session_id ORDER BY last DESC, session_id DESC",
            )
            .map_err(io_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(SessionId(row.map_err(io_err)?));
        }
        Ok(out)
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
        log.append(
            1,
            Timestamp(2),
            EventKind::Replied {
                text: "hello".into(),
            },
        );
        store.append(&sid, log.events()).await.unwrap();

        let loaded = store.load(&sid).await.unwrap();
        assert_eq!(loaded, log.events().to_vec());
        assert!(EventLog::from_events(sid.clone(), loaded)
            .verify_chain()
            .is_ok());
        assert_eq!(
            store.load(&SessionId("other".into())).await.unwrap(),
            vec![]
        );
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
            log.append(
                1,
                Timestamp(1),
                EventKind::UserSaid {
                    text: "persist me".into(),
                },
            );
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
            valid_from: Timestamp(1),
            ..Default::default()
        };
        store.put_fact(f.clone()).await.unwrap();
        f.value = serde_json::json!("en");
        store.put_fact(f.clone()).await.unwrap(); // same valid_from: in place
        assert_eq!(store.facts("global", "user.prefs").await.unwrap(), vec![f]);
        assert_eq!(store.facts("global", "orders").await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn fact_versions_scopes_search_forget_and_purge() {
        let (_d, store) = tmp_store();
        nsengine_conformance(&store).await;
    }

    /// The shared conformance suite lives in ns-engine's store module; this
    /// crate cannot depend on ns-engine, so the same assertions are inlined.
    async fn nsengine_conformance(store: &dyn MemoryStore) {
        let f = |key: &str, value: &str, at: u64| Fact {
            key: key.into(),
            value: serde_json::json!(value),
            last_validated: Timestamp(at),
            valid_from: Timestamp(at),
            prov: Provenance::Constant,
            ..Default::default()
        };
        store.put_fact(f("user.name", "Martin", 10)).await.unwrap();
        let mut bumped = f("user.name", "Martin", 10);
        bumped.uses = 3;
        store.put_fact(bumped).await.unwrap();
        let cur = store.facts("global", "user").await.unwrap();
        assert_eq!((cur.len(), cur[0].uses), (1, 3));
        store.put_fact(f("user.name", "Peter", 20)).await.unwrap();
        let cur = store.facts("global", "user.name").await.unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].value, serde_json::json!("Peter"));
        let hist = store.fact_history("global", "user.name").await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[1].state, FactState::Superseded);
        assert_eq!(hist[1].valid_to, Some(Timestamp(20)));
        let mut other = f("user.name", "Jana", 30);
        other.scope = "chat42".into();
        store.put_fact(other).await.unwrap();
        assert_eq!(store.facts("global", "").await.unwrap().len(), 1);
        assert_eq!(
            store.scopes().await.unwrap(),
            vec!["chat42".to_string(), "global".to_string()]
        );
        store.put_fact(f("user.city", "Brno", 40)).await.unwrap();
        let hits = store.search_facts("global", "which city", 5).await.unwrap();
        assert_eq!(hits[0].key, "user.city");
        assert!(store
            .forget_fact("global", "user.name", Timestamp(50))
            .await
            .unwrap());
        assert!(store.facts("global", "user.name").await.unwrap().is_empty());
        assert_eq!(
            store.fact_history("global", "user.name").await.unwrap()[0].state,
            FactState::Forgotten
        );
        assert!(!store
            .forget_fact("global", "user.name", Timestamp(51))
            .await
            .unwrap());
        store.put_fact(f("user.name", "Martin", 60)).await.unwrap();
        assert_eq!(
            store
                .fact_history("global", "user.name")
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(store.purge_facts("global").await.unwrap(), 4);
        assert!(store.facts("global", "").await.unwrap().is_empty());
        assert_eq!(store.facts("chat42", "").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_turns_uses_fts5_and_indexes_a_pre_existing_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts.sqlite");
        let sid = SessionId("cli".into());
        {
            let store = SqliteStore::open(&path).unwrap();
            let mut log = EventLog::new(sid.clone());
            for (turn, user, bot) in [
                (1, "what time is it", "It is noon."),
                (2, "remember my name is Martin", "Got it."),
                (3, "and the time again, please?", "Still noon."),
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
            let hits = store.search_turns(&sid, "what time?", 5).await.unwrap();
            assert_eq!(hits[0].turn, 1, "{hits:?}");
            assert_eq!(hits[0].speaker, "user");
            assert!(hits.iter().all(|h| h.text.contains("time")));
            assert!(store.search_turns(&sid, "hi", 5).await.unwrap().is_empty());
            assert!(store
                .search_turns(&SessionId("other".into()), "time", 5)
                .await
                .unwrap()
                .is_empty());
            // proposals / tool outputs are not returned as turns
            assert!(store
                .search_turns(&sid, "martin", 5)
                .await
                .unwrap()
                .iter()
                .all(|h| h.speaker == "user"));
        }
        // A log written before the index existed is indexed on open.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("DROP TRIGGER events_fts_ai; DROP TABLE events_fts;")
                .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let hits = store.search_turns(&sid, "noon", 5).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.speaker == "bot"));
    }

    #[tokio::test]
    async fn pre_m6_facts_table_is_migrated_to_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE facts (
                     key TEXT PRIMARY KEY, value_json TEXT NOT NULL, confidence REAL NOT NULL,
                     uses INTEGER NOT NULL, last_validated INTEGER NOT NULL, prov_json TEXT NOT NULL);
                 INSERT INTO facts VALUES ('user.name', '\"Peter\"', 1.0, 14, 1788370524628, '{\"type\":\"Residual\"}');
                 INSERT INTO facts VALUES ('user.age', '\"17\"', 1.0, 14, 1788370533598, '{\"type\":\"Residual\"}');",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let facts = store.facts("global", "").await.unwrap();
        assert_eq!(facts.len(), 2);
        let name = facts.iter().find(|f| f.key == "user.name").unwrap();
        assert_eq!(name.value, serde_json::json!("Peter"));
        assert_eq!(name.uses, 14);
        assert_eq!(name.valid_from, Timestamp(1788370524628));
        assert_eq!(name.state, FactState::Current);
        assert_eq!(name.trust, Trust::System);
        assert_eq!(
            name.last_used,
            Timestamp(1788370524628),
            "migrated rows count as just used"
        );
        // reopening is a no-op
        drop(store);
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.facts("global", "").await.unwrap().len(), 2);
        // and the new schema takes versions
        store
            .put_fact(Fact {
                key: "user.name".into(),
                value: serde_json::json!("Martin"),
                valid_from: Timestamp(1788370524629),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .fact_history("global", "user.name")
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn artifacts_are_content_addressed() {
        let (_d, store) = tmp_store();
        let id = store.put_artifact(b"payload".to_vec()).await.unwrap();
        assert_eq!(id, ArtifactId::for_content(b"payload"));
        assert_eq!(store.artifact(&id).await.unwrap(), b"payload".to_vec());
        assert!(matches!(
            store.artifact(&ArtifactId([9u8; 32])).await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn sessions_lists_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("s.sqlite")).unwrap();
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
