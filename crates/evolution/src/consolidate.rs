//! Fact consolidation (M6 spec §6.2, §6.4), run by the evolution pass:
//! passive decay to `cold`, safety-triggered forgetting of unverified
//! external values, and duplicate-key merging where the values agree.
//! Deterministic acceptance only: the pass never merges differing values,
//! it reports them.
use nscore::{squash, Fact, FactState, MemoryStore, StoreError, Timestamp, Trust};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct ConsolidateConfig {
    /// A live fact neither validated nor used for this long goes cold.
    pub stale_ms: u64,
    /// Count and report, write nothing.
    pub dry_run: bool,
}

/// Write one digest per session that has a rolling summary (M7 Phase 4).
///
/// A digest is the session's *last* `SessionSummary`, copied — so it costs no
/// model call at all. The summary was already paid for during the session,
/// off the user's critical path, and re-summarizing it here would spend a
/// request to produce something the log already holds.
///
/// Idempotent by session id: the consolidator runs on every idle period, and
/// a session gains turns between runs, so the digest is rewritten rather than
/// duplicated. `put_session_digest` upserts, and the FTS index follows.
///
/// `scope` is one value because the CLI maps every session to `global`
/// (M6 §15). A multi-user channel will need this to become the same mapping
/// `EngineConfig::scope_for` applies, and the digest table already carries
/// the column for it.
pub async fn write_session_digests(
    store: &dyn MemoryStore,
    scope: &str,
    dry_run: bool,
    now: Timestamp,
) -> Result<usize, StoreError> {
    let mut written = 0;
    for session in store.sessions().await? {
        let events = store.load(&session).await?;
        let Some(summary) = last_summary(&events) else {
            // No summary means the session never outgrew its window. There is
            // nothing to digest that `search_turns_in` cannot already reach
            // verbatim, and verbatim is the better source (findings §1).
            continue;
        };
        let last_turn = events.iter().map(|e| e.turn).max().unwrap_or(0);
        written += 1;
        if dry_run {
            continue;
        }
        store
            .put_session_digest(&nscore::SessionDigest {
                session: session.clone(),
                scope: scope.to_string(),
                summary,
                last_turn,
                at: now,
            })
            .await?;
    }
    Ok(written)
}

/// The last `Summarized` event of a session, which is the summary in force
/// when it ended.
fn last_summary(events: &[nscore::Event]) -> Option<nscore::SessionSummary> {
    events.iter().rev().find_map(|e| match &e.kind {
        nscore::EventKind::Summarized { summary } => Some(summary.clone()),
        _ => None,
    })
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConsolidationReport {
    pub cold: usize,
    pub purged_external: usize,
    pub merged: usize,
    /// (scope, kept key, other key): same squashed key, different values.
    pub conflicting: Vec<(String, String, String)>,
    /// Sessions whose digest was written or refreshed (M7 Phase 4).
    pub digests: usize,
}

impl std::fmt::Display for ConsolidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "facts: cold {}, purged external {}, merged {}; digests {}",
            self.cold, self.purged_external, self.merged, self.digests
        )?;
        for (scope, keep, other) in &self.conflicting {
            write!(f, "\n  conflicting keys in {scope}: {keep} vs {other}")?;
        }
        Ok(())
    }
}

fn last_touch(f: &Fact) -> Timestamp {
    f.last_validated.max(f.last_used)
}

