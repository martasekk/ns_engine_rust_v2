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

use nstestkit::eval::{render_table, run_all, Ability};
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
             {} requests.\n",
            current.passed,
            current.of,
            current.requests()
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
        "    requests {} → {} · {}/{} → {}/{}\n",
        previous.requests(),
        current.requests(),
        previous.passed,
        previous.of,
        current.passed,
        current.of
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
}

const USAGE: &str =
    "usage: ns-app eval [<ledger-path>] [--paraphrase] [--ablate facts|summary|guidance]";

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut ledger = None;
    let mut paraphrase = false;
    let mut ablate = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--paraphrase" => paraphrase = true,
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
    Ok(Args {
        ledger: ledger.unwrap_or_else(|| PathBuf::from(DEFAULT_LEDGER)),
        paraphrase,
        ablate,
    })
}

/// `ns-app eval --ablate <block>` — one context block's marginal effect (M9
/// T0.4).
///
/// Exits 0 whatever the delta is. The ablated arm failing is the measurement
/// succeeding: it says the block was carrying those abilities. A zero delta
/// is equally a result — either the block is not earning its tokens, or this
/// scripted suite cannot see what it earns — and neither is a release
/// failure, so neither may turn the exit code red.
pub async fn run_ablate(block: nscore::Ablate) -> i32 {
    use nstestkit::ablate;

    let report = ablate::measure(block).await;
    print!("{}", ablate::render(&report));
    println!(
        "  both arms are the same {} fixtures against the same scripted doubles; \
         nothing here spends a request.",
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
pub async fn run_paraphrase() -> i32 {
    use nstestkit::paraphrase;

    let k = nsengine::turn::EngineConfig::default().recall_top_k;
    let mut reports = Vec::new();

    let memory = nsengine::store::InMemoryStore::new();
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
            reports.push(paraphrase::measure(&sqlite, "sqlite (fts5 bm25)", k).await);
        }
        Err(e) => eprintln!("paraphrase: the sqlite arm did not run ({e})"),
    }

    print!("{}", paraphrase::render(&reports));
    println!(
        "  k = {k} (recall_top_k), {} cases, no model calls and no requests spent.",
        paraphrase::corpus().len()
    );
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

/// Runs the set, prints the table and the ledger diff, returns the process
/// exit code: 0 when every ability passed, 1 otherwise.
pub async fn run(ledger_path: &Path) -> i32 {
    report(&run_all().await, ledger_path)
}

/// The gate, separated from the run so that a failing set can be tested
/// without one. The six abilities pass, which is exactly why the non-zero
/// path needs its own test: an exit code nothing exercises is a gate nobody
/// has checked.
fn report(abilities: &[Ability], ledger_path: &Path) -> i32 {
    print!("{}", render_table(abilities));

    let current = Row::build(harness_hash(Path::new(".")), now_ms(), abilities);
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
            peak_chars: 700,
            tool_calls: 3,
            recall_fired: false,
            recall_hits: 0,
            flags: 0,
            clipped_chars: 0,
            inspections: 0,
            budget_drops: 0,
            escalations: 0,
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
        assert_eq!(report(&[ability("abstention", true, 7)], &path), 0);
        assert_eq!(
            report(&[ability("abstention", false, 7)], &path),
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
        assert_eq!(report(&[ability("abstention", true, 7)], &path), 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "the operator's file is untouched"
        );
        assert_eq!(report(&[ability("abstention", false, 7)], &path), 1);
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
    }

    /// `--ablate` takes a block name in the next argument, composes with a
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
