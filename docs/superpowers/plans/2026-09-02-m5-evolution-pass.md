# M5 Evolution Pass Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The harness mines its own event logs for failures, distils two kinds of improvement (deterministic input-repair patches and model-written guidance notes), verifies each against the recordings, and auto-applies what passes — without ever loosening a guard.

**Architecture:** `ns-core` gains the `LearnedRules` data type (patches + notes, pure application functions), a small `validate_args` JSON-schema subset, a `guidance` field on `EmitterContext`, and `MemoryStore::sessions()`. `ns-engine` applies learned rules inside `run_turn`, validates tool args before classification, exposes `replay_with` (rule-aware replay that returns the replayed events), and grows an idle-timer in `Engine::run`. A new crate `ns-evolution` holds the pass: mine → propose → gate (replay-based for patches, live-probe for notes) → apply → ledger, implemented as a `Consolidator`. `ns-app` wires `[evolution]` config, `ns-app evolve [--dry-run]`, startup loading of `learned.toml`, and the idle driver.

**Tech Stack:** Rust workspace. New deps: `arc-swap 1` (hot-swappable rules), `strsim 0.11` (Damerau-Levenshtein), `tokio` gains the `time` feature. Existing: `toml`, `tempfile`, `sha2`, `serde`.

**Spec:** `docs/superpowers/specs/2026-09-02-evolution-pass-design.md`

## Global Constraints

- **Safety invariant (spec §2):** learned rules run *before* legality and validation and their output goes through the same checks as raw model output. No task may add a code path that skips a guard, widens a legal set, or changes a side-effect class.
- No new event kinds. The pass never appends to the event log.
- `learned.toml` and `evolution-ledger.json` are written temp-then-rename in their own directory.
- Decision record (settled at plan time, amends the spec where noted):
  - The engine had no tool-arg schema validation; `Rejected{Malformed}` for tool args therefore never occurred. Task 1 adds a JSON-schema subset (`required`, `type`, `enum`) in `ns-core::validate_args`, applied by the engine right before classification. This is what makes the `normalize_arg` lane observable. `remember_fact`'s dotted-key rule stays engine-custom and is not a `normalize_arg` target in v1.
  - Note hash is `sha256:<hex>` of `scope + "\n" + text` (spec §3.3 said blake3; `sha2` is already a workspace dep). Patch hash is `sha256:<hex>` of the patch's canonical JSON.
  - The legal set used by `alias_action` mining is the set of production tool specs (`known_specs`) plus the engine's synthetic actions, not a per-event replay reconstruction (spec §3.1). Replay doubles now carry the real `ActionSpec` when a known spec exists, so legality and validation match production.
  - Regression check (spec §4.1): a session that already diverges from its recording under the *current* rules (baseline) is excluded from the regression set and counted as `skipped_baseline`. Only a session that replays cleanly at baseline and diverges with the candidate counts as a regression.
  - Tool errors "attributable to args" (spec §3.1) means `ToolReturned{Err{kind}}` with `kind == "bad_args"`.
  - `Corrected` text of the form `key = value` (key a dotted identifier) is written as a `Fact` with `Provenance::Residual`, confidence 1.0, by the pass. This is the only store write the pass makes besides nothing — it never touches events.
  - Probe cost: every turn re-run counts 1 toward `probe_budget_turns`, so one session probed with and without a note costs 2 × turns.
- TDD per task; commit per task with the given message; git identity is repo-local (`user.name=Martin`, `user.email=hrabal@jtjdreams.cz`, already set) plus the trailer `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`; work on a feature branch `m5-evolution` in an isolated worktree; delete the worktree's `target/` before merging (disk is tight, 29 GB).
- Run `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` before every commit.
- Live smoke tests need `MISTRAL_API_KEY` (persisted in `~/.bashrc`, but `~/.bashrc` returns early for non-interactive shells: pass it explicitly, `MISTRAL_API_KEY=$(bash -ic 'echo $MISTRAL_API_KEY' 2>/dev/null) cargo run -p ns-app -- ...`).

## File Structure (end state of M5)

```
Cargo.toml                              # + crates/evolution member; arc-swap, strsim; tokio "time"
crates/core/src/validate.rs             # validate_args (required/type/enum)         [new]
crates/core/src/learned.rs              # LearnedRules, NormalizeArg, AliasAction, Note, Op [new]
crates/core/src/traits.rs               # EmitterContext.guidance; MemoryStore::sessions
crates/core/src/lib.rs                  # + pub mod validate; pub mod learned;
crates/engine/src/store.rs              # InMemoryStore::sessions
crates/memory-sqlite/src/lib.rs         # SqliteStore::sessions
crates/llm/src/emitter.rs               # render guidance block
crates/engine/src/turn.rs               # learned handle in EngineConfig; alias/normalize/guidance; arg validation; idle timer; pub FALLBACK_REPLY
crates/engine/src/replay.rs             # ReplayOptions, Replayed, doubles_from, replay_with, diff
crates/evolution/Cargo.toml             # ns-evolution (lib nsevolution)                [new]
crates/evolution/src/lib.rs             # mods                                          [new]
crates/evolution/src/files.rs           # load/save learned.toml atomically             [new]
crates/evolution/src/ledger.rs          # Ledger, LedgerEntry, Evidence, Verdict        [new]
crates/evolution/src/mine.rs            # Signature, SignatureKind, mine, near_tool     [new]
crates/evolution/src/symbolic.rs        # Patch, propose_patches, verify_patch          [new]
crates/evolution/src/notes.rs           # NoteProposer, ProbeRunner, LiveProbe, verify_note [new]
crates/evolution/src/pass.rs            # EvolutionPass (Consolidator), PassConfig, Report [new]
app/src/config.rs                       # + [evolution] section
app/src/main.rs                         # evolve subcommand, startup rules, idle driver
config.example.toml                     # + [evolution] block
docs/research/2026-09-01-findings.md    # §6 amendments (new citations)
```

---

### Task 1: Tool-arg validation (`validate_args`) in core, applied by the engine

**Files:**
- Create: `crates/core/src/validate.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod validate;` and `pub use validate::*;`)
- Modify: `crates/engine/src/turn.rs` (step "g" — find tool, then validate, then classify)
- Test: inline in `validate.rs`; `crates/engine/tests/turn_loop.rs`

**Interfaces:**
- Produces: `pub fn validate_args(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String>` in `nscore`. Checks, in order: args is a JSON object; every name in `schema.required` is present; for each present arg with `schema.properties[name].type` the JSON type matches (`string|number|integer|boolean|object|array`); for each present arg with `schema.properties[name].enum` the value is a member. Unknown extra args are allowed. `Err` carries the first failure as a short sentence, e.g. `missing required arg "city"`, `arg "unit" must be one of ["c","f"], got "Celsius"`.
- Engine: a failing validation appends `Rejected { proposal_of: pid, reason: Malformed { detail: format!("{action}: {detail}") } }`, pushes `format!("malformed args for {action}: {detail}")` to `rejections_this_turn`, and `continue`s. It does **not** add the action to `denied_this_turn` (the emitter should retry with repaired args).

- [ ] **Step 1: Write the failing core tests**

```rust
// crates/core/src/validate.rs
use serde_json::Value;

/// Minimal JSON-schema subset the harness enforces on tool args before
/// classification: `required`, per-property `type`, per-property `enum`.
/// Unknown extra args are allowed (the model may pass harmless extras).
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "days": {"type": "integer"},
                "unit": {"type": "string", "enum": ["c", "f"]}
            },
            "required": ["city"]
        })
    }

    #[test]
    fn valid_args_pass() {
        assert_eq!(validate_args(&schema(), &json!({"city": "Brno", "days": 2, "unit": "c"})), Ok(()));
        assert_eq!(validate_args(&schema(), &json!({"city": "Brno", "extra": 1})), Ok(()));
    }

    #[test]
    fn missing_required_fails() {
        let err = validate_args(&schema(), &json!({"days": 2})).unwrap_err();
        assert_eq!(err, r#"missing required arg "city""#);
    }

    #[test]
    fn wrong_type_fails() {
        let err = validate_args(&schema(), &json!({"city": 5})).unwrap_err();
        assert_eq!(err, r#"arg "city" must be string, got 5"#);
        let err = validate_args(&schema(), &json!({"city": "x", "days": 1.5})).unwrap_err();
        assert_eq!(err, r#"arg "days" must be integer, got 1.5"#);
    }

    #[test]
    fn enum_mismatch_fails_and_names_choices() {
        let err = validate_args(&schema(), &json!({"city": "x", "unit": "Celsius"})).unwrap_err();
        assert_eq!(err, r#"arg "unit" must be one of ["c","f"], got "Celsius""#);
    }

    #[test]
    fn non_object_args_fail_and_empty_schema_accepts_anything() {
        assert!(validate_args(&schema(), &json!("nope")).is_err());
        assert_eq!(validate_args(&json!({"type": "object", "properties": {}}), &json!({"a": 1})), Ok(()));
    }
}
```

Add to `crates/core/src/lib.rs`:

```rust
pub mod validate;
pub use validate::*;
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ns-core validate`
Expected: panics with `not yet implemented` (todo!).

- [ ] **Step 3: Implement**

```rust
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Err(format!("args must be an object, got {args}"));
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required {
            if let Some(name) = r.as_str() {
                if !obj.contains_key(name) {
                    return Err(format!("missing required arg {name:?}"));
                }
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    for (name, value) in obj {
        let Some(prop) = props.and_then(|p| p.get(name)) else { continue };
        if let Some(ty) = prop.get("type").and_then(Value::as_str) {
            let ok = match ty {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.is_i64() || value.is_u64(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => true,
            };
            if !ok {
                return Err(format!("arg {name:?} must be {ty}, got {value}"));
            }
        }
        if let Some(choices) = prop.get("enum").and_then(Value::as_array) {
            if !choices.contains(value) {
                let list = Value::Array(choices.clone());
                return Err(format!("arg {name:?} must be one of {list}, got {value}"));
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run core tests**

Run: `cargo test -p ns-core validate`
Expected: 5 passed.

- [ ] **Step 5: Write the failing engine test** (append to `crates/engine/tests/turn_loop.rs`)

```rust
#[tokio::test]
async fn tool_args_failing_schema_are_rejected_as_malformed_not_called() {
    let store = Arc::new(InMemoryStore::new());
    // echo requires a string "text"; an integer must be rejected before the tool runs.
    let bad = Proposal { rationale: "r".into(), action: "echo".into(), args: serde_json::json!({"text": 42}) };
    let mut e = engine_with(vec![bad, echo_proposal("ok")], vec![], store.clone());
    let sid = SessionId("val".into());
    e.run_turn(Incoming { session: sid.clone(), text: "go".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    let reasons = rejection_reasons(&events);
    assert_eq!(reasons.len(), 1);
    assert!(matches!(reasons[0], RejectReason::Malformed { detail } if detail.contains("must be string")), "got {:?}", reasons[0]);
    // The action stays legal: the repaired second proposal runs.
    assert_eq!(tool_calls(&events, "echo"), 1);
}
```

- [ ] **Step 6: Run to verify it fails**

Run: `cargo test -p ns-engine --test turn_loop tool_args_failing_schema`
Expected: FAIL — `reasons.len()` is 0 (the tool ran with bad args and returned `bad_args`).

- [ ] **Step 7: Implement in `turn.rs`** — replace the start of step "g" (`// g. classify args ...` up to and including the `let tool = ...clone();` statement) with:

```rust
            // g0. find the tool (legality guaranteed it exists)
            let tool = self
                .parts
                .tools
                .iter()
                .find(|t| t.spec().name == proposal.action)
                .expect("legality checked above")
                .clone();

            // g1. schema validation (spec M5 §3.2): malformed args are
            // rejected before classification; the action stays legal so the
            // emitter can retry with repaired args.
            if let Err(detail) = nscore::validate_args(&tool.spec().args_schema, &proposal.args) {
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason: RejectReason::Malformed {
                            detail: format!("{}: {detail}", proposal.action),
                        },
                    },
                );
                rejections_this_turn.push(format!("malformed args for {}: {detail}", proposal.action));
                continue;
            }

            // g. classify args against the session's history (spec §5.4)
```

- [ ] **Step 8: Run the whole workspace**

Run: `cargo test --workspace`
Expected: all pass (existing tests only ever pass valid args).

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/validate.rs crates/core/src/lib.rs crates/engine/src/turn.rs crates/engine/tests/turn_loop.rs
git commit -m "feat(core,engine): validate tool args (required/type/enum) before classification"
```

---

### Task 2: `LearnedRules` data type in core

**Files:**
- Create: `crates/core/src/learned.rs`
- Modify: `crates/core/src/lib.rs` (`pub mod learned; pub use learned::*;`)
- Test: inline

**Interfaces (produces):**

```rust
pub enum Op { Trim, StripPunct, Lowercase, StripPrefix(String) }   // serde as "trim" | "strip_punct" | "lowercase" | "strip_prefix:<lit>"
impl Op { pub fn apply(&self, s: &str) -> String }
impl std::str::FromStr for Op; impl std::fmt::Display for Op;
pub fn apply_ops(ops: &[Op], s: &str) -> String

pub struct NormalizeArg { pub action: String, pub arg: String, pub ops: Vec<Op> }
pub struct AliasAction { pub from: String, pub to: String }
pub struct Note { pub scope: String, pub text: String, pub lift: f64, pub hash: String }
impl Note { pub fn new(scope: &str, text: &str, lift: f64) -> Note; pub fn hash_of(scope: &str, text: &str) -> String; pub fn applies_to(&self, legal: &[String]) -> bool }

pub struct LearnedRules { pub version: u32, pub normalize_arg: Vec<NormalizeArg>, pub alias_action: Vec<AliasAction>, pub notes: Vec<Note> }  // TOML tables: [[normalize_arg]] [[alias_action]] [[note]]
impl LearnedRules {
    pub fn alias(&self, action: &str) -> Option<&str>;                        // first matching from
    pub fn normalize(&self, action: &str, args: &mut serde_json::Value);    // in place, string args only
    pub fn guidance_for(&self, legal: &[String]) -> Vec<String>;             // global + action:<name> for names in legal
}
```

- [ ] **Step 1: Write the failing tests** (file with `todo!()` bodies first)

```rust
// crates/core/src/learned.rs
//! Learned rules (spec M5 §3): deterministic input repairs the engine applies
//! before legality/validation, plus guidance notes for the emitter prompt.
//! Application is pure; loading/saving lives in ns-evolution.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Op {
    Trim,
    StripPunct,
    Lowercase,
    StripPrefix(String),
}

impl Op {
    pub fn apply(&self, s: &str) -> String {
        todo!()
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!()
    }
}

impl std::str::FromStr for Op {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        todo!()
    }
}

