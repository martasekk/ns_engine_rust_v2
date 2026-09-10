//! The evolution pass (spec M5 §2): load rules + ledger → load and
//! chain-verify sessions → mine → symbolic lane → corrections as facts →
//! notes lane → apply (atomic file + hot swap) → ledger. Implements
//! `Consolidator`, so the idle driver and `ns-app evolve` share one path.
use crate::evaluate::{evaluate, EvaluateConfig};
use crate::files::{load_rules, save_rules_atomic, FileError};
use crate::ledger::{Evidence, Ledger, LedgerEntry, Verdict};
use crate::mine::{mine, render_turn, Signature, SignatureKind};
use crate::notes::{verify_note, NoteProposer, ProbeRunner};
use crate::symbolic::{propose_patches, verify_patch, Recorded};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use nscore::{
    ActionSpec, Consolidator, EventLog, Fact, LearnedRules, MemoryStore, Provenance, StoreError,
    Timestamp,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PassConfig {
    /// Negatives a note may regress and still be accepted.
    pub regression_budget: u32,
    /// Live-emitter turns one pass may spend on note probes (2× turns per session).
    pub probe_budget_turns: u32,
    /// Notes library cap; the lowest-lift note is evicted past it.
    pub max_notes: usize,
    /// Sessions replayed per patch for the regression check.
    pub regression_replay_cap: usize,
    /// Report only: no file writes, no hot swap, no facts.
    pub dry_run: bool,
    /// M6 §6.2: a live fact neither validated nor used this long goes cold.
    pub fact_stale_days: u64,
    /// M7 Phase 4: the scope session digests are written under. One value
    /// because the CLI maps every session to `global` (M6 §15); a
    /// multi-user channel turns this into the mapping `scope_for` applies.
    pub digest_scope: String,
    /// M8 T2.1: the symbolic evaluation checks. Nested rather than flattened
    /// because the lane is going to grow an evaluator, a κ threshold and a
    /// turn budget beside these, and they belong together.
    pub evaluate: EvaluateConfig,
}

impl Default for PassConfig {
    fn default() -> Self {
        Self {
            regression_budget: 0,
            probe_budget_turns: 40,
            max_notes: 20,
            regression_replay_cap: 200,
            dry_run: false,
            fact_stale_days: 90,
            digest_scope: "global".into(),
            evaluate: EvaluateConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PassError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("file: {0}")]
    File(#[from] FileError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CandidateReport {
    pub hash: String,
    pub lane: &'static str,
    pub summary: String,
    pub verdict: Verdict,
    pub numbers: serde_json::Value,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub sessions: usize,
    pub skipped_broken: usize,
    pub signatures: BTreeMap<&'static str, usize>,
    /// The turns each signature fired on, in order, deduplicated.
    ///
    /// A count says a check works; only the turns say *what it found*, and
    /// M6's exit criterion for Phase 5 is written in turn numbers
    /// ("`UserReask` for turns 52–60, 67, 90, 100, 105"). A dry run that
    /// cannot be read against that sentence cannot settle it.
    pub signature_turns: BTreeMap<&'static str, Vec<u32>>,
    pub candidates: Vec<CandidateReport>,
    pub probe_turns_used: u32,
    pub facts_written: usize,
    pub written: bool,
    /// M6 §6.4 fact consolidation numbers.
    pub consolidation: crate::consolidate::ConsolidationReport,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "sessions: {} (skipped broken: {})",
            self.sessions, self.skipped_broken
        )?;
        writeln!(f, "signatures:")?;
        for (k, n) in &self.signatures {
            match self.signature_turns.get(k) {
                Some(turns) if !turns.is_empty() => {
                    let list: Vec<String> = turns.iter().map(u32::to_string).collect();
                    writeln!(f, "  {k}: {n} (turns {})", list.join(", "))?;
                }
                _ => writeln!(f, "  {k}: {n}")?,
            }
        }
        writeln!(f, "candidates: {}", self.candidates.len())?;
        for c in &self.candidates {
            let verdict = match c.verdict {
                Verdict::Accepted => "accepted",
                Verdict::Rejected => "rejected",
                Verdict::Unverified => "unverified",
            };
            writeln!(f, "  [{}] {} — {verdict} {}", c.lane, c.summary, c.numbers)?;
        }
        writeln!(f, "probe turns used: {}", self.probe_turns_used)?;
        writeln!(f, "facts written: {}", self.facts_written)?;
        writeln!(f, "{}", self.consolidation)?;
        write!(f, "learned.toml written: {}", self.written)
    }
}

pub struct EvolutionPass {
    rules: Arc<ArcSwap<LearnedRules>>,
    known_specs: Vec<ActionSpec>,
    probe: Option<Box<dyn ProbeRunner>>,
    proposer: Option<Box<dyn NoteProposer>>,
    learned_path: PathBuf,
    ledger_path: PathBuf,
    cfg: PassConfig,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `key = value` with a dotted-identifier key → (key, value).
fn parse_correction(text: &str) -> Option<(String, String)> {
    let (k, v) = text.split_once('=')?;
    let k = k.trim();
    let v = v.trim();
    let key_ok = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c));
    (key_ok && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
}

impl EvolutionPass {
    pub fn new(
        rules: Arc<ArcSwap<LearnedRules>>,
        known_specs: Vec<ActionSpec>,
        learned_path: PathBuf,
        ledger_path: PathBuf,
        cfg: PassConfig,
    ) -> Self {
        Self {
            rules,
            known_specs,
            probe: None,
            proposer: None,
            learned_path,
            ledger_path,
            cfg,
            clock: Box::new(now_ms),
        }
    }
    /// Enable the notes lane (needs a live emitter to probe with).
    pub fn with_notes(
        mut self,
        probe: Box<dyn ProbeRunner>,
        proposer: Box<dyn NoteProposer>,
    ) -> Self {
        self.probe = Some(probe);
        self.proposer = Some(proposer);
        self
    }
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub async fn run_report(&self, store: &dyn MemoryStore) -> Result<Report, PassError> {
        // 1–2. An unparsable learned.toml aborts before anything is proposed.
        let base = load_rules(&self.learned_path)?;
        let mut ledger = Ledger::load(&self.ledger_path)?;
        let mut report = Report::default();
        let now = (self.clock)();

        // 3. Load and chain-verify sessions; a broken chain is not evidence.
        let mut sessions: Vec<Recorded> = Vec::new();
        for sid in store.sessions().await? {
            let events = store.load(&sid).await?;
            if EventLog::from_events(sid.clone(), events.clone())
                .verify_chain()
                .is_err()
            {
                report.skipped_broken += 1;
                continue;
            }
            sessions.push((sid, events));
        }
        report.sessions = sessions.len();

        // 4. Mine, then evaluate.
        //
        // Two sources, one list. `mine` reads what the harness did; `evaluate`
        // (M8 T2.1) reads what the user had to do about it — a re-ask, a
        // recorded grounding flag, an ignored question or request. The second
        // costs nothing: no model, no network, no store access, so it runs on
        // every turn of every pass and needs no budget of its own until an
        // evaluator that spends requests arrives behind it (T2.8).
        let mut sigs: Vec<Signature> = Vec::new();
        for (sid, events) in &sessions {
            sigs.extend(mine(sid, events, &self.known_specs));
            sigs.extend(evaluate(sid, events, &self.cfg.evaluate));
        }
        for s in &sigs {
            *report.signatures.entry(s.kind.name()).or_insert(0) += 1;
            let turns = report.signature_turns.entry(s.kind.name()).or_default();
            if !turns.contains(&s.turn) {
                turns.push(s.turn);
            }
        }
        for turns in report.signature_turns.values_mut() {
            turns.sort_unstable();
        }

        // `working` accumulates this run's accepted candidates so later ones
        // are verified against base + everything accepted before them.
        let mut working = base.clone();

        // 5. Symbolic lane.
        for (patch, evidence) in propose_patches(&sigs) {
            let hash = patch.hash();
            if ledger.settled(&hash) {
                continue;
            }
            let v = verify_patch(
                &patch,
                &evidence,
                &sessions,
                &working,
                &self.known_specs,
                self.cfg.regression_replay_cap,
            )
            .await;
            let verdict = if v.accepted {
                Verdict::Accepted
            } else {
                Verdict::Rejected
            };
            if v.accepted {
                patch.apply_to(&mut working);
            }
            let numbers = serde_json::json!({
                "flipped": v.flipped, "not_flipped": v.not_flipped,
                "regressions": v.regressions, "skipped_baseline": v.skipped_baseline,
            });
            ledger.entries.insert(
                hash.clone(),
                LedgerEntry {
                    verdict,
                    numbers: numbers.clone(),
                    evidence: evidence.clone(),
                    at: now,
                },
            );
            report.candidates.push(CandidateReport {
                hash,
                lane: "symbolic",
                summary: patch.summary(),
                verdict,
                numbers,
            });
        }

        // 6. Corrections that parse as `key = value` become facts (dry run: none).
        if !self.cfg.dry_run {
            for s in &sigs {
                if let SignatureKind::Corrected { text } = &s.kind {
                    if let Some((key, value)) = parse_correction(text) {
                        store
                            .put_fact(Fact {
                                key,
                                value: serde_json::json!(value),
                                confidence: 1.0,
                                uses: 0,
                                last_validated: Timestamp(now),
                                prov: Provenance::Residual,
                                valid_from: Timestamp(now),
                                ..Default::default()
                            })
                            .await?;
                        report.facts_written += 1;
                    }
                }
            }
        }

        // 6b. Fact consolidation (M6 §6.4): decay, safety purge, duplicate
        //     keys. Deterministic; dry run only counts.
        report.consolidation = crate::consolidate::consolidate_facts(
            store,
            crate::consolidate::ConsolidateConfig {
                stale_ms: self.cfg.fact_stale_days.saturating_mul(86_400_000),
                dry_run: self.cfg.dry_run,
            },
            Timestamp(now),
        )
        .await?;

        // 6c. Session digests (M7 Phase 4): the last rolling summary of each
        //     session, copied into a searchable table so recall can reach
        //     across sessions. No model call — the summary was already paid
        //     for while the session ran.
        report.consolidation.digests = crate::consolidate::write_session_digests(
            store,
            &self.cfg.digest_scope,
            self.cfg.dry_run,
            Timestamp(now),
        )
        .await?;

        // 7. Notes lane (only with a live probe and a proposer).
        if let (Some(probe), Some(proposer)) = (&self.probe, &self.proposer) {
            let mut budget = self.cfg.probe_budget_turns;
            let clean: Vec<&Recorded> = sessions
                .iter()
                .filter(|(sid, _)| !sigs.iter().any(|s| &s.session == sid))
                .collect();
            for s in sigs.iter().filter(|s| s.kind.lane() == "note") {
                if budget == 0 {
                    break;
                }
                let Some((_, events)) = sessions.iter().find(|(sid, _)| sid == &s.session) else {
                    continue;
                };
                let trace = render_turn(events, s.turn);
                let note = match proposer.propose(&trace, &working.notes).await {
                    Ok(Some(n)) => n,
                    Ok(None) => continue,
                    Err(e) => {
                        eprintln!("note proposer: {e}");
                        continue;
                    }
                };
                if ledger.settled(&note.hash) || working.notes.iter().any(|n| n.hash == note.hash) {
                    continue;
                }
                // Positives: every session showing this signature kind.
                // Negatives: as many signature-free sessions, in store order.
                let positives: Vec<Vec<nscore::Event>> = sessions
                    .iter()
                    .filter(|(sid, _)| {
                        sigs.iter()
                            .any(|x| &x.session == sid && x.kind.name() == s.kind.name())
                    })
                    .map(|(_, e)| e.clone())
                    .collect();
                let negatives: Vec<Vec<nscore::Event>> = clean
                    .iter()
                    .take(positives.len())
                    .map(|(_, e)| e.clone())
                    .collect();
                let v = verify_note(
                    &note,
                    &positives,
                    &negatives,
                    &working,
                    probe.as_ref(),
                    self.cfg.regression_budget,
                    &mut budget,
                )
                .await;
                report.probe_turns_used += v.turns_used;
                let verdict = if v.accepted {
                    Verdict::Accepted
                } else if v.detail.contains("budget exhausted") {
                    Verdict::Unverified
                } else {
                    Verdict::Rejected
                };
                if v.accepted {
                    let mut accepted = note.clone();
                    accepted.lift = v.lift;
                    working.notes.push(accepted);
                    if working.notes.len() > self.cfg.max_notes {
                        let (idx, _) = working
                            .notes
                            .iter()
                            .enumerate()
                            .min_by(|(ia, a), (ib, b)| {
                                a.lift
                                    .partial_cmp(&b.lift)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                                    .then(ia.cmp(ib))
                            })
                            .expect("non-empty");
                        working.notes.remove(idx);
                    }
                }
                let numbers = serde_json::json!({
                    "improved": v.improved, "regressed": v.regressed,
                    "lift": v.lift, "turns_used": v.turns_used,
                });
                let evidence = vec![Evidence {
                    session: s.session.0.clone(),
                    turn: s.turn,
                    event_id: s.event_id.0,
                }];
                ledger.entries.insert(
                    note.hash.clone(),
                    LedgerEntry {
                        verdict,
                        numbers: numbers.clone(),
                        evidence,
                        at: now,
                    },
                );
                report.candidates.push(CandidateReport {
                    hash: note.hash.clone(),
                    lane: "note",
                    summary: format!("note [{}] {}", note.scope, note.text),
                    verdict,
                    numbers,
                });
            }
        }

        // 8. Apply: file first (atomic), then the hot swap; the ledger is
        //    saved even when nothing was accepted (rejections are verdicts too).
        if !self.cfg.dry_run {
            if working != base {
                save_rules_atomic(&self.learned_path, &working)?;
                self.rules.store(Arc::new(working.clone()));
                report.written = true;
            }
            ledger.save_atomic(&self.ledger_path)?;
        }
        Ok(report)
    }
}

#[async_trait]
impl Consolidator for EvolutionPass {
    async fn run(&self, store: &dyn MemoryStore) -> Result<(), StoreError> {
        match self.run_report(store).await {
            Ok(_) => Ok(()),
            Err(PassError::Store(e)) => Err(e),
            Err(PassError::File(e)) => Err(StoreError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Verdict;
    use crate::notes::{NoteProposer, ProbeRunner, TurnOutcome};
    use nscore::*;
    use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
    use nsengine::store::{InMemoryStore, NoopConsolidator};
    use nsengine::turn::{Engine, EngineConfig};

    struct Closed;
    #[async_trait::async_trait]
    impl Channel for Closed {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    async fn record_into(store: Arc<InMemoryStore>, name: &str, proposals: Vec<Proposal>) {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(Closed));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(1)),
        );
        e.run_turn(Incoming {
            session: SessionId(name.into()),
            text: "go".into(),
        })
        .await
        .unwrap();
    }

    fn typo() -> Proposal {
        Proposal {
            rationale: "".into(),
            action: "eko".into(),
            args: serde_json::json!({"text": "hi"}),
        }
    }
    fn good() -> Proposal {
        Proposal {
            rationale: "".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "hi"}),
        }
    }

    fn pass(
        dir: &std::path::Path,
        rules: Arc<arc_swap::ArcSwap<LearnedRules>>,
        dry_run: bool,
    ) -> EvolutionPass {
        EvolutionPass::new(
            rules,
            vec![EchoTool::new().spec().clone()],
            dir.join("learned.toml"),
            dir.join("ledger.json"),
            PassConfig {
                dry_run,
                ..Default::default()
            },
        )
        .with_clock(Box::new(|| 123))
    }

    #[tokio::test]
    async fn symbolic_patch_is_verified_applied_swapped_in_and_ledgered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "bad", vec![typo()]).await;
        record_into(store.clone(), "clean", vec![good()]).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let p = pass(dir.path(), rules.clone(), false);
        let report = p.run_report(&*store).await.unwrap();
        assert_eq!(report.sessions, 2);
        assert_eq!(report.signatures.get("IllegalNearTool"), Some(&1));
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].verdict, Verdict::Accepted);
        assert!(report.written);
        assert_eq!(
            rules.load().alias("eko"),
            Some("echo"),
            "swapped into the live handle"
        );
        let on_disk = crate::files::load_rules(&dir.path().join("learned.toml")).unwrap();
        assert_eq!(on_disk.alias("eko"), Some("echo"));
        let ledger = Ledger::load(&dir.path().join("ledger.json")).unwrap();
        assert!(ledger.settled(&report.candidates[0].hash));
        let text = report.to_string();
        assert!(
            text.contains("alias_action eko -> echo") && text.contains("accepted"),
            "{text}"
        );

        // Second run: nothing new is proposed (ledger + rules already carry it).
        let report2 = p.run_report(&*store).await.unwrap();
        assert!(report2.candidates.is_empty());
        assert!(!report2.written);
    }

    #[tokio::test]
    async fn dry_run_reports_but_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "bad", vec![typo()]).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules.clone(), true)
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!(report.candidates[0].verdict, Verdict::Accepted);
        assert!(!report.written);
        assert!(!dir.path().join("learned.toml").exists());
        assert!(!dir.path().join("ledger.json").exists());
        assert_eq!(rules.load().alias("eko"), None);
    }

    #[tokio::test]
    async fn broken_chain_sessions_are_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "bad", vec![typo()]).await;
        let mut events = store.load(&SessionId("bad".into())).await.unwrap();
        if let EventKind::UserSaid { text } = &mut events[0].kind {
            *text = "TAMPERED".into();
        }
        let tampered = Arc::new(InMemoryStore::new());
        tampered
            .append(&SessionId("bad".into()), &events)
            .await
            .unwrap();
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules, true)
            .run_report(&*tampered)
            .await
            .unwrap();
        assert_eq!((report.sessions, report.skipped_broken), (0, 1));
    }

    #[tokio::test]
    async fn corrected_key_value_becomes_a_fact() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("c".into());
        let mut l = EventLog::new(sid.clone());
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(
            1,
            Timestamp(2),
            EventKind::Corrected {
                target: None,
                text: "user.city = Brno".into(),
            },
        );
        l.append(
            1,
            Timestamp(3),
            EventKind::Corrected {
                target: None,
                text: "not a fact".into(),
            },
        );
        store.append(&sid, l.events()).await.unwrap();
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules, false)
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!(report.facts_written, 1);
        let facts = store.facts("global", "user.city").await.unwrap();
        assert_eq!(facts[0].value, serde_json::json!("Brno"));
        assert_eq!(facts[0].confidence, 1.0);
    }

    struct FixedProposer(&'static str);
    #[async_trait::async_trait]
    impl NoteProposer for FixedProposer {
        async fn propose(&self, _t: &str, _e: &[Note]) -> Result<Option<Note>, String> {
            Ok(Some(Note::new("global", self.0, 0.0)))
        }
    }
    /// Probe double: the turn succeeds only when a note mentioning "echo" is
    /// in force (the pre-existing "old low" note must not count).
    struct HelpsIfNoted;
    #[async_trait::async_trait]
    impl ProbeRunner for HelpsIfNoted {
        async fn run(
            &self,
            _r: &[Event],
            rules: Arc<LearnedRules>,
        ) -> Result<Vec<TurnOutcome>, String> {
            let helped = rules.notes.iter().any(|n| n.text.contains("echo"));
            Ok(vec![if helped {
                TurnOutcome::Ok
            } else {
                TurnOutcome::Fallback
            }])
        }
    }

    #[tokio::test]
    async fn accepted_note_lands_in_rules_with_its_lift_and_library_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        // A fallback session: scripted emitter proposes 5 illegal actions →
        // max_iterations → fallback.
        record_into(
            store.clone(),
            "fb",
            vec![typo(), typo(), typo(), typo(), typo()],
        )
        .await;
        let mut rules_val = LearnedRules::default();
        rules_val.notes.push(Note::new("global", "old low", 0.1));
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(rules_val.clone()));
        crate::files::save_rules_atomic(&dir.path().join("learned.toml"), &rules_val).unwrap();
        let p = EvolutionPass::new(
            rules.clone(),
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig {
                max_notes: 1,
                ..Default::default()
            },
        )
        .with_notes(
            Box::new(HelpsIfNoted),
            Box::new(FixedProposer("Use echo to repeat text.")),
        );
        let report = p.run_report(&*store).await.unwrap();
        let note_cands: Vec<_> = report
            .candidates
            .iter()
            .filter(|c| c.lane == "note")
            .collect();
        assert!(!note_cands.is_empty(), "{report}");
        assert_eq!(note_cands[0].verdict, Verdict::Accepted);
        let live = rules.load();
        assert_eq!(
            live.notes.len(),
            1,
            "max_notes=1 evicted the lowest-lift note"
        );
        assert_eq!(live.notes[0].text, "Use echo to repeat text.");
        assert!(live.notes[0].lift > 0.0);
        assert!(report.probe_turns_used > 0);
    }
}
