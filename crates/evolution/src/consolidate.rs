//! Fact consolidation (M6 spec §6.2, §6.4), run by the evolution pass:
//! passive decay to `cold`, safety-triggered forgetting of unverified
//! external values, and duplicate-key merging where the values agree.
//! Deterministic acceptance only: the pass never merges differing values,
//! it reports them.
use nscore::{squash, Fact, FactState, MemoryStore, StoreError, Timestamp, Trust};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct ConsolidateConfig {
    /// A live fact neither validated nor used for this long goes cold.
    pub stale_ms: u64,
    /// Count and report, write nothing.
    pub dry_run: bool,
    /// M9 T4.4: exposures at or above which a fact with no credits is a
    /// demotion candidate. Below it the fact has not had a fair trial — a
    /// fact shown twice and unused twice is noise, not evidence.
    pub fitness_min_exposures: u32,
    /// Whether the fitness signal *demotes* or only reports. Off for one
    /// release (plan §P4): the rule is that fitness reports before it demotes,
    /// and the knob flips only after a release's dry runs show the candidate
    /// set is not simply the facts nothing ever queried.
    pub fitness_demote: bool,
    /// Key prefixes never demoted for low fitness, from `[memory]
    /// pinned_prefixes`. Excluded *before* the branch: a pinned fact is in
    /// every prompt by construction, so it accrues exposures whether or not
    /// anything needed it, and its credit rate measures the prompt rather
    /// than the fact.
    pub pinned_prefixes: Vec<String>,
    /// This pass's freshly derived counters, `(scope, key) -> (exposures,
    /// credits)`. Passed in rather than read off the facts because on a dry
    /// run nothing was written: without them the report would judge the store
    /// by the numbers the *last* real pass left, and a dry run must show what
    /// a real run would do now.
    pub fitness: BTreeMap<(String, String), (u32, u32)>,
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
    /// M9 T4.4: `(scope, key, exposures)` for every fact the fitness signal
    /// would send cold — reported whether or not `fitness_demote` acted.
    pub fitness_demote_candidates: Vec<(String, String, u32)>,
    /// Whether those candidates were written. The knob's state, printed, so a
    /// report cannot be read as "nothing happened" when it means "nothing was
    /// allowed to happen".
    pub fitness_demote_applied: bool,
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
        write!(
            f,
            "\nfitness demote: {} candidates, knob {}",
            self.fitness_demote_candidates.len(),
            if self.fitness_demote_applied {
                "on (applied)"
            } else {
                "off (reported only)"
            }
        )?;
        for (scope, key, exposures) in &self.fitness_demote_candidates {
            write!(f, "\n  {scope}/{key}  {exposures} exposures, 0 credits")?;
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

        // 1b. M9 T4.4: the third demotion signal — shown often enough to have
        //     had a fair trial, and credited by nothing. Cold, not forgotten:
        //     a cold fact stays searchable and revives on restatement, so the
        //     worst case of a wrong demotion is a rank position, not a loss.
        //
        //     `fitness_demote` is off for a release. The rule (plan §P4) is
        //     that fitness reports before it demotes: until a release of dry
        //     runs shows this set is not simply the facts nobody ever asked
        //     about, the candidates are printed and the store is untouched.
        report.fitness_demote_applied = cfg.fitness_demote;
        for f in live.iter().filter(|f| f.state == FactState::Current) {
            // Pinned first, before any number is looked at: a pinned fact is
            // in every prompt whether or not it was wanted, so its exposures
            // measure the prompt and its credit rate would demote the user's
            // own name.
            if cfg.pinned_prefixes.iter().any(|p| f.key.starts_with(p)) {
                continue;
            }
            let (exposures, credits) = cfg
                .fitness
                .get(&(scope.clone(), f.key.clone()))
                .copied()
                .unwrap_or((f.exposures, f.credits));
            if cfg.fitness_min_exposures == 0
                || exposures < cfg.fitness_min_exposures
                || credits > 0
            {
                continue;
            }
            report
                .fitness_demote_candidates
                .push((scope.clone(), f.key.clone(), exposures));
            if cfg.fitness_demote && !cfg.dry_run {
                let mut c = f.clone();
                c.state = FactState::Cold;
                store.put_fact(c).await?;
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
                ..Default::default()
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
                ..Default::default()
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

    // ---- M9 T4.4: the third demotion signal ----

    /// Four facts, all fresh enough that decay cannot touch them, so the only
    /// thing that can move any of them is fitness.
    async fn fit_store(now: u64) -> InMemoryStore {
        let store = InMemoryStore::new();
        // (key, exposures, credits)
        for (key, exposures, credits) in [
            ("user.name", 40u32, 0u32),
            ("shop.promo", 12, 0),
            ("shop.hours", 12, 1),
            ("shop.rare", 2, 0),
        ] {
            store
                .put_fact(Fact {
                    key: key.into(),
                    value: serde_json::json!("x"),
                    confidence: 1.0,
                    last_validated: Timestamp(now),
                    last_used: Timestamp(now),
                    valid_from: Timestamp(now),
                    prov: nscore::Provenance::Constant,
                    exposures,
                    credits,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        store
    }

    fn fit_cfg(demote: bool, dry_run: bool) -> ConsolidateConfig {
        ConsolidateConfig {
            stale_ms: 90 * DAY,
            dry_run,
            fitness_min_exposures: 8,
            fitness_demote: demote,
            pinned_prefixes: vec!["user.".into()],
            // Empty: these tests seed the counters on the facts themselves, so
            // the fallback path — reading what the last pass wrote — is what
            // is under test here. `pass.rs` exercises the override.
            fitness: BTreeMap::new(),
        }
    }

    async fn state_of(store: &InMemoryStore, key: &str) -> FactState {
        store.fact_history("global", key).await.unwrap()[0].state
    }

    /// A pinned fact is in every prompt by construction. Its exposures measure
    /// the prompt, not the fact, and its credit rate would send the user's own
    /// name cold on the first quiet week. Excluded before the branch.
    #[tokio::test]
    async fn a_pinned_fact_is_never_demoted_for_low_fitness() {
        let now = 100 * DAY;
        let store = fit_store(now).await;
        let report = consolidate_facts(&store, fit_cfg(true, false), Timestamp(now))
            .await
            .unwrap();
        assert!(
            !report
                .fitness_demote_candidates
                .iter()
                .any(|(_, k, _)| k == "user.name"),
            "{:?}",
            report.fitness_demote_candidates
        );
        assert_eq!(state_of(&store, "user.name").await, FactState::Current);
    }

    /// One credit is enough. The signal is "shown often and never once
    /// useful", not "shown more often than it was useful" — the second is a
    /// ratio, and a ratio would demote the fact that answers a rare question
    /// perfectly.
    #[tokio::test]
    async fn a_fact_with_credits_is_not_demoted() {
        let now = 100 * DAY;
        let store = fit_store(now).await;
        let report = consolidate_facts(&store, fit_cfg(true, false), Timestamp(now))
            .await
            .unwrap();
        let keys: Vec<&str> = report
            .fitness_demote_candidates
            .iter()
            .map(|(_, k, _)| k.as_str())
            .collect();
        assert_eq!(keys, vec!["shop.promo"], "one credit saves shop.hours");
        // And a fact below the exposure floor has not had a fair trial.
        assert_eq!(state_of(&store, "shop.rare").await, FactState::Current);
    }

    /// The rule the phase is built on: fitness reports before it demotes. With
    /// the knob off the candidate set is printed and the store is untouched.
    #[tokio::test]
    async fn demotion_is_reported_not_applied_while_the_knob_is_off() {
        let now = 100 * DAY;
        let store = fit_store(now).await;
        let report = consolidate_facts(&store, fit_cfg(false, false), Timestamp(now))
            .await
            .unwrap();
        assert_eq!(
            report.fitness_demote_candidates,
            vec![("global".to_string(), "shop.promo".to_string(), 12)]
        );
        assert!(!report.fitness_demote_applied);
        assert_eq!(state_of(&store, "shop.promo").await, FactState::Current);
        assert!(
            format!("{report}").contains("knob off (reported only)"),
            "{report}"
        );
    }

    /// And with the knob on it is cold — cold, not forgotten: a cold fact
    /// stays searchable and revives on restatement, so a wrong demotion costs
    /// a rank position rather than the value.
    #[tokio::test]
    async fn demotion_applies_when_the_knob_is_on() {
        let now = 100 * DAY;
        let store = fit_store(now).await;
        let report = consolidate_facts(&store, fit_cfg(true, false), Timestamp(now))
            .await
            .unwrap();
        assert!(report.fitness_demote_applied);
        assert_eq!(state_of(&store, "shop.promo").await, FactState::Cold);
        assert_eq!(report.fitness_demote_candidates.len(), 1);
        // A dry run with the knob on still writes nothing.
        let store = fit_store(now).await;
        consolidate_facts(&store, fit_cfg(true, true), Timestamp(now))
            .await
            .unwrap();
        assert_eq!(state_of(&store, "shop.promo").await, FactState::Current);
    }
}
