//! The evolution pass (spec M5 §2): load rules + ledger → load and
//! chain-verify sessions → mine → symbolic lane → corrections as facts →
//! notes lane → apply (atomic file + hot swap) → ledger. Implements
//! `Consolidator`, so the idle driver and `ns-app evolve` share one path.
use crate::evaluate::{evaluate, EvaluateConfig, Evaluator, SymbolicEvaluator};
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
    /// Which evaluator's recorded grade the notes gate believes when several
    /// have graded the same turn (M9 T1.2).
    ///
    /// One name rather than a merge rule: two scorers that disagree are a κ
    /// measurement, not something to average away, and the gate has to be
    /// able to say *whose* verdict it acted on.
    pub authoritative_evaluator: String,
    /// M9 T4.4: exposures a fact needs before zero credits mean anything.
    pub fitness_min_exposures: u32,
    /// Whether the fitness signal demotes or only reports. Off for a release.
    pub fitness_demote: bool,
    /// Key prefixes the fitness signal never demotes (`[memory]
    /// pinned_prefixes`). Threaded through the pass the way `fact_stale_days`
    /// is: the knob belongs to memory, and the pass is where it is applied.
    pub pinned_prefixes: Vec<String>,
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
            authoritative_evaluator: "symbolic".into(),
            fitness_min_exposures: 8,
            fitness_demote: false,
            pinned_prefixes: vec!["user.".into()],
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
    /// Turns this run graded for the first time (M9 T1.3).
    pub graded_turns: usize,
    /// Turns whose grade was read back out of the log instead of recomputed.
    /// The number T2.3a exists for: on a second run it is everything.
    pub grades_reused: usize,
    /// Grade attempts the scorer could not answer. Not a failure of a turn
    /// and not counted against anything — the service was down.
    pub grades_unavailable: usize,
    pub facts_written: usize,
    pub written: bool,
    /// M6 §6.4 fact consolidation numbers.
    pub consolidation: crate::consolidate::ConsolidationReport,
    /// M9 T4.3 fitness numbers, derived from the log on every run.
    pub fitness: crate::fitness::FitnessReport,
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
        writeln!(
            f,
            "graded turns: {} newly graded, {} read back from the log, {} unavailable",
            self.graded_turns, self.grades_reused, self.grades_unavailable
        )?;
        writeln!(f, "probe turns used: {}", self.probe_turns_used)?;
        writeln!(f, "facts written: {}", self.facts_written)?;
        // M9 T4.5: the numbers this phase exists to make readable without
        // opening SQLite. `exposures` without `credits` is the whole point —
        // a fact in every prompt and in no answer is the one worth finding.
        writeln!(
            f,
            "fitness: {} facts derived, {} with exposures, {} zero-credit",
            self.fitness.facts.len(),
            self.fitness.with_exposures(),
            self.fitness.zero_credit()
        )?;
        for (key, exposures) in self.fitness.top_zero_credit(10) {
            writeln!(f, "  {key}  {exposures}")?;
        }
        for session in &self.fitness.graded_sessions_with_zero_exposures {
            writeln!(f, "alarm: graded session {session} exposes no fact")?;
        }
        for (hash, n) in &self.fitness.notes {
            let lift = self.fitness.note_lifts.get(hash).copied().unwrap_or(0.0);
            writeln!(
                f,
                "  note {hash}  lift {lift:.3}  {} exposures  {} credits",
                n.exposures, n.credits
            )?;
        }
        writeln!(f, "{}", self.consolidation)?;
        write!(f, "learned.toml written: {}", self.written)
    }
}