impl TryFrom<String> for Op {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Op> for String {
    fn from(op: Op) -> String {
        op.to_string()
    }
}

pub fn apply_ops(ops: &[Op], s: &str) -> String {
    ops.iter().fold(s.to_string(), |acc, op| op.apply(&acc))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizeArg {
    pub action: String,
    pub arg: String,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AliasAction {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// "global" or "action:<name>"
    pub scope: String,
    pub text: String,
    #[serde(default)]
    pub lift: f64,
    #[serde(default)]
    pub hash: String,
}

impl Note {
    pub fn new(scope: &str, text: &str, lift: f64) -> Note {
        todo!()
    }
    pub fn hash_of(scope: &str, text: &str) -> String {
        todo!()
    }
    pub fn applies_to(&self, legal: &[String]) -> bool {
        todo!()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearnedRules {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub normalize_arg: Vec<NormalizeArg>,
    #[serde(default)]
    pub alias_action: Vec<AliasAction>,
    #[serde(default, rename = "note")]
    pub notes: Vec<Note>,
}

fn default_version() -> u32 {
    1
}

impl Default for LearnedRules {
    fn default() -> Self {
        Self { version: 1, normalize_arg: vec![], alias_action: vec![], notes: vec![] }
    }
}

impl LearnedRules {
    pub fn alias(&self, action: &str) -> Option<&str> {
        todo!()
    }
    pub fn normalize(&self, action: &str, args: &mut serde_json::Value) {
        todo!()
    }
    pub fn guidance_for(&self, legal: &[String]) -> Vec<String> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ops_apply_and_round_trip_as_strings() {
        assert_eq!(Op::Trim.apply("  x "), "x");
        assert_eq!(Op::StripPunct.apply("\"Brno.\""), "Brno");
        assert_eq!(Op::Lowercase.apply("Celsius"), "celsius");
        assert_eq!(Op::StripPrefix("city:".into()).apply("city:Brno"), "Brno");
        assert_eq!(Op::StripPrefix("city:".into()).apply("Brno"), "Brno");
        for op in [Op::Trim, Op::StripPunct, Op::Lowercase, Op::StripPrefix("a:b".into())] {
            assert_eq!(op.to_string().parse::<Op>().unwrap(), op);
        }
        assert!("explode".parse::<Op>().is_err());
        assert_eq!(apply_ops(&[Op::Trim, Op::StripPunct, Op::Lowercase], " \"Celsius\". "), "celsius");
    }

    #[test]
    fn toml_round_trip_with_ops_as_strings() {
        let text = r#"
            version = 1
            [[normalize_arg]]
            action = "get_weather"
            arg = "unit"
            ops = ["trim", "lowercase", "strip_prefix:unit="]
            [[alias_action]]
            from = "getTime"
            to = "get_time"
            [[note]]
            scope = "global"
            text = "Prefer a tool over guessing."
            lift = 0.5
            hash = "sha256:abc"
        "#;
        let rules: LearnedRules = toml::from_str(text).unwrap();
        assert_eq!(rules.normalize_arg[0].ops, vec![Op::Trim, Op::Lowercase, Op::StripPrefix("unit=".into())]);
        assert_eq!(rules.alias("getTime"), Some("get_time"));
        let back = toml::to_string(&rules).unwrap();
        assert!(back.contains("\"strip_prefix:unit=\""), "{back}");
        assert_eq!(toml::from_str::<LearnedRules>(&back).unwrap(), rules);
        assert_eq!(toml::from_str::<LearnedRules>("").unwrap(), LearnedRules::default());
    }

    #[test]
    fn normalize_rewrites_only_matching_string_args() {
        let rules = LearnedRules {
            normalize_arg: vec![NormalizeArg { action: "get_weather".into(), arg: "unit".into(), ops: vec![Op::Trim, Op::Lowercase] }],
            ..Default::default()
        };
        let mut args = json!({"unit": " Celsius", "city": " Brno", "n": 3});
        rules.normalize("get_weather", &mut args);
        assert_eq!(args, json!({"unit": "celsius", "city": " Brno", "n": 3}));
        let mut other = json!({"unit": " Celsius"});
        rules.normalize("other_action", &mut other);
        assert_eq!(other, json!({"unit": " Celsius"}));
        let mut num = json!({"unit": 7});
        rules.normalize("get_weather", &mut num);
        assert_eq!(num, json!({"unit": 7}));
    }

    #[test]
    fn guidance_is_global_plus_legal_action_scoped() {
        let rules = LearnedRules {
            notes: vec![
                Note::new("global", "G", 0.0),
                Note::new("action:echo", "E", 0.0),
                Note::new("action:wipe", "W", 0.0),
            ],
            ..Default::default()
        };
        assert_eq!(rules.guidance_for(&["echo".to_string()]), vec!["G".to_string(), "E".to_string()]);
        assert_eq!(rules.guidance_for(&[]), vec!["G".to_string()]);
    }

    #[test]
    fn note_hash_is_stable_and_scope_sensitive() {
        let a = Note::new("global", "x", 0.0);
        assert!(a.hash.starts_with("sha256:") && a.hash.len() == 7 + 64);
        assert_eq!(a.hash, Note::hash_of("global", "x"));
        assert_ne!(Note::hash_of("global", "x"), Note::hash_of("action:echo", "x"));
    }
}
```

Add `toml = { workspace = true }` under `[dev-dependencies]` in `crates/core/Cargo.toml` (tests only; core itself stays TOML-agnostic).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-core learned`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement the bodies**

```rust
impl Op {
    pub fn apply(&self, s: &str) -> String {
        match self {
            Op::Trim => s.trim().to_string(),
            Op::StripPunct => s.trim_matches(|c: char| c.is_ascii_punctuation()).to_string(),
            Op::Lowercase => s.to_lowercase(),
            Op::StripPrefix(p) => s.strip_prefix(p.as_str()).unwrap_or(s).to_string(),
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::Trim => write!(f, "trim"),
            Op::StripPunct => write!(f, "strip_punct"),
            Op::Lowercase => write!(f, "lowercase"),
            Op::StripPrefix(p) => write!(f, "strip_prefix:{p}"),
        }
    }
}

impl std::str::FromStr for Op {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trim" => Ok(Op::Trim),
            "strip_punct" => Ok(Op::StripPunct),
            "lowercase" => Ok(Op::Lowercase),
            _ => match s.strip_prefix("strip_prefix:") {
                Some(p) if !p.is_empty() => Ok(Op::StripPrefix(p.to_string())),
                _ => Err(format!("unknown op {s:?}")),
            },
        }
    }
}

impl Note {
    pub fn new(scope: &str, text: &str, lift: f64) -> Note {
        Note { scope: scope.into(), text: text.into(), lift, hash: Note::hash_of(scope, text) }
    }
    pub fn hash_of(scope: &str, text: &str) -> String {
        let mut h = Sha256::new();
        h.update(scope.as_bytes());
        h.update(b"\n");
        h.update(text.as_bytes());
        format!("sha256:{:x}", h.finalize())
    }
    pub fn applies_to(&self, legal: &[String]) -> bool {
        match self.scope.strip_prefix("action:") {
            None => self.scope == "global",
            Some(name) => legal.iter().any(|l| l == name),
        }
    }
}

impl LearnedRules {
    pub fn alias(&self, action: &str) -> Option<&str> {
        self.alias_action.iter().find(|a| a.from == action).map(|a| a.to.as_str())
    }

    pub fn normalize(&self, action: &str, args: &mut serde_json::Value) {
        let Some(obj) = args.as_object_mut() else { return };
        for rule in self.normalize_arg.iter().filter(|r| r.action == action) {
            if let Some(serde_json::Value::String(s)) = obj.get_mut(&rule.arg) {
                *s = apply_ops(&rule.ops, s);
            }
        }
    }

    pub fn guidance_for(&self, legal: &[String]) -> Vec<String> {
        self.notes.iter().filter(|n| n.applies_to(legal)).map(|n| n.text.clone()).collect()
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p ns-core learned`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/learned.rs crates/core/src/lib.rs crates/core/Cargo.toml
git commit -m "feat(core): LearnedRules — normalize_arg / alias_action patches and guidance notes"
```

---

### Task 3: `EmitterContext.guidance` rendered by the cloud emitter

**Files:**
- Modify: `crates/core/src/traits.rs` (`EmitterContext` gains `pub guidance: Vec<String>`)
- Modify: every `EmitterContext { ... }` literal: `crates/engine/src/turn.rs`, `crates/engine/src/script.rs` (test), `crates/llm/src/emitter.rs` (test `ctx()`), `crates/engine/tests/turn_loop.rs` (search `EmitterContext {`)
- Modify: `crates/llm/src/emitter.rs` (`render_context`)
- Test: `crates/llm/src/emitter.rs`

**Interfaces:**
- Produces: `EmitterContext.guidance: Vec<String>`; rendered by `CloudEmitter` as a block `Guidance:\n- <note>\n...` placed after "Rejected this turn" and before "Propose the next action." (inside the user message, so the system prompt — the cacheable prefix — is unchanged).

- [ ] **Step 1: Add the field and fix all literals** (`guidance: vec![]` everywhere; in `turn.rs` also `vec![]` for now — Task 5 fills it)

```rust
pub struct EmitterContext {
    pub state_summary: String,
    /// (speaker, text), speaker: "user" | "assistant"
    pub recent_turns: Vec<(String, String)>,
    pub rejections_this_turn: Vec<String>,
    /// Learned guidance notes (spec M5 §3.3): global + scoped to legal actions.
    pub guidance: Vec<String>,
}
```

Run: `cargo build --workspace --all-targets` — fix every literal until it compiles (`grep -rn "EmitterContext {" crates app` lists them).

- [ ] **Step 2: Write the failing emitter test** (append inside `mod tests` in `emitter.rs`)

```rust
    #[tokio::test]
    async fn guidance_notes_are_rendered_in_the_user_message_not_the_system_prompt() {
        let mock = std::sync::Arc::new(MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: tool_call_response("echo", serde_json::json!({"text": "x"})),
        })]));
        let e = emitter(mock.clone());
        let mut c = ctx();
        c.guidance = vec!["Call get_time before answering time questions.".into()];
        e.propose(c, &legal()).await.unwrap();
        let req = mock.requests()[0].body.clone();
        let user = req["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("Guidance:\n- Call get_time before answering time questions."), "{user}");
        let system = req["messages"][0]["content"].as_str().unwrap();
        assert!(!system.contains("Guidance"));
    }
```

(Match the exact `MockTransport` constructor and `requests()` accessor already used by `request_carries_schema_context_and_forced_tool_choice` in that file; copy its shape.)

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p ns-llm guidance_notes`
Expected: FAIL — user message has no "Guidance:".

- [ ] **Step 4: Implement in `render_context`** (before the final `Propose the next action.` line)

```rust
    if !ctx.guidance.is_empty() {
        s.push_str("Guidance:\n");
        for g in &ctx.guidance {
            s.push_str(&format!("- {g}\n"));
        }
    }
```

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A crates app
git commit -m "feat(core,llm): EmitterContext.guidance rendered as a Guidance block"
```

---

### Task 4: `MemoryStore::sessions()` for both stores

**Files:**
- Modify: `crates/core/src/traits.rs` (trait method), `crates/engine/src/store.rs`, `crates/memory-sqlite/src/lib.rs`, every other `impl MemoryStore` (test doubles: `grep -rn "impl nscore::MemoryStore\|impl MemoryStore" crates app`)
- Test: `store.rs`, `memory-sqlite/src/lib.rs`

**Interfaces:**
- Produces: `async fn sessions(&self) -> Result<Vec<SessionId>, StoreError>` — newest activity first (by max event `at`, ties by session id descending for determinism).

- [ ] **Step 1: Add the trait method**

```rust
    /// All sessions with at least one event, most recently active first.
    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError>;
```

Make every test double compile with `async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> { Ok(vec![]) }`.

- [ ] **Step 2: Failing tests**

In `crates/engine/src/store.rs` tests:

```rust
    #[tokio::test]
    async fn sessions_lists_newest_first() {
        let store = InMemoryStore::new();
        for (name, at) in [("old", 1u64), ("new", 9), ("mid", 5)] {
            let sid = SessionId(name.into());
            let mut log = EventLog::new(sid.clone());
            log.append(1, Timestamp(at), EventKind::UserSaid { text: "x".into() });
            store.append(&sid, log.events()).await.unwrap();
        }
        let names: Vec<String> = store.sessions().await.unwrap().into_iter().map(|s| s.0).collect();
        assert_eq!(names, vec!["new", "mid", "old"]);
    }
```

In `crates/memory-sqlite/src/lib.rs` tests, the same test body against `SqliteStore::open(&dir.path().join("s.sqlite"))` (copy the `tempfile::tempdir()` pattern from `survives_reopen`).

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p ns-engine sessions_lists && cargo test -p ns-memory-sqlite sessions_lists`
Expected: compile error (method missing) → then FAIL after adding stubs.

- [ ] **Step 4: Implement**

`InMemoryStore`:

```rust
    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        let map = self.events.lock().await;
        let mut v: Vec<(u64, SessionId)> = map
            .iter()
            .filter(|(_, evs)| !evs.is_empty())
            .map(|(sid, evs)| (evs.iter().map(|e| e.at.0).max().unwrap_or(0), sid.clone()))
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1 .0.cmp(&a.1 .0)));
        Ok(v.into_iter().map(|(_, s)| s).collect())
    }
```

`SqliteStore`:

```rust
    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, MAX(at) AS last FROM events
                 GROUP BY session_id ORDER BY last DESC, session_id DESC",
            )
            .map_err(io_err)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(io_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(SessionId(row.map_err(io_err)?));
        }
        Ok(out)
    }
```

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A crates app
git commit -m "feat(core,stores): MemoryStore::sessions() newest-first"
```

---

### Task 5: Engine applies learned rules (alias, normalize, guidance) behind a hot-swappable handle

**Files:**
- Modify: `Cargo.toml` (workspace dep `arc-swap = "1"`), `crates/engine/Cargo.toml` (`arc-swap = { workspace = true }`)
- Modify: `crates/engine/src/turn.rs` (`EngineConfig.learned`, `pub const FALLBACK_REPLY`, step "f0", guidance in ctx)
- Test: `crates/engine/tests/turn_loop.rs`

**Interfaces:**
- Produces: `EngineConfig.learned: std::sync::Arc<arc_swap::ArcSwap<nscore::LearnedRules>>` (default: empty rules). `pub const FALLBACK_REPLY: &str` becomes public (the pass detects fallback replies by it).
- Behavior: at the top of `run_turn`, `let rules = self.cfg.learned.load_full();` (one snapshot per turn). After the `Proposed` event is recorded and before the `respond_directly` check: alias the action, then normalize args. Guidance: `ctx.guidance = rules.guidance_for(&legal_names)`.

- [ ] **Step 1: Add the dependency and the config field**

`Cargo.toml` `[workspace.dependencies]`: `arc-swap = "1"`. `crates/engine/Cargo.toml`: `arc-swap = { workspace = true }`.

```rust
pub struct EngineConfig {
    pub max_iterations: u32,
    pub max_emit_retries: u32,
    pub persona: String,
    pub templates: std::collections::HashMap<String, String>,
    /// Learned input repairs + guidance (spec M5). Hot-swappable: a driver
    /// replaces the set; each turn loads one snapshot at its start.
    pub learned: std::sync::Arc<arc_swap::ArcSwap<nscore::LearnedRules>>,
}
```

`Default`: `learned: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(nscore::LearnedRules::default()))`. Change `const FALLBACK_REPLY` to `pub const FALLBACK_REPLY`. Fix `app/src/main.rs`'s `EngineConfig { ... }` literal with `learned: Default::default()` for now (Task 13 replaces it).

- [ ] **Step 2: Write the failing tests** (append to `turn_loop.rs`)

```rust
fn rules_handle(rules: LearnedRules) -> Arc<arc_swap::ArcSwap<LearnedRules>> {
    Arc::new(arc_swap::ArcSwap::from_pointee(rules))
}

fn engine_with_rules(proposals: Vec<Proposal>, store: Arc<InMemoryStore>, rules: LearnedRules) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig { learned: rules_handle(rules), ..EngineConfig::default() };
    Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)))
}

#[tokio::test]
async fn alias_action_rewrites_a_near_miss_name_but_proposed_event_keeps_the_raw_name() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        alias_action: vec![AliasAction { from: "eko".into(), to: "echo".into() }],
        ..Default::default()
    };
    let p = Proposal { rationale: "r".into(), action: "eko".into(), args: serde_json::json!({"text": "hi"}) };
    let mut e = engine_with_rules(vec![p], store.clone(), rules);
    let sid = SessionId("alias".into());
    e.run_turn(Incoming { session: sid.clone(), text: "go".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Proposed { proposal } if proposal.action == "eko")));
    assert_eq!(tool_calls(&events, "echo"), 1);
    assert!(rejection_reasons(&events).is_empty());
}

#[tokio::test]
async fn normalize_arg_repairs_args_before_validation() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        normalize_arg: vec![NormalizeArg { action: "echo".into(), arg: "text".into(), ops: vec![Op::Trim, Op::StripPunct] }],
        ..Default::default()
    };
    let p = Proposal { rationale: "r".into(), action: "echo".into(), args: serde_json::json!({"text": " \"hi\" "}) };
    let mut e = engine_with_rules(vec![p], store.clone(), rules);
    let sid = SessionId("norm".into());
    let reply = e.run_turn(Incoming { session: sid.clone(), text: "go".into() }).await.unwrap();
    assert!(reply.contains("echo: hi"), "{reply}");
}

#[tokio::test]
async fn aliased_action_still_goes_through_guards() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        alias_action: vec![AliasAction { from: "eko".into(), to: "echo".into() }],
        ..Default::default()
    };
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal { rationale: "r".into(), action: "eko".into(), args: serde_json::json!({"text": "hi"}) }])));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    b.add_guard(Box::new(DenyAction { action: "echo".into(), reason: "no".into() }));
    let cfg = EngineConfig { learned: rules_handle(rules), ..EngineConfig::default() };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let sid = SessionId("alias-guard".into());
    e.run_turn(Incoming { session: sid.clone(), text: "go".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(tool_calls(&events, "echo"), 0);
    assert!(matches!(rejection_reasons(&events)[0], RejectReason::GuardDenied { .. }));
}

#[tokio::test]
async fn guidance_reaches_the_emitter_scoped_to_legal_actions() {
    // CtxProbe (defined above in this file) records the EmitterContext it receives.
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        notes: vec![Note::new("global", "G", 0.0), Note::new("action:echo", "E", 0.0), Note::new("action:wipe", "W", 0.0)],
        ..Default::default()
    };
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let probe = GuidanceProbe(seen.clone());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(probe));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig { learned: rules_handle(rules), ..EngineConfig::default() };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    e.run_turn(Incoming { session: SessionId("g".into()), text: "go".into() }).await.unwrap();
    assert_eq!(seen.lock().unwrap()[0], vec!["G".to_string(), "E".to_string()]);
}

struct GuidanceProbe(Arc<std::sync::Mutex<Vec<Vec<String>>>>);
#[async_trait::async_trait]
impl Emitter for GuidanceProbe {
    async fn propose(&self, ctx: EmitterContext, _legal: &LegalActionSet) -> Result<Proposal, EmitError> {
        self.0.lock().unwrap().push(ctx.guidance.clone());
        Ok(Proposal { rationale: "".into(), action: "respond_directly".into(), args: serde_json::json!({}) })
    }
}
```

Add `arc-swap = { workspace = true }` to `[dev-dependencies]` of `crates/engine/Cargo.toml` is unnecessary (it is a normal dep); tests import `arc_swap` through the engine crate's dependency graph only if re-exported — simplest: add `pub use arc_swap;` to `crates/engine/src/lib.rs` and write `nsengine::arc_swap::ArcSwap` in tests.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p ns-engine --test turn_loop alias_action normalize_arg aliased_action guidance_reaches`
Expected: the alias tests FAIL (IllegalAction rejected), normalize FAILS (`echo: "hi"` with quotes), guidance FAILS (empty vec).

