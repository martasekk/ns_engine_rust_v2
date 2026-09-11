//! `ns-app eval [<ledger-path>]` — the memory task set as a release gate
//! (M7 plan T5.2).
//!
//! The set is `nstestkit::eval`: six abilities, every model a scripted double.
//! This command runs it, prints the table, appends one row to the eval ledger
//! and diffs that row against the previous one. It exits non-zero when an
//! ability failed, which is the whole of what makes it a gate rather than a
//! report — "every harness release runs it once; that is the whole procedure"
//! (plan §9).
//!
//! **A separate file from `learned-ledger.json` on purpose.** The candidate
//! ledger (`nsevolution::ledger`) is a map from a candidate's content hash to
//! a verdict, and its whole job is to remember that a patch has been settled
//! so it is never re-proposed. This ledger is an append-only history of runs,
//! ordered, keyed by the commit that produced them. Two schemas with two
//! lifetimes and two readers; putting them in one file would make
//! `Ledger::load` responsible for both, and a parse error in either would
//! take out the other.
//!
//! Only the write helper is shared: `write_atomic` is what the candidate
//! ledger already uses, and a run killed mid-write must leave the previous
//! rows intact rather than half a file.

use nstestkit::eval::{render_table, run_all_for, Ability, Run};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Beside `learned.toml` and `learned-ledger.json` in the working directory,
/// which is where every other file this binary owns already lives.
pub const DEFAULT_LEDGER: &str = "eval-ledger.json";

/// The set has no provider in it. Recorded rather than left out because the
/// plan keys a row by "harness git hash and model id" (T5.2), and a row that
/// silently omitted the model would be indistinguishable from a `--live` row
/// once one exists.
const SCRIPTED: &str = "scripted";

/// One graded ability, flattened. The same numbers [`Ability`] carries and no
/// others: a ledger that re-derived anything would be a second implementation
/// of the grading to keep in step with the first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AbilityRow {
    ability: String,
    passed: bool,
    turns: usize,
    requests: usize,
    prompt_chars: usize,
    prompt_tokens: u32,
    peak_chars: usize,
    tool_calls: usize,
    recall_fired: bool,
    recall_hits: usize,
    flags: usize,
    /// What M7's own phases did on this ability: characters the cap dropped,
    /// `inspect_result` calls, budget drops, and tiers that rose mid-turn.
    /// Zero on the six memory fixtures, and that is a measurement rather than
    /// a placeholder — it says those fixtures exercise a different half of
    /// the harness. `serde(default)` so ledgers written before these columns
    /// existed still parse and still diff.
    #[serde(default)]
    clipped_chars: usize,
    #[serde(default)]
    inspections: usize,
    #[serde(default)]
    budget_drops: usize,
    #[serde(default)]
    escalations: usize,
    /// Bits-over-Random on the deciding call (M10 T0.4): the action this
    /// ability is about, the legal-set size it was chosen out of, whether it
    /// was chosen at all, and `log₂(n)` bits for a hit against `0` for a
    /// miss. Recorded in the ledger rather than only printed, because the
    /// number this column exists for is a *difference* — P2 narrows the
    /// legal set and the question is what the narrowing cost, which needs
    /// the row before it to still be readable.
    #[serde(default)]
    target_action: String,
    #[serde(default)]
    target_proposed: bool,
    #[serde(default)]
    legal_size: usize,
    #[serde(default)]
    bits: f64,
    /// Why it failed. Empty on a pass, and then absent from the file: the
    /// interesting rows are the ones with text in this field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    detail: String,
}

impl From<&Ability> for AbilityRow {
    fn from(a: &Ability) -> Self {
        Self {
            ability: a.ability.to_string(),
            passed: a.passed,
            turns: a.turns,
            requests: a.requests,
            prompt_chars: a.prompt_chars,
            prompt_tokens: a.prompt_tokens,
            peak_chars: a.peak_chars,
            tool_calls: a.tool_calls,
            recall_fired: a.recall_fired,
            recall_hits: a.recall_hits,
            flags: a.flags,
            clipped_chars: a.clipped_chars,
            inspections: a.inspections,
            budget_drops: a.budget_drops,
            escalations: a.escalations,
            target_action: a.target_action.to_string(),
            target_proposed: a.target_proposed,
            legal_size: a.legal_size,
            bits: a.bits,
            detail: a.detail.clone(),
        }
    }
}

/// One run of the set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    /// The commit the harness was at, or `unknown`.
    harness: String,
    model: String,
    /// `2026-09-08 12:03:11 UTC (Tuesday)` — for a person reading the file.
    at: String,
    /// The same instant in unix milliseconds, for anything that sorts.
    at_ms: u64,
    passed: usize,
    of: usize,
    abilities: Vec<AbilityRow>,
}

impl Row {
    fn build(harness: String, at_ms: u64, abilities: &[Ability]) -> Self {
        Self {
            harness,
            model: SCRIPTED.into(),
            at: nscore::format_utc(at_ms),
            at_ms,
            passed: abilities.iter().filter(|a| a.passed).count(),
            of: abilities.len(),
            abilities: abilities.iter().map(AbilityRow::from).collect(),
        }
    }

    /// Bits-over-Random summed over the set (M10 T0.4). A ledger row is
    /// compared on this the way it is compared on requests: it is the number
    /// P2's narrowing has to hold, and unlike the pass count it cannot be
    /// bought by making the guess easier.
    fn bits(&self) -> f64 {
        self.abilities.iter().map(|a| a.bits).sum()
    }

    fn requests(&self) -> usize {
        self.abilities.iter().map(|a| a.requests).sum()
    }

    fn find(&self, ability: &str) -> Option<&AbilityRow> {
        self.abilities.iter().find(|a| a.ability == ability)
    }
}

/// Append-only, in run order.
///
/// A list rather than a map keyed by the commit hash, even though the plan
/// calls the hash the key: "the previous row" only means something in an
/// order, and hashes do not have one. Re-running the same commit appends
/// again rather than replacing, and that is deliberate — the set is
/// deterministic, so two rows on one hash that disagree are a finding about
/// the harness, not a duplicate to be tidied away.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct EvalLedger {
    #[serde(default)]
    rows: Vec<Row>,
}

impl EvalLedger {
    /// Missing file → an empty ledger, the same convention `load_rules` uses.
    /// Unparsable file → an error the caller must not paper over: overwriting
    /// it would discard every previous row, which is the one thing this file
    /// exists to keep.
    fn load(path: &Path) -> Result<EvalLedger, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(EvalLedger::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        text.push('\n');
        nsevolution::files::write_atomic(path, text.as_bytes()).map_err(|e| e.to_string())
    }
}

/// The eight leading characters of a hash — enough to name a commit in a
/// line a person reads, and `unknown` passes through unchanged.
fn short(hash: &str) -> &str {
    match hash.len() > 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        true => &hash[..8],
        false => hash,
    }
}

