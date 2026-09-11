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
    /// M8 T2.7 / M10 T5.3: the κ an evaluator must reach against the symbolic
    /// proxies before the notes gate will believe it.
    ///
    /// Below it — or on a table too small to estimate κ from at all — the
    /// evaluator still grades, and every grade is still recorded as a
    /// `Graded` event (T2.3a). What it loses is authority: the gate falls
    /// back to the symbolic checks, so the evaluator contributes
    /// observations and no candidates. That is the same treatment a paid
    /// judge below threshold gets, which is the whole argument for admitting
    /// a local scorer here at all.
    pub evaluator_min_kappa: f64,
    /// M9 T4.4: exposures a fact needs before zero credits mean anything.
    pub fitness_min_exposures: u32,
    /// Whether the fitness signal demotes or only reports. Off for a release.
    pub fitness_demote: bool,
    /// Key prefixes the fitness signal never demotes (`[memory]
    /// pinned_prefixes`). Threaded through the pass the way `fact_stale_days`
    /// is: the knob belongs to memory, and the pass is where it is applied.
    pub pinned_prefixes: Vec<String>,
    /// M8 T3.1: rows the embeddings backfill may embed in one batch, and how
    /// many batches one pass may run.
    ///
    /// Two numbers rather than one total because they bound different
    /// things. The batch is one `/embed` round trip and therefore one
    /// memory spike on a CPU encoder; the cap is how long the *pass* may
    /// spend on it before the idle window is needed for something else. The
    /// backfill is resumable, so a cap that stops early costs nothing but a
    /// later pass finishing the job.
    ///
    /// This never runs in a turn. It is a step of `run_report`, which runs
    /// while the harness is waiting for the next message — the guard M8
    /// names for the risk "the embeddings backfill runs in a turn".
    pub embed_backfill_batch: usize,
    pub embed_backfill_batches: usize,
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
            evaluator_min_kappa: 0.4,
            fitness_min_exposures: 8,
            fitness_demote: false,
            pinned_prefixes: vec!["user.".into()],
            embed_backfill_batch: 64,
            embed_backfill_batches: 16,
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

/// The evaluator every other one is calibrated against (M8 T2.7).
///
/// Not `authoritative_evaluator`: that knob says whose verdict the gate acts
/// on, and it is the thing κ decides. The reference has to be the scorer that
/// costs nothing, never fails and is already in the diff — otherwise a scorer
/// could be promoted by agreeing with itself.
const KAPPA_REFERENCE: &str = "symbolic";

/// `n` a ±0.2 interval on κ needs (Donner–Eliasziw, see `kappa.rs`), and the
/// positives below which the estimate is resting on a handful of turns.
const KAPPA_MIN_N: u32 = 96;
const KAPPA_MIN_POSITIVES: u32 = 10;

/// How one evaluator agreed with the reference, over the turns both graded.
///
/// M10 T5.3. The whole table travels rather than κ alone, because κ alone is
/// unreadable at this lane's prevalence — see the module docs of
/// [`crate::kappa`].
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatorAgreement {
    pub evaluator: String,
    pub reference: String,
    /// `None` when the two never graded the same turn — which, for a scorer
    /// that reports `Unavailable` on every call, is what a service being
    /// down looks like from here.
    pub scores: Option<crate::kappa::Scores>,
    /// Whether the table is big enough for its κ to be worth reading.
    pub decisive: bool,
    /// Whether the notes gate acted on this evaluator's grades this run.
    pub authoritative: bool,
    /// Why it did not, empty when it did.
    pub caveat: String,
}

impl EvaluatorAgreement {
    /// May the gate believe this evaluator? Only with a κ at or above the
    /// threshold *and* a table decisive enough to have measured it. An
    /// indecisive κ is treated as below threshold, never as absent.
    pub fn trusted(&self, min_kappa: f64) -> bool {
        self.decisive && self.scores.as_ref().is_some_and(|s| s.kappa >= min_kappa)
    }
}