pub struct EvolutionPass {
    rules: Arc<ArcSwap<LearnedRules>>,
    known_specs: Vec<ActionSpec>,
    probe: Option<Box<dyn ProbeRunner>>,
    proposer: Option<Box<dyn NoteProposer>>,
    /// Who grades a turn (M8 T2.3a). The symbolic checks are always here —
    /// they cost nothing and they are what every other scorer is calibrated
    /// against — and `with_evaluator` adds the ones that do cost something.
    evaluators: Vec<Arc<dyn Evaluator>>,
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

/// Does `turn` already carry a grade from `by` — in the log, or in what this
/// run has just decided to append to it?
fn graded_by(
    events: &[nscore::Event],
    pending: Option<&Vec<nscore::EventKind>>,
    turn: u32,
    by: &str,
) -> bool {
    let hit = |k: &nscore::EventKind| matches!(k, nscore::EventKind::Graded { turn: t, by: b, .. } if *t == turn && b == by);
    events.iter().any(|e| hit(&e.kind)) || pending.map(|v| v.iter().any(hit)).unwrap_or(false)
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
        let symbolic = Arc::new(SymbolicEvaluator {
            cfg: cfg.evaluate.clone(),
        });
        Self {
            rules,
            known_specs,
            probe: None,
            proposer: None,
            evaluators: vec![symbolic],
            learned_path,
            ledger_path,
            cfg,
            clock: Box::new(now_ms),
        }
    }
    /// Add a scorer beside the symbolic one. Its grades are recorded under
    /// its own id, so two evaluators never overwrite each other.
    pub fn with_evaluator(mut self, e: Arc<dyn Evaluator>) -> Self {
        self.evaluators.push(e);
        self
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

    /// Step 4b: grade what nothing has graded yet, and read back the rest.
    ///
    /// The recorded grades are consulted *first* and the scorer is never
    /// asked about a turn that already has one from it. That single rule is
    /// T2.3a: a second pass costs nothing, a replay costs nothing, and no
    /// path from a recorded session to a verdict runs through a model.
    ///
    /// The new events go into `sessions` whether or not this is a dry run —
    /// the gate downstream should see the same grades either way — but they
    /// only reach the store when it is not.
    async fn grade_sessions(
        &self,
        store: &dyn MemoryStore,
        sessions: &mut [Recorded],
        report: &mut Report,
        now: u64,
    ) -> Result<(), PassError> {
        let per_session: Vec<Vec<crate::evaluate::RecordedTurn>> = sessions
            .iter()
            .map(|(_, e)| crate::evaluate::recorded_turns(e))
            .collect();
        // Newest first — a budget too small for every turn should be spent on
        // what just happened — and ties broken by position, so the queue is a
        // total order and two runs walk it identically.
        let mut queue: Vec<(usize, usize)> = Vec::new();
        for (si, turns) in per_session.iter().enumerate() {
            for ti in 0..turns.len() {
                queue.push((si, ti));
            }
        }
        queue.sort_by(|a, b| {
            per_session[b.0][b.1]
                .at
                .cmp(&per_session[a.0][a.1].at)
                .then(a.cmp(b))
        });

        let mut budget = self.cfg.evaluate.budget_turns;
        let mut fresh: BTreeMap<usize, Vec<nscore::EventKind>> = BTreeMap::new();
        for (si, ti) in queue {
            let t = &per_session[si][ti];
            let mut wanted: Vec<&Arc<dyn Evaluator>> = Vec::new();
            for ev in &self.evaluators {
                if graded_by(&sessions[si].1, fresh.get(&si), t.turn, &ev.id()) {
                    report.grades_reused += 1;
                } else {
                    wanted.push(ev);
                }
            }
            if wanted.is_empty() || budget == 0 {
                continue;
            }
            budget -= 1;
            let shown = t.shown_refs();
            let view = t.view(&shown);
            let mut any = false;
            for ev in wanted {
                match ev.grade(&view).await {
                    Ok(g) => {
                        any = true;
                        fresh
                            .entry(si)
                            .or_default()
                            .push(nscore::EventKind::Graded {
                                turn: t.turn,
                                grade: (&g).into(),
                                by: ev.id(),
                                revision: ev.revision(),
                            });
                    }
                    // The service was down or answered with nonsense. Neither
                    // is a fact about the turn, so nothing is written and the
                    // turn stays ungraded for the next pass to try.
                    Err(_) => report.grades_unavailable += 1,
                }
            }
            if any {
                report.graded_turns += 1;
            }
        }

        for (si, kinds) in fresh {
            let (sid, events) = &mut sessions[si];
            let mut log = EventLog::from_events(sid.clone(), events.clone());
            let before = log.events().len();
            for k in kinds {
                let turn = match &k {
                    nscore::EventKind::Graded { turn, .. } => *turn,
                    _ => 0,
                };
                log.append(turn, Timestamp(now), k);
            }
            let added: Vec<nscore::Event> = log.events()[before..].to_vec();
            if !self.cfg.dry_run {
                store.append(sid, &added).await?;
            }
            *events = log.events().to_vec();
        }
        Ok(())
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
        // 4b. Grade (M8 T2.3a, M9 T1.1/T1.3).
        //
        // A grade is a recorded value. Every turn already carrying a
        // `Graded` event from this evaluator is *read back*, never
        // recomputed — that is the replay property, and it is why a second
        // run of this pass grades nothing and a replay needs no service. The
        // rest are graded newest first until the budget runs out, and each
        // new grade is appended to its session's log as one more event: the
        // chain is extended, never rewritten.
        self.grade_sessions(store, &mut sessions, &mut report, now)
            .await?;

        // 4b'. Mine the success lane (M9 T5.1).
        //
        // A third source, and the only one that reads every session at once:
        // `Succeeded` is a route two graded-good turns both took, and no
        // single session's log can show that. It runs *after* grading and
        // not with the rest of step 4, because `grade_sessions` appends this
        // run's fresh verdicts into `sessions` — mining first would mean a
        // session could never yield a strategy until a second pass, which is
        // the sort of silence that reads as "nothing found".
        sigs.extend(crate::mine::mine_succeeded(
            &sessions,
            &self.cfg.authoritative_evaluator,
        ));
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

        // 4c. Fitness (M9 T4.3): the join from outcome back to selection.
        //
        // After grading, so this run's fresh verdicts count; before
        // consolidation, so the demotion signal reads numbers derived from the
        // log rather than whatever the last pass left in the columns. The
        // numbers are *derived and set*, never incremented — running this
        // twice over one log produces one answer.
        report.fitness =
            crate::fitness::derive(store, &sessions, &self.cfg.authoritative_evaluator).await?;
        for note in &base.notes {
            report
                .fitness
                .note_lifts
                .insert(note.hash.clone(), note.lift);
            report.fitness.notes.entry(note.hash.clone()).or_default();
        }
        if !self.cfg.dry_run {
            for (scope, key, exposures, credits) in &report.fitness.facts {
                store
                    .set_fact_fitness(scope, key, *exposures, *credits)
                    .await?;
            }
            ledger.fitness = report.fitness.notes.clone();
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
                fitness_min_exposures: self.cfg.fitness_min_exposures,
                fitness_demote: self.cfg.fitness_demote,
                pinned_prefixes: self.cfg.pinned_prefixes.clone(),
                fitness: report
                    .fitness
                    .facts
                    .iter()
                    .map(|(s, k, e, c)| ((s.clone(), k.clone()), (*e, *c)))
                    .collect(),
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
                let note = match proposer
                    .propose(&trace, &working.notes, &s.kind.ask())
                    .await
                {
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
                    // M9 T1.2: a reply-quality candidate needs a recorded
                    // grade in the sessions it is probed on; an emitter-side
                    // one keeps the path it had.
                    &crate::notes::NoteGate {
                        require_graded: s.kind.from_grades(),
                        authoritative: self.cfg.authoritative_evaluator.clone(),
                    },
                )
                .await;
                report.probe_turns_used += v.turns_used;
                let verdict = if v.accepted {
                    Verdict::Accepted
                } else if v.unverified {
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
                    // Why a candidate is unverified is the only part of this
                    // that is not a number, and the only part worth reading
                    // when it is.
                    "detail": v.detail,
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
        async fn recv(&self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
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
        async fn propose(
            &self,
            _t: &str,
            _e: &[Note],
            _ask: &crate::notes::Ask,
        ) -> Result<Option<Note>, String> {
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

    // -----------------------------------------------------------------
    // M8 T2.3a / M9 T1.1, T1.3 — a grade is a recorded value
    // -----------------------------------------------------------------

    /// A scripted scorer that answers once and then reports the service gone.
    ///
    /// The second state is the test's whole point: if anything downstream of
    /// the first pass asks it a second question, the answer is an error, and
    /// the recorded grade has to carry the session on its own.
    struct StoppableEvaluator {
        stopped: std::sync::atomic::AtomicBool,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Evaluator for StoppableEvaluator {
        fn id(&self) -> String {
            "scripted".into()
        }
        fn revision(&self) -> String {
            "corpus-2026-09-09".into()
        }
        async fn grade(
            &self,
            _view: &crate::evaluate::TurnView<'_>,
        ) -> Result<crate::evaluate::TurnGrade, crate::evaluate::GradeError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::evaluate::GradeError::Unavailable("stopped".into()));
            }
            Ok(crate::evaluate::TurnGrade {
                issue: crate::evaluate::Issue::Reask,
                answers_user: 0,
                grounded: true,
                scorer: "scripted".into(),
            })
        }
    }

    fn graded_events(events: &[nscore::Event]) -> Vec<&nscore::Event> {
        events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Graded { .. }))
            .collect()
    }

    /// T2.3a's exit criterion, in one test: grade a session, stop the
    /// scorer, read the log back — the verdicts are byte-identical and the
    /// fold is untouched, because nothing recomputed them.
    #[tokio::test]
    async fn grading_a_session_then_replaying_it_yields_identical_grades_without_a_service() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "s", vec![good()]).await;
        let sid = SessionId("s".into());

        let before = store.load(&sid).await.unwrap();
        let fold_before = nsengine::state::fold(&before);
        let normalized_before = nsengine::replay::normalize(&before);
        assert!(graded_events(&before).is_empty());

        let scorer = Arc::new(StoppableEvaluator {
            stopped: std::sync::atomic::AtomicBool::new(false),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules.clone(), false)
            .with_evaluator(scorer.clone())
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!(report.graded_turns, 1, "{report}");
        assert_eq!(scorer.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let after = store.load(&sid).await.unwrap();
        let recorded: Vec<String> = graded_events(&after)
            .iter()
            .map(|e| serde_json::to_string(&e.kind).unwrap())
            .collect();
        // Both evaluators graded: the always-present symbolic one and ours.
        assert!(
            recorded
                .iter()
                .any(|j| j.contains(r#""by":"scripted""#) && j.contains("corpus-2026-09-09")),
            "{recorded:?}"
        );
        assert!(recorded.iter().any(|j| j.contains(r#""by":"symbolic""#)));
        assert!(
            recorded
                .iter()
                .any(|j| j.contains(r#""ok":false"#) && j.contains("reask")),
            "{recorded:?}"
        );
        // The chain still verifies — the grades extended it, nothing was
        // rewritten.
        assert!(EventLog::from_events(sid.clone(), after.clone())
            .verify_chain()
            .is_ok());
        // And the behavioural projections are untouched.
        assert_eq!(nsengine::replay::normalize(&after), normalized_before);
        assert_eq!(nsengine::state::fold(&after), fold_before);

        // Now stop the service and run the pass again with a scorer that
        // cannot answer anything.
        scorer
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let report2 = pass(dir.path(), rules, false)
            .with_evaluator(scorer.clone())
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!(
            (report2.graded_turns, report2.grades_unavailable),
            (0, 0),
            "a stopped scorer was never asked: {report2}"
        );
        assert!(report2.grades_reused >= 2, "{report2}");
        assert_eq!(scorer.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let replayed = store.load(&sid).await.unwrap();
        let again: Vec<String> = graded_events(&replayed)
            .iter()
            .map(|e| serde_json::to_string(&e.kind).unwrap())
            .collect();
        assert_eq!(again, recorded, "verdicts are byte-identical");
        assert_eq!(nsengine::state::fold(&replayed), fold_before);
    }

    /// M9 T1.3. The budget only ever pays for turns nothing has graded yet.
    #[tokio::test]
    async fn a_second_pass_grades_nothing_already_graded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "a", vec![good()]).await;
        record_into(store.clone(), "b", vec![good()]).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));

        let first = pass(dir.path(), rules.clone(), false)
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!((first.graded_turns, first.grades_reused), (2, 0), "{first}");

        let second = pass(dir.path(), rules.clone(), false)
            .run_report(&*store)
            .await
            .unwrap();
        assert_eq!(
            (second.graded_turns, second.grades_reused),
            (0, 2),
            "{second}"
        );
    }

    /// The budget is a cap on *new* grades, and a dry run writes none.
    #[tokio::test]
    async fn the_budget_caps_new_grades_and_a_dry_run_writes_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        record_into(store.clone(), "a", vec![good()]).await;
        record_into(store.clone(), "b", vec![good()]).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));

        let capped = EvolutionPass::new(
            rules.clone(),
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig {
                evaluate: EvaluateConfig {
                    budget_turns: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .with_clock(Box::new(|| 123));
        let r = capped.run_report(&*store).await.unwrap();
        assert_eq!(r.graded_turns, 1, "{r}");

        // A dry run reports what it would grade and leaves the log alone.
        let store2 = Arc::new(InMemoryStore::new());
        record_into(store2.clone(), "a", vec![good()]).await;
        let dry = pass(dir.path(), rules, true)
            .run_report(&*store2)
            .await
            .unwrap();
        assert_eq!(dry.graded_turns, 1, "{dry}");
        let events = store2.load(&SessionId("a".into())).await.unwrap();
        assert!(graded_events(&events).is_empty(), "dry run wrote a grade");
    }

    /// M9 T4.5. The exit criterion for the whole phase is that the numbers are
    /// readable without opening SQLite — a fact in every prompt and in no
    /// answer has to be visible in the dry run, beside the notes and beside
    /// the alarm that says the join matched nothing.
    #[tokio::test]
    async fn the_dry_run_prints_exposures_and_credits_per_fact_and_note() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        let note = Note::new("global", "prefer the shortest action", 0.25);
        crate::files::save_rules_atomic(
            &dir.path().join("learned.toml"),
            &LearnedRules {
                notes: vec![note.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        for key in ["shop.promo", "shop.hours"] {
            store
                .put_fact(Fact {
                    key: key.into(),
                    value: serde_json::json!("x"),
                    valid_from: Timestamp(1),
                    last_validated: Timestamp(1),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        // One session: two turns, both showing both facts and the note; the
        // first graded good, the second graded bad and citing one fact.
        let sid = SessionId("s".into());
        let mut log = EventLog::new(sid.clone());
        for (turn, ok) in [(1u32, true), (2, false)] {
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::UserSaid {
                    text: "what is the promo".into(),
                },
            );
            log.append(
                turn,
                Timestamp(turn as u64),
                EventKind::ModelCall {
                    usage: usage(),
                    manifest: ContextManifest {
                        fact_keys: vec!["shop.promo".into(), "shop.hours".into()],
                        guidance: 1,
                        note_hashes: vec![note.hash.clone()],
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
        log.append(
            2,
            Timestamp(2),
            EventKind::ReplyCited {
                sources: vec!["fact:shop.hours".into()],
            },
        );
        store.append(&sid, log.events()).await.unwrap();
        // A second session that was graded and showed nothing: the alarm.
        let blind = SessionId("blind".into());
        let mut b = EventLog::new(blind.clone());
        b.append(1, Timestamp(9), EventKind::UserSaid { text: "hi".into() });
        b.append(
            1,
            Timestamp(9),
            EventKind::ModelCall {
                usage: usage(),
                manifest: ContextManifest::default(),
            },
        );
        b.append(
            1,
            Timestamp(9),
            EventKind::Graded {
                turn: 1,
                grade: Grade {
                    ok: true,
                    issues: vec![],
                },
                by: "symbolic".into(),
                revision: "r1".into(),
            },
        );
        store.append(&blind, b.events()).await.unwrap();

        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let p = EvolutionPass::new(
            rules,
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig {
                dry_run: true,
                // Every recorded grade is read back, so the scorer is never
                // asked and the numbers are the fixture's own.
                fitness_min_exposures: 2,
                ..Default::default()
            },
        )
        .with_clock(Box::new(|| 123));
        let report = p.run_report(&*store).await.unwrap();

        // `shop.promo` was shown twice and credited once (the good turn);
        // `shop.hours` twice and credited twice (good turn, then cited in the
        // bad one). Neither is zero-credit, so the top-10 list is empty here
        // and the demote set with it.
        assert_eq!(
            report.fitness.facts,
            vec![
                ("global".into(), "shop.hours".into(), 2, 2),
                ("global".into(), "shop.promo".into(), 2, 1),
            ]
        );
        let text = report.to_string();
        assert!(
            text.contains("fitness: 2 facts derived, 2 with exposures, 0 zero-credit"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "note {}  lift 0.250  2 exposures  1 credits",
                note.hash
            )),
            "{text}"
        );
        assert!(
            text.contains("alarm: graded session blind exposes no fact"),
            "{text}"
        );
        assert!(
            text.contains("fitness demote: 0 candidates, knob off"),
            "{text}"
        );

        // Dry run: nothing reached the store.
        let stored = store.facts("global", "shop").await.unwrap();
        assert!(
            stored.iter().all(|f| f.exposures == 0 && f.credits == 0),
            "{stored:?}"
        );
        assert!(Ledger::load(&dir.path().join("ledger.json"))
            .unwrap()
            .fitness
            .is_empty());

        // And the same pass, not dry, writes exactly those numbers — then a
        // third derivation over the same log reproduces them.
        let p = EvolutionPass::new(
            Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default())),
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig {
                fitness_min_exposures: 2,
                ..Default::default()
            },
        )
        .with_clock(Box::new(|| 123));
        let wet = p.run_report(&*store).await.unwrap();
        assert_eq!(wet.fitness.facts, report.fitness.facts);
        let stored = store.facts("global", "shop").await.unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|f| (f.key.clone(), f.exposures, f.credits))
                .collect::<Vec<_>>(),
            vec![
                ("shop.hours".to_string(), 2, 2),
                ("shop.promo".to_string(), 2, 1)
            ]
        );
        assert_eq!(
            Ledger::load(&dir.path().join("ledger.json"))
                .unwrap()
                .fitness
                .get(&note.hash)
                .copied(),
            Some(crate::fitness::NoteFitness {
                exposures: 2,
                credits: 1
            })
        );
        let again = p.run_report(&*store).await.unwrap();
        assert_eq!(again.fitness.facts, wet.fitness.facts, "idempotent");
        let stored_again = store.facts("global", "shop").await.unwrap();
        assert_eq!(
            stored_again
                .iter()
                .map(|f| (f.exposures, f.credits))
                .collect::<Vec<_>>(),
            vec![(2, 2), (2, 1)]
        );
        // No version was added by the rescoring.
        assert_eq!(
            store
                .fact_history("global", "shop.promo")
                .await
                .unwrap()
                .len(),
            1
        );
    }

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
}