pub async fn consolidate_facts(
    store: &dyn MemoryStore,
    cfg: ConsolidateConfig,
    now: Timestamp,
) -> Result<ConsolidationReport, StoreError> {
    let mut report = ConsolidationReport::default();
    for scope in store.scopes().await? {
        let live = store.facts(&scope, "").await?;

        // 1. Passive decay: current → cold. Cold facts stay searchable and
        //    revive on restatement; nothing is deleted.
        for f in live.iter().filter(|f| f.state == FactState::Current) {
            if now.0.saturating_sub(last_touch(f).0) > cfg.stale_ms {
                report.cold += 1;
                if !cfg.dry_run {
                    let mut c = f.clone();
                    c.state = FactState::Cold;
                    store.put_fact(c).await?;
                }
            }
        }

        // 2. Safety-triggered forgetting: values that came from an external
        //    source and were never confirmed by the user (findings §5).
        for f in live
            .iter()
            .filter(|f| f.trust == Trust::External && f.confidence < 0.75)
        {
            report.purged_external += 1;
            if !cfg.dry_run {
                store.forget_fact(&scope, &f.key, now).await?;
            }
        }

        // 3. Duplicate keys by squashed spelling: merge when the values
        //    agree (keep the more used, then the newer), report otherwise.
        let mut groups: BTreeMap<String, Vec<&Fact>> = BTreeMap::new();
        for f in live.iter().filter(|f| f.state == FactState::Current) {
            groups.entry(squash(&f.key)).or_default().push(f);
        }
        for group in groups.values().filter(|g| g.len() > 1) {
            let keep = group
                .iter()
                .max_by(|a, b| {
                    a.uses
                        .cmp(&b.uses)
                        .then_with(|| a.last_validated.cmp(&b.last_validated))
                        .then_with(|| b.key.cmp(&a.key))
                })
                .expect("non-empty group");
            for other in group.iter().filter(|f| f.key != keep.key) {
                if other.value == keep.value {
                    report.merged += 1;
                    if !cfg.dry_run {
                        store.forget_fact(&scope, &other.key, now).await?;
                    }
                } else {
                    report
                        .conflicting
                        .push((scope.clone(), keep.key.clone(), other.key.clone()));
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsengine::store::InMemoryStore;

    const DAY: u64 = 86_400_000;

    /// A session is digested from the summary it already has, so the step
    /// costs no model call — and a session that never outgrew its window has
    /// no summary and is skipped, because `search_turns_in` reaches its
    /// turns verbatim and verbatim is the better source (findings §1).
    #[tokio::test]
    async fn a_session_with_a_summary_is_digested_and_one_without_is_skipped() {
        let store = InMemoryStore::new();
        let with = nscore::SessionId("long".into());
        let without = nscore::SessionId("short".into());
        let mut log = nscore::EventLog::new(with.clone());
        log.append(
            1,
            Timestamp(1),
            nscore::EventKind::UserSaid { text: "hi".into() },
        );
        log.append(
            7,
            Timestamp(2),
            nscore::EventKind::Summarized {
                summary: nscore::SessionSummary {
                    through_turn: 4,
                    topic: "the vault".into(),
                    established: vec![],
                    open: vec![],
                    trust: nscore::Trust::User,
                    rebuilt_from: 1,
                },
            },
        );
        store.append(&with, log.events()).await.unwrap();
        let mut short = nscore::EventLog::new(without.clone());
        short.append(
            1,
            Timestamp(1),
            nscore::EventKind::UserSaid { text: "hi".into() },
        );
        store.append(&without, short.events()).await.unwrap();

        let written = write_session_digests(&store, "global", false, Timestamp(9))
            .await
            .unwrap();
        assert_eq!(written, 1, "only the session that had a summary");
        let digests = store.session_digests("global", 10).await.unwrap();
        assert_eq!(digests.len(), 1);
        assert_eq!(digests[0].session, with);
        assert_eq!(digests[0].summary.topic, "the vault");
        assert_eq!(digests[0].last_turn, 7, "the last turn, not the summary's");

        // Idempotent: the consolidator runs on every idle period, and a
        // session gains turns between runs.
        write_session_digests(&store, "global", false, Timestamp(10))
            .await
            .unwrap();
        assert_eq!(store.session_digests("global", 10).await.unwrap().len(), 1);

        // A dry run counts and writes nothing.
        let fresh = InMemoryStore::new();
        fresh.append(&with, log.events()).await.unwrap();
        assert_eq!(
            write_session_digests(&fresh, "global", true, Timestamp(9))
                .await
                .unwrap(),
            1
        );
        assert!(fresh
            .session_digests("global", 10)
            .await
            .unwrap()
            .is_empty());
    }

    fn fact(key: &str, value: &str, validated: u64) -> Fact {
        Fact {
            key: key.into(),
            value: serde_json::json!(value),
            last_validated: Timestamp(validated),
            valid_from: Timestamp(validated),
            ..Default::default()
        }
    }

    async fn seeded() -> InMemoryStore {
        let store = InMemoryStore::new();
        // stale, never used: goes cold
        store.put_fact(fact("user.city", "Brno", 1)).await.unwrap();
        // stale by validation but used recently: stays
        let mut used = fact("user.name", "Martin", 1);
        used.last_used = Timestamp(100 * DAY);
        store.put_fact(used).await.unwrap();
        // fresh
        store
            .put_fact(fact("user.age", "17", 100 * DAY))
            .await
            .unwrap();
        // unverified external value
        let mut ext = fact("shop.promo", "50% off", 100 * DAY);
        ext.trust = Trust::External;
        ext.confidence = 0.5;
        store.put_fact(ext).await.unwrap();
        // duplicate spellings, equal values: merge; differing: report
        store
            .put_fact(fact("memory_reset_requested", "true", 100 * DAY))
            .await
            .unwrap();
        let mut dup = fact("memory.reset.requested", "true", 100 * DAY - 1);
        dup.uses = 3;
        store.put_fact(dup).await.unwrap();
        store
            .put_fact(fact("user.lang", "cs", 100 * DAY))
            .await
            .unwrap();
        store
            .put_fact(fact("user_lang", "en", 100 * DAY))
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn decay_purge_and_merge_are_applied_and_reported() {
        let store = seeded().await;
        let report = consolidate_facts(
            &store,
            ConsolidateConfig {
                stale_ms: 90 * DAY,
                dry_run: false,
            },
            Timestamp(101 * DAY),
        )
        .await
        .unwrap();
        assert_eq!(report.cold, 1);
        assert_eq!(report.purged_external, 1);
        assert_eq!(report.merged, 1);
        assert_eq!(
            report.conflicting,
            vec![(
                "global".to_string(),
                "user.lang".to_string(),
                "user_lang".to_string()
            )]
        );
        let by_key = |k: &'static str| {
            let store = &store;
            async move { store.fact_history("global", k).await.unwrap().remove(0) }
        };
        assert_eq!(by_key("user.city").await.state, FactState::Cold);
        assert_eq!(
            by_key("user.name").await.state,
            FactState::Current,
            "recent use keeps it warm"
        );
        assert_eq!(by_key("shop.promo").await.state, FactState::Forgotten);
        assert_eq!(
            by_key("memory_reset_requested").await.state,
            FactState::Forgotten,
            "the less-used spelling is merged away"
        );
        assert_eq!(
            by_key("memory.reset.requested").await.state,
            FactState::Current
        );
        assert_eq!(by_key("user.lang").await.state, FactState::Current);
        assert_eq!(
            by_key("user_lang").await.state,
            FactState::Current,
            "differing values are only reported"
        );
        assert!(report
            .to_string()
            .contains("conflicting keys in global: user.lang vs user_lang"));
    }

    #[tokio::test]
    async fn dry_run_counts_but_writes_nothing() {
        let store = seeded().await;
        let report = consolidate_facts(
            &store,
            ConsolidateConfig {
                stale_ms: 90 * DAY,
                dry_run: true,
            },
            Timestamp(101 * DAY),
        )
        .await
        .unwrap();
        assert_eq!(
            (report.cold, report.purged_external, report.merged),
            (1, 1, 1)
        );
        let live = store.facts("global", "").await.unwrap();
        assert_eq!(live.len(), 8);
        assert!(live.iter().all(|f| f.state == FactState::Current));
    }
}