fn state(passed: bool) -> &'static str {
    if passed {
        "ok"
    } else {
        "FAIL"
    }
}

/// What moved since the previous run.
///
/// The point of "fix the model, vary the harness" (2607.03691) is noticing
/// the release that moved an ability, and a table printed on its own never
/// does: 6/6 today looks exactly like 6/6 last week even when the set is
/// three times more expensive. So the state changes are named one per line,
/// and the request total — metric 1 in the plan's list, because the free tier
/// meters requests and not tokens — is carried beside them, since a harness
/// change that doubles the cost of a passing set is the regression this file
/// is here to catch.
fn diff_lines(previous: Option<&Row>, current: &Row) -> String {
    let Some(previous) = previous else {
        return format!(
            "  first row in this ledger — nothing to diff against. {}/{} abilities pass, \
             {} requests, {:.2} bits over random.\n",
            current.passed,
            current.of,
            current.requests(),
            current.bits()
        );
    };
    let mut out = format!("  since {} ({}):\n", short(&previous.harness), previous.at);
    let mut moved = 0;
    for a in &current.abilities {
        match previous.find(&a.ability) {
            Some(was) if was.passed != a.passed => {
                moved += 1;
                out.push_str(&format!(
                    "    {}: {} → {}{}\n",
                    a.ability,
                    state(was.passed),
                    state(a.passed),
                    if a.detail.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", a.detail)
                    }
                ));
            }
            Some(_) => {}
            // A renamed or added ability has no previous state to change
            // from, and silently comparing it against nothing would report a
            // set that grew as a set that held still.
            None => {
                moved += 1;
                out.push_str(&format!("    {}: new — {}\n", a.ability, state(a.passed)));
            }
        }
    }
    for was in &previous.abilities {
        if current.find(&was.ability).is_none() {
            moved += 1;
            out.push_str(&format!("    {}: gone from the set\n", was.ability));
        }
    }
    if moved == 0 {
        out.push_str("    no ability changed state.\n");
    }
    out.push_str(&format!(
        "    requests {} → {} · {}/{} → {}/{} · BoR {:.2} → {:.2}\n",
        previous.requests(),
        current.requests(),
        previous.passed,
        previous.of,
        current.passed,
        current.of,
        previous.bits(),
        current.bits()
    ));
    out
}

/// `ns-app eval [<ledger-path>]` → the ledger to append to.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub ledger: PathBuf,
    /// Run the paraphrased-recall arm instead of the ability set (M8 T1.2).
    ///
    /// A separate mode rather than an extra column, because it grades a
    /// different thing: the ability set grades the harness and gates a
    /// release on it, while this measures a *retriever* and decides a design
    /// question that M6 §12.8 left open. Folding the second into the first
    /// would make a fired trigger look like a regression, which it is not —
    /// it is the trigger doing its job.
    pub paraphrase: bool,
    /// Run the ability set twice with this block blanked instead of once
    /// whole (M9 T0.4).
    ///
    /// A third mode for the same reason `--paraphrase` is a second one: it
    /// grades a *block*, not the harness, and the ablated arm is meant to
    /// fail. Folded into the gate it would read as a regression, which is
    /// the opposite of what a working ablation means.
    pub ablate: Option<nscore::Ablate>,
    /// `[memory] activation_weight` for this run (M9 T3.3), without editing
    /// the config.
    ///
    /// A flag rather than a config edit because the decision it serves is a
    /// sweep: the plan runs `--paraphrase` and `--ablate facts` at three
    /// weights and compares, and a sweep that needed three edits to
    /// `ns-run/ns.toml` would be a sweep nobody reran. Applied to the
    /// harness's `EngineConfig` **and** to the stores it searches — the
    /// prior lives on the store.
    pub activation: f32,
    /// `[router] depth` for this run (M10 T2.1), without editing the config.
    ///
    /// Mirrors `--activation` for the same reason: the question is whether
    /// BoR on the desktop abilities and the hard-query fixture survive the
    /// narrowing, and that is two runs of the same set, not two configs.
    pub depth: nscore::Depth,
    /// `[memory] obligation_check` on vs off (M9 T2.1, read by M10 T5.4).
    ///
    /// A mode rather than a value like `--activation`: the knob is a
    /// boolean, and the only useful run of it is both arms at once, which is
    /// what the mode does.
    pub obligations: bool,
    /// `[memory] summary_guidelines` empty vs three hand-written lines
    /// (M9 T5.2, read by M10 T5.4). Same shape, same reason.
    pub guidelines: bool,
    /// `--paraphrase --facts`: run the paraphrased-recall arm over the
    /// **facts** corpus instead of the turns corpus (M11 T1.1).
    ///
    /// A modifier on `--paraphrase` rather than a fourth mode, because it is
    /// the same measurement — a miss rate over a paraphrase arm with a
    /// verbatim control — asked of the other retriever. `search_facts` is
    /// `lexical_rank`, not bm25, and the turns number says nothing about it.
    pub facts: bool,
    /// `[memory] window_turns` and `facts_in_context` for this run (M12
    /// T5.2), without editing the config.
    pub profile: Profile,
}

/// The context profile one run is measured at (M12 T5.2).
///
/// `None` on both is the default arm — the engine's own `window_turns = 6`
/// and `facts_in_context = 10` — and is the only arm the ledger records. A
/// pair rather than two loose arguments because the two move together: the
/// question the arm answers is what a wider context costs per emitter call,
/// and a run that widened one of them is half a reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Profile {
    pub window: Option<usize>,
    pub facts: Option<usize>,
}

impl Profile {
    /// Whether this is the default arm, and so whether the ledger may record
    /// it.
    fn is_default(&self) -> bool {
        self.window.is_none() && self.facts.is_none()
    }
}

