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
    /// M9 T3.1/T3.2: the activation prior's knobs, off by default.
    ///
    /// On the struct rather than on the `MemoryStore` method: the trait has
    /// four implementors, two of them test doubles with no opinion about
    /// ranking, and a builder leaves every `open()` call site and both
    /// conformance suites untouched.
    activation: nscore::Activation,
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
                            last_validated, prov_json, trust, last_used, exposures, credits";

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
        exposures: r.get(12)?,
        credits: r.get(13)?,
    })
}

/// Digest columns in `row_to_digest` order, qualified with the `d` alias
/// every digest query gives the base table: the FTS index repeats `topic`,
/// `established_json` and `open_json`, so an unqualified list is ambiguous
/// in the join `search_digests` makes.
const DIGEST_COLUMNS: &str = "d.session_id, d.scope, d.topic, d.established_json, d.open_json, \
                              d.trust, d.through_turn, d.rebuilt_from, d.last_turn, d.at";

fn row_to_digest(r: &rusqlite::Row<'_>) -> rusqlite::Result<nscore::SessionDigest> {
    let established: String = r.get(3)?;
    let open: String = r.get(4)?;
    let trust: String = r.get(5)?;
    Ok(nscore::SessionDigest {
        session: SessionId(r.get(0)?),
        scope: r.get(1)?,
        summary: nscore::SessionSummary {
            topic: r.get(2)?,
            established: serde_json::from_str(&established).unwrap_or_default(),
            open: serde_json::from_str(&open).unwrap_or_default(),
            trust: parse_trust(&trust),
            through_turn: r.get(6)?,
            rebuilt_from: r.get(7)?,
        },
        last_turn: r.get(8)?,
        at: Timestamp(r.get::<_, u64>(9)?),
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
        Self::ensure_session_digests(&conn)?;
        Ok(Self {
            activation: nscore::Activation::default(),
            conn: Mutex::new(conn),
        })
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

    /// M7 §8: one digest per closed session — its last `SessionSummary`,
    /// filed under the session's scope and made searchable.
    ///
    /// The summary is stored in columns rather than as one JSON blob so the
    /// FTS index can cover the text a reader searches for (topic,
    /// established, open) and nothing else: an index over a blob would also
    /// match on field names and on `trust`, and `trust` has to stay a
    /// queryable value — a digest built from External tool output stays
    /// External (M6 §5.1, the laundering rule).
    ///
    /// `session_id` is the primary key because the trait says writes are
    /// idempotent by session: the consolidator re-digests the same closed
    /// sessions on every run.
    ///
    /// That is also why this table needs more than the insert trigger
    /// `events` gets. `events` is append-only; a digest row is UPDATEd in
    /// place, and an external-content FTS5 index does not follow the update
    /// on its own — the old text would stay in the index and
    /// `search_digests` would keep returning a topic the digest no longer
    /// has. The update trigger deletes the old row from the index (the
    /// `('delete', rowid, …)` form FTS5 requires, which needs the *old*
    /// column values) before inserting the new one; the delete trigger is
    /// the same pairing for a row that goes away, and costs nothing while
    /// nothing deletes.
    fn ensure_session_digests(conn: &Connection) -> Result<(), StoreError> {
        let existed: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'session_digests_fts'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(io_err)?
            > 0;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_digests (
                 session_id       TEXT PRIMARY KEY,
                 scope            TEXT NOT NULL,
                 topic            TEXT NOT NULL,
                 established_json TEXT NOT NULL,
                 open_json        TEXT NOT NULL,
                 trust            TEXT NOT NULL,
                 through_turn     INTEGER NOT NULL,
                 rebuilt_from     INTEGER NOT NULL,
                 last_turn        INTEGER NOT NULL,
                 at               INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS session_digests_recent
                 ON session_digests(scope, at DESC);
             CREATE VIRTUAL TABLE IF NOT EXISTS session_digests_fts USING fts5(
                 topic, established_json, open_json,
                 content='session_digests', content_rowid='rowid'
             );
             CREATE TRIGGER IF NOT EXISTS session_digests_fts_ai
             AFTER INSERT ON session_digests BEGIN
                 INSERT INTO session_digests_fts(rowid, topic, established_json, open_json)
                 VALUES (new.rowid, new.topic, new.established_json, new.open_json);
             END;
             CREATE TRIGGER IF NOT EXISTS session_digests_fts_au
             AFTER UPDATE ON session_digests BEGIN
                 INSERT INTO session_digests_fts(session_digests_fts, rowid, topic,
                                                 established_json, open_json)
                 VALUES ('delete', old.rowid, old.topic, old.established_json, old.open_json);
                 INSERT INTO session_digests_fts(rowid, topic, established_json, open_json)
                 VALUES (new.rowid, new.topic, new.established_json, new.open_json);
             END;
             CREATE TRIGGER IF NOT EXISTS session_digests_fts_ad
             AFTER DELETE ON session_digests BEGIN
                 INSERT INTO session_digests_fts(session_digests_fts, rowid, topic,
                                                 established_json, open_json)
                 VALUES ('delete', old.rowid, old.topic, old.established_json, old.open_json);
             END;",
        )
        .map_err(io_err)?;
        if !existed {
            conn.execute_batch(
                "INSERT INTO session_digests_fts(session_digests_fts) VALUES('rebuild')",
            )
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
                 exposures      INTEGER NOT NULL DEFAULT 0,
                 credits        INTEGER NOT NULL DEFAULT 0,
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
        // M9 T4.1: a database written before the fitness counters existed has
        // the versioned table but not the two columns. Same `table_info`
        // sniff, one `ADD COLUMN` each — there is no `user_version` here, and
        // `NOT NULL DEFAULT 0` is what makes "a pre-M9 database opens and
        // reports 0/0" true of every row already in it.
        let mut stmt = conn.prepare("PRAGMA table_info(facts)").map_err(io_err)?;
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(io_err)?
            .collect::<Result<_, _>>()
            .map_err(io_err)?;
        drop(stmt);
        for col in ["exposures", "credits"] {
            if !cols.iter().any(|c| c == col) {
                conn.execute_batch(&format!(
                    "ALTER TABLE facts ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0"
                ))
                .map_err(io_err)?;
            }
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
        // One session is the one-element case of the same query; keeping a
        // second copy of the FTS statement is how the two would drift.
        self.search_turns_in(std::slice::from_ref(session), query, k)
            .await
    }

    /// One FTS5 query with an `IN` list, not one query per session. Recall
    /// runs on the hot path and M7 §8 has it search `recall_sessions` + 1
    /// sessions on every `Deep` turn; the trait's default would prepare a
    /// statement and walk the index once per session, which is the cost that
    /// would make cross-session recall not worth turning on.
    async fn search_turns_in(
        &self,
        sessions: &[SessionId],
        query: &str,
        k: usize,
    ) -> Result<Vec<nscore::TurnHit>, StoreError> {
        let Some(expr) = fts_query(query) else {
            return Ok(vec![]);
        };
        if k == 0 || sessions.is_empty() {
            return Ok(vec![]);
        }
        // ?1 is the MATCH expression, ?2..=?n+1 the sessions, ?n+2 the limit.
        let placeholders = (2..2 + sessions.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let limit_param = sessions.len() + 2;
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT e.session_id, e.turn, e.kind_json, bm25(events_fts) AS score
                 FROM events_fts JOIN events e ON e.rowid = events_fts.rowid
                 WHERE events_fts MATCH ?1 AND e.session_id IN ({placeholders})
                   AND (e.kind_json LIKE '{{\"type\":\"UserSaid\"%'
                        OR e.kind_json LIKE '{{\"type\":\"Replied\"%')
                 ORDER BY score, e.turn DESC LIMIT ?{limit_param}"
            ))
            .map_err(io_err)?;
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(sessions.len() + 2);
        params.push(expr.into());
        params.extend(sessions.iter().map(|s| s.0.clone().into()));
        // M9 T3.2: more candidates than `k`, because rescoring only the top
        // `k` could reorder them but never lift a recent line from below the
        // cut. `rescore_by_recency` does the truncation.
        params.push((nscore::recency_candidates(k) as i64).into());
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            })
            .map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (session_id, turn, kind_json, score) = row.map_err(io_err)?;
            let kind: nscore::EventKind = serde_json::from_str(&kind_json).map_err(io_err)?;
            let (speaker, text) = match kind {
                nscore::EventKind::UserSaid { text } => ("user", text),
                nscore::EventKind::Replied { text } => ("bot", text),
                _ => continue,
            };
            out.push(nscore::TurnHit {
                session: SessionId(session_id),
                turn,
                speaker,
                text,
                // bm25 is negative, lower = better; flip so higher is better.
                score: -score,
            });
        }
        // Post-hoc, over bm25's ranking rather than inside it: FTS5 has no
        // hook for a per-row term, and a recency boost expressed as a MATCH
        // expression would be a second ranker to keep in step with the
        // first. Across sessions the anchor is the newest *matching* turn in
        // the whole candidate set — turn numbers are per session, so this
        // favours the longer conversation; the knob is 0 until a suite says
        // otherwise (M9 T3.2, T3.3).
        nscore::rescore_by_recency(&mut out, self.activation.weight, k);
        Ok(out)
    }

    async fn put_session_digest(&self, digest: &nscore::SessionDigest) -> Result<(), StoreError> {
        let established = serde_json::to_string(&digest.summary.established).map_err(io_err)?;
        let open = serde_json::to_string(&digest.summary.open).map_err(io_err)?;
        let conn = self.conn.lock().await;
        // Upsert, not `INSERT OR REPLACE`: REPLACE deletes the conflicting
        // row to make room, and SQLite fires delete triggers for that delete
        // only when `recursive_triggers` is on — it is off by default here,
        // so the old row's text would survive in `session_digests_fts` and a
        // stale topic would keep matching. `ON CONFLICT DO UPDATE` runs the
        // update trigger, which does the FTS delete explicitly.
        conn.execute(
            "INSERT INTO session_digests (session_id, scope, topic, established_json, open_json,
                                          trust, through_turn, rebuilt_from, last_turn, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                 scope = excluded.scope, topic = excluded.topic,
                 established_json = excluded.established_json,
                 open_json = excluded.open_json, trust = excluded.trust,
                 through_turn = excluded.through_turn, rebuilt_from = excluded.rebuilt_from,
                 last_turn = excluded.last_turn, at = excluded.at",
            rusqlite::params![
                digest.session.0,
                digest.scope,
                digest.summary.topic,
                established,
                open,
                trust_str(digest.summary.trust),
                digest.summary.through_turn,
                digest.summary.rebuilt_from,
                digest.last_turn,
                digest.at.0,
            ],
        )
        .map_err(io_err)?;
        Ok(())
    }

    async fn session_digests(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<nscore::SessionDigest>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {DIGEST_COLUMNS} FROM session_digests d WHERE d.scope = ?1
                 ORDER BY d.at DESC, d.session_id DESC LIMIT ?2"
            ))
            .map_err(io_err)?;
        let rows = stmt
            .query_map(rusqlite::params![scope, limit as i64], row_to_digest)
            .map_err(io_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_err)
    }

    async fn search_digests(
        &self,
        scope: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<nscore::SessionDigest>, StoreError> {
        let Some(expr) = fts_query(query) else {
            return Ok(vec![]);
        };
        if k == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {DIGEST_COLUMNS} FROM session_digests_fts
                 JOIN session_digests d ON d.rowid = session_digests_fts.rowid
                 WHERE session_digests_fts MATCH ?1 AND d.scope = ?2
                 ORDER BY bm25(session_digests_fts), d.at DESC LIMIT ?3"
            ))
            .map_err(io_err)?;
        let rows = stmt
            .query_map(rusqlite::params![expr, scope, k as i64], row_to_digest)
            .map_err(io_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(io_err)
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
                     uses = ?8, last_validated = ?9, prov_json = ?10, trust = ?11, last_used = ?12,
                     exposures = ?13, credits = ?14
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
                    fact.exposures,
                    fact.credits,
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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
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
                fact.exposures,
                fact.credits,
            ],
        )
        .map_err(io_err)?;
        Ok(())
    }

    /// M9 T4.3, overriding the trait's read-modify-write default: one UPDATE
    /// on the primary key, touching only the two derived columns, so a pass
    /// that rescores every fact costs one statement per fact and cannot
    /// disturb a value, a timestamp or a version.
    async fn set_fact_fitness(
        &self,
        scope: &str,
        key: &str,
        exposures: u32,
        credits: u32,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE facts SET exposures = ?3, credits = ?4
             WHERE scope = ?1 AND key = ?2 AND state IN ('current', 'cold')",
            rusqlite::params![scope, key, exposures, credits],
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

    /// M9 T4.1/T4.3, the second half of the shared conformance suite (it
    /// lives in ns-engine's `fitness_conformance`; this crate cannot depend
    /// on ns-engine, so the same assertions are inlined).
    #[tokio::test]
    async fn setting_fitness_updates_the_current_version_in_place_without_superseding() {
        let (_d, store) = tmp_store();
        let f = |key: &str, value: &str, at: u64| Fact {
            key: key.into(),
            value: serde_json::json!(value),
            last_validated: Timestamp(at),
            valid_from: Timestamp(at),
            prov: Provenance::Constant,
            ..Default::default()
        };
        let mut born = f("user.name", "Martin", 10);
        born.exposures = 4;
        born.credits = 1;
        store.put_fact(born).await.unwrap();
        let cur = store.facts("global", "user.name").await.unwrap();
        assert_eq!((cur[0].exposures, cur[0].credits), (4, 1));
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
        assert_eq!(
            after.iter().map(|f| f.value.clone()).collect::<Vec<_>>(),
            before.iter().map(|f| f.value.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            after.iter().map(|f| f.state).collect::<Vec<_>>(),
            before.iter().map(|f| f.state).collect::<Vec<_>>()
        );
        assert_eq!(
            after.iter().map(|f| f.valid_from).collect::<Vec<_>>(),
            before.iter().map(|f| f.valid_from).collect::<Vec<_>>()
        );
        assert_eq!((after[0].exposures, after[0].credits), (9, 3));
        assert_eq!((after[1].exposures, after[1].credits), (4, 1));
        store
            .set_fact_fitness("global", "user.name", 2, 0)
            .await
            .unwrap();
        let cur = store.facts("global", "user.name").await.unwrap();
        assert_eq!((cur[0].exposures, cur[0].credits), (2, 0));
        store
            .set_fact_fitness("global", "user.nothing", 5, 5)
            .await
            .unwrap();
    }

    /// A database written before the fitness columns existed opens, and every
    /// row in it reports 0/0 — the migration test that matters (M9 T4.1).
    #[tokio::test]
    async fn a_pre_m9_database_opens_and_reports_zero_exposures_and_credits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pre-m9.sqlite");
        {
            // The M6 versioned table, exactly as it was before M9 — scope is
            // present, so the v1 migration leaves it alone.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE facts (
                     scope TEXT NOT NULL DEFAULT 'global',
                     key TEXT NOT NULL,
                     valid_from INTEGER NOT NULL,
                     valid_to INTEGER,
                     state TEXT NOT NULL DEFAULT 'current',
                     value_json TEXT NOT NULL,
                     confidence REAL NOT NULL,
                     uses INTEGER NOT NULL,
                     last_validated INTEGER NOT NULL,
                     prov_json TEXT NOT NULL,
                     trust TEXT NOT NULL DEFAULT 'System',
                     last_used INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (scope, key, valid_from)
                 );
                 INSERT INTO facts (scope, key, valid_from, state, value_json, confidence, uses,
                                    last_validated, prov_json, trust, last_used)
                 VALUES ('global', 'user.name', 10, 'current', '\"Martin\"', 1.0, 3, 10,
                         '{\"type\":\"Residual\"}', 'System', 10);",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let cur = store.facts("global", "user").await.unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].uses, 3, "the pre-M9 columns are untouched");
        assert_eq!((cur[0].exposures, cur[0].credits), (0, 0));
        // And the new columns are writable on the migrated table.
        store
            .set_fact_fitness("global", "user.name", 6, 2)
            .await
            .unwrap();
        let cur = store.facts("global", "user").await.unwrap();
        assert_eq!((cur[0].exposures, cur[0].credits), (6, 2));
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

    /// The corpus of [`search_turns_uses_fts5_and_indexes_a_pre_existing_log`],
    /// written into a fresh store built however `build` says.
    async fn fts_corpus(
        path: &std::path::Path,
        build: impl Fn(SqliteStore) -> SqliteStore,
    ) -> (SqliteStore, SessionId) {
        let store = build(SqliteStore::open(path).unwrap());
        let sid = SessionId("cli".into());
        let mut log = EventLog::new(sid.clone());
        for (turn, user, bot) in [
            (1u32, "what time is it", "It is noon."),
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
        (store, sid)
    }

    /// M9 T3.2: the rescoring pass is wired in unconditionally, so the test
    /// that matters is that at weight 0 it is not a pass at all — same hits,
    /// same order, same scores as a store that never heard of it.
    #[tokio::test]
    async fn search_turns_order_is_unchanged_at_weight_zero() {
        let dir = tempfile::tempdir().unwrap();
        let (plain, sid) = fts_corpus(&dir.path().join("plain.sqlite"), |s| s).await;
        let (zero, _) = fts_corpus(&dir.path().join("zero.sqlite"), |s| {
            s.with_activation(0.0, 7.0)
        })
        .await;
        for q in ["noon", "time", "what time?", "martin"] {
            for k in [1usize, 5] {
                let a = plain.search_turns(&sid, q, k).await.unwrap();
                let b = zero.search_turns(&sid, q, k).await.unwrap();
                assert_eq!(a.len(), b.len(), "{q:?} k={k}");
                assert_eq!(
                    a.iter().map(|h| (h.turn, h.speaker)).collect::<Vec<_>>(),
                    b.iter().map(|h| (h.turn, h.speaker)).collect::<Vec<_>>(),
                    "{q:?} k={k}"
                );
                for (x, y) in a.iter().zip(b.iter()) {
                    assert_eq!(x.score, y.score, "{q:?} k={k}: the score moved too");
                }
            }
        }
    }

    /// "time" is said once in turn 1 and once in turn 3. bm25 puts the
    /// shorter line first by 0.033; two turns of decay at weight 1 is worth
    /// 0.095, so the newer line wins — which is the whole claim.
    #[tokio::test]
    async fn a_recent_turn_outranks_an_older_equal_match_at_weight_one() {
        let dir = tempfile::tempdir().unwrap();
        let (off, sid) = fts_corpus(&dir.path().join("off.sqlite"), |s| s).await;
        let (on, _) = fts_corpus(&dir.path().join("on.sqlite"), |s| {
            s.with_activation(1.0, 7.0)
        })
        .await;
        let turns = |hits: &[nscore::TurnHit]| hits.iter().map(|h| h.turn).collect::<Vec<_>>();
        assert_eq!(
            turns(&off.search_turns(&sid, "time", 5).await.unwrap()),
            vec![1, 3],
            "bm25 alone prefers the older, shorter line"
        );
        assert_eq!(
            turns(&on.search_turns(&sid, "time", 5).await.unwrap()),
            vec![3, 1],
            "the recency term reverses it"
        );
        // And it reorders rather than admitting: a query nothing matches
        // still returns nothing, however recent the session is.
        assert!(on
            .search_turns(&sid, "invoice", 5)
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
                open: vec!["waiting on the reply".into()],
                trust: Trust::External,
                rebuilt_from: 1,
            },
            last_turn: 11,
            at: Timestamp(at),
        }
    }

    /// Pins the digest round trip, including the two things the columns
    /// exist for: `trust` survives the write (M6 §5.1 — an External digest
    /// must not launder into a System one), and the digest is still there
    /// after a reopen.
    #[tokio::test]
    async fn session_digest_round_trips_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sqlite");
        let d = digest("s1", "global", "renewing the domain", 100);
        {
            let store = SqliteStore::open(&path).unwrap();
            store.put_session_digest(&d).await.unwrap();
            assert_eq!(
                store.session_digests("global", 5).await.unwrap(),
                vec![d.clone()]
            );
        }
        let store = SqliteStore::open(&path).unwrap();
        let back = store.session_digests("global", 5).await.unwrap();
        assert_eq!(back, vec![d]);
        assert_eq!(back[0].summary.trust, Trust::External);
    }

    /// The FTS index is external-content over a table that is *rewritten*:
    /// without the update trigger's `('delete', …)` half the old topic stays
    /// indexed and `search_digests` keeps returning the digest by a word it
    /// no longer contains.
    #[tokio::test]
    async fn rewriting_a_digest_replaces_the_row_and_its_fts_entry() {
        let (_d, store) = tmp_store();
        store
            .put_session_digest(&digest("s1", "global", "renewing the domain", 100))
            .await
            .unwrap();
        assert_eq!(
            store
                .search_digests("global", "domain", 5)
                .await
                .unwrap()
                .len(),
            1
        );

        store
            .put_session_digest(&digest("s1", "global", "booking the flight", 200))
            .await
            .unwrap();
        let all = store.session_digests("global", 5).await.unwrap();
        assert_eq!(all.len(), 1, "one digest per session, not two");
        assert_eq!(all[0].summary.topic, "booking the flight");
        assert!(
            store
                .search_digests("global", "domain", 5)
                .await
                .unwrap()
                .is_empty(),
            "the replaced topic must leave the index with the row"
        );
        assert_eq!(
            store.search_digests("global", "flight", 5).await.unwrap()[0].session,
            SessionId("s1".into())
        );
    }

    /// A digest is searchable by a word of its topic, and scope is a wall:
    /// digests of one scope are never an answer for another (M6 §6.6).
    #[tokio::test]
    async fn search_digests_matches_topic_text_and_never_crosses_scope() {
        let (_d, store) = tmp_store();
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
        assert!(store
            .search_digests("global", "unrelated", 5)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.session_digests("chat42", 5).await.unwrap().len(), 1);
        // `established` and `open` are indexed too, not just the topic.
        assert_eq!(
            store
                .search_digests("chat42", "invoice", 5)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// A database written before the digest index existed is indexed on
    /// open, the way `events_fts` is — otherwise the first upgrade silently
    /// loses every digest already written.
    #[tokio::test]
    async fn session_digests_fts_is_rebuilt_for_a_pre_existing_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .put_session_digest(&digest("s1", "global", "renewing the domain", 100))
                .await
                .unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP TRIGGER session_digests_fts_ai;
                 DROP TRIGGER session_digests_fts_au;
                 DROP TRIGGER session_digests_fts_ad;
                 DROP TABLE session_digests_fts;",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(
            store
                .search_digests("global", "domain", 5)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// Cross-session recall (M7 §8): one FTS query over both sessions, with
    /// the hits of each ranked against the other and tagged with the session
    /// they came from.
    #[tokio::test]
    async fn search_turns_in_ranks_two_sessions_together() {
        let (_d, store) = tmp_store();
        for (name, turn, user, bot) in [
            ("s1", 1u32, "where is the invoice folder", "On the desktop."),
            ("s2", 1, "print the invoice please", "Printed."),
            ("s3", 1, "the invoice is late", "Noted."),
        ] {
            let sid = SessionId(name.into());
            let mut log = EventLog::new(sid.clone());
            log.append(
                turn,
                Timestamp(1),
                EventKind::UserSaid { text: user.into() },
            );
            log.append(turn, Timestamp(2), EventKind::Replied { text: bot.into() });
            store.append(&sid, log.events()).await.unwrap();
        }
        let sessions = [SessionId("s1".into()), SessionId("s2".into())];
        let hits = store
            .search_turns_in(&sessions, "invoice", 10)
            .await
            .unwrap();
        let seen: std::collections::HashSet<&str> =
            hits.iter().map(|h| h.session.0.as_str()).collect();
        assert_eq!(seen, ["s1", "s2"].into_iter().collect());
        assert!(hits.iter().all(|h| h.text.contains("invoice")));
        assert!(
            hits.windows(2).all(|w| w[0].score >= w[1].score),
            "merged, best first: {hits:?}"
        );
        assert!(
            !hits.iter().any(|h| h.session.0 == "s3"),
            "a session not asked for is not searched"
        );
        // Single-session `search_turns` is the same query with one id.
        let one = store
            .search_turns(&SessionId("s1".into()), "invoice", 10)
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].session, SessionId("s1".into()));
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