impl std::fmt::Display for EvaluatorAgreement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "κ {} vs {}: ", self.evaluator, self.reference)?;
        match &self.scores {
            None => write!(f, "unavailable (service down)")?,
            Some(s) => write!(
                f,
                "{:.2} [{:.2}, {:.2}] over {} turns (ac1 {:.2}, rate {:.2} vs {:.2})",
                s.kappa, s.kappa_lo, s.kappa_hi, s.n, s.ac1, s.a_rate, s.b_rate
            )?,
        }
        if !self.caveat.is_empty() {
            write!(f, " — {}", self.caveat)?;
        }
        Ok(())
    }
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
    /// M10 T5.3: one entry per evaluator beside the symbolic baseline.
    pub agreement: Vec<EvaluatorAgreement>,
    /// M11 T1.3: requests each evaluator spent this pass, for the ones that
    /// spend any. Absent for the free lanes rather than zero — "this scorer
    /// costs nothing" and "this scorer was asked nothing" are different
    /// facts, and only the second is a number.
    pub evaluator_requests: BTreeMap<String, u32>,
    /// The evaluator the notes gate actually believed this run, after the κ
    /// threshold had its say — which is not always the configured one.
    pub authoritative_evaluator: String,
    /// Set when the configured authoritative evaluator was demoted, to the
    /// name it was demoted from.
    pub demoted_from: Option<String>,
    pub facts_written: usize,
    pub written: bool,
    /// M6 §6.4 fact consolidation numbers.
    pub consolidation: crate::consolidate::ConsolidationReport,
    /// M9 T4.3 fitness numbers, derived from the log on every run.
    pub fitness: crate::fitness::FitnessReport,
    /// Rejected proposals bucketed by the guard that fired, summed over
    /// every session this pass read (M10 T0.2). A rejection is a request
    /// already spent, and the pass is the only place that sees every session
    /// at once — which is what makes the rate comparable between runs, and
    /// what makes it the instrument M10's prompt-side loop fix is graded on.
    pub rejections: nscore::RejectionTally,
    /// Rows the embeddings backfill embedded this run (M8 T3.1). 0 on a
    /// store with no encoder, and 0 on a second pass over the same log —
    /// that second 0 is the resumability property, reported rather than
    /// asserted.
    pub embedded: usize,
    /// Set when the backfill stopped on `embed_backfill_batches` with rows
    /// still unembedded, so a reader knows the next pass has work rather
    /// than that the log is fully indexed.
    pub embed_backfill_incomplete: bool,
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
        for a in &self.agreement {
            writeln!(f, "{a}")?;
        }
        // Printed next to κ deliberately: for a paid judge the two numbers
        // are one sentence — what the agreement cost.
        for (id, n) in &self.evaluator_requests {
            writeln!(f, "requests spent: {id} {n}")?;
        }
        write!(
            f,
            "authoritative evaluator: {}",
            self.authoritative_evaluator
        )?;
        match &self.demoted_from {
            Some(from) => writeln!(
                f,
                " (configured {from:?} demoted: observations only, no candidates)"
            )?,
            None => writeln!(f)?,
        }
        writeln!(
            f,
            "embeddings backfilled: {}{}",
            self.embedded,
            if self.embed_backfill_incomplete {
                " (batch cap reached — the next pass continues)"
            } else {
                ""
            }
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
        // M10 T0.2. Printed next to the signatures because it is the same
        // kind of number and the cheaper one: a signature says a turn went
        // wrong, this says how much of the day's request budget the harness
        // spent finding out.
        writeln!(f, "rejections by reason: {}", self.rejections.line())?;
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

    /// Step 4b': κ of every evaluator beside the reference (M8 T2.7, M10 T5.3).
    ///
    /// Read out of the *log*, not out of this run's grades. A pass that
    /// regraded nothing still reports κ, because the grades it is computed
    /// from are recorded values — which is the same property T2.3a buys for
    /// replay, applied to calibration.
    ///
    /// Only turns *both* graded enter the table. A turn the local scorer
    /// could not answer is absent rather than counted as agreement, so a
    /// service that is half up cannot inflate its own κ by staying silent on
    /// the turns it would have got wrong.
    fn agreement(&self, sessions: &[Recorded]) -> Vec<EvaluatorAgreement> {
        // evaluator id -> (session, turn) -> did it see a problem here.
        // `BTreeMap` twice so the two label streams are aligned by key and
        // the order is the same on every run.
        let mut labels: BTreeMap<String, BTreeMap<(usize, u32), bool>> = BTreeMap::new();
        for (si, (_, events)) in sessions.iter().enumerate() {
            for e in events {
                if let nscore::EventKind::Graded {
                    turn, grade, by, ..
                } = &e.kind
                {
                    labels
                        .entry(by.clone())
                        .or_default()
                        .insert((si, *turn), !grade.ok);
                }
            }
        }
        let empty = BTreeMap::new();
        let reference = labels.get(KAPPA_REFERENCE).unwrap_or(&empty);

        let mut out = Vec::new();
        for ev in &self.evaluators {
            let id = ev.id();
            if id == KAPPA_REFERENCE {
                continue;
            }
            let mut under_test = Vec::new();
            let mut against = Vec::new();
            if let Some(mine) = labels.get(&id) {
                for (key, v) in mine {
                    if let Some(r) = reference.get(key) {
                        under_test.push(*v);
                        against.push(*r);
                    }
                }
            }
            let (scores, decisive) = if under_test.is_empty() {
                (None, false)
            } else {
                let table = crate::kappa::Agreement::tally(&under_test, &against);
                let d = table.decisive(KAPPA_MIN_N, KAPPA_MIN_POSITIVES);
                (Some(table.scores()), d)
            };
            let caveat = match &scores {
                None => "no turn was graded by both; observations only".to_string(),
                Some(sc) if sc.kappa < self.cfg.evaluator_min_kappa => format!(
                    "κ below evaluator_min_kappa {:.2}: observations only, no candidates",
                    self.cfg.evaluator_min_kappa
                ),
                Some(sc) if !decisive => format!(
                    "not decisive (n={} needs {KAPPA_MIN_N}, positives={} needs                      {KAPPA_MIN_POSITIVES}): observations only, no candidates",
                    sc.n, sc.positives
                ),
                Some(_) => String::new(),
            };
            out.push(EvaluatorAgreement {
                evaluator: id,
                reference: KAPPA_REFERENCE.to_string(),
                scores,
                decisive,
                authoritative: false,
                caveat,
            });
        }
        out
    }

    /// Whose grades the notes gate believes this run.
    ///
    /// The configured name, unless κ says it has not earned it — and then the
    /// reference, which needs no calibration because it *is* the
    /// calibration. Demotion is the conservative direction by construction:
    /// the grades are all still in the log, so nothing is lost but authority.
    fn effective_authoritative(
        &self,
        agreement: &[EvaluatorAgreement],
    ) -> (String, Option<String>) {
        let want = self.cfg.authoritative_evaluator.clone();
        if want == KAPPA_REFERENCE {
            return (want, None);
        }
        match agreement.iter().find(|a| a.evaluator == want) {
            Some(a) if a.trusted(self.cfg.evaluator_min_kappa) => (want, None),
            _ => (KAPPA_REFERENCE.to_string(), Some(want)),
        }
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
            // M10 T0.2: the same tally `ns-app budget` prints per session,
            // summed over the pass's sessions. Free — it is one walk of
            // events already in memory, and it needs no model.
            let t = nscore::tally_rejections(events);
            report.rejections.proposals += t.proposals;
            for (reason, n) in t.by_reason {
                *report.rejections.by_reason.entry(reason).or_default() += n;
            }
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
        // M11 T1.3. Read off the evaluators rather than counted here: only
        // the scorer knows what it dialled, retries included, and a tally
        // kept beside `grade_sessions` would count grade *attempts*, which
        // is a different number from requests the moment anything retries.
        for ev in &self.evaluators {
            if let Some(n) = ev.requests() {
                report.evaluator_requests.insert(ev.id(), n);
            }
        }

        // 4b°. The embeddings backfill (M8 T3.1).
        //
        // Here and nowhere else. The recall path reads vectors; something has
        // to write them, and the only two places that could are a turn and
        // this pass. A turn is ruled out by M8's own risk line — an
        // embedding is a network round trip per batch, and a turn that waited
        // on one would pay a recall cost at the moment it has none to spare.
        //
        // After grading rather than before it: grading is what the idle
        // window is *for*, and a backfill of a long log would otherwise spend
        // the window and leave the grades for next time. Bounded, resumable,
        // and silent on a store with no encoder — `backfill_embeddings`
        // returns 0 without dialling anything.
        //
        // A dry run writes nothing anywhere else and writes nothing here.
        if !self.cfg.dry_run {
            let batch = self.cfg.embed_backfill_batch;
            // A full last batch is how "there may be more" presents itself
            // without a second query: the store selects `limit` rows and
            // stops. Reported as *may have more*, which is the honest claim.
            let mut last_was_full = false;
            for _ in 0..self.cfg.embed_backfill_batches {
                match store.backfill_embeddings(batch).await {
                    Ok(0) => {
                        last_was_full = false;
                        break;
                    }
                    Ok(n) => {
                        report.embedded += n;
                        last_was_full = n == batch;
                    }
                    // The service being down is not a failed pass. Every
                    // other lane here degrades the same way.
                    Err(_) => {
                        last_was_full = false;
                        break;
                    }
                }
            }
            report.embed_backfill_incomplete = last_was_full;
        }

        // 4b′. Calibration (M8 T2.7, M10 T5.3), before anything downstream
        // asks whose verdict to believe. It has to run here and not at
        // report time: `authoritative` is an input to the success lane, to
        // fitness and to the notes gate, and all three are below.
        report.agreement = self.agreement(&sessions);
        let (authoritative, demoted_from) = self.effective_authoritative(&report.agreement);
        for a in &mut report.agreement {
            a.authoritative = a.evaluator == authoritative;
        }
        report.authoritative_evaluator = authoritative.clone();
        report.demoted_from = demoted_from;

        // 4b''. Mine the success lane (M9 T5.1).
        //
        // A third source, and the only one that reads every session at once:
        // `Succeeded` is a route two graded-good turns both took, and no
        // single session's log can show that. It runs *after* grading and
        // not with the rest of step 4, because `grade_sessions` appends this
        // run's fresh verdicts into `sessions` — mining first would mean a
        // session could never yield a strategy until a second pass, which is
        // the sort of silence that reads as "nothing found".
        sigs.extend(crate::mine::mine_succeeded(&sessions, &authoritative));
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
        report.fitness = crate::fitness::derive(store, &sessions, &authoritative).await?;
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
                        authoritative: authoritative.clone(),
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

    /// A reply that answers in the user's own words — and, when the user
    /// asks for "details", invents some.
    ///
    /// A κ table whose reference never changes its mind is the prevalence
    /// paradox rather than a measurement, so the fixture has to be able to
    /// make the symbolic checks say *yes* on some turns and *no* on others.
    /// `ScriptedReplier` cannot: it echoes the whole turn trace, which the
    /// grounding check reads as a page of unsupported claims on every turn.
    /// The three invented claims below are what `ground::ungrounded` is
    /// built to catch — a name, a number and a place nothing showed it.
    struct PlainReplier;
    #[async_trait::async_trait]
    impl nscore::Replier for PlainReplier {
        async fn reply(&self, ctx: nscore::ReplyContext) -> Result<String, nscore::ReplyError> {
            let mut r = format!("sure, about {}: done", ctx.user_text);
            if ctx.user_text.contains("details") {
                r.push_str(", Martin has 42 orders in Oslo");
            }
            Ok(r)
        }
    }

    /// `record_into` with the user's words under the harness's control, so a
    /// scorer can be scripted per turn without a session id it cannot see.
    async fn record_asking(store: Arc<InMemoryStore>, name: &str, text: &str, p: Vec<Proposal>) {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(p)));
        b.set_replier(Box::new(PlainReplier));
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
            text: text.into(),
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

    /// A constant-vector encoder. The pass does not care what a vector
    /// *means* — the ranking is tested where the ranking lives — only that
    /// something was written and that a second run writes nothing.
    struct FlatEncoder(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl nscore::TextEncoder for FlatEncoder {
        fn model(&self) -> &str {
            "bge-m3"
        }
        async fn embed(&self, texts: &[String], _kind: &str) -> Result<Vec<Vec<f32>>, StoreError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32 / 100.0, 1.0])
                .collect())
        }
        async fn rerank(
            &self,
            _q: &str,
            docs: &[String],
            k: usize,
        ) -> Result<Vec<(usize, f32)>, StoreError> {
            Ok((0..docs.len().min(k)).map(|i| (i, 1.0)).collect())
        }
    }

    /// M8 T3.1: the backfill is a step of the idle pass, it is resumable,
    /// and a dry run — which writes nothing anywhere else — writes no
    /// vectors either.
    #[tokio::test]
    async fn the_idle_pass_backfills_embeddings_once_and_then_has_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let enc = Arc::new(FlatEncoder(Default::default()));
        let sqlite_dir = tempfile::tempdir().unwrap();
        let store = nsmemory_sqlite::SqliteStore::open(&sqlite_dir.path().join("p.sqlite"))
            .unwrap()
            .with_encoder(enc.clone());
        let sid = SessionId("s".into());
        let mut l = EventLog::new(sid.clone());
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(
            1,
            Timestamp(2),
            EventKind::Replied {
                text: "hello".into(),
            },
        );
        store.append(&sid, l.events()).await.unwrap();

        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let dry = pass(dir.path(), rules.clone(), true)
            .run_report(&store)
            .await
            .unwrap();
        assert_eq!(
            dry.embedded, 0,
            "a dry run writes nothing, vectors included"
        );
        assert_eq!(enc.0.load(std::sync::atomic::Ordering::Relaxed), 0);

        let first = pass(dir.path(), rules.clone(), false)
            .run_report(&store)
            .await
            .unwrap();
        assert_eq!(first.embedded, 2);
        assert!(!first.embed_backfill_incomplete);
        let dialled = enc.0.load(std::sync::atomic::Ordering::Relaxed);

        let second = pass(dir.path(), rules, false)
            .run_report(&store)
            .await
            .unwrap();
        assert_eq!(second.embedded, 0, "resumable: the second pass has nothing");
        assert_eq!(
            enc.0.load(std::sync::atomic::Ordering::Relaxed),
            dialled,
            "and it does not dial the service to discover that"
        );
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

    // -----------------------------------------------------------------
    // M8 T2.7 / M10 T5.3 — κ per evaluator, and what a low one costs
    // -----------------------------------------------------------------

    /// A second scorer that *is* the symbolic one, disagreeing on exactly the
    /// turns it is told to.
    ///
    /// Nothing weaker gives a test control of a κ table. The reference's own
    /// labels are whatever the checks make of the recorded turns, so a fixed
    /// script would agree or disagree by accident and the table would be
    /// measuring the fixture rather than the arithmetic. Mirroring and
    /// flipping makes the off-diagonal exactly `flip_on.len()`.
    struct MirrorEvaluator {
        flip_on: Vec<String>,
        inner: crate::evaluate::SymbolicEvaluator,
    }
    #[async_trait::async_trait]
    impl Evaluator for MirrorEvaluator {
        fn id(&self) -> String {
            "scripted".into()
        }
        async fn grade(
            &self,
            view: &crate::evaluate::TurnView<'_>,
        ) -> Result<crate::evaluate::TurnGrade, crate::evaluate::GradeError> {
            use crate::evaluate::Issue;
            let mut g = self.inner.grade(view).await?;
            if self.flip_on.iter().any(|t| t == view.user) {
                // The binary κ is computed over is `!grade.ok`, and `ok` is
                // false when *either* the issue or the grounding flag says
                // so — so both have to move together for the flip to land.
                if g.issue.is_problem() || !g.grounded {
                    g.issue = Issue::None;
                    g.grounded = true;
                } else {
                    g.issue = Issue::Reask;
                }
            }
            g.scorer = self.id();
            Ok(g)
        }
    }

    /// Ten one-turn sessions, five of which the symbolic checks call a
    /// problem. Half and half on purpose: κ at a degenerate prevalence is
    /// zero however well two raters agree, and a fixture that could not be
    /// read is not a test of the arithmetic.
    async fn ten_turns(store: &Arc<InMemoryStore>) {
        for i in 0..10 {
            let text = if i % 2 == 0 {
                format!("ask {i}")
            } else {
                format!("ask {i} with details")
            };
            record_asking(store.clone(), &format!("s{i}"), &text, vec![good()]).await;
        }
    }

    fn mirror(flip: &[&str]) -> Arc<MirrorEvaluator> {
        Arc::new(MirrorEvaluator {
            flip_on: flip.iter().map(|s| s.to_string()).collect(),
            inner: crate::evaluate::SymbolicEvaluator::default(),
        })
    }

    /// T5.3's first exit line: a second evaluator that agrees on 8 of 10
    /// turns gets a κ against the symbolic baseline, and the number is in
    /// the report a dry run prints rather than only in a struct.
    #[tokio::test]
    async fn a_second_evaluator_prints_kappa_against_the_symbolic_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        ten_turns(&store).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));

        let report = pass(dir.path(), rules, true)
            .with_evaluator(mirror(&["ask 0", "ask 3 with details"]))
            .run_report(&*store)
            .await
            .unwrap();

        assert_eq!(report.agreement.len(), 1, "{report}");
        let a = &report.agreement[0];
        assert_eq!(
            (a.evaluator.as_str(), a.reference.as_str()),
            ("scripted", "symbolic")
        );
        let sc = a
            .scores
            .as_ref()
            .expect("both scorers graded all ten turns");
        assert_eq!(sc.n, 10, "{a}");
        // Two flips, so the off-diagonal is exactly two: raw agreement 0.8.
        assert!((sc.observed - 0.8).abs() < 1e-9, "{a}");
        // Five replies invented a name, a number and a place, so the
        // reference calls half of them a problem — which is what makes κ
        // readable here: (0.80 − 0.50) / (1 − 0.50).
        assert!((sc.b_rate - 0.5).abs() < 1e-9, "{a}");
        assert!((sc.kappa - 0.6).abs() < 1e-9, "κ was not computed: {a}");
        // … and it is printed, which is the half of the task a struct field
        // does not satisfy.
        let printed = report.to_string();
        assert!(
            printed.contains("κ scripted vs symbolic:") && printed.contains("over 10 turns"),
            "{printed}"
        );
        // Ten turns cannot resolve the gate, and the line says so rather
        // than letting a small-sample κ pass for a measurement.
        assert!(!a.decisive, "{a}");
    }

    /// T5.3's second: below the threshold an evaluator still grades and its
    /// grades are still recorded — what it loses is the gate.
    #[tokio::test]
    async fn an_evaluator_below_min_kappa_is_reported_but_not_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        ten_turns(&store).await;
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));

        // Disagrees on half the turns, and is nonetheless configured as the
        // scorer the gate should believe.
        let p = EvolutionPass::new(
            rules,
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig {
                authoritative_evaluator: "scripted".into(),
                evaluator_min_kappa: 0.4,
                ..Default::default()
            },
        )
        .with_clock(Box::new(|| 123))
        .with_evaluator(mirror(&[
            "ask 0",
            "ask 1 with details",
            "ask 2",
            "ask 3 with details",
            "ask 4",
        ]));
        let report = p.run_report(&*store).await.unwrap();

        let a = &report.agreement[0];
        let sc = a.scores.as_ref().expect("it graded every turn");
        assert!(
            sc.kappa < 0.4,
            "the fixture has to land under the gate: {a}"
        );
        assert!(!a.trusted(0.4) && !a.authoritative, "{a}");
        assert_eq!(report.authoritative_evaluator, "symbolic", "{report}");
        assert_eq!(report.demoted_from.as_deref(), Some("scripted"), "{report}");
        let printed = report.to_string();
        assert!(
            printed.contains("below evaluator_min_kappa")
                && printed.contains("observations only, no candidates")
                && printed.contains("demoted"),
            "{printed}"
        );

        // Observations only — but observations, not silence: every grade it
        // produced is in the log under its own name (T2.3a).
        let events = store.load(&SessionId("s0".into())).await.unwrap();
        let recorded: Vec<String> = graded_events(&events)
            .iter()
            .map(|e| serde_json::to_string(&e.kind).unwrap())
            .collect();
        assert!(
            recorded.iter().any(|j| j.contains(r#""by":"scripted""#)),
            "{recorded:?}"
        );
        assert!(recorded.iter().any(|j| j.contains(r#""by":"symbolic""#)));
    }
}