const USAGE: &str = "usage: ns-app eval [<ledger-path>] [--paraphrase [--facts]] \
     [--ablate facts|summary|guidance] [--activation <weight>] [--depth full|adaptive] \
     [--window <turns>] [--facts <count>] [--obligations] [--guidelines]";

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut ledger = None;
    let mut paraphrase = false;
    let mut ablate = None;
    let mut activation = 0.0f32;
    let mut depth = nscore::Depth::Full;
    let mut obligations = false;
    let mut guidelines = false;
    let mut facts = false;
    let mut profile = Profile::default();
    let mut rest = args.iter().peekable();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--paraphrase" => paraphrase = true,
            // Two flags spelled the same, told apart by what follows them:
            // `--facts 16` is M12 T5.2's cap and a bare `--facts` is M11
            // T1.1's corpus modifier on `--paraphrase`. The alternative was
            // to rename one of them, and the modifier is in the M11 results
            // as `--paraphrase --facts` — a spelling that already means
            // something to a reader of those numbers.
            "--facts" => match rest.peek().and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => {
                    rest.next();
                    profile.facts = Some(positive(n, "--facts")?);
                }
                None => facts = true,
            },
            "--window" => {
                let n = rest
                    .next()
                    .ok_or_else(|| format!("{USAGE} (--window needs a turn count)"))?;
                let n = n
                    .parse::<usize>()
                    .map_err(|_| format!("{USAGE} (got {n:?})"))?;
                profile.window = Some(positive(n, "--window")?);
            }
            "--obligations" => obligations = true,
            "--guidelines" => guidelines = true,
            "--depth" => {
                let d = rest
                    .next()
                    .ok_or_else(|| format!("{USAGE} (--depth needs full or adaptive)"))?;
                depth = nscore::Depth::parse(d).map_err(|e| format!("{USAGE} ({e})"))?;
            }
            "--activation" => {
                let w = rest
                    .next()
                    .ok_or_else(|| format!("{USAGE} (--activation needs a weight)"))?;
                activation = w
                    .parse::<f32>()
                    .ok()
                    .filter(|w| w.is_finite() && *w >= 0.0)
                    .ok_or_else(|| format!("{USAGE} (got {w:?})"))?;
            }
            "--ablate" => {
                let block = rest
                    .next()
                    .ok_or_else(|| format!("{USAGE} (--ablate needs a block)"))?;
                ablate = Some(
                    nstestkit::ablate::parse_block(block)
                        .ok_or_else(|| format!("{USAGE} (got {block:?})"))?,
                );
            }
            path if !path.starts_with('-') && ledger.is_none() => {
                ledger = Some(PathBuf::from(path))
            }
            other => return Err(format!("{USAGE} (got {other:?})")),
        }
    }
    // M10 T5.2. The weight used to reach the default mode through a static
    // cell, because `main.rs` called `run(&parsed.ledger)` with no weight and
    // `main.rs` belonged to another task. It now calls `run_at(&ledger,
    // parsed.activation)`, so the cell and its `run` wrapper are gone and the
    // flag travels as a parameter like every other argument here.
    if facts && !paraphrase {
        return Err(format!("{USAGE} (--facts is a modifier on --paraphrase)"));
    }
    Ok(Args {
        ledger: ledger.unwrap_or_else(|| PathBuf::from(DEFAULT_LEDGER)),
        paraphrase,
        ablate,
        activation,
        depth,
        obligations,
        guidelines,
        facts,
        profile,
    })
}

/// A cap of zero is not a profile: it is the context block switched off,
/// which is `--ablate`'s question and is measured against a control there.
fn positive(n: usize, flag: &str) -> Result<usize, String> {
    if n == 0 {
        return Err(format!(
            "{USAGE} ({flag} 0 blanks the block — use --ablate, which runs a control beside it)"
        ));
    }
    Ok(n)
}

/// `ns-app eval --obligations` / `--guidelines` — the two M9 knobs that are
/// neither a context block nor a ranking weight (M10 T5.4).
///
/// Exits 0 whatever the numbers are, for [`run_ablate`]'s reason. The
/// guidelines arm prints *not measurable* rather than a zero: on the scripted
/// summarizer the two arms are identical by construction, and a zero printed
/// as a result would confirm the default with the instrument switched off.
pub async fn run_knob(knob: Knob) -> i32 {
    use nstestkit::knobs;

    let report = match knob {
        Knob::Obligations => knobs::measure_obligations().await,
        Knob::Guidelines => knobs::measure_guidelines().await,
    };
    print!("{}", knobs::render(&report));
    0
}

/// Which of the two knob modes to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    Obligations,
    Guidelines,
}

/// `ns-app eval --ablate <block>` — one context block's marginal effect (M9
/// T0.4).
///
/// Exits 0 whatever the delta is. The ablated arm failing is the measurement
/// succeeding: it says the block was carrying those abilities. A zero delta
/// is equally a result — either the block is not earning its tokens, or this
/// scripted suite cannot see what it earns — and neither is a release
/// failure, so neither may turn the exit code red.
pub async fn run_ablate(block: nscore::Ablate, activation: f32) -> i32 {
    use nstestkit::ablate;

    let report = ablate::measure(block, activation).await;
    print!("{}", ablate::render(&report));
    println!(
        "  both arms are the same {} fixtures against the same scripted doubles at \
         activation_weight = {activation}; nothing here spends a request.",
        report.full.len()
    );
    0
}

/// `ns-app eval --paraphrase` — the M6 §12.8 measurement, against both
/// retrievers (M8 T1.2).
///
/// Both, because they are not the same retriever and only one of them ships.
/// `InMemoryStore::search_turns` counts query tokens found in the line;
/// `SqliteStore` runs FTS5 `bm25` over an index. The ability suite has only
/// ever exercised the first. A trigger decided on it would be a decision
/// about test scaffolding.
///
/// Exits 0 whatever the number is. A fired trigger is not a failure — it is
/// permission to build something, and a gate that went red on it would make
/// the measurement something to avoid taking.
pub async fn run_paraphrase(activation: f32, facts: bool) -> i32 {
    use nstestkit::paraphrase;

    // M11 T1.1 follow-up: `--facts` used to reach here through a static
    // cell, because `main.rs` belonged to another task while T1.1 was
    // written. It is a parameter now, like every other argument — the arm
    // is chosen by the caller, so two runs in one process cannot see each
    // other's flag.
    if facts {
        return run_paraphrase_facts(activation).await;
    }
    let k = nsengine::turn::EngineConfig::default().recall_top_k;
    // M9 T3.3: the same half-life the shipped config defaults to, so a
    // sweep over `--activation` measures the knob and not a second one.
    let half_life = nsengine::turn::EngineConfig::default().activation_half_life_days;
    let mut reports = Vec::new();

    let memory = nsengine::store::InMemoryStore::new().with_activation(activation, half_life);
    reports.push(paraphrase::measure(&memory, "in-memory (token hits)", k).await);

    // A throwaway database rather than the live one: the corpus writes
    // twelve sessions, and a measurement that left them in `ns.sqlite` would
    // be editing the thing every other number here is read from.
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("paraphrase: no temp dir for the sqlite arm ({e})");
            return 2;
        }
    };
    match nsmemory_sqlite::SqliteStore::open(&dir.path().join("paraphrase.sqlite")) {
        Ok(sqlite) => {
            let sqlite = sqlite.with_activation(activation, half_life);
            reports.push(paraphrase::measure(&sqlite, "sqlite (fts5 bm25)", k).await);
        }
        Err(e) => eprintln!("paraphrase: the sqlite arm did not run ({e})"),
    }

    // M8 T3.4 / M10 P3: the third arm, and the one the exit criterion is
    // about. It is added **only when the service answers** — a hybrid arm
    // against a dead service would be the bm25 arm again by design, and
    // printing it as "hybrid" would be reporting a number for something that
    // did not run. When it is absent the line below says so, which is what
    // T3.4 means by "report the number as not measured".
    //
    // A separate database from the bm25 arm, so the two are not the same
    // store measured twice with a knob moved: the vectors are written into
    // this one by the same `backfill_embeddings` the idle pass calls.
    let models = crate::config::ModelsSection {
        enabled: true,
        ..Default::default()
    };
    let recall = crate::config::RecallSection::default();
    let mut hybrid_ran = false;
    if crate::models::reachable(&models).await {
        match nsmemory_sqlite::SqliteStore::open(&dir.path().join("paraphrase-hybrid.sqlite")) {
            Ok(sqlite) => {
                let sqlite = sqlite
                    .with_activation(activation, half_life)
                    .with_recall(crate::models::recall_tuning(&recall));
                let sqlite = match crate::models::encoder(&models, &recall) {
                    Some(enc) => sqlite.with_encoder(enc),
                    None => sqlite,
                };
                let name = format!(
                    "sqlite hybrid ({}, coarse {} → rerank)",
                    recall.embed_model, recall.coarse_k
                );
                reports.push(paraphrase::measure(&sqlite, &name, k).await);
                hybrid_ran = true;
            }
            Err(e) => eprintln!("paraphrase: the hybrid arm did not run ({e})"),
        }
    }

    print!("{}", paraphrase::render(&reports));
    println!(
        "  k = {k} (recall_top_k), activation_weight = {activation}, {} cases, \
         no requests spent.",
        paraphrase::corpus().len()
    );
    if hybrid_ran {
        println!(
            "  the hybrid arm ran against nsmodels on {} — local CPU, no requests.",
            models.base_url
        );
    } else {
        println!(
            "  hybrid arm: NOT MEASURED — no nsmodels service on {}. Start it with\n  \
             `cd ~/models && ./.venv/Scripts/python.exe -m nsmodels serve --model quality \
             --rerank`.",
            models.base_url
        );
    }
    0
}