- [ ] **Step 4: Implement in `run_turn`**

At the top, after `let now = &self.clock;`:

```rust
        let rules = self.cfg.learned.load_full();
```

In "b. emitter context", build the legal names and pass guidance:

```rust
            let legal_names: Vec<String> = legal.actions.iter().map(|a| a.name.clone()).collect();
            let ctx = nscore::EmitterContext {
                state_summary: summary,
                recent_turns: recent,
                rejections_this_turn: rejections_this_turn.clone(),
                guidance: rules.guidance_for(&legal_names),
            };
```

Right after "d. record proposal" (the `let pid = ...` statement) and before "e. direct reply":

```rust
            // f0. learned input repairs (spec M5 §3.2). The Proposed event above
            // keeps the raw model output; ToolCalled records what actually ran.
            // Repairs only rewrite the proposal — legality, validation and
            // guards below judge the rewritten proposal exactly as raw output.
            if let Some(to) = rules.alias(&proposal.action) {
                proposal.action = to.to_string();
            }
            rules.normalize(&proposal.action, &mut proposal.args);
```

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/engine app/src/main.rs
git commit -m "feat(engine): apply learned alias/normalize rules and guidance behind an ArcSwap handle"
```

---

### Task 6: `replay_with` — rule-aware replay that returns the replayed events

**Files:**
- Modify: `crates/engine/src/replay.rs`
- Test: inline

**Interfaces (produces):**

```rust
pub struct ReplayOptions {
    pub learned: Arc<LearnedRules>,                 // default empty
    pub extra_guards: Vec<Box<dyn Guard>>,          // default none
    pub known_specs: Vec<ActionSpec>,               // production tool specs; doubles carry them
    pub synthetic_ok_for_new_calls: bool,           // default false
}
impl Default for ReplayOptions;
pub struct Replayed { pub events: Vec<Event> }
pub struct Doubles { pub user_inputs: Vec<String>, pub proposals: Vec<Proposal>, pub tools: Vec<Arc<dyn Tool>>, pub replier: Box<dyn Replier> }
pub fn doubles_from(recorded: &[Event], known_specs: &[ActionSpec], synthetic_ok: bool) -> Doubles
pub async fn replay_with(session: SessionId, recorded: &[Event], opts: ReplayOptions) -> Result<Replayed, ReplayError>   // chain check + run, no diff
pub fn diff(recorded: &[Event], replayed: &[Event]) -> Result<(), ReplayError>
pub async fn replay_session(session, recorded, extra_guards) -> Result<(), ReplayError>  // = replay_with(default opts + guards) then diff
```

- Double semantics: a `ReplayTool` exists for every name in `known_specs` (carrying that spec) **and** for every recorded `ToolCalled` action not in `known_specs` (dummy spec, as today), minus the synthetic actions. When its outcome queue is empty: `synthetic_ok` → `Ok(ToolOutput { summary: "(verified: synthetic ok)".into(), artifact: None, trust: Trust::System })`, else the existing `outcome queue exhausted` error.

- [ ] **Step 1: Write the failing tests** (append to the existing `mod tests` in `replay.rs`)

```rust
    #[tokio::test]
    async fn replay_with_returns_events_and_replay_session_is_its_wrapper() {
        let (sid, events) = record_session(vec![]).await;
        let r = replay_with(sid.clone(), &events, ReplayOptions::default()).await.unwrap();
        assert_eq!(normalize(&r.events), normalize(&events));
        assert!(diff(&events, &r.events).is_ok());
        replay_session(sid, &events, vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn learned_alias_flips_a_recorded_illegal_action_into_a_call() {
        // Record a session where the emitter proposed "eko" (illegal) then gave up.
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("flip".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "typo".into(),
            action: "eko".into(),
            args: serde_json::json!({"text": "hi"}),
        }])));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(b.build().unwrap(), EngineConfig::default(), Box::new(|| Timestamp(1)));
        e.run_turn(Incoming { session: sid.clone(), text: "say hi".into() }).await.unwrap();
        let recorded = store.load(&sid).await.unwrap();
        let rejected_at = normalize(&recorded).iter().position(|l| l == "Rejected IllegalAction").unwrap();

        // echo was never called in the recording, so the double only exists
        // because known_specs carries it; the call has no recorded outcome.
        let opts = ReplayOptions {
            learned: Arc::new(nscore::LearnedRules {
                alias_action: vec![nscore::AliasAction { from: "eko".into(), to: "echo".into() }],
                ..Default::default()
            }),
            known_specs: vec![EchoTool::new().spec().clone()],
            synthetic_ok_for_new_calls: true,
            ..Default::default()
        };
        let r = replay_with(sid, &recorded, opts).await.unwrap();
        assert_eq!(normalize(&r.events)[rejected_at], "ToolCalled echo");
        assert_eq!(normalize(&r.events)[rejected_at + 1], "ToolReturned Ok");
    }

    #[tokio::test]
    async fn without_synthetic_ok_a_new_call_errors() {
        let (_sid, events) = record_session(vec![]).await;
        // Strip the recorded echo outcome so the double's queue is empty
        // (the chain is broken now, so build doubles directly, no replay).
        let stripped: Vec<Event> = events.iter().filter(|e| !matches!(e.kind, EventKind::ToolReturned { .. })).cloned().collect();
        let d = doubles_from(&stripped, &[EchoTool::new().spec().clone()], false);
        let echo = d.tools.iter().find(|t| t.spec().name == "echo").unwrap();
        let out = echo.call(&serde_json::json!({"text": "x"}), &ToolCtx { session: SessionId("s".into()), artifacts: None }).await;
        assert!(matches!(out, Err(ToolError::Failed { ref detail, .. }) if detail.contains("exhausted")));
        let d = doubles_from(&stripped, &[EchoTool::new().spec().clone()], true);
        let echo = d.tools.iter().find(|t| t.spec().name == "echo").unwrap();
        let out = echo.call(&serde_json::json!({"text": "x"}), &ToolCtx { session: SessionId("s".into()), artifacts: None }).await.unwrap();
        assert!(out.summary.contains("synthetic ok"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-engine replay`
Expected: compile errors (new items missing).

- [ ] **Step 3: Implement** — restructure `replay.rs`:

```rust
use nscore::{LearnedRules, Trust};

pub struct ReplayOptions {
    pub learned: Arc<LearnedRules>,
    pub extra_guards: Vec<Box<dyn Guard>>,
    pub known_specs: Vec<ActionSpec>,
    pub synthetic_ok_for_new_calls: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            learned: Arc::new(LearnedRules::default()),
            extra_guards: vec![],
            known_specs: vec![],
            synthetic_ok_for_new_calls: false,
        }
    }
}

pub struct Replayed {
    pub events: Vec<Event>,
}

pub struct Doubles {
    pub user_inputs: Vec<String>,
    pub proposals: Vec<Proposal>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub replier: Box<dyn Replier>,
}

const SYNTHETIC: [&str; 3] = ["remember_fact", "ask_clarification", "confirm_pending"];

struct ReplayTool {
    spec: ActionSpec,
    outcomes: Mutex<VecDeque<ToolOutcome>>,
    synthetic_ok: bool,
}

impl ReplayTool {
    fn new(spec: ActionSpec, outcomes: VecDeque<ToolOutcome>, synthetic_ok: bool) -> Self {
        Self { spec, outcomes: Mutex::new(outcomes), synthetic_ok }
    }
    fn dummy_spec(action: &str) -> ActionSpec {
        ActionSpec {
            name: action.into(),
            description: format!("replay double for {action}"),
            args_schema: serde_json::json!({"type": "object", "properties": {}}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }
}

#[async_trait]
impl Tool for ReplayTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        match self.outcomes.lock().expect("replay outcomes lock").pop_front() {
            Some(ToolOutcome::Ok { output }) => Ok(output),
            Some(ToolOutcome::Err { kind, detail }) => Err(ToolError::Failed { kind, detail }),
            None if self.synthetic_ok => Ok(ToolOutput {
                summary: "(verified: synthetic ok)".into(),
                artifact: None,
                trust: Trust::System,
            }),
            None => Err(ToolError::Failed { kind: "replay".into(), detail: "outcome queue exhausted".into() }),
        }
    }
}

/// Reconstruct scripted doubles from a recording. Every known spec gets a
/// double carrying the REAL spec (legality/validation match production);
/// recorded actions without a known spec get a permissive dummy spec.
pub fn doubles_from(recorded: &[Event], known_specs: &[ActionSpec], synthetic_ok: bool) -> Doubles {
    let mut user_inputs = Vec::new();
    let mut proposals = Vec::new();
    let mut call_actions: HashMap<u64, String> = HashMap::new();
    let mut outcomes: HashMap<String, VecDeque<ToolOutcome>> = HashMap::new();
    let mut last_settled_per_turn: HashMap<u32, ReplyPolicy> = HashMap::new();
    let mut generated_replies: VecDeque<String> = VecDeque::new();
    for e in recorded {
        match &e.kind {
            EventKind::UserSaid { text } => user_inputs.push(text.clone()),
            EventKind::Proposed { proposal } => proposals.push(proposal.clone()),
            EventKind::ToolCalled { action, .. } => {
                call_actions.insert(e.id.0, action.clone());
            }
            EventKind::ToolReturned { call, outcome } => {
                if let Some(action) = call_actions.get(&call.0) {
                    outcomes.entry(action.clone()).or_default().push_back(outcome.clone());
                }
            }
            EventKind::Settled { policy } => {
                last_settled_per_turn.insert(e.turn, policy.clone());
            }
            EventKind::Replied { text } => {
                if matches!(last_settled_per_turn.get(&e.turn), Some(ReplyPolicy::Generate)) {
                    generated_replies.push_back(text.clone());
                }
            }
            _ => {}
        }
    }
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for spec in known_specs {
        if SYNTHETIC.contains(&spec.name.as_str()) {
            continue;
        }
        let queue = outcomes.remove(&spec.name).unwrap_or_default();
        seen.insert(spec.name.clone());
        tools.push(Arc::new(ReplayTool::new(spec.clone(), queue, synthetic_ok)));
    }
    for (action, queue) in outcomes {
        if SYNTHETIC.contains(&action.as_str()) || seen.contains(&action) {
            continue;
        }
        tools.push(Arc::new(ReplayTool::new(ReplayTool::dummy_spec(&action), queue, synthetic_ok)));
    }
    Doubles {
        user_inputs,
        proposals,
        tools,
        replier: Box::new(QueueReplier { texts: Mutex::new(generated_replies) }),
    }
}

pub async fn replay_with(
    session: SessionId,
    recorded: &[Event],
    opts: ReplayOptions,
) -> Result<Replayed, ReplayError> {
    EventLog::from_events(session.clone(), recorded.to_vec())
        .verify_chain()
        .map_err(|e| ReplayError::ChainBroken(e.to_string()))?;
    let d = doubles_from(recorded, &opts.known_specs, opts.synthetic_ok_for_new_calls);
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(d.proposals)));
    b.set_replier(d.replier);
    b.set_memory(store.clone());
    b.set_channel(Box::new(ReplayChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    for t in d.tools {
        b.add_tool(t);
    }
    for g in opts.extra_guards {
        b.add_guard(g);
    }
    let parts = b.build().map_err(|e| ReplayError::Engine(e.to_string()))?;
    let cfg = EngineConfig {
        learned: Arc::new(arc_swap::ArcSwap::new(opts.learned)),
        ..EngineConfig::default()
    };
    let mut engine = Engine::with_clock(parts, cfg, Box::new(|| Timestamp(0)));
    for text in d.user_inputs {
        engine
            .run_turn(Incoming { session: session.clone(), text })
            .await
            .map_err(|e| ReplayError::Engine(e.to_string()))?;
    }
    let events = store.load(&session).await.map_err(|e| ReplayError::Engine(e.to_string()))?;
    Ok(Replayed { events })
}

pub fn diff(recorded: &[Event], replayed: &[Event]) -> Result<(), ReplayError> {
    let expected = normalize(recorded);
    let got = normalize(replayed);
    for (at, (want, have)) in expected.iter().zip(got.iter()).enumerate() {
        if want != have {
            return Err(ReplayError::Divergence { at, expected: want.clone(), got: have.clone() });
        }
    }
    if expected.len() != got.len() {
        return Err(ReplayError::LengthMismatch { expected: expected.len(), got: got.len() });
    }
    Ok(())
}

pub async fn replay_session(
    session: SessionId,
    recorded: &[Event],
    extra_guards: Vec<Box<dyn Guard>>,
) -> Result<(), ReplayError> {
    let r = replay_with(session, recorded, ReplayOptions { extra_guards, ..Default::default() }).await?;
    diff(recorded, &r.events)
}
```

Delete the old `replay_session` body and the old `ReplayTool::new(action, outcomes)`. Keep `QueueReplier`, `ReplayChannel`, `normalize`, `ReplayError`.

- [ ] **Step 4: Run**

Run: `cargo test -p ns-engine`
Expected: all pass (the three old replay tests plus three new).

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/replay.rs
git commit -m "feat(engine): replay_with — rule-aware replay with real specs and synthetic outcomes"
```

---

### Task 7: Idle-timer driver in `Engine::run`

**Files:**
- Modify: `Cargo.toml` (tokio features += `"time"`), `crates/engine/src/turn.rs` (`EngineConfig.idle_after`, `run`)
- Test: `crates/engine/tests/turn_loop.rs`

**Interfaces:**
- Produces: `EngineConfig.idle_after: Option<std::time::Duration>` (default `None`). In `run`, when set, `channel.recv()` is wrapped in `tokio::time::timeout`; on elapse, if at least one turn ran since the last pass, `self.parts.consolidator.run(&*self.parts.memory).await` runs once (an `Err` is printed to stderr as `evolution pass failed: {e}` and otherwise ignored), then the loop resumes waiting.

- [ ] **Step 1: Add `"time"` to the workspace tokio features and the config field**

```rust
    /// Driver B (spec M5 §5): after this much silence on the channel, run the
    /// consolidator once if any turn ran since the last pass. None = off.
    pub idle_after: Option<std::time::Duration>,
```

Default `None`. Fix the `EngineConfig` literal in `app/src/main.rs` (`idle_after: None` for now).

- [ ] **Step 2: Write the failing test** (append to `turn_loop.rs`)

```rust
enum Step {
    Say(&'static str),
    Idle,
}

/// Channel double: pops one step per recv. `Idle` sleeps long enough for the
/// engine's idle timeout to cancel the recv future (timeouts drop it).
struct ScriptedChannel(std::sync::Mutex<std::collections::VecDeque<Step>>);
#[async_trait::async_trait]
impl Channel for ScriptedChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        let next = self.0.lock().unwrap().pop_front();
        match next {
            Some(Step::Say(t)) => Ok(Incoming { session: SessionId("idle".into()), text: t.into() }),
            Some(Step::Idle) => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Err(ChannelError::Closed)
            }
            None => Err(ChannelError::Closed),
        }
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

/// Consolidator double: counts runs and swaps an alias into the shared handle.
struct SwapIn {
    rules: Arc<nsengine::arc_swap::ArcSwap<LearnedRules>>,
    runs: Arc<std::sync::atomic::AtomicU32>,
}
#[async_trait::async_trait]
impl Consolidator for SwapIn {
    async fn run(&self, _store: &dyn MemoryStore) -> Result<(), StoreError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.rules.store(Arc::new(LearnedRules {
            alias_action: vec![AliasAction { from: "eko".into(), to: "echo".into() }],
            ..Default::default()
        }));
        Ok(())
    }
}

#[tokio::test]
async fn idle_timer_runs_the_consolidator_once_per_quiet_period_with_new_turns() {
    let store = Arc::new(InMemoryStore::new());
    let rules = rules_handle(LearnedRules::default());
    let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut b = HarnessBuilder::new();
    // turn 1: respond directly; turn 2: propose the typo, which only works once the alias is swapped in.
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![
        Proposal { rationale: "".into(), action: "respond_directly".into(), args: serde_json::json!({}) },
        Proposal { rationale: "".into(), action: "eko".into(), args: serde_json::json!({"text": "hi"}) },
    ])));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(ScriptedChannel(std::sync::Mutex::new(
        [Step::Say("one"), Step::Idle, Step::Say("two"), Step::Idle, Step::Idle].into(),
    ))));
    b.set_consolidator(Box::new(SwapIn { rules: rules.clone(), runs: runs.clone() }));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        learned: rules,
        idle_after: Some(std::time::Duration::from_millis(20)),
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(1)));
    e.run().await.unwrap();
    // pass after "one", pass after "two", and NOT a third time (no turn in between).
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    let events = store.load(&SessionId("idle".into())).await.unwrap();
    assert_eq!(tool_calls(&events, "echo"), 1, "turn two saw the swapped-in alias");
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p ns-engine --test turn_loop idle_timer`
Expected: FAIL — the engine blocks on the 5s sleep then returns `Closed` with 0 runs (or times out).

- [ ] **Step 4: Implement `run`**

```rust
    /// Outer loop: recv → run_turn → send, until the channel closes. With
    /// `idle_after` set, a quiet period runs the consolidator once (driver B,
    /// spec M5 §5) — only when at least one turn ran since the last pass, and
    /// never interleaved with a turn (same task).
    pub async fn run(&mut self) -> Result<(), EngineError> {
        let mut turns_since_pass: u32 = 0;
        loop {
            let received = match self.cfg.idle_after {
                Some(d) => tokio::time::timeout(d, self.parts.channel.recv()).await,
                None => Ok(self.parts.channel.recv().await),
            };
            let incoming = match received {
                Ok(Ok(i)) => i,
                Ok(Err(ChannelError::Closed)) => return Ok(()),
                Ok(Err(e)) => return Err(EngineError::Channel(e.to_string())),
                Err(_elapsed) => {
                    if turns_since_pass > 0 {
                        if let Err(e) = self.parts.consolidator.run(&*self.parts.memory).await {
                            eprintln!("evolution pass failed: {e}");
                        }
                        turns_since_pass = 0;
                    }
                    continue;
                }
            };
            let session = incoming.session.clone();
            let text = self.run_turn(incoming).await?;
            turns_since_pass += 1;
            self.parts
                .channel
                .send(&session, &text)
                .await
                .map_err(|e| EngineError::Channel(e.to_string()))?;
        }
    }
```

- [ ] **Step 5: Run workspace tests**

Run: `cargo test --workspace`
Expected: all pass; the idle test finishes in well under a second.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/engine app/src/main.rs
git commit -m "feat(engine): idle-timer driver runs the consolidator during quiet periods"
```

---

### Task 8: `ns-evolution` crate scaffold: atomic files and the ledger

**Files:**
- Modify: `Cargo.toml` (member `crates/evolution`; workspace dep `strsim = "0.11"`)
- Create: `crates/evolution/Cargo.toml`, `crates/evolution/src/lib.rs`, `crates/evolution/src/files.rs`, `crates/evolution/src/ledger.rs`
- Test: inline in `files.rs`, `ledger.rs`

**Interfaces (produces):**

```rust
// files.rs
pub enum FileError { Io(String), Parse(String) }           // thiserror
pub fn load_rules(path: &Path) -> Result<LearnedRules, FileError>     // missing file → Ok(default)
pub fn save_rules_atomic(path: &Path, rules: &LearnedRules) -> Result<(), FileError>
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FileError>   // NamedTempFile in parent dir + persist

// ledger.rs
pub struct Evidence { pub session: String, pub turn: u32, pub event_id: u64 }
pub enum Verdict { Accepted, Rejected, Unverified }         // serde lowercase
pub struct LedgerEntry { pub verdict: Verdict, pub numbers: serde_json::Value, pub evidence: Vec<Evidence>, pub at: u64 }
pub struct Ledger { pub entries: BTreeMap<String, LedgerEntry> }   // key = candidate hash
impl Ledger { pub fn load(path) -> Result<Ledger, FileError>; pub fn save_atomic(&self, path) -> Result<(), FileError>; pub fn settled(&self, hash: &str) -> bool /* accepted or rejected */ }
```

- [ ] **Step 1: Scaffold**

`crates/evolution/Cargo.toml`:

```toml
[package]
name = "ns-evolution"
version = "0.1.0"
edition = "2021"

[lib]
name = "nsevolution"

[dependencies]
ns-core = { path = "../core" }
ns-engine = { path = "../engine" }
ns-llm = { path = "../llm" }
serde = { workspace = true }
serde_json = { workspace = true }
toml = { workspace = true }
tempfile = { workspace = true }
sha2 = { workspace = true }
strsim = { workspace = true }
async-trait = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
arc-swap = { workspace = true }
```

`src/lib.rs`:

```rust
//! Evolution pass (spec docs/superpowers/specs/2026-09-02-evolution-pass-design.md):
//! mine → propose → gate → apply → ledger. Pure library; ns-app drives it.
pub mod files;
pub mod ledger;
```

Add `"crates/evolution"` to workspace members and `strsim = "0.11"` to workspace deps.

- [ ] **Step 2: Failing tests**

`files.rs`:

```rust
use nscore::LearnedRules;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    todo!()
}