/// `ns-app eval --paraphrase --facts` — the same measurement over the facts
/// corpus (M11 T1.1).
///
/// Two arms, and the comparison is the whole point: `search_facts` under
/// `lexical_rank` against `search_facts_hybrid` with the encoder. The exit
/// criterion is "the hybrid paraphrase miss rate is below the lexical one
/// with the verbatim arm at 0% on both", so both numbers have to be printed
/// side by side and read off one table.
///
/// The hybrid arm is added **only when the service answers**, for the turns
/// arm's reason: with nsmodels down `search_facts_hybrid` *is* `search_facts`
/// by design, and printing that as "hybrid" would be reporting a number for
/// something that did not run.
async fn run_paraphrase_facts(activation: f32) -> i32 {
    use nstestkit::paraphrase;

    let k = nsengine::turn::EngineConfig::default().recall_top_k;
    let half_life = nsengine::turn::EngineConfig::default().activation_half_life_days;
    let mut reports = Vec::new();

    // A throwaway database, never `ns.sqlite`: this arm *writes facts*, and a
    // measurement that left twelve of them in the live store would be editing
    // the memory every other number here is read from.
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("paraphrase --facts: no temp dir ({e})");
            return 2;
        }
    };
    match nsmemory_sqlite::SqliteStore::open(&dir.path().join("facts-lexical.sqlite")) {
        Ok(sqlite) => {
            let sqlite = sqlite.with_activation(activation, half_life);
            reports
                .push(paraphrase::measure_facts(&sqlite, "facts lexical (lexical_rank)", k).await);
        }
        Err(e) => eprintln!("paraphrase --facts: the lexical arm did not run ({e})"),
    }

    let models = crate::config::ModelsSection {
        enabled: true,
        ..Default::default()
    };
    let recall = crate::config::RecallSection::default();
    let mut hybrid_ran = false;
    if crate::models::reachable(&models).await {
        match nsmemory_sqlite::SqliteStore::open(&dir.path().join("facts-hybrid.sqlite")) {
            Ok(sqlite) => {
                let sqlite = sqlite
                    .with_activation(activation, half_life)
                    .with_recall(crate::models::recall_tuning(&recall));
                let sqlite = match crate::models::encoder(&models, &recall) {
                    Some(enc) => sqlite.with_encoder(enc),
                    None => sqlite,
                };
                let name = format!(
                    "facts hybrid ({}, coarse {} → rerank)",
                    recall.embed_model, recall.coarse_k
                );
                reports.push(paraphrase::measure_facts(&sqlite, &name, k).await);
                hybrid_ran = true;
            }
            Err(e) => eprintln!("paraphrase --facts: the hybrid arm did not run ({e})"),
        }
    }

    print!("{}", paraphrase::render(&reports));
    println!(
        "  k = {k} (recall_top_k), activation_weight = {activation}, {} facts in one \
         scope, no requests spent.",
        paraphrase::fact_corpus().len()
    );
    println!(
        "  the corpus is built so every paraphrase shares zero tokens with its fact, \
         which is\n  the lexical floor `lexical_rank` cannot climb: it drops a fact with \
         no query token\n  in it before it ranks anything."
    );
    if hybrid_ran {
        println!(
            "  the hybrid facts arm ran against nsmodels on {} — local CPU, no requests.",
            models.base_url
        );
    } else {
        println!(
            "  hybrid facts arm: NOT MEASURED — no nsmodels service on {}. Start it with\n  \
             `cd ~/models && ./.venv/Scripts/python.exe -m nsmodels serve --model quality \
             --rerank`.",
            models.base_url
        );
    }
    0
}

/// The commit the harness was built from, read out of `.git` rather than
/// shelled out to `git`.
///
/// `option_env!` cannot serve: it is resolved at compile time, so without a
/// `build.rs` that reruns on every commit a cached binary would key every row
/// to whichever commit it was first compiled at — which is precisely the
/// confusion the key exists to prevent. Reading the ref at run time keys the
/// row to the tree that is being graded.
///
/// Worktree-aware, because that is where this branch is built: `.git` is a
/// *file* in a worktree, `HEAD` lives in the worktree's own gitdir, and the
/// branch ref it names lives in the common dir that `commondir` points at.
/// `packed-refs` is the fallback for a branch `git gc` has packed away.
///
/// It records the commit, not the tree: an uncommitted edit is invisible to
/// it, so a row whose numbers moved without the hash moving means somebody
/// ran the gate on a dirty checkout.
///
/// Every failure returns `unknown`. A row with no hash is worth less; a run
/// that refused to grade the harness because it could not find a ref is worth
/// nothing.
fn harness_hash(root: &Path) -> String {
    resolve_head(root).unwrap_or_else(|| "unknown".to_string())
}