pub fn load_rules(path: &Path) -> Result<LearnedRules, FileError> {
    todo!()
}

pub fn save_rules_atomic(path: &Path, rules: &LearnedRules) -> Result<(), FileError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{AliasAction, Note};

    #[test]
    fn missing_file_loads_as_defaults_and_round_trips_after_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("learned.toml");
        assert_eq!(load_rules(&p).unwrap(), LearnedRules::default());
        let rules = LearnedRules {
            alias_action: vec![AliasAction { from: "a".into(), to: "b".into() }],
            notes: vec![Note::new("global", "Be brief.", 0.25)],
            ..Default::default()
        };
        save_rules_atomic(&p, &rules).unwrap();
        assert_eq!(load_rules(&p).unwrap(), rules);
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("learned.toml")]);
    }

    #[test]
    fn unparsable_file_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("learned.toml");
        std::fs::write(&p, "version = \"one\"\n[[alias_action]]\nfrom = 1").unwrap();
        assert!(matches!(load_rules(&p), Err(FileError::Parse(_))));
    }
}
```

`ledger.rs`:

```rust
use crate::files::{write_atomic, FileError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub session: String,
    pub turn: u32,
    pub event_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accepted,
    Rejected,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub verdict: Verdict,
    pub numbers: serde_json::Value,
    pub evidence: Vec<Evidence>,
    /// Unix milliseconds.
    pub at: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub entries: BTreeMap<String, LedgerEntry>,
}

impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger, FileError> {
        todo!()
    }
    pub fn save_atomic(&self, path: &Path) -> Result<(), FileError> {
        todo!()
    }
    /// Accepted or rejected candidates are never re-proposed / re-verified.
    pub fn settled(&self, hash: &str) -> bool {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: Verdict) -> LedgerEntry {
        LedgerEntry { verdict: v, numbers: serde_json::json!({"flipped": 1}), evidence: vec![Evidence { session: "s".into(), turn: 1, event_id: 4 }], at: 7 }
    }

    #[test]
    fn settled_only_for_accepted_or_rejected() {
        let mut l = Ledger::default();
        l.entries.insert("a".into(), entry(Verdict::Accepted));
        l.entries.insert("r".into(), entry(Verdict::Rejected));
        l.entries.insert("u".into(), entry(Verdict::Unverified));
        assert!(l.settled("a") && l.settled("r"));
        assert!(!l.settled("u") && !l.settled("missing"));
    }

    #[test]
    fn round_trips_through_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ledger.json");
        assert_eq!(Ledger::load(&p).unwrap(), Ledger::default());
        let mut l = Ledger::default();
        l.entries.insert("sha256:x".into(), entry(Verdict::Accepted));
        l.save_atomic(&p).unwrap();
        assert_eq!(Ledger::load(&p).unwrap(), l);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"verdict\": \"accepted\""), "{text}");
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p ns-evolution`
Expected: panics on `todo!()`.

- [ ] **Step 4: Implement**

```rust
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    use std::io::Write;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| FileError::Io(e.to_string()))?;
    tmp.write_all(bytes).map_err(|e| FileError::Io(e.to_string()))?;
    tmp.as_file().sync_all().map_err(|e| FileError::Io(e.to_string()))?;
    tmp.persist(path).map_err(|e| FileError::Io(e.error.to_string()))?;
    Ok(())
}

pub fn load_rules(path: &Path) -> Result<LearnedRules, FileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| FileError::Parse(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LearnedRules::default()),
        Err(e) => Err(FileError::Io(e.to_string())),
    }
}

pub fn save_rules_atomic(path: &Path, rules: &LearnedRules) -> Result<(), FileError> {
    let text = toml::to_string_pretty(rules).map_err(|e| FileError::Parse(e.to_string()))?;
    write_atomic(path, text.as_bytes())
}
```

```rust
impl Ledger {
    pub fn load(path: &Path) -> Result<Ledger, FileError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| FileError::Parse(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(FileError::Io(e.to_string())),
        }
    }
    pub fn save_atomic(&self, path: &Path) -> Result<(), FileError> {
        let text = serde_json::to_string_pretty(self).map_err(|e| FileError::Parse(e.to_string()))?;
        write_atomic(path, text.as_bytes())
    }
    pub fn settled(&self, hash: &str) -> bool {
        matches!(self.entries.get(hash).map(|e| e.verdict), Some(Verdict::Accepted | Verdict::Rejected))
    }
}
```

- [ ] **Step 5: Run**

Run: `cargo test -p ns-evolution`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/evolution
git commit -m "feat(evolution): crate scaffold — atomic learned.toml I/O and the verdict ledger"
```

---

### Task 9: Mining failure signatures

**Files:**
- Create: `crates/evolution/src/mine.rs`; add `pub mod mine;` to `lib.rs`
- Test: inline

**Interfaces (produces):**

```rust
pub enum SignatureKind {
    MalformedArg { action: String, arg: String, ops: Vec<Op> },  // a normalize candidate that makes validate_args pass
    IllegalNearTool { proposed: String, candidate: String },
    FallbackReply,
    RepeatedGuardDenial { guard: String, reason: String },
    ToolErrArgs { action: String, detail: String },
    Corrected { text: String },
}
impl SignatureKind { pub fn lane(&self) -> &'static str /* "symbolic" | "note" */; pub fn name(&self) -> &'static str /* variant name for the report */ }
pub struct Signature { pub session: SessionId, pub turn: u32, pub event_id: EventId, pub kind: SignatureKind }
pub fn mine(session: &SessionId, events: &[Event], known_specs: &[ActionSpec]) -> Vec<Signature>
pub fn near_tool(proposed: &str, names: &[String]) -> Option<String>
pub fn normalize_candidate(spec: &ActionSpec, args: &serde_json::Value) -> Option<(String, Vec<Op>)>
pub fn is_fallback(policy: &ReplyPolicy) -> bool
pub fn render_turn(events: &[Event], turn: u32) -> String    // trace text for the note proposer
```

Rules:
- `MalformedArg`: a `Rejected{Malformed}` whose `proposal_of` names a `Proposed` event for an action in `known_specs`, where `normalize_candidate` finds an op sequence (tried in order `[Trim]`, `[Trim, StripPunct]`, `[Trim, StripPunct, Lowercase]`) on exactly one string arg that makes `validate_args` pass.
- `IllegalNearTool`: `Rejected{IllegalAction{action}}` where `near_tool(action, known names + ["respond_directly","ask_clarification","remember_fact"])` is `Some`. `near_tool`: normalize both sides (lowercase, keep only ASCII alphanumerics); exact normalized match wins; else the unique name with `strsim::damerau_levenshtein(a, b) <= 2`; ties → `None`; a candidate equal to the proposed name → `None`.
- `FallbackReply`: `Settled{policy}` with `is_fallback` (Verbatim text starting with `nsengine::turn::FALLBACK_REPLY`, or `Template { id: "cant_help", .. }`). `event_id` = the Settled event.
- `RepeatedGuardDenial`: two or more `Rejected{GuardDenied{guard, reason}}` with equal `(guard, reason)` in the same turn; one signature per (turn, guard, reason), `event_id` = the second occurrence.
- `ToolErrArgs`: `ToolReturned{Err{kind: "bad_args", detail}}`; `action` from the matching `ToolCalled`.
- `Corrected`: every `Corrected{text}` event.