fn resolve_head(root: &Path) -> Option<String> {
    let dot = root.join(".git");
    let gitdir = if dot.is_dir() {
        dot
    } else {
        PathBuf::from(
            std::fs::read_to_string(&dot)
                .ok()?
                .strip_prefix("gitdir:")?
                .trim(),
        )
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim().to_string();
    let Some(reference) = head.strip_prefix("ref:") else {
        // Detached HEAD: the hash is the file.
        return is_hash(&head).then_some(head);
    };
    let reference = reference.trim();
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => gitdir.join(rel.trim()),
        Err(_) => gitdir.clone(),
    };
    for dir in [&gitdir, &common] {
        if let Ok(text) = std::fs::read_to_string(dir.join(reference)) {
            let hash = text.trim().to_string();
            if is_hash(&hash) {
                return Some(hash);
            }
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| {
        let (hash, name) = l.split_once(' ')?;
        (name.trim() == reference && is_hash(hash)).then(|| hash.to_string())
    })
}

/// 40 hex characters for SHA-1, 64 once a repository is on SHA-256.
fn is_hash(s: &str) -> bool {
    s.len() >= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Runs the set at one `[memory] activation_weight` (M9 T3.3, M10 T5.2),
/// prints the table and the ledger diff, and returns the process exit code:
/// 0 when every ability passed, 1 otherwise.
///
/// The ability set **and** the tie-heavy recall corpus, because the ability
/// set alone is what M9 T3.3 already tried: *"suites insensitive … identical
/// lists at every weight"*. Nothing there ties, so nothing there can move.
/// The tie corpus is the arm built to move, and printing the two together is
/// what makes the difference attributable — a run whose abilities held and
/// whose ties changed is the reading the knob needs.
pub async fn run_at(
    ledger_path: &Path,
    activation: f32,
    depth: nscore::Depth,
    profile: Profile,
) -> i32 {
    let abilities = run_all_for(Run {
        activation_weight: activation,
        depth,
        window_turns: profile.window,
        facts_in_context: profile.facts,
        ..Run::default()
    })
    .await;
    if depth != nscore::Depth::Full {
        println!("router depth: {} (M10 T2.1)", depth.as_str());
    }
    let code = report(&abilities, ledger_path, activation, depth, profile);
    // M12 T5.2. Printed on every run, not only on the arm: the default's
    // prefix level is the number the arm is compared against, and a footer
    // that appeared only when a flag was passed would leave the comparison
    // to be reconstructed from an older paste.
    print!("{}", profile_footer(&abilities, profile));
    print!(
        "{}",
        nstestkit::ties::render(&nstestkit::ties::measure(activation).await)
    );
    // M10 T5.4 arm 2. The tie corpus is where the weight is *meant* to move
    // things; the thirty sessions are where it must not. Printing both under
    // one flag is what makes `w = 1` a decision rather than a hope: the
    // non-regressing half of the M9 rule is this table, not the tie table.
    let fx = nstestkit::fixtures::run_all_for(Run {
        activation_weight: activation,
        depth,
        window_turns: profile.window,
        facts_in_context: profile.facts,
        ..Run::default()
    })
    .await;
    print!(
        "{}",
        nstestkit::fixtures::render_by_ability(&format!("w = {activation}"), &fx)
    );
    code
}

/// What this run was measured at, and what its emitter prefix came to
/// (M12 T5.2).
///
/// The prefix is the run of blocks a provider cache breakpoint would sit
/// behind, and it is worth nothing under the 1,024-token floor every current
/// provider applies — so the floor is printed beside the level rather than
/// left to be remembered. A returned string rather than a `println!` because
/// the line is the arm's whole deliverable, and a line nothing can assert on
/// is a line that drifts.
///
/// The median and max are over the ability rows, each of which already
/// carries the median over *its* emitter calls. A median of medians, not of
/// calls: [`Ability`] records one number per ability, and a raw per-call list
/// in the ledger row would be a schema change for a footer.
///
/// An ability whose emitter was sent no stable blocks at all is left out,
/// the way `ns-app budget`'s own prefix summary leaves such a call out: the
/// four desktop tasks are one turn on an empty store, and folding their
/// zeros in would drag the median under the floor for free.
fn profile_footer(rows: &[Ability], profile: Profile) -> String {
    let default = nsengine::turn::EngineConfig::default();
    let mut levels: Vec<u32> = rows
        .iter()
        .map(|r| r.emitter_prefix_tokens)
        .filter(|t| *t > 0)
        .collect();
    levels.sort_unstable();
    let (median, max) = match levels.last() {
        Some(max) => (levels[levels.len() / 2], *max),
        None => (0, 0),
    };
    format!(
        "context profile: window {}, facts {} · emitter prefix (est.) median {median} tokens, \
         max {max} · breakpoint floor 1,024\n",
        profile.window.unwrap_or(default.window_turns),
        profile.facts.unwrap_or(default.facts_in_context),
    )
}

/// The gate, separated from the run so that a failing set can be tested
/// without one. The six abilities pass, which is exactly why the non-zero
/// path needs its own test: an exit code nothing exercises is a gate nobody
/// has checked.
fn report(
    abilities: &[Ability],
    ledger_path: &Path,
    activation: f32,
    depth: nscore::Depth,
    profile: Profile,
) -> i32 {
    print!("{}", render_table(abilities));

    let current = Row::build(harness_hash(Path::new(".")), now_ms(), abilities);
    // M12 T5.2, the same rule again: a run at a wider window or a deeper
    // fact list is a different arm, and a row of its numbers would make the
    // next default diff read as a harness change.
    if !profile.is_default() {
        println!(
            "ledger: not written — this run is the window = {}, facts = {} arm, \
             not the default one the ledger diffs.",
            profile
                .window
                .map(|n| n.to_string())
                .unwrap_or_else(|| "default".into()),
            profile
                .facts
                .map(|n| n.to_string())
                .unwrap_or_else(|| "default".into()),
        );
        let failed = abilities.iter().filter(|a| !a.passed).count();
        return i32::from(failed > 0);
    }
    // Same rule as the activation arm, and for the same reason: a run at
    // `depth = adaptive` is a different arm, and recording it would make the
    // next diff read as a harness change.
    if depth != nscore::Depth::Full {
        println!(
            "ledger: not written — this run is the depth = {} arm, \
             not the default one the ledger diffs.",
            depth.as_str()
        );
        let failed = abilities.iter().filter(|a| !a.passed).count();
        return i32::from(failed > 0);
    }
    // A row is the *default* arm's numbers, and the ledger's whole job is to
    // let two runs be diffed position by position. A run at a non-default
    // `activation_weight` is a different arm; recording it would make the
    // next diff read as a harness change. So it prints and is not recorded,
    // and says so rather than leaving a gap.
    if activation != 0.0 {
        println!(
            "ledger: not written — this run is the activation_weight = {activation} arm, \
             not the default one the ledger diffs."
        );
        let failed = abilities.iter().filter(|a| !a.passed).count();
        return i32::from(failed > 0);
    }
    println!(
        "ledger: {} · harness {}",
        ledger_path.display(),
        current.harness
    );
    match EvalLedger::load(ledger_path) {
        Ok(mut ledger) => {
            print!("{}", diff_lines(ledger.rows.last(), &current));
            ledger.rows.push(current);
            // A ledger that could not be written does not change what the six
            // abilities did, so it does not change the exit code: reporting a
            // harness regression that did not happen would be worse than
            // losing one row. It is loud on stderr instead.
            if let Err(e) = ledger.save(ledger_path) {
                eprintln!("eval: the ledger was not written: {e}");
            }
        }
        Err(e) => {
            eprintln!("eval: {} did not parse ({e}).", ledger_path.display());
            eprintln!("eval: this run was not recorded and the file is left as it is — move it aside to start a new ledger.");
        }
    }

    let failed = abilities.iter().filter(|a| !a.passed).count();
    if failed == 0 {
        return 0;
    }
    eprintln!("eval: {failed} of {} abilities failed.", abilities.len());
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ability(name: &'static str, passed: bool, requests: usize) -> Ability {
        Ability {
            ability: name,
            passed,
            turns: 9,
            requests,
            prompt_chars: 512,
            prompt_tokens: 128,
            context_chars: 300,
            emitter_prefix_tokens: 75,
            peak_chars: 700,
            tool_calls: 3,
            recall_fired: false,
            recall_hits: 0,
            flags: 0,
            clipped_chars: 0,
            inspections: 0,
            budget_drops: 0,
            escalations: 0,
            target_action: "remember_fact",
            target_proposed: passed,
            legal_size: 8,
            bits: nstestkit::eval::bits_over_random(passed, 8),
            detail: if passed {
                String::new()
            } else {
                "superseded value not rendered".into()
            },
        }
    }

    fn row(abilities: &[Ability], harness: &str) -> Row {
        Row::build(harness.into(), 1_788_345_688_203, abilities)
    }

    /// **Bits-over-Random is chance-corrected** (M10 T0.4, tool-loading
    /// §5.2).
    ///
    /// The property that makes the column worth printing: a target that was
    /// not chosen scores nothing, a target chosen out of a legal set of one
    /// also scores nothing — because there was no choice to get right — and
    /// a target chosen out of a wider set scores strictly more than the same
    /// target chosen out of a narrower one. That last line is the whole
    /// defence against P2: a narrowing that keeps the pass count by making
    /// the guess easier shows up here as a fall.
    #[test]
    fn bits_over_random_is_zero_at_chance_and_positive_when_the_target_is_chosen() {
        use nstestkit::eval::bits_over_random;

        // At chance, twice over: never proposed, and proposed out of a set
        // with nothing to choose against.
        assert_eq!(bits_over_random(false, 17), 0.0);
        assert_eq!(bits_over_random(false, 1), 0.0);
        assert_eq!(bits_over_random(true, 1), 0.0);
        assert_eq!(bits_over_random(true, 0), 0.0);

        // Chosen: log2(n) bits, exactly.
        assert_eq!(bits_over_random(true, 2), 1.0);
        assert_eq!(bits_over_random(true, 16), 4.0);
        assert!((bits_over_random(true, 17) - 4.087_462_841_250_339).abs() < 1e-9);

        // And breadth is what it pays for — the same hit out of a wider set
        // is worth more, so a narrowing cannot buy the column.
        assert!(bits_over_random(true, 17) > bits_over_random(true, 3));

        // The ledger carries the sum, which is what a row is compared on.
        let wide = row(
            &[ability("information extraction", true, 19)],
            &"a".repeat(40),
        );
        let missed = row(
            &[ability("information extraction", false, 19)],
            &"b".repeat(40),
        );
        assert_eq!(wide.bits(), 3.0, "log2(8)");
        assert_eq!(missed.bits(), 0.0);
        let diff = diff_lines(Some(&wide), &missed);
        assert!(
            diff.contains("BoR 3.00 → 0.00"),
            "the diff has to carry the fall:\n{diff}"
        );
    }

    /// The reason the ledger exists: 6/6 today reads exactly like 6/6 last
    /// week, so a run that moved an ability has to say which one and in which
    /// direction. Without this line the release that broke temporal
    /// reasoning is found by whoever hits it in a conversation.
    #[test]
    fn the_diff_names_the_ability_that_changed_state_and_the_direction() {
        let before = row(
            &[ability("temporal reasoning", true, 26)],
            "a".repeat(40).as_str(),
        );
        let after = row(
            &[ability("temporal reasoning", false, 26)],
            "b".repeat(40).as_str(),
        );
        let d = diff_lines(Some(&before), &after);
        assert!(d.contains("temporal reasoning: ok → FAIL"), "{d}");
        assert!(
            d.contains("superseded value not rendered"),
            "the condition, not only the ability: {d}"
        );
        assert!(d.contains("aaaaaaaa"), "the previous commit is named: {d}");
        assert!(d.contains("1/1 → 0/1"), "{d}");
    }

    /// An unchanged set still reports its cost. A harness change that keeps
    /// all six passing while doubling the requests is a regression against
    /// the plan's first metric, and it is invisible in the pass column.
    #[test]
    fn an_unchanged_set_still_reports_the_request_total() {
        let before = row(&[ability("abstention", true, 7)], "a".repeat(40).as_str());
        let after = row(&[ability("abstention", true, 14)], "a".repeat(40).as_str());
        let d = diff_lines(Some(&before), &after);
        assert!(d.contains("no ability changed state"), "{d}");
        assert!(d.contains("requests 7 → 14"), "{d}");
    }

    /// A set that grew or shrank is not a set that held still: an added
    /// ability has no previous state, and a removed one stopped being graded.
    #[test]
    fn an_ability_added_or_removed_is_named_rather_than_skipped() {
        let before = row(&[ability("abstention", true, 7)], "a".repeat(40).as_str());
        let after = row(&[ability("forgetting", true, 7)], "b".repeat(40).as_str());
        let d = diff_lines(Some(&before), &after);
        assert!(d.contains("forgetting: new — ok"), "{d}");
        assert!(d.contains("abstention: gone from the set"), "{d}");
    }

    #[test]
    fn the_first_row_says_it_has_nothing_to_compare_with() {
        let first = row(&[ability("abstention", true, 7)], "a".repeat(40).as_str());
        let d = diff_lines(None, &first);
        assert!(d.contains("first row"), "{d}");
        assert!(d.contains("1/1 abilities pass"), "{d}");
    }

    #[test]
    fn a_row_round_trips_through_the_file_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_LEDGER);
        let mut l = EvalLedger::load(&path).expect("a missing ledger is an empty one");
        assert!(l.rows.is_empty());
        l.rows.push(row(
            &[ability("abstention", true, 7)],
            "a".repeat(40).as_str(),
        ));
        l.save(&path).unwrap();
        let mut reloaded = EvalLedger::load(&path).unwrap();
        assert_eq!(reloaded, l);
        reloaded.rows.push(row(
            &[ability("abstention", false, 7)],
            "b".repeat(40).as_str(),
        ));
        reloaded.save(&path).unwrap();
        let again = EvalLedger::load(&path).unwrap();
        assert_eq!(again.rows.len(), 2, "rows append, they do not replace");
        assert_eq!(again.rows[0].harness, "a".repeat(40));
        // A passing row carries no `detail`; a failing one must.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("superseded value not rendered"), "{text}");
        assert!(text.contains("\"model\": \"scripted\""), "{text}");
    }

    /// The file this branch is actually built in: `.git` is a file naming a
    /// worktree gitdir, HEAD is a symbolic ref, and the branch it names lives
    /// in the common dir — not in the worktree's own. Reading only the
    /// worktree gitdir would key every row `unknown`.
    #[test]
    fn head_resolves_through_a_worktree_gitdir_and_commondir() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("repo/.git");
        let gitdir = common.join("worktrees/wt");
        std::fs::create_dir_all(gitdir.join("refs")).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/m7\n").unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            common.join("refs/heads/m7"),
            format!("{}\n", "c".repeat(40)),
        )
        .unwrap();

        let root = dir.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();

        assert_eq!(harness_hash(&root), "c".repeat(40));
    }

    #[test]
    fn a_plain_checkout_a_detached_head_and_a_packed_ref_all_resolve() {
        let dir = tempfile::tempdir().unwrap();

        let plain = dir.path().join("plain");
        std::fs::create_dir_all(plain.join(".git/refs/heads")).unwrap();
        std::fs::write(plain.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            plain.join(".git/refs/heads/main"),
            format!("{}\n", "1".repeat(40)),
        )
        .unwrap();
        assert_eq!(harness_hash(&plain), "1".repeat(40));

        let detached = dir.path().join("detached");
        std::fs::create_dir_all(detached.join(".git")).unwrap();
        std::fs::write(detached.join(".git/HEAD"), format!("{}\n", "2".repeat(40))).unwrap();
        assert_eq!(harness_hash(&detached), "2".repeat(40));

        // `git gc` deletes refs/heads/main and leaves the hash in packed-refs.
        let packed = dir.path().join("packed");
        std::fs::create_dir_all(packed.join(".git")).unwrap();
        std::fs::write(packed.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            packed.join(".git/packed-refs"),
            format!(
                "# pack-refs with: peeled fully-peeled sorted \n{} refs/heads/main\n",
                "3".repeat(40)
            ),
        )
        .unwrap();
        assert_eq!(harness_hash(&packed), "3".repeat(40));
    }

    /// A checkout the gate cannot read its own commit out of still has to
    /// grade the harness. `unknown` costs one row's key; refusing to run
    /// costs the gate.
    #[test]
    fn a_tree_with_no_git_at_all_records_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(harness_hash(dir.path()), "unknown");
        // A HEAD that names a ref nothing wrote is the same case.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/gone\n").unwrap();
        assert_eq!(harness_hash(dir.path()), "unknown");
    }

    /// The difference between a gate and a report. A release script runs
    /// `ns-app eval` and reads the exit code; a set that failed and exited 0
    /// would let the regression through and leave the evidence in a table
    /// nobody diffed.
    #[test]
    fn a_failing_ability_exits_non_zero_and_still_records_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_LEDGER);
        assert_eq!(
            report(
                &[ability("abstention", true, 7)],
                &path,
                0.0,
                nscore::Depth::Full,
                Profile::default()
            ),
            0
        );
        assert_eq!(
            report(
                &[ability("abstention", false, 7)],
                &path,
                0.0,
                nscore::Depth::Full,
                Profile::default()
            ),
            1,
            "a failed ability has to reach the exit code"
        );
        let l = EvalLedger::load(&path).unwrap();
        assert_eq!(l.rows.len(), 2, "the failing run is recorded, not skipped");
        assert!(!l.rows[1].abilities[0].passed);
        assert_eq!(
            l.rows[1].abilities[0].detail,
            "superseded value not rendered"
        );
    }

    /// A ledger somebody hand-edited into invalid JSON must not be
    /// overwritten: every previous row is the only copy there is. The run
    /// still grades the harness, because the file has nothing to do with what
    /// the six abilities did.
    #[test]
    fn an_unparsable_ledger_is_left_alone_and_does_not_change_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_LEDGER);
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(
            report(
                &[ability("abstention", true, 7)],
                &path,
                0.0,
                nscore::Depth::Full,
                Profile::default()
            ),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "the operator's file is untouched"
        );
        assert_eq!(
            report(
                &[ability("abstention", false, 7)],
                &path,
                0.0,
                nscore::Depth::Full,
                Profile::default()
            ),
            1
        );
    }

    #[test]
    fn eval_args_accept_nothing_or_one_path() {
        let plain = parse_args(&[]).unwrap();
        assert_eq!(plain.ledger, PathBuf::from(DEFAULT_LEDGER));
        assert!(!plain.paraphrase);

        let named = parse_args(&["other.json".to_string()]).unwrap();
        assert_eq!(named.ledger, PathBuf::from("other.json"));

        assert!(parse_args(&["a".to_string(), "b".to_string()]).is_err());
    }

    /// M10 T5.4: the two knob modes are flags, they are off by default, and
    /// they are separate — a run that turned both on would print one table
    /// and silently drop the other, which is the failure a single `--knob`
    /// argument would have made easy.
    #[test]
    fn the_two_knob_arms_are_separate_flags_and_default_off() {
        let plain = parse_args(&[]).unwrap();
        assert!(!plain.obligations);
        assert!(!plain.guidelines);

        let o = parse_args(&["--obligations".to_string()]).unwrap();
        assert!(o.obligations && !o.guidelines);

        let g = parse_args(&["--guidelines".to_string()]).unwrap();
        assert!(g.guidelines && !g.obligations);

        assert!(parse_args(&["--obligation".to_string()]).is_err());
        assert!(parse_args(&["--guideline".to_string()]).is_err());
        assert!(USAGE.contains("--obligations"));
        assert!(USAGE.contains("--guidelines"));
    }

    /// `--paraphrase` is the M8 arm; `--live` is still the flag M7 refused,
    /// and adding one must not have quietly opened the other.
    #[test]
    fn the_paraphrase_arm_is_a_flag_and_live_is_still_refused() {
        let arm = parse_args(&["--paraphrase".to_string()]).unwrap();
        assert!(arm.paraphrase);
        assert_eq!(arm.ledger, PathBuf::from(DEFAULT_LEDGER));

        // It composes with a ledger path, in either order.
        for args in [
            vec!["runs.json".to_string(), "--paraphrase".to_string()],
            vec!["--paraphrase".to_string(), "runs.json".to_string()],
        ] {
            let parsed = parse_args(&args).unwrap();
            assert!(parsed.paraphrase);
            assert_eq!(parsed.ledger, PathBuf::from("runs.json"));
        }

        assert!(parse_args(&["--live".to_string()]).is_err());
        assert!(parse_args(&["--paraphrases".to_string()]).is_err());

        // M11 T1.1's modifier, and its follow-up: the flag is carried on
        // `Args` and handed to `run_paraphrase` as a parameter, so parsing
        // it sets nothing outside the value returned here. `--facts` alone
        // is still refused.
        assert!(!arm.facts, "a bare --paraphrase is the turns corpus");
        let both = parse_args(&["--paraphrase".to_string(), "--facts".to_string()]).unwrap();
        assert!(both.paraphrase && both.facts);
        assert!(parse_args(&["--facts".to_string()]).is_err());
    }

    /// `--ablate` takes a block name in the next argument, composes with a
    /// M9 T3.3: the weight parses like `--ablate` does, defaults to the
    /// shipped 0.0, composes with the other two arms, and refuses anything
    /// that is not a finite non-negative number — a negative weight would
    /// rank a fact *down* for having been useful, which is not a sweep point
    /// but a sign error.
    #[test]
    fn the_activation_flag_takes_a_weight_and_defaults_to_zero() {
        let a =
            |args: Vec<&str>| parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        assert_eq!(a(vec![]).unwrap().activation, 0.0);
        assert_eq!(a(vec!["--activation", "0.5"]).unwrap().activation, 0.5);
        let both = a(vec!["--paraphrase", "--activation", "1"]).unwrap();
        assert_eq!(both.activation, 1.0);
        assert!(both.paraphrase);
        let with_block = a(vec!["--ablate", "facts", "--activation", "1.0"]).unwrap();
        assert_eq!(with_block.activation, 1.0);
        assert_eq!(with_block.ablate, Some(nscore::Ablate::Facts));

        // M10 T2.1, mirroring `--activation`: a second arm of the same set,
        // not a second config.
        assert_eq!(a(vec![]).unwrap().depth, nscore::Depth::Full);
        assert_eq!(
            a(vec!["--depth", "adaptive"]).unwrap().depth,
            nscore::Depth::Adaptive
        );
        assert_eq!(
            a(vec!["--depth", "full", "--activation", "0.5"])
                .unwrap()
                .depth,
            nscore::Depth::Full
        );
        assert!(a(vec!["--depth"]).is_err(), "the depth is required");
        assert!(a(vec!["--depth", "shallow"]).is_err(), "and it is checked");

        assert!(a(vec!["--activation"]).is_err(), "the weight is required");
        assert!(a(vec!["--activation", "-1"]).is_err(), "no negative weight");
        assert!(a(vec!["--activation", "nan"]).is_err());
        assert!(a(vec!["--activation", "heavy"]).is_err());
    }

    /// **M12 T5.2: the context-profile arm.**
    ///
    /// Three things, and each of them is a way the arm could quietly stop
    /// being an arm: the caps parse, the footer says what the run was
    /// measured at, and the ledger is left alone — a profile run recorded in
    /// it would make the next default diff read as a harness change, which is
    /// the rule `--depth` and `--activation` already follow.
    #[test]
    fn the_profile_arm_prints_and_does_not_write_the_ledger() {
        let a =
            |args: Vec<&str>| parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        assert_eq!(a(vec![]).unwrap().profile, Profile::default());
        let parsed = a(vec!["--window", "10", "--facts", "16"]).unwrap();
        assert_eq!(
            parsed.profile,
            Profile {
                window: Some(10),
                facts: Some(16)
            }
        );
        assert_eq!(
            a(vec!["--window", "10"]).unwrap().profile,
            Profile {
                window: Some(10),
                facts: None
            },
            "either flag alone is an arm"
        );
        // `--facts` with no number after it is still `--paraphrase`'s
        // modifier, which is the flag it was before this arm existed.
        let corpus = a(vec!["--paraphrase", "--facts"]).unwrap();
        assert!(corpus.facts && corpus.profile.facts.is_none());
        assert!(a(vec!["--window"]).is_err(), "the cap is required");
        assert!(
            a(vec!["--window", "0"]).is_err(),
            "and a window of none is not a profile"
        );
        assert!(a(vec!["--facts", "0"]).is_err());

        // The footer is the deliverable of the arm: without the prefix level
        // the caps are two numbers nobody can act on.
        let rows = [ability("abstention", true, 7)];
        assert_eq!(
            profile_footer(&rows, parsed.profile),
            "context profile: window 10, facts 16 · emitter prefix (est.) median 75 tokens, \
             max 75 · breakpoint floor 1,024\n"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEFAULT_LEDGER);
        assert_eq!(
            report(&rows, &path, 0.0, nscore::Depth::Full, Profile::default()),
            0
        );
        let recorded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            report(&rows, &path, 0.0, nscore::Depth::Full, parsed.profile),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            recorded,
            "the profile arm must not add a row to the ledger the default arm diffs"
        );
        // And the verdict still travels: an arm that swallowed a failure
        // would be a gate with the light removed.
        assert_eq!(
            report(
                &[ability("abstention", false, 7)],
                &path,
                0.0,
                nscore::Depth::Full,
                parsed.profile
            ),
            1
        );
    }

    /// ledger path either way round, and refuses anything that is not one of
    /// the three blocks — including `obligations`, which the plan lists but
    /// which has no block to blank yet.
    #[test]
    fn the_ablate_arm_takes_a_block_and_refuses_anything_else() {
        let a =
            |args: Vec<&str>| parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        assert_eq!(a(vec![]).unwrap().ablate, None);
        assert_eq!(
            a(vec!["--ablate", "facts"]).unwrap().ablate,
            Some(nscore::Ablate::Facts)
        );
        assert_eq!(
            a(vec!["--ablate", "summary"]).unwrap().ablate,
            Some(nscore::Ablate::Summary)
        );
        for args in [
            vec!["runs.json", "--ablate", "guidance"],
            vec!["--ablate", "guidance", "runs.json"],
        ] {
            let parsed = a(args).unwrap();
            assert_eq!(parsed.ablate, Some(nscore::Ablate::Guidance));
            assert_eq!(parsed.ledger, PathBuf::from("runs.json"));
        }

        assert!(
            a(vec!["--ablate"]).is_err(),
            "a bare --ablate names no block"
        );
        assert!(a(vec!["--ablate", "obligations"]).is_err());
        assert!(a(vec!["--ablate", "runs.json"]).is_err());
    }
}