- [ ] **Step 1: Failing tests** (file with `todo!()` bodies; tests build events with `EventLog::append`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn spec(name: &str) -> ActionSpec {
        ActionSpec {
            name: name.into(),
            description: "".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {"unit": {"type": "string", "enum": ["c", "f"]}, "city": {"type": "string"}},
                "required": ["city"]
            }),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    fn log() -> EventLog {
        EventLog::new(SessionId("m".into()))
    }

    #[test]
    fn near_tool_prefers_exact_normalized_match_then_unique_edit_distance() {
        let names: Vec<String> = ["get_time", "get_weather", "wipe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(near_tool("getTime", &names), Some("get_time".into()));
        assert_eq!(near_tool("get_tme", &names), Some("get_time".into()));
        assert_eq!(near_tool("get_time", &names), None, "already legal is not a near miss");
        assert_eq!(near_tool("zzzzzz", &names), None);
        let ambiguous: Vec<String> = ["ab", "ac"].iter().map(|s| s.to_string()).collect();
        assert_eq!(near_tool("ad", &ambiguous), None, "ties yield nothing");
    }

    #[test]
    fn normalize_candidate_finds_the_shortest_op_sequence_that_validates() {
        let s = spec("get_weather");
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": " \"C\" "})),
            Some(("unit".into(), vec![Op::Trim, Op::StripPunct, Op::Lowercase]))
        );
        assert_eq!(normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": "kelvin"})), None);
        assert_eq!(normalize_candidate(&s, &serde_json::json!({"unit": "c"})), None, "missing required cannot be normalized");
    }

    #[test]
    fn mines_malformed_illegal_fallback_repeat_toolerr_and_corrected() {
        let mut l = log();
        // turn 1: malformed unit, then illegal near-miss, then repeated guard denial, fallback.
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "weather".into() });
        let p1 = l.append(1, Timestamp(2), EventKind::Proposed { proposal: Proposal { rationale: "".into(), action: "get_weather".into(), args: serde_json::json!({"city": "Brno", "unit": "Celsius"}) } }).id;
        l.append(1, Timestamp(3), EventKind::Rejected { proposal_of: p1, reason: RejectReason::Malformed { detail: "x".into() } });
        let p2 = l.append(1, Timestamp(4), EventKind::Proposed { proposal: Proposal { rationale: "".into(), action: "getWeather".into(), args: serde_json::json!({}) } }).id;
        l.append(1, Timestamp(5), EventKind::Rejected { proposal_of: p2, reason: RejectReason::IllegalAction { action: "getWeather".into() } });
        for _ in 0..2 {
            let p = l.append(1, Timestamp(6), EventKind::Proposed { proposal: Proposal { rationale: "".into(), action: "wipe".into(), args: serde_json::json!({}) } }).id;
            l.append(1, Timestamp(7), EventKind::Rejected { proposal_of: p, reason: RejectReason::GuardDenied { guard: "taint".into(), reason: "external".into() } });
        }
        l.append(1, Timestamp(8), EventKind::Settled { policy: ReplyPolicy::Verbatim { text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY) } });
        l.append(1, Timestamp(9), EventKind::Replied { text: "…".into() });
        // turn 2: tool error attributable to args, then a correction.
        l.append(2, Timestamp(10), EventKind::UserSaid { text: "again".into() });
        let c = l.append(2, Timestamp(11), EventKind::ToolCalled { action: "get_weather".into(), args: vec![] }).id;
        l.append(2, Timestamp(12), EventKind::ToolReturned { call: c, outcome: ToolOutcome::Err { kind: "bad_args".into(), detail: "city unknown".into() } });
        l.append(2, Timestamp(13), EventKind::Corrected { target: None, text: "user.city = Brno".into() });

        let sigs = mine(&SessionId("m".into()), l.events(), &[spec("get_weather"), spec("wipe")]);
        let names: Vec<&str> = sigs.iter().map(|s| s.kind.name()).collect();
        assert_eq!(names, vec!["MalformedArg", "IllegalNearTool", "RepeatedGuardDenial", "FallbackReply", "ToolErrArgs", "Corrected"]);
        assert!(matches!(&sigs[0].kind, SignatureKind::MalformedArg { action, arg, ops } if action == "get_weather" && arg == "unit" && ops == &vec![Op::Trim, Op::StripPunct, Op::Lowercase]));
        assert!(matches!(&sigs[1].kind, SignatureKind::IllegalNearTool { proposed, candidate } if proposed == "getWeather" && candidate == "get_weather"));
        assert_eq!(sigs[0].turn, 1);
        assert_eq!(sigs[4].turn, 2);
        assert!(sigs.iter().all(|s| s.session.0 == "m"));
    }

    #[test]
    fn render_turn_is_one_line_per_event_of_that_turn() {
        let mut l = log();
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(2, Timestamp(2), EventKind::UserSaid { text: "again".into() });
        l.append(2, Timestamp(3), EventKind::Replied { text: "ok".into() });
        let t = render_turn(l.events(), 2);
        assert_eq!(t, "UserSaid: again\nReplied: ok");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-evolution mine`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
use nscore::{
    validate_args, ActionSpec, Event, EventId, EventKind, Op, Proposal, RejectReason, ReplyPolicy,
    SessionId, ToolOutcome,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum SignatureKind {
    MalformedArg { action: String, arg: String, ops: Vec<Op> },
    IllegalNearTool { proposed: String, candidate: String },
    FallbackReply,
    RepeatedGuardDenial { guard: String, reason: String },
    ToolErrArgs { action: String, detail: String },
    Corrected { text: String },
}

impl SignatureKind {
    pub fn lane(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } | SignatureKind::IllegalNearTool { .. } => "symbolic",
            _ => "note",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } => "MalformedArg",
            SignatureKind::IllegalNearTool { .. } => "IllegalNearTool",
            SignatureKind::FallbackReply => "FallbackReply",
            SignatureKind::RepeatedGuardDenial { .. } => "RepeatedGuardDenial",
            SignatureKind::ToolErrArgs { .. } => "ToolErrArgs",
            SignatureKind::Corrected { .. } => "Corrected",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub session: SessionId,
    pub turn: u32,
    pub event_id: EventId,
    pub kind: SignatureKind,
}

pub const SYNTHETIC_ACTIONS: [&str; 3] = ["respond_directly", "ask_clarification", "remember_fact"];

fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

pub fn near_tool(proposed: &str, names: &[String]) -> Option<String> {
    if names.iter().any(|n| n == proposed) {
        return None;
    }
    let p = squash(proposed);
    if let Some(exact) = names.iter().find(|n| squash(n) == p) {
        return Some(exact.clone());
    }
    let mut best: Vec<(usize, &String)> = names
        .iter()
        .map(|n| (strsim::damerau_levenshtein(&p, &squash(n)), n))
        .filter(|(d, _)| *d <= 2)
        .collect();
    best.sort_by_key(|(d, _)| *d);
    match best.as_slice() {
        [(d0, _), (d1, _), ..] if d0 == d1 => None,
        [(_, n0), ..] => Some((*n0).clone()),
        [] => None,
    }
}

const OP_SEQUENCES: [&[Op]; 3] = [
    &[Op::Trim],
    &[Op::Trim, Op::StripPunct],
    &[Op::Trim, Op::StripPunct, Op::Lowercase],
];

pub fn normalize_candidate(spec: &ActionSpec, args: &serde_json::Value) -> Option<(String, Vec<Op>)> {
    let obj = args.as_object()?;
    if validate_args(&spec.args_schema, args).is_ok() {
        return None;
    }
    for (name, value) in obj {
        let Some(s) = value.as_str() else { continue };
        for ops in OP_SEQUENCES {
            let mut patched = args.clone();
            patched[name] = serde_json::Value::String(nscore::apply_ops(ops, s));
            if validate_args(&spec.args_schema, &patched).is_ok() {
                return Some((name.clone(), ops.to_vec()));
            }
        }
    }
    None
}

pub fn is_fallback(policy: &ReplyPolicy) -> bool {
    match policy {
        ReplyPolicy::Verbatim { text } => text.starts_with(nsengine::turn::FALLBACK_REPLY),
        ReplyPolicy::Template { id, .. } => id == "cant_help",
        ReplyPolicy::Generate => false,
    }
}

pub fn render_turn(events: &[Event], turn: u32) -> String {
    events
        .iter()
        .filter(|e| e.turn == turn)
        .map(|e| match &e.kind {
            EventKind::UserSaid { text } => format!("UserSaid: {text}"),
            EventKind::Proposed { proposal } => format!("Proposed: {} {}", proposal.action, proposal.args),
            EventKind::Rejected { reason, .. } => match reason {
                RejectReason::Malformed { detail } => format!("Rejected: malformed — {detail}"),
                RejectReason::IllegalAction { action } => format!("Rejected: illegal action {action}"),
                RejectReason::GuardDenied { guard, reason } => format!("Rejected: guard {guard} — {reason}"),
            },
            EventKind::ToolCalled { action, .. } => format!("ToolCalled: {action}"),
            EventKind::ToolReturned { outcome, .. } => match outcome {
                ToolOutcome::Ok { output } => format!("ToolReturned: ok — {}", output.summary),
                ToolOutcome::Err { kind, detail } => format!("ToolReturned: err {kind} — {detail}"),
            },
            EventKind::PendingConfirmation { .. } => "PendingConfirmation".into(),
            EventKind::Confirmed { .. } => "Confirmed".into(),
            EventKind::Corrected { text, .. } => format!("Corrected: {text}"),
            EventKind::Settled { policy } => match policy {
                ReplyPolicy::Verbatim { text } => format!("Settled: verbatim — {text}"),
                ReplyPolicy::Template { id, .. } => format!("Settled: template {id}"),
                ReplyPolicy::Generate => "Settled: generate".into(),
            },
            EventKind::Replied { text } => format!("Replied: {text}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn mine(session: &SessionId, events: &[Event], known_specs: &[ActionSpec]) -> Vec<Signature> {
    let specs: HashMap<&str, &ActionSpec> = known_specs.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut legal_names: Vec<String> = known_specs.iter().map(|s| s.name.clone()).collect();
    legal_names.extend(SYNTHETIC_ACTIONS.iter().map(|s| s.to_string()));
    let proposals: HashMap<u64, &Proposal> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => Some((e.id.0, proposal)),
            _ => None,
        })
        .collect();
    let calls: HashMap<u64, &str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCalled { action, .. } => Some((e.id.0, action.as_str())),
            _ => None,
        })
        .collect();
    let mut seen_denials: HashSet<(u32, String, String)> = HashSet::new();
    let mut counted_denials: HashSet<(u32, String, String)> = HashSet::new();
    let mut out = Vec::new();
    let sig = |e: &Event, kind: SignatureKind| Signature { session: session.clone(), turn: e.turn, event_id: e.id, kind };
    for e in events {
        match &e.kind {
            EventKind::Rejected { proposal_of, reason } => match reason {
                RejectReason::Malformed { .. } => {
                    if let Some(p) = proposals.get(&proposal_of.0) {
                        if let Some(spec) = specs.get(p.action.as_str()) {
                            if let Some((arg, ops)) = normalize_candidate(spec, &p.args) {
                                out.push(sig(e, SignatureKind::MalformedArg { action: p.action.clone(), arg, ops }));
                            }
                        }
                    }
                }
                RejectReason::IllegalAction { action } => {
                    if let Some(candidate) = near_tool(action, &legal_names) {
                        out.push(sig(e, SignatureKind::IllegalNearTool { proposed: action.clone(), candidate }));
                    }
                }
                RejectReason::GuardDenied { guard, reason } => {
                    let key = (e.turn, guard.clone(), reason.clone());
                    if !seen_denials.insert(key.clone()) && counted_denials.insert(key) {
                        out.push(sig(e, SignatureKind::RepeatedGuardDenial { guard: guard.clone(), reason: reason.clone() }));
                    }
                }
            },
            EventKind::Settled { policy } if is_fallback(policy) => out.push(sig(e, SignatureKind::FallbackReply)),
            EventKind::ToolReturned { call, outcome: ToolOutcome::Err { kind, detail } } if kind == "bad_args" => {
                let action = calls.get(&call.0).unwrap_or(&"?").to_string();
                out.push(sig(e, SignatureKind::ToolErrArgs { action, detail: detail.clone() }));
            }
            EventKind::Corrected { text, .. } => out.push(sig(e, SignatureKind::Corrected { text: text.clone() })),
            _ => {}
        }
    }
    out
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p ns-evolution mine`
Expected: 4 passed. (If `near_tool` tie handling reads awkwardly, keep the behavior: equal best distances → `None`.)

- [ ] **Step 5: Commit**

```bash
git add crates/evolution
git commit -m "feat(evolution): mine failure signatures from session logs"
```

---

### Task 10: Symbolic lane — propose patches and gate them by replay

**Files:**
- Create: `crates/evolution/src/symbolic.rs`; add `pub mod symbolic;`
- Test: inline (records sessions with the engine like `replay.rs` tests do)

**Interfaces (produces):**

```rust
pub enum Patch { NormalizeArg(NormalizeArg), AliasAction(AliasAction) }
impl Patch {
    pub fn hash(&self) -> String;                        // "sha256:" + hex(sha256(canonical json))
    pub fn apply_to(&self, rules: &mut LearnedRules);    // push unless an equal rule exists
    pub fn expected_call(&self) -> &str;                 // action name a flipped line must show
    pub fn summary(&self) -> String;                     // one line for the report
}
pub fn propose_patches(sigs: &[Signature]) -> Vec<(Patch, Vec<Evidence>)>     // symbolic-lane sigs → deduped by hash, evidence merged, input order kept
pub struct SymbolicVerdict { pub accepted: bool, pub flipped: usize, pub not_flipped: usize, pub regressions: usize, pub skipped_baseline: usize, pub detail: String }
pub type Recorded = (SessionId, Vec<Event>);
pub async fn verify_patch(patch: &Patch, evidence: &[Evidence], sessions: &[Recorded], base: &LearnedRules, known_specs: &[ActionSpec], regression_cap: usize) -> SymbolicVerdict
```

`verify_patch`:
1. `candidate = base.clone(); patch.apply_to(&mut candidate)`.
2. Flip: for each evidence item, find its session; `idx` = position of the event with `event_id` in the recording; replay with `candidate`, `known_specs`, `synthetic_ok_for_new_calls: true`; flipped iff `normalize(replayed)[idx] == format!("ToolCalled {}", patch.expected_call())`. Replay errors count as not flipped.
3. Regression: sessions not in evidence, in input order, at most `regression_cap`; replay with `base` (synthetic ok false) and `diff` → if it diverges, `skipped_baseline += 1` and skip; else replay with `candidate` and `diff` → divergence → `regressions += 1`.
4. `accepted = flipped >= 1 && regressions == 0`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mine::{mine, Signature, SignatureKind};
    use nscore::*;
    use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
    use nsengine::store::{InMemoryStore, NoopConsolidator};
    use nsengine::turn::{Engine, EngineConfig};
    use std::sync::Arc;

    struct Closed;
    #[async_trait::async_trait]
    impl Channel for Closed {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> { Err(ChannelError::Closed) }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> { Ok(()) }
    }

    /// Record one session: each proposal list entry is one turn's first proposal.
    async fn record(name: &str, turns: Vec<Proposal>, guards: Vec<Box<dyn Guard>>) -> Recorded {
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId(name.into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(turns.clone())));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(Closed));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        for g in guards { b.add_guard(g); }
        let mut e = Engine::with_clock(b.build().unwrap(), EngineConfig::default(), Box::new(|| Timestamp(1)));
        for i in 0..turns.len() {
            e.run_turn(Incoming { session: sid.clone(), text: format!("turn {i}") }).await.unwrap();
        }
        (sid, store.load(&SessionId(name.into())).await.unwrap())
    }

    fn typo() -> Proposal { Proposal { rationale: "".into(), action: "eko".into(), args: serde_json::json!({"text": "hi"}) } }
    fn good() -> Proposal { Proposal { rationale: "".into(), action: "echo".into(), args: serde_json::json!({"text": "hi"}) } }
    fn specs() -> Vec<ActionSpec> { vec![EchoTool::new().spec().clone()] }

    #[test]
    fn propose_dedupes_by_hash_and_merges_evidence() {
        let sig = |id: u64| Signature { session: SessionId("s".into()), turn: 1, event_id: EventId(id), kind: SignatureKind::IllegalNearTool { proposed: "eko".into(), candidate: "echo".into() } };
        let note = Signature { session: SessionId("s".into()), turn: 1, event_id: EventId(9), kind: SignatureKind::FallbackReply };
        let out = propose_patches(&[sig(3), note, sig(7)]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].0, Patch::AliasAction(a) if a.from == "eko" && a.to == "echo"));
        assert_eq!(out[0].1.iter().map(|e| e.event_id).collect::<Vec<_>>(), vec![3, 7]);
        assert!(out[0].0.hash().starts_with("sha256:"));
    }

    #[tokio::test]
    async fn alias_patch_flips_its_evidence_and_passes_a_clean_regression_set() {
        let bad = record("bad", vec![typo()], vec![]).await;
        let clean = record("clean", vec![good()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(&patch, &evidence, &[bad.clone(), clean.clone()], &LearnedRules::default(), &specs(), 200).await;
        assert!(v.accepted, "{}", v.detail);
        assert_eq!((v.flipped, v.regressions, v.skipped_baseline), (1, 0, 0));
    }

    #[tokio::test]
    async fn a_patch_that_changes_another_recording_is_rejected() {
        // "eko" was recorded as illegal in BOTH sessions, but the second one
        // is not evidence (we pass only the first as evidence): aliasing it
        // changes that recording → regression.
        let bad = record("bad", vec![typo()], vec![]).await;
        let other = record("other", vec![typo()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(&patch, &evidence, &[bad.clone(), other.clone()], &LearnedRules::default(), &specs(), 200).await;
        assert!(!v.accepted);
        assert_eq!(v.regressions, 1);
    }

    #[tokio::test]
    async fn a_patch_that_does_not_flip_its_evidence_is_rejected() {
        // With no known specs the alias target "echo" has no double in replay,
        // so the rewritten proposal is still illegal: the line does not flip.
        let bad = record("bad", vec![typo()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(&patch, &evidence, &[bad.clone()], &LearnedRules::default(), &[], 200).await;
        assert!(!v.accepted);
        assert_eq!((v.flipped, v.not_flipped), (0, 1));
    }

    #[tokio::test]
    async fn composition_verifies_against_base_plus_patch() {
        // Base already aliases eko→echo; a second, identical patch flips nothing new but must not regress.
        let bad = record("bad", vec![typo()], vec![]).await;
        let base = LearnedRules { alias_action: vec![AliasAction { from: "eko".into(), to: "echo".into() }], ..Default::default() };
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(&patch, &evidence, &[bad.clone()], &base, &specs(), 200).await;
        assert_eq!(v.flipped, 1);
        assert_eq!(v.regressions, 0);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-evolution symbolic`
Expected: compile errors / `todo!()` panics.

- [ ] **Step 3: Implement**

```rust
use crate::ledger::Evidence;
use crate::mine::{Signature, SignatureKind};
use nscore::{ActionSpec, AliasAction, Event, LearnedRules, NormalizeArg, SessionId};
use nsengine::replay::{diff, normalize, replay_with, ReplayOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Patch {
    NormalizeArg(NormalizeArg),
    AliasAction(AliasAction),
}

impl Patch {
    pub fn hash(&self) -> String {
        let json = serde_json::to_string(self).expect("patch serializes");
        format!("sha256:{:x}", Sha256::digest(json.as_bytes()))
    }
    pub fn apply_to(&self, rules: &mut LearnedRules) {
        match self {
            Patch::NormalizeArg(n) => {
                if !rules.normalize_arg.contains(n) {
                    rules.normalize_arg.push(n.clone());
                }
            }
            Patch::AliasAction(a) => {
                if !rules.alias_action.contains(a) {
                    rules.alias_action.push(a.clone());
                }
            }
        }
    }
    pub fn expected_call(&self) -> &str {
        match self {
            Patch::NormalizeArg(n) => &n.action,
            Patch::AliasAction(a) => &a.to,
        }
    }
    pub fn summary(&self) -> String {
        match self {
            Patch::NormalizeArg(n) => format!(
                "normalize_arg {}.{} [{}]",
                n.action,
                n.arg,
                n.ops.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", ")
            ),
            Patch::AliasAction(a) => format!("alias_action {} -> {}", a.from, a.to),
        }
    }
}

pub fn propose_patches(sigs: &[Signature]) -> Vec<(Patch, Vec<Evidence>)> {
    let mut out: Vec<(Patch, Vec<Evidence>)> = Vec::new();
    for s in sigs {
        let patch = match &s.kind {
            SignatureKind::MalformedArg { action, arg, ops } => {
                Patch::NormalizeArg(NormalizeArg { action: action.clone(), arg: arg.clone(), ops: ops.clone() })
            }
            SignatureKind::IllegalNearTool { proposed, candidate } => {
                Patch::AliasAction(AliasAction { from: proposed.clone(), to: candidate.clone() })
            }
            _ => continue,
        };
        let ev = Evidence { session: s.session.0.clone(), turn: s.turn, event_id: s.event_id.0 };
        match out.iter_mut().find(|(p, _)| *p == patch) {
            Some((_, evs)) => evs.push(ev),
            None => out.push((patch, vec![ev])),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolicVerdict {
    pub accepted: bool,
    pub flipped: usize,
    pub not_flipped: usize,
    pub regressions: usize,
    pub skipped_baseline: usize,
    pub detail: String,
}

pub type Recorded = (SessionId, Vec<Event>);

fn opts(rules: &LearnedRules, specs: &[ActionSpec], synthetic_ok: bool) -> ReplayOptions {
    ReplayOptions {
        learned: Arc::new(rules.clone()),
        extra_guards: vec![],
        known_specs: specs.to_vec(),
        synthetic_ok_for_new_calls: synthetic_ok,
    }
}

pub async fn verify_patch(
    patch: &Patch,
    evidence: &[Evidence],
    sessions: &[Recorded],
    base: &LearnedRules,
    known_specs: &[ActionSpec],
    regression_cap: usize,
) -> SymbolicVerdict {
    let mut candidate = base.clone();
    patch.apply_to(&mut candidate);
    let mut v = SymbolicVerdict { accepted: false, flipped: 0, not_flipped: 0, regressions: 0, skipped_baseline: 0, detail: String::new() };
    let want = format!("ToolCalled {}", patch.expected_call());

    // 1. flip check on every evidence pointer
    for ev in evidence {
        let Some((sid, recorded)) = sessions.iter().find(|(s, _)| s.0 == ev.session) else {
            v.not_flipped += 1;
            v.detail.push_str(&format!("evidence session {} not loaded; ", ev.session));
            continue;
        };
        let Some(idx) = recorded.iter().position(|e| e.id.0 == ev.event_id) else {
            v.not_flipped += 1;
            continue;
        };
        match replay_with(sid.clone(), recorded, opts(&candidate, known_specs, true)).await {
            Ok(r) => {
                let lines = normalize(&r.events);
                if lines.get(idx).map(|l| l == &want).unwrap_or(false) {
                    v.flipped += 1;
                } else {
                    v.not_flipped += 1;
                    v.detail.push_str(&format!("{}@{}: {} (wanted {want}); ", ev.session, idx, lines.get(idx).cloned().unwrap_or_default()));
                }
            }
            Err(e) => {
                v.not_flipped += 1;
                v.detail.push_str(&format!("{}: replay error {e}; ", ev.session));
            }
        }
    }

    // 2. regression check on every other session, baseline-clean ones only
    let evidence_sessions: std::collections::HashSet<&str> = evidence.iter().map(|e| e.session.as_str()).collect();
    for (sid, recorded) in sessions.iter().filter(|(s, _)| !evidence_sessions.contains(s.0.as_str())).take(regression_cap) {
        let baseline = match replay_with(sid.clone(), recorded, opts(base, known_specs, false)).await {
            Ok(r) => diff(recorded, &r.events).is_ok(),
            Err(_) => false,
        };
        if !baseline {
            v.skipped_baseline += 1;
            continue;
        }
        let same = match replay_with(sid.clone(), recorded, opts(&candidate, known_specs, false)).await {
            Ok(r) => diff(recorded, &r.events).is_ok(),
            Err(_) => false,
        };
        if !same {
            v.regressions += 1;
            v.detail.push_str(&format!("regression in {}; ", sid.0));
        }
    }

    v.accepted = v.flipped >= 1 && v.regressions == 0;
    v
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p ns-evolution symbolic`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/evolution
git commit -m "feat(evolution): symbolic lane — patch proposals gated by flip + zero-regression replay"
```

---

### Task 11: Notes lane — proposer, probe runner, and the GRASP-style gate

**Files:**
- Create: `crates/evolution/src/notes.rs`; add `pub mod notes;`
- Test: inline (scripted proposer and probe doubles; `LiveProbe` gets one test with a scripted emitter)

**Interfaces (produces):**

```rust
#[async_trait] pub trait NoteProposer: Send + Sync {
    /// One imperative sentence that would have changed the failing turn, or None if nothing helps.
    async fn propose(&self, trace: &str, existing: &[Note]) -> Result<Option<Note>, String>;
}
pub struct ClientNoteProposer { pub client: nsllm::client::OpenRouterClient, pub model: String }   // impl NoteProposer via chat()

#[derive(Clone, Copy, PartialEq, Debug)] pub enum TurnOutcome { Ok, Fallback, Rejections(u32) }
pub fn classify_turns(events: &[Event]) -> Vec<TurnOutcome>       // one per turn, in turn order
pub fn score(o: TurnOutcome) -> i64                                // Ok=0, Rejections(n)=-(n), Fallback=-1000

#[async_trait] pub trait ProbeRunner: Send + Sync {
    /// Re-run a recorded session's user inputs through a LIVE emitter with recorded tool outcomes replayed.
    async fn run(&self, recorded: &[Event], rules: Arc<LearnedRules>) -> Result<Vec<TurnOutcome>, String>;
}
pub type EmitterFactory = Arc<dyn Fn() -> Box<dyn Emitter> + Send + Sync>;
pub struct LiveProbe { pub emitter: EmitterFactory, pub known_specs: Vec<ActionSpec>, pub persona: String }   // impl ProbeRunner

pub struct NoteVerdict { pub accepted: bool, pub improved: usize, pub regressed: usize, pub lift: f64, pub turns_used: u32, pub detail: String }
pub async fn verify_note(note: &Note, positives: &[Vec<Event>], negatives: &[Vec<Event>], base: &LearnedRules, probe: &dyn ProbeRunner, regression_budget: u32, budget: &mut u32) -> NoteVerdict
```

`verify_note`: `with = base + note`. For each positive (then each negative): if `budget < 2 × turns` → stop probing (remaining sessions unprobed, `detail` says `budget exhausted`); else `budget -= 2 × turns`, run without and with, compare `sum(score)`; a positive with `with > without` is `improved`; a negative with `with < without` is `regressed`; a probe error counts as neither and is noted in `detail`. `accepted = improved >= 1 && regressed <= regression_budget`. `lift = (improved − regressed) / positives_probed` (0.0 when none probed). `turns_used` is what was subtracted from `budget`.

`LiveProbe::run`: `doubles_from(recorded, &known_specs, true)` for tools; emitter from the factory; `ScriptedReplier` (the reply text is irrelevant to classification); engine with `EngineConfig { learned: ArcSwap::new(rules), persona, ..default }` over an `InMemoryStore`; feed `user_inputs`; `classify_turns(store.load(...))`.

`classify_turns`: group by `turn` (skip turn 0 if none); per turn: `Fallback` if any `Settled` is `is_fallback`; else `Rejections(n)` counting `Rejected` events if n > 0; else `Ok`.

`ClientNoteProposer::propose` request (same wire format as `CloudEmitter`):

```json
{"model": <model>, "max_tokens": 300, "temperature": 0,
 "messages": [
   {"role": "system", "content": "You tune the action-selection prompt of a tool-using assistant. You see the trace of ONE failed turn and the guidance notes already in force. Reply with JSON only: {\"scope\": \"global\" | \"action:<tool name>\", \"text\": \"<one imperative sentence>\"} if a single new sentence would have changed the outcome and does not repeat an existing note; otherwise {\"none\": true}."},
   {"role": "user", "content": "Existing notes:\n- ...\n\nFailed turn:\n<trace>"}
 ]}
```

Parse `choices[0].message.content` (strip a ```json fence if present) → `{"none": true}` → `Ok(None)`; else `Note::new(scope, text, 0.0)` where `scope` must be `global` or start with `action:` and `text` must be 1–200 chars, else `Err`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;
    use std::sync::Mutex;

    fn turn_log(turns: &[&[EventKind]]) -> Vec<Event> {
        let mut l = EventLog::new(SessionId("p".into()));
        for (i, kinds) in turns.iter().enumerate() {
            for k in kinds.iter() {
                l.append(i as u32 + 1, Timestamp(1), k.clone());
            }
        }
        l.events().to_vec()
    }

    #[test]
    fn classify_turns_orders_fallback_below_rejections_below_ok() {
        let fb = EventKind::Settled { policy: ReplyPolicy::Verbatim { text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY) } };
        let rej = EventKind::Rejected { proposal_of: EventId(1), reason: RejectReason::IllegalAction { action: "x".into() } };
        let ok = EventKind::Settled { policy: ReplyPolicy::Generate };
        let ev = turn_log(&[&[EventKind::UserSaid { text: "a".into() }, ok.clone()], &[EventKind::UserSaid { text: "b".into() }, rej.clone(), rej.clone(), ok.clone()], &[EventKind::UserSaid { text: "c".into() }, rej, fb]]);
        assert_eq!(classify_turns(&ev), vec![TurnOutcome::Ok, TurnOutcome::Rejections(2), TurnOutcome::Fallback]);
        assert!(score(TurnOutcome::Ok) > score(TurnOutcome::Rejections(1)));
        assert!(score(TurnOutcome::Rejections(1)) > score(TurnOutcome::Rejections(3)));
        assert!(score(TurnOutcome::Rejections(3)) > score(TurnOutcome::Fallback));
    }

    /// Probe double: outcome depends on whether the rules carry a note containing MARKER.
    struct MarkerProbe { with_marker: Vec<TurnOutcome>, without: Vec<TurnOutcome>, negatives_regress: bool }
    const MARKER: &str = "CALL THE TOOL";
    #[async_trait::async_trait]
    impl ProbeRunner for MarkerProbe {
        async fn run(&self, recorded: &[Event], rules: Arc<LearnedRules>) -> Result<Vec<TurnOutcome>, String> {
            let has = rules.notes.iter().any(|n| n.text.contains(MARKER));
            let is_negative = recorded.iter().any(|e| matches!(&e.kind, EventKind::UserSaid { text } if text == "negative"));
            Ok(if is_negative {
                if has && self.negatives_regress { vec![TurnOutcome::Fallback] } else { vec![TurnOutcome::Ok] }
            } else if has { self.with_marker.clone() } else { self.without.clone() })
        }
    }

    fn session(text: &str) -> Vec<Event> {
        turn_log(&[&[EventKind::UserSaid { text: text.into() }, EventKind::Settled { policy: ReplyPolicy::Generate }]])
    }

    #[tokio::test]
    async fn note_that_helps_a_positive_and_hurts_no_negative_is_accepted_with_lift() {
        let probe = MarkerProbe { with_marker: vec![TurnOutcome::Ok], without: vec![TurnOutcome::Fallback], negatives_regress: false };
        let note = Note::new("global", &format!("{MARKER} before answering."), 0.0);
        let mut budget = 40;
        let v = verify_note(&note, &[session("positive")], &[session("negative")], &LearnedRules::default(), &probe, 0, &mut budget).await;
        assert!(v.accepted, "{}", v.detail);
        assert_eq!((v.improved, v.regressed), (1, 0));
        assert!((v.lift - 1.0).abs() < 1e-9);
        assert_eq!(v.turns_used, 4);
        assert_eq!(budget, 36);
    }

    #[tokio::test]
    async fn note_that_breaks_a_negative_is_rejected_under_budget_zero_and_accepted_under_one() {
        let probe = MarkerProbe { with_marker: vec![TurnOutcome::Ok], without: vec![TurnOutcome::Fallback], negatives_regress: true };
        let note = Note::new("global", &format!("{MARKER} always."), 0.0);
        let mut b = 40;
        let v0 = verify_note(&note, &[session("positive")], &[session("negative")], &LearnedRules::default(), &probe, 0, &mut b).await;
        assert!(!v0.accepted && v0.regressed == 1);
        let mut b = 40;
        let v1 = verify_note(&note, &[session("positive")], &[session("negative")], &LearnedRules::default(), &probe, 1, &mut b).await;
        assert!(v1.accepted);
        assert!((v1.lift - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn probe_budget_stops_probing_and_leaves_the_note_unaccepted() {
        let probe = MarkerProbe { with_marker: vec![TurnOutcome::Ok], without: vec![TurnOutcome::Fallback], negatives_regress: false };
        let note = Note::new("global", &format!("{MARKER}."), 0.0);
        let mut budget = 1;
        let v = verify_note(&note, &[session("positive")], &[], &LearnedRules::default(), &probe, 0, &mut budget).await;
        assert!(!v.accepted);
        assert!(v.detail.contains("budget exhausted"));
        assert_eq!(budget, 1);
    }

    /// Emitter double for LiveProbe: with MARKER in guidance it calls echo once
    /// then responds directly; without it, it proposes an illegal action every
    /// time (→ max_iterations → fallback). Fresh per factory call, so the
    /// "called once" flag is per probe run.
    struct GuidedEmitter(std::sync::atomic::AtomicBool);
    #[async_trait::async_trait]
    impl Emitter for GuidedEmitter {
        async fn propose(&self, ctx: EmitterContext, _l: &LegalActionSet) -> Result<Proposal, EmitError> {
            let guided = ctx.guidance.iter().any(|g| g.contains(MARKER));
            let action = if !guided {
                "nope"
            } else if !self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                "echo"
            } else {
                "respond_directly"
            };
            Ok(Proposal { rationale: "".into(), action: action.into(), args: serde_json::json!({"text": "hi"}) })
        }
    }

    #[tokio::test]
    async fn live_probe_runs_user_inputs_through_the_factory_emitter_with_rules() {
        let recorded = turn_log(&[&[EventKind::UserSaid { text: "say hi".into() }, EventKind::Settled { policy: ReplyPolicy::Generate }]]);
        let probe = LiveProbe {
            emitter: Arc::new(|| Box::new(GuidedEmitter(Default::default())) as Box<dyn Emitter>),
            known_specs: vec![nsengine::script::EchoTool::new().spec().clone()],
            persona: String::new(),
        };
        let without = probe.run(&recorded, Arc::new(LearnedRules::default())).await.unwrap();
        let with = probe.run(&recorded, Arc::new(LearnedRules { notes: vec![Note::new("global", MARKER, 0.0)], ..Default::default() })).await.unwrap();
        assert_eq!(without[0], TurnOutcome::Fallback);
        assert_eq!(with[0], TurnOutcome::Ok);
    }

    #[tokio::test]
    async fn client_proposer_parses_json_and_none() {
        use nsllm::transport::{HttpResponse, MockTransport};
        let reply = |content: &str| HttpResponse { status: 200, body: serde_json::json!({"choices": [{"message": {"content": content}}]}) };
        let mock = Arc::new(MockTransport::new(vec![
            Ok(reply("```json\n{\"scope\": \"action:echo\", \"text\": \"Call echo when asked to repeat.\"}\n```")),
            Ok(reply("{\"none\": true}")),
            Ok(reply("{\"scope\": \"bogus\", \"text\": \"x\"}")),
        ]));
        let p = ClientNoteProposer { client: nsllm::client::OpenRouterClient::new(mock.clone(), "k".into()), model: "m".into() };
        let n = p.propose("trace", &[]).await.unwrap().unwrap();
        assert_eq!((n.scope.as_str(), n.text.as_str()), ("action:echo", "Call echo when asked to repeat."));
        assert_eq!(p.propose("trace", &[]).await.unwrap(), None);
        assert!(p.propose("trace", &[]).await.is_err());
        let sent = &mock.requests()[0].body;
        assert!(sent["messages"][1]["content"].as_str().unwrap().contains("Failed turn:\ntrace"));
    }
}
```

(Use the exact `MockTransport` / `HttpResponse` constructor shapes from `crates/llm/src/emitter.rs` tests; if `MockTransport` is `#[cfg(test)]`-only in `ns-llm`, move it behind a `pub mod testing` gated by a `testing` feature or make it unconditional `pub` — do the smallest change that lets `ns-evolution` tests use it, and note it in the commit message.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-evolution notes`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
use crate::mine::is_fallback;
use async_trait::async_trait;
use nscore::{
    ActionSpec, ChannelError, Emitter, Event, EventKind, HarnessBuilder, Incoming, LearnedRules,
    Note, SessionId, Timestamp,
};
use nsengine::replay::doubles_from;
use nsengine::script::ScriptedReplier;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
use std::collections::BTreeMap;
use std::sync::Arc;

#[async_trait]
pub trait NoteProposer: Send + Sync {
    async fn propose(&self, trace: &str, existing: &[Note]) -> Result<Option<Note>, String>;
}

pub struct ClientNoteProposer {
    pub client: nsllm::client::OpenRouterClient,
    pub model: String,
}

const PROPOSER_SYSTEM: &str = "You tune the action-selection prompt of a tool-using assistant. You see the trace of ONE failed turn and the guidance notes already in force. Reply with JSON only: {\"scope\": \"global\" | \"action:<tool name>\", \"text\": \"<one imperative sentence>\"} if a single new sentence would have changed the outcome and does not repeat an existing note; otherwise {\"none\": true}.";

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

#[async_trait]
impl NoteProposer for ClientNoteProposer {
    async fn propose(&self, trace: &str, existing: &[Note]) -> Result<Option<Note>, String> {
        let mut user = String::from("Existing notes:\n");
        for n in existing {
            user.push_str(&format!("- [{}] {}\n", n.scope, n.text));
        }
        user.push_str("\nFailed turn:\n");
        user.push_str(trace);
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": 300,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": PROPOSER_SYSTEM},
                {"role": "user", "content": user},
            ],
        });
        let body = self.client.chat(request).await.map_err(|e| e.to_string())?;
        let content = body["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
        let v: serde_json::Value = serde_json::from_str(strip_fence(&content)).map_err(|e| format!("proposer returned non-JSON: {e}: {content}"))?;
        if v.get("none").and_then(|b| b.as_bool()) == Some(true) {
            return Ok(None);
        }
        let scope = v["scope"].as_str().unwrap_or("").to_string();
        let text = v["text"].as_str().unwrap_or("").trim().to_string();
        let scope_ok = scope == "global" || scope.strip_prefix("action:").map(|n| !n.is_empty()).unwrap_or(false);
        if !scope_ok || text.is_empty() || text.chars().count() > 200 {
            return Err(format!("proposer returned an invalid note: {v}"));
        }
        Ok(Some(Note::new(&scope, &text, 0.0)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Ok,
    Fallback,
    Rejections(u32),
}

pub fn score(o: TurnOutcome) -> i64 {
    match o {
        TurnOutcome::Ok => 0,
        TurnOutcome::Rejections(n) => -(n as i64),
        TurnOutcome::Fallback => -1000,
    }
}

pub fn classify_turns(events: &[Event]) -> Vec<TurnOutcome> {
    let mut per_turn: BTreeMap<u32, (bool, u32)> = BTreeMap::new();
    for e in events {
        let entry = per_turn.entry(e.turn).or_insert((false, 0));
        match &e.kind {
            EventKind::Settled { policy } if is_fallback(policy) => entry.0 = true,
            EventKind::Rejected { .. } => entry.1 += 1,
            _ => {}
        }
    }
    per_turn
        .into_values()
        .map(|(fallback, rejections)| {
            if fallback {
                TurnOutcome::Fallback
            } else if rejections > 0 {
                TurnOutcome::Rejections(rejections)
            } else {
                TurnOutcome::Ok
            }
        })
        .collect()
}

#[async_trait]
pub trait ProbeRunner: Send + Sync {
    async fn run(&self, recorded: &[Event], rules: Arc<LearnedRules>) -> Result<Vec<TurnOutcome>, String>;
}

pub type EmitterFactory = Arc<dyn Fn() -> Box<dyn Emitter> + Send + Sync>;

pub struct LiveProbe {
    pub emitter: EmitterFactory,
    pub known_specs: Vec<ActionSpec>,
    pub persona: String,
}

struct ClosedChannel;
#[async_trait]
impl nscore::Channel for ClosedChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

#[async_trait]
impl ProbeRunner for LiveProbe {
    async fn run(&self, recorded: &[Event], rules: Arc<LearnedRules>) -> Result<Vec<TurnOutcome>, String> {
        let d = doubles_from(recorded, &self.known_specs, true);
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("probe".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter((self.emitter)());
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        for t in d.tools {
            b.add_tool(t);
        }
        let parts = b.build().map_err(|e| e.to_string())?;
        let cfg = EngineConfig {
            persona: self.persona.clone(),
            learned: Arc::new(arc_swap::ArcSwap::new(rules)),
            ..EngineConfig::default()
        };
        let mut engine = Engine::with_clock(parts, cfg, Box::new(|| Timestamp(0)));
        for text in d.user_inputs {
            engine.run_turn(Incoming { session: sid.clone(), text }).await.map_err(|e| e.to_string())?;
        }
        let events = store.load(&sid).await.map_err(|e| e.to_string())?;
        Ok(classify_turns(&events))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteVerdict {
    pub accepted: bool,
    pub improved: usize,
    pub regressed: usize,
    pub lift: f64,
    pub turns_used: u32,
    pub detail: String,
}

fn turns_in(events: &[Event]) -> u32 {
    events.iter().filter(|e| matches!(e.kind, EventKind::UserSaid { .. })).count() as u32
}

pub async fn verify_note(
    note: &Note,
    positives: &[Vec<Event>],
    negatives: &[Vec<Event>],
    base: &LearnedRules,
    probe: &dyn ProbeRunner,
    regression_budget: u32,
    budget: &mut u32,
) -> NoteVerdict {
    let without = Arc::new(base.clone());
    let mut with_rules = base.clone();
    with_rules.notes.push(note.clone());
    let with = Arc::new(with_rules);
    let mut v = NoteVerdict { accepted: false, improved: 0, regressed: 0, lift: 0.0, turns_used: 0, detail: String::new() };
    let mut positives_probed = 0usize;

    let mut probe_pair = |events: &Vec<Event>| -> Option<u32> {
        let cost = 2 * turns_in(events);
        if cost == 0 || *budget < cost {
            return None;
        }
        *budget -= cost;
        Some(cost)
    };

    for (is_positive, events) in positives.iter().map(|e| (true, e)).chain(negatives.iter().map(|e| (false, e))) {
        let Some(cost) = probe_pair(events) else {
            v.detail.push_str("budget exhausted; ");
            break;
        };
        v.turns_used += cost;
        let a = probe.run(events, without.clone()).await;
        let b = probe.run(events, with.clone()).await;
        match (a, b) {
            (Ok(a), Ok(b)) => {
                let sa: i64 = a.iter().copied().map(score).sum();
                let sb: i64 = b.iter().copied().map(score).sum();
                if is_positive {
                    positives_probed += 1;
                    if sb > sa {
                        v.improved += 1;
                    }
                } else if sb < sa {
                    v.regressed += 1;
                }
            }
            (Err(e), _) | (_, Err(e)) => v.detail.push_str(&format!("probe error: {e}; ")),
        }
    }
    v.accepted = v.improved >= 1 && v.regressed <= regression_budget;
    v.lift = if positives_probed == 0 { 0.0 } else { (v.improved as f64 - v.regressed as f64) / positives_probed as f64 };
    v
}
```

- [ ] **Step 4: Run**

Run: `cargo test -p ns-evolution notes`
Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/evolution crates/llm
git commit -m "feat(evolution): notes lane — LLM proposer, live probe runner, regression-budgeted gate"
```

---

### Task 12: `EvolutionPass` — the `Consolidator` that runs the whole pipeline

**Files:**
- Create: `crates/evolution/src/pass.rs`; add `pub mod pass;`
- Test: inline (in-memory store, scripted proposer/probe doubles)

**Interfaces (produces):**

```rust
pub struct PassConfig { pub regression_budget: u32, pub probe_budget_turns: u32, pub max_notes: usize, pub regression_replay_cap: usize, pub dry_run: bool }
impl Default for PassConfig  // 0, 40, 20, 200, false
pub struct EvolutionPass {
    rules: Arc<ArcSwap<LearnedRules>>, known_specs: Vec<ActionSpec>,
    probe: Option<Box<dyn ProbeRunner>>, proposer: Option<Box<dyn NoteProposer>>,
    learned_path: PathBuf, ledger_path: PathBuf, cfg: PassConfig,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}
impl EvolutionPass { pub fn new(rules, known_specs, learned_path, ledger_path, cfg) -> Self; pub fn with_notes(self, probe: Box<dyn ProbeRunner>, proposer: Box<dyn NoteProposer>) -> Self; pub fn with_clock(self, clock) -> Self; pub async fn run_report(&self, store: &dyn MemoryStore) -> Result<Report, PassError> }
impl Consolidator for EvolutionPass   // run_report → Ok(()), errors mapped to StoreError::Io
pub enum PassError { Store(StoreError), File(FileError) }   // thiserror, From impls
pub struct CandidateReport { pub hash: String, pub lane: &'static str, pub summary: String, pub verdict: Verdict, pub numbers: serde_json::Value }
pub struct Report { pub sessions: usize, pub skipped_broken: usize, pub signatures: BTreeMap<&'static str, usize>, pub candidates: Vec<CandidateReport>, pub probe_turns_used: u32, pub facts_written: usize, pub written: bool }
impl std::fmt::Display for Report
```

`run_report` flow:
1. `base = load_rules(learned_path)?` (a parse error aborts before proposing).
2. `ledger = Ledger::load(ledger_path)?`.
3. `sessions = store.sessions()`; load each; `EventLog::from_events(...).verify_chain()` — broken → `skipped_broken += 1`, dropped.
4. `sigs = mine(...)` per session, concatenated; count per `kind.name()`.
5. Symbolic: `propose_patches(&sigs)`; skip `ledger.settled(hash)`; `verify_patch(&patch, &evidence, &sessions, &working, &known_specs, cap)`; accepted → `patch.apply_to(&mut working)`; ledger entry with `numbers = {"flipped","not_flipped","regressions","skipped_baseline"}` and the evidence.
6. Corrected facts: for each `SignatureKind::Corrected{text}` matching `^\s*([A-Za-z0-9_.-]+)\s*=\s*(.+?)\s*$` (hand-parse on the first `=`; key chars restricted as shown) → `store.put_fact(Fact { key, value: json!(value), confidence: 1.0, uses: 0, last_validated: Timestamp(now), prov: Provenance::Residual })`, `facts_written += 1`. Not gated by `dry_run`? **Yes it is gated** — a dry run writes nothing.
7. Notes (only if both `probe` and `proposer` are set): for each note-lane signature, in order: `trace = render_turn(session events, sig.turn)`; `proposer.propose(&trace, &working.notes)` → `Err` → recorded in a `detail` and skipped; `None` → skipped; `Some(note)` → skip if `ledger.settled(&note.hash)` or an equal hash is already in `working.notes`; positives = every session containing a signature with the same `kind.name()` (this one included); negatives = the same number of sessions with zero signatures, in store order; `verify_note(..., &mut budget)`; accepted → set `note.lift`, insert into `working.notes`, and if `working.notes.len() > max_notes` remove the note with the lowest `lift` (ties: the oldest, i.e. lowest index); ledger entry with `numbers = {"improved","regressed","lift","turns_used"}`; when the verdict's detail contains `budget exhausted` the ledger verdict is `Unverified`. Stop proposing more notes once `budget == 0`.
8. If `!dry_run`: when `working != base` → `save_rules_atomic` and `rules.store(Arc::new(working.clone()))`, `written = true`; always `ledger.save_atomic` (ledger changes even when nothing is accepted).
9. Return the report.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{NoteProposer, ProbeRunner, TurnOutcome};
    use nscore::*;
    use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
    use nsengine::store::{InMemoryStore, NoopConsolidator};
    use nsengine::turn::{Engine, EngineConfig};

    struct Closed;
    #[async_trait::async_trait]
    impl Channel for Closed {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> { Err(ChannelError::Closed) }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> { Ok(()) }
    }

    async fn record_into(store: Arc<InMemoryStore>, name: &str, proposals: Vec<Proposal>) {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(Closed));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(b.build().unwrap(), EngineConfig::default(), Box::new(|| Timestamp(1)));
        e.run_turn(Incoming { session: SessionId(name.into()), text: "go".into() }).await.unwrap();
    }

    fn typo() -> Proposal { Proposal { rationale: "".into(), action: "eko".into(), args: serde_json::json!({"text": "hi"}) } }
    fn good() -> Proposal { Proposal { rationale: "".into(), action: "echo".into(), args: serde_json::json!({"text": "hi"}) } }

    fn pass(dir: &std::path::Path, rules: Arc<arc_swap::ArcSwap<LearnedRules>>, dry_run: bool) -> EvolutionPass {
        EvolutionPass::new(
            rules,
            vec![EchoTool::new().spec().clone()],
            dir.join("learned.toml"),
            dir.join("ledger.json"),
            PassConfig { dry_run, ..Default::default() },
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
        assert_eq!(rules.load().alias("eko"), Some("echo"), "swapped into the live handle");
        let on_disk = crate::files::load_rules(&dir.path().join("learned.toml")).unwrap();
        assert_eq!(on_disk.alias("eko"), Some("echo"));
        let ledger = Ledger::load(&dir.path().join("ledger.json")).unwrap();
        assert!(ledger.settled(&report.candidates[0].hash));
        let text = report.to_string();
        assert!(text.contains("alias_action eko -> echo") && text.contains("accepted"), "{text}");

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
        let report = pass(dir.path(), rules.clone(), true).run_report(&*store).await.unwrap();
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
        if let EventKind::UserSaid { text } = &mut events[0].kind { *text = "TAMPERED".into(); }
        let tampered = Arc::new(InMemoryStore::new());
        tampered.append(&SessionId("bad".into()), &events).await.unwrap();
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules, true).run_report(&*tampered).await.unwrap();
        assert_eq!((report.sessions, report.skipped_broken), (0, 1));
    }

    #[tokio::test]
    async fn corrected_key_value_becomes_a_fact() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("c".into());
        let mut l = EventLog::new(sid.clone());
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(1, Timestamp(2), EventKind::Corrected { target: None, text: "user.city = Brno".into() });
        l.append(1, Timestamp(3), EventKind::Corrected { target: None, text: "not a fact".into() });
        store.append(&sid, l.events()).await.unwrap();
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(LearnedRules::default()));
        let report = pass(dir.path(), rules, false).run_report(&*store).await.unwrap();
        assert_eq!(report.facts_written, 1);
        let facts = store.facts("user.city").await.unwrap();
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
    /// Probe double: the turn succeeds only when a note mentioning "echo" is in force
    /// (the pre-existing "old low" note must not count).
    struct HelpsIfNoted;
    #[async_trait::async_trait]
    impl ProbeRunner for HelpsIfNoted {
        async fn run(&self, _r: &[Event], rules: Arc<LearnedRules>) -> Result<Vec<TurnOutcome>, String> {
            let helped = rules.notes.iter().any(|n| n.text.contains("echo"));
            Ok(vec![if helped { TurnOutcome::Ok } else { TurnOutcome::Fallback }])
        }
    }

    #[tokio::test]
    async fn accepted_note_lands_in_rules_with_its_lift_and_library_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryStore::new());
        // A fallback session: scripted emitter proposes 5 illegal actions → max_iterations → fallback.
        record_into(store.clone(), "fb", vec![typo(), typo(), typo(), typo(), typo()]).await;
        let mut rules_val = LearnedRules::default();
        rules_val.notes.push(Note::new("global", "old low", 0.1));
        let rules = Arc::new(arc_swap::ArcSwap::from_pointee(rules_val.clone()));
        crate::files::save_rules_atomic(&dir.path().join("learned.toml"), &rules_val).unwrap();
        let p = EvolutionPass::new(
            rules.clone(),
            vec![EchoTool::new().spec().clone()],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            PassConfig { max_notes: 1, ..Default::default() },
        )
        .with_notes(Box::new(HelpsIfNoted), Box::new(FixedProposer("Use echo to repeat text.")));
        let report = p.run_report(&*store).await.unwrap();
        let note_cands: Vec<_> = report.candidates.iter().filter(|c| c.lane == "note").collect();
        assert!(!note_cands.is_empty(), "{report}");
        assert_eq!(note_cands[0].verdict, Verdict::Accepted);
        let live = rules.load();
        assert_eq!(live.notes.len(), 1, "max_notes=1 evicted the lowest-lift note");
        assert_eq!(live.notes[0].text, "Use echo to repeat text.");
        assert!(live.notes[0].lift > 0.0);
        assert!(report.probe_turns_used > 0);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p ns-evolution pass`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
use crate::files::{load_rules, save_rules_atomic, FileError};
use crate::ledger::{Evidence, Ledger, LedgerEntry, Verdict};
use crate::mine::{mine, render_turn, Signature, SignatureKind};
use crate::notes::{verify_note, NoteProposer, ProbeRunner};
use crate::symbolic::{propose_patches, verify_patch, Recorded};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use nscore::{ActionSpec, Consolidator, EventLog, Fact, LearnedRules, MemoryStore, Provenance, StoreError, Timestamp};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PassConfig {
    pub regression_budget: u32,
    pub probe_budget_turns: u32,
    pub max_notes: usize,
    pub regression_replay_cap: usize,
    pub dry_run: bool,
}

impl Default for PassConfig {
    fn default() -> Self {
        Self { regression_budget: 0, probe_budget_turns: 40, max_notes: 20, regression_replay_cap: 200, dry_run: false }
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
    pub candidates: Vec<CandidateReport>,
    pub probe_turns_used: u32,
    pub facts_written: usize,
    pub written: bool,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "sessions: {} (skipped broken: {})", self.sessions, self.skipped_broken)?;
        writeln!(f, "signatures:")?;
        for (k, n) in &self.signatures {
            writeln!(f, "  {k}: {n}")?;
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `key = value` with a dotted-identifier key → (key, value).
fn parse_correction(text: &str) -> Option<(String, String)> {
    let (k, v) = text.split_once('=')?;
    let k = k.trim();
    let v = v.trim();
    let key_ok = !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c));
    (key_ok && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
}

impl EvolutionPass {
    pub fn new(rules: Arc<ArcSwap<LearnedRules>>, known_specs: Vec<ActionSpec>, learned_path: PathBuf, ledger_path: PathBuf, cfg: PassConfig) -> Self {
        Self { rules, known_specs, probe: None, proposer: None, learned_path, ledger_path, cfg, clock: Box::new(now_ms) }
    }
    pub fn with_notes(mut self, probe: Box<dyn ProbeRunner>, proposer: Box<dyn NoteProposer>) -> Self {
        self.probe = Some(probe);
        self.proposer = Some(proposer);
        self
    }
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub async fn run_report(&self, store: &dyn MemoryStore) -> Result<Report, PassError> {
        let base = load_rules(&self.learned_path)?;
        let mut ledger = Ledger::load(&self.ledger_path)?;
        let mut report = Report::default();
        let now = (self.clock)();

        // 3. load + chain-verify sessions
        let mut sessions: Vec<Recorded> = Vec::new();
        for sid in store.sessions().await? {
            let events = store.load(&sid).await?;
            if EventLog::from_events(sid.clone(), events.clone()).verify_chain().is_err() {
                report.skipped_broken += 1;
                continue;
            }
            sessions.push((sid, events));
        }
        report.sessions = sessions.len();

        // 4. mine
        let mut sigs: Vec<Signature> = Vec::new();
        for (sid, events) in &sessions {
            sigs.extend(mine(sid, events, &self.known_specs));
        }
        for s in &sigs {
            *report.signatures.entry(s.kind.name()).or_insert(0) += 1;
        }

        let mut working = base.clone();

        // 5. symbolic lane
        for (patch, evidence) in propose_patches(&sigs) {
            let hash = patch.hash();
            if ledger.settled(&hash) {
                continue;
            }
            let v = verify_patch(&patch, &evidence, &sessions, &working, &self.known_specs, self.cfg.regression_replay_cap).await;
            let verdict = if v.accepted { Verdict::Accepted } else { Verdict::Rejected };
            if v.accepted {
                patch.apply_to(&mut working);
            }
            let numbers = serde_json::json!({
                "flipped": v.flipped, "not_flipped": v.not_flipped,
                "regressions": v.regressions, "skipped_baseline": v.skipped_baseline,
            });
            ledger.entries.insert(hash.clone(), LedgerEntry { verdict, numbers: numbers.clone(), evidence: evidence.clone(), at: now });
            report.candidates.push(CandidateReport { hash, lane: "symbolic", summary: patch.summary(), verdict, numbers });
        }

        // 6. corrections that parse as facts
        if !self.cfg.dry_run {
            for s in &sigs {
                if let SignatureKind::Corrected { text } = &s.kind {
                    if let Some((key, value)) = parse_correction(text) {
                        store
                            .put_fact(Fact { key, value: serde_json::json!(value), confidence: 1.0, uses: 0, last_validated: Timestamp(now), prov: Provenance::Residual })
                            .await?;
                        report.facts_written += 1;
                    }
                }
            }
        }

        // 7. notes lane
        if let (Some(probe), Some(proposer)) = (&self.probe, &self.proposer) {
            let mut budget = self.cfg.probe_budget_turns;
            let clean: Vec<&Recorded> = sessions.iter().filter(|(sid, _)| !sigs.iter().any(|s| &s.session == sid)).collect();
            for s in sigs.iter().filter(|s| s.kind.lane() == "note") {
                if budget == 0 {
                    break;
                }
                let Some((_, events)) = sessions.iter().find(|(sid, _)| sid == &s.session) else { continue };
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
                let positives: Vec<Vec<nscore::Event>> = sessions
                    .iter()
                    .filter(|(sid, _)| sigs.iter().any(|x| &x.session == sid && x.kind.name() == s.kind.name()))
                    .map(|(_, e)| e.clone())
                    .collect();
                let negatives: Vec<Vec<nscore::Event>> = clean.iter().take(positives.len()).map(|(_, e)| e.clone()).collect();
                let v = verify_note(&note, &positives, &negatives, &working, probe.as_ref(), self.cfg.regression_budget, &mut budget).await;
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
                            .min_by(|(ia, a), (ib, b)| a.lift.partial_cmp(&b.lift).unwrap_or(std::cmp::Ordering::Equal).then(ia.cmp(ib)))
                            .expect("non-empty");
                        working.notes.remove(idx);
                    }
                }
                let numbers = serde_json::json!({"improved": v.improved, "regressed": v.regressed, "lift": v.lift, "turns_used": v.turns_used});
                let evidence = vec![Evidence { session: s.session.0.clone(), turn: s.turn, event_id: s.event_id.0 }];
                ledger.entries.insert(note.hash.clone(), LedgerEntry { verdict, numbers: numbers.clone(), evidence, at: now });
                report.candidates.push(CandidateReport { hash: note.hash.clone(), lane: "note", summary: format!("note [{}] {}", note.scope, note.text), verdict, numbers });
            }
        }

        // 8. apply
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
```

`Verdict` needs `Copy` (it has it). `Note` needs `PartialEq` on `f64` — `LearnedRules: PartialEq` already derives it in Task 2; `working != base` compares floats exactly, which is what we want (nothing changed ⇒ bit-identical).

- [ ] **Step 4: Run**

Run: `cargo test -p ns-evolution`
Expected: all pass (files 2, ledger 2, mine 4, symbolic 5, notes 6, pass 5).

- [ ] **Step 5: Commit**

```bash
git add crates/evolution
git commit -m "feat(evolution): EvolutionPass consolidator — mine, gate, apply, ledger, report"
```

---

### Task 13: App wiring — `[evolution]` config, `ns-app evolve [--dry-run]`, startup rules, idle driver

**Files:**
- Modify: `app/Cargo.toml` (`ns-evolution = { path = "../crates/evolution" }`, `arc-swap = { workspace = true }`), `app/src/config.rs`, `app/src/main.rs`, `config.example.toml`
- Test: `app/src/config.rs` (parse test), `app/src/main.rs` (evolve arg parsing helper test), manual run of `evolve --dry-run`

**Interfaces:**

```rust
// config.rs
pub struct EvolutionSection { pub enabled: bool, pub learned_path: String, pub ledger_path: String, pub idle_after_secs: u64, pub regression_budget: u32, pub probe_budget_turns: u32, pub max_notes: usize, pub regression_replay_cap: usize }
impl Default  // true, "learned.toml", "evolution-ledger.json", 300, 0, 40, 20, 200
AppConfig.evolution: EvolutionSection   // #[serde(default)]
impl EvolutionSection { pub fn pass_config(&self, dry_run: bool) -> nsevolution::pass::PassConfig; pub fn idle_after(&self) -> Option<std::time::Duration> /* None when !enabled or secs == 0 */ }

// main.rs helpers
fn build_tools(cfg: &AppConfig) -> Vec<Arc<dyn Tool>>
fn parse_evolve_args(args: &[String]) -> Result<bool /* dry_run */, String>
```

- [ ] **Step 1: Failing config test** (append to `config.rs` tests)

```rust
    #[test]
    fn evolution_section_defaults_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert!(cfg.evolution.enabled);
        assert_eq!(cfg.evolution.learned_path, "learned.toml");
        assert_eq!(cfg.evolution.idle_after(), Some(std::time::Duration::from_secs(300)));
        let cfg = AppConfig::parse("[evolution]\nenabled = false\nprobe_budget_turns = 7\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
        assert_eq!(cfg.evolution.pass_config(true).probe_budget_turns, 7);
        assert!(cfg.evolution.pass_config(true).dry_run);
        let cfg = AppConfig::parse("[evolution]\nidle_after_secs = 0\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-app evolution_section` → compile error.

- [ ] **Step 3: Implement the section**

```rust
#[derive(Debug, serde::Deserialize)]
pub struct EvolutionSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_learned_path")]
    pub learned_path: String,
    #[serde(default = "default_ledger_path")]
    pub ledger_path: String,
    /// 0 = batch (`ns-app evolve`) only.
    #[serde(default = "default_idle_after_secs")]
    pub idle_after_secs: u64,
    #[serde(default)]
    pub regression_budget: u32,
    #[serde(default = "default_probe_budget")]
    pub probe_budget_turns: u32,
    #[serde(default = "default_max_notes")]
    pub max_notes: usize,
    #[serde(default = "default_replay_cap")]
    pub regression_replay_cap: usize,
}

fn default_true() -> bool { true }
fn default_learned_path() -> String { "learned.toml".into() }
fn default_ledger_path() -> String { "evolution-ledger.json".into() }
fn default_idle_after_secs() -> u64 { 300 }
fn default_probe_budget() -> u32 { 40 }
fn default_max_notes() -> usize { 20 }
fn default_replay_cap() -> usize { 200 }

impl Default for EvolutionSection {
    fn default() -> Self {
        Self {
            enabled: true,
            learned_path: default_learned_path(),
            ledger_path: default_ledger_path(),
            idle_after_secs: default_idle_after_secs(),
            regression_budget: 0,
            probe_budget_turns: default_probe_budget(),
            max_notes: default_max_notes(),
            regression_replay_cap: default_replay_cap(),
        }
    }
}

impl EvolutionSection {
    pub fn pass_config(&self, dry_run: bool) -> nsevolution::pass::PassConfig {
        nsevolution::pass::PassConfig {
            regression_budget: self.regression_budget,
            probe_budget_turns: self.probe_budget_turns,
            max_notes: self.max_notes,
            regression_replay_cap: self.regression_replay_cap,
            dry_run,
        }
    }
    pub fn idle_after(&self) -> Option<std::time::Duration> {
        (self.enabled && self.idle_after_secs > 0).then(|| std::time::Duration::from_secs(self.idle_after_secs))
    }
}
```

Add `#[serde(default)] pub evolution: EvolutionSection,` to `AppConfig`.

- [ ] **Step 4: Run** — `cargo test -p ns-app` → passes.

- [ ] **Step 5: Rewrite `main.rs`** (whole file; keep `render_dump` and its test)

```rust
mod config;

use config::AppConfig;
use nscore::{HarnessBuilder, SessionId, Tool};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

fn build_tools(cfg: &AppConfig) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(nscomponents_std::time_tool::GetTimeTool::new())];
    let tool_transport = Arc::new(nscomponents_std::transport::ReqwestToolTransport::new());
    for hc in &cfg.http_components {
        tools.push(Arc::new(nscomponents_std::http_tool::HttpTool::new(hc.clone(), tool_transport.clone())));
    }
    tools
}

/// `ns-app evolve [--dry-run]` → Ok(dry_run)
fn parse_evolve_args(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--dry-run" => Ok(true),
        other => Err(format!("usage: ns-app evolve [--dry-run] (got {other:?})")),
    }
}

/// The provider key, if the configured env var is set and non-empty.
fn api_key(cfg: &AppConfig) -> Option<String> {
    std::env::var(&cfg.llm.api_key_env).ok().filter(|k| !k.is_empty())
}

fn make_client(cfg: &AppConfig, transport: Arc<nsllm::transport::ReqwestTransport>, key: &str) -> nsllm::client::OpenRouterClient {
    let c = nsllm::client::OpenRouterClient::new(transport, key.to_string());
    match &cfg.llm.base_url {
        Some(url) => c.with_base_url(url.clone()),
        None => c,
    }
}

/// Load learned.toml (fatal when unparsable: a bad rule set must not be silently ignored).
fn load_rules_or_exit(cfg: &AppConfig) -> Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>> {
    match nsevolution::files::load_rules(std::path::Path::new(&cfg.evolution.learned_path)) {
        Ok(r) => Arc::new(nsengine::arc_swap::ArcSwap::from_pointee(r)),
        Err(e) => {
            eprintln!("{}: {e}", cfg.evolution.learned_path);
            std::process::exit(1);
        }
    }
}

fn build_pass(
    cfg: &AppConfig,
    rules: Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>>,
    tools: &[Arc<dyn Tool>],
    key: Option<&str>,
    dry_run: bool,
) -> nsevolution::pass::EvolutionPass {
    let specs: Vec<nscore::ActionSpec> = tools.iter().map(|t| t.spec().clone()).collect();
    let pass = nsevolution::pass::EvolutionPass::new(
        rules,
        specs.clone(),
        std::path::PathBuf::from(&cfg.evolution.learned_path),
        std::path::PathBuf::from(&cfg.evolution.ledger_path),
        cfg.evolution.pass_config(dry_run),
    );
    match key {
        None => {
            eprintln!("{} is not set — notes lane skipped (symbolic lane needs no key).", cfg.llm.api_key_env);
            pass
        }
        Some(key) => {
            let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
            let model = cfg.llm.emitter.model.clone();
            let factory_cfg = (cfg.llm.base_url.clone(), key.to_string(), model.clone());
            let factory_transport = transport.clone();
            let emitter: nsevolution::notes::EmitterFactory = Arc::new(move || {
                let (base_url, key, model) = factory_cfg.clone();
                let c = nsllm::client::OpenRouterClient::new(factory_transport.clone(), key);
                let c = match base_url {
                    Some(u) => c.with_base_url(u),
                    None => c,
                };
                Box::new(nsllm::emitter::CloudEmitter::new(c, model)) as Box<dyn nscore::Emitter>
            });
            let probe = nsevolution::notes::LiveProbe { emitter, known_specs: specs, persona: cfg.persona.text.clone() };
            let proposer = nsevolution::notes::ClientNoteProposer { client: make_client(cfg, transport, key), model };
            pass.with_notes(Box::new(probe), Box::new(proposer))
        }
    }
}

#[tokio::main]
async fn main() {
    let cfg_text = std::fs::read_to_string("config.toml").unwrap_or_default();
    let cfg = match AppConfig::parse(&cfg_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().collect();

    // `ns-app dump <session_id>`: print the session log as JSONL and exit.
    if args.get(1).map(String::as_str) == Some("dump") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app dump <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path)).expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone())).await.expect("load session");
        println!("{}", render_dump(&events));
        return;
    }

    // `ns-app evolve [--dry-run]`: driver A (spec M5 §5).
    if args.get(1).map(String::as_str) == Some("evolve") {
        let dry_run = match parse_evolve_args(&args[2..]) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let rules = load_rules_or_exit(&cfg);
        let tools = build_tools(&cfg);
        let key = api_key(&cfg);
        let pass = build_pass(&cfg, rules, &tools, key.as_deref(), dry_run);
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path)).expect("open sqlite store");
        match pass.run_report(&store).await {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("evolve: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Some(key) = api_key(&cfg) else {
        let key_env = &cfg.llm.api_key_env;
        eprintln!("{key_env} is not set — the harness needs a provider API key.");
        eprintln!("export {key_env}=... and run again (see [llm] api_key_env in config.toml).");
        std::process::exit(1);
    };

    let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
    let rules = load_rules_or_exit(&cfg);
    let tools = build_tools(&cfg);

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(nsllm::emitter::CloudEmitter::new(make_client(&cfg, transport.clone(), &key), cfg.llm.emitter.model.clone())));
    b.set_replier(Box::new(
        nsllm::replier::CloudReplier::new(make_client(&cfg, transport.clone(), &key), cfg.llm.replier.model.clone())
            .with_prompt_cache(cfg.llm.prompt_cache()),
    ));
    b.set_memory(Arc::new(nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path)).expect("open sqlite store")));
    b.set_channel(Box::new(nschannel_cli::CliChannel::new_stdio()));
    if cfg.evolution.enabled {
        b.set_consolidator(Box::new(build_pass(&cfg, rules.clone(), &tools, Some(&key), false)));
    } else {
        b.set_consolidator(Box::new(NoopConsolidator));
    }
    for t in &tools {
        b.add_tool(t.clone());
    }

    let parts = b.build().expect("harness assembly");
    let engine_cfg = EngineConfig {
        max_iterations: cfg.engine.max_iterations,
        max_emit_retries: cfg.engine.max_emit_retries,
        persona: cfg.persona.text.clone(),
        templates: cfg.templates.clone(),
        learned: rules,
        idle_after: cfg.evolution.idle_after(),
    };
    let mut engine = Engine::new(parts, engine_cfg);
    println!("ns-harness M5 — type text, /quit to exit");
    if let Err(e) = engine.run().await {
        eprintln!("engine stopped: {e}");
    }
}
```

Keep `render_dump` and its test unchanged; add:

```rust
    #[test]
    fn evolve_args_accept_only_dry_run() {
        assert_eq!(parse_evolve_args(&[]), Ok(false));
        assert_eq!(parse_evolve_args(&["--dry-run".to_string()]), Ok(true));
        assert!(parse_evolve_args(&["--wat".to_string()]).is_err());
    }
```

Check the exact names `nsllm::emitter::CloudEmitter::new(client, model)`, `nsllm::transport::ReqwestTransport::new()`, `nschannel_cli::CliChannel::new_stdio()` against the current `main.rs` before replacing it — they are the ones already in use.

- [ ] **Step 6: `config.example.toml`** — append after `[store]`:

```toml
# Evolution pass (self-improvement, spec 2026-09-02). Both drivers:
#   batch:  ns-app evolve [--dry-run]      (cron-friendly; audit via git diff learned.toml)
#   idle:   after idle_after_secs of silence while running (0 = off)
[evolution]
enabled = true
learned_path = "learned.toml"
ledger_path = "evolution-ledger.json"
idle_after_secs = 300
regression_budget = 0        # notes gate: negatives allowed to regress
probe_budget_turns = 40      # live probe turns per pass (notes lane)
max_notes = 20
regression_replay_cap = 200  # newest sessions replayed by the symbolic gate
```

- [ ] **Step 7: Build, test, and dry-run against a scratch DB**

```bash
cargo build --workspace && cargo test --workspace
cp config.example.toml config.toml   # only if no config.toml exists
cargo run -p ns-app -- evolve --dry-run
```

Expected: the report prints with `sessions: N`, a `signatures:` block, `learned.toml written: false`, and a warning about the missing key when `OPENROUTER_API_KEY`/`MISTRAL_API_KEY` is unset. No files are created.

- [ ] **Step 8: Commit**

```bash
git add app config.example.toml Cargo.lock
git commit -m "feat(app): [evolution] config, ns-app evolve [--dry-run], learned.toml at startup, idle driver"
```

---

### Task 14: Live smoke, docs, and merge

**Files:**
- Modify: `docs/research/2026-09-01-findings.md` (§6), `docs/superpowers/specs/2026-09-02-evolution-pass-design.md` (decision-record amendments), `.gitignore` (no change: `learned.toml` and the ledger stay tracked — they are the audit surface)

- [ ] **Step 1: Live smoke (manual, Mistral)** — with `config.toml` on the Mistral preset:

```bash
KEY=$(bash -ic 'echo $MISTRAL_API_KEY' 2>/dev/null)
rm -f smoke.sqlite; sed -i 's#^path = .*#path = "smoke.sqlite"#' config.toml
printf 'what time is it?\n/quit\n' | MISTRAL_API_KEY=$KEY cargo run -q -p ns-app
MISTRAL_API_KEY=$KEY cargo run -q -p ns-app -- evolve --dry-run
MISTRAL_API_KEY=$KEY cargo run -q -p ns-app -- evolve
git diff --stat learned.toml evolution-ledger.json
```

Expected: the first run answers the time via `get_time`; `evolve --dry-run` prints the report (typically zero candidates on a clean session — that is fine: the point is the end-to-end path runs against the real store and the notes lane logs its probe budget); if a session in the DB carries a misspelled tool name, the alias appears in `learned.toml` after the real run and the next session's identical misspelling runs the tool. Restore `config.toml`'s store path afterwards; `smoke.sqlite` is git-ignored.

- [ ] **Step 2: Docs** — append to findings §6:

```markdown
- **GRASP (gated skill proposer)** — arxiv.org/abs/2605.29668 — **[ADOPTED]** M5 notes gate: balanced held-out probe with a hard regression budget (default 0); ablation shows the gate carries the gain.
- **PreAct** — arxiv.org/abs/2606.17929 — **[ADOPTED (store-time verification)]** verify from a clean state before caching → M5 flip + regression replay from a clean store. Compiled state-machine replay **[LATER]**.
- **HarnessFix** — arxiv.org/abs/2606.06324 — **[ADAPTED]** narrow, attributed repairs with regression-aware validation → M5 two-op patch vocabulary and the never-relax-a-guard invariant.
- **MetaSkill-Evolve** — arxiv.org/abs/2607.05297 — **[LATER]** two-timescale skill loop; revisit with compiled flows.
- **ClawTrace** — arxiv.org/abs/2604.23853 — **[NOTED]** prune/repair patches outrank preserve patches; M5's symbolic lane is repair-only by construction.
```

And in the spec, add a short "Amendments (2026-09-02, plan time)" section at the end listing the four decision-record items from this plan's Global Constraints (sha256 hash, known-specs legal set, baseline-divergent exclusion, `bad_args` kind).

- [ ] **Step 3: Full verification**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Expected: clean.

- [ ] **Step 4: Commit and merge**

```bash
git add docs
git commit -m "docs: M5 evolution pass — findings citations and spec amendments"
```

Then follow `superpowers:finishing-a-development-branch` (merge `m5-evolution` into `main`, delete the worktree's `target/` first).
