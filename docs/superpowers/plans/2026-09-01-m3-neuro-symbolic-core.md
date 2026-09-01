# M3 Neuro-Symbolic Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The thesis becomes testable — real provenance classification of every emitted argument, trust propagation, the four built-in guards (`ResidualPolicy`, `TaintPolicy`, `DedupeGate`, `SideEffectGate`), the rejection→narrowed-schema loop, `ask_clarification` mechanics, and the two-turn confirmation flow for irreversible actions.

**Architecture:** New `ns-provenance` crate (depends only on `ns-core`): a `ValueIndex` built from the session's event log records every value the session has legitimately seen (user spans per turn, tool-output JSON leaves with their trust); `classify_args` matches each emitted argument against the index in spec order (tool-result paths → user spans → constants → normalizers → Residual) and propagates minimum trust. `ns-core`'s `Guard` trait gets the typed `GuardCtx` view promised in M1. The engine replaces its M1 classification stub, hardwires the four built-in guards ahead of plugin guards, narrows the legal set after each rejection, synthesizes `ask_clarification`/`confirm_pending` actions, and drives the PendingConfirmation→Confirmed paper trail.

**Tech Stack:** Rust 2021; no new external dependencies (serde_json matching only).

**Spec:** `docs/superpowers/specs/2026-09-01-neuro-symbolic-harness-design.md` (§4 core types, §5 steps 1+4, §6 traits, §9 confirmation, §12 M3 scope)

## Global Constraints

- New workspace member: `crates/provenance` (pkg `ns-provenance`, lib `nsprovenance`), depending ONLY on `ns-core` + serde_json.
- `ns-core` IS modified in M3 (this is the milestone that upgrades the guard contract): `Guard::check` takes `&GuardCtx` instead of `&serde_json::Value`; `ActionSpec` gains `dedupe_tag: Option<String>`. Every `ActionSpec` literal and `Guard` impl in the workspace is updated in the same task — the workspace never stays broken across a commit.
- Decision record (settled at plan time, from spec §5):
  - Trust per provenance: `UserInput` → `User`; `CopiedOutput` → the recorded trust of that tool's output; `Constant` → `System`; `Transform` → min of input trusts; `Residual` → `System` (model-invented: not user, not external).
  - Normalizers v1: `trim` and `lowercase` only, expressed as `Transform { func, inputs: [underlying] }`.
  - `ask_clarification` is FORCED (sole legal action) whenever a `NeverResidual` rejection occurred this turn — the spec's "and no grounding source exists" refinement is approximated by the rejection itself, documented in code.
  - A `PendingConfirmation` is active only on the turn immediately following its creation (`state.turn == pending_turn + 1`); otherwise expired.
  - DedupeGate fires only for specs with `dedupe_tag: Some(_)`; the spec's "off-list values dropped, never coerced" is already enforced structurally by the strict emitter schema (enum-constrained args) — noted, not re-implemented.
- Guard evaluation order (fixed): built-ins `ResidualPolicy` → `TaintPolicy` → `DedupeGate` → `SideEffectGate`, then plugin guards. Built-ins are constructed inside the engine — plugins cannot remove them.
- Synthetic actions (`ask_clarification`, `confirm_pending`, `respond_directly`) are engine-owned; they are never `Tool`s. `respond_directly` stays schema-level (ns-llm); the other two enter the `LegalActionSet` so the schema builder exposes them naturally.
- TDD per task: failing test → watch it fail → implement → watch it pass → commit. `cargo test -p <crate>` from repo root.
- Commit after every task with the message given. Git identity: `git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit ...` plus the executing model's `Co-Authored-By` trailer.
- Work in an isolated worktree; delete the worktree's `target/` before merging (disk is 29G total — two full build trees have filled it once already).

## File Structure (end state of M3)

```
crates/core/src/action.rs           # + dedupe_tag on ActionSpec
crates/core/src/traits.rs           # Guard trait: check(&ClassifiedProposal, &GuardCtx); GuardCtx<'a>
crates/provenance/Cargo.toml
crates/provenance/src/lib.rs        # pub mod index; pub mod classify;
crates/provenance/src/index.rs      # ValueIndex, IndexedOutput, UserSpanHit
crates/provenance/src/classify.rs   # classify_args
crates/engine/src/state.rs          # fold: fired_actions on ToolCalled, pending turn tracking
crates/engine/src/guards.rs         # ResidualPolicy, TaintPolicy, DedupeGate, SideEffectGate
crates/engine/src/turn.rs           # real classification, built-in guard chain, narrowing,
                                    # ask_clarification, confirm_pending, staging
crates/engine/tests/turn_loop.rs    # + M3 integration tests
```

---

### Task 1: Core — typed GuardCtx + dedupe_tag (workspace-wide sweep)

**Files:**
- Modify: `crates/core/src/traits.rs` (Guard trait + GuardCtx), `crates/core/src/action.rs` (ActionSpec)
- Modify (mechanical): every `ActionSpec { .. }` literal and `Guard` impl in the workspace: `crates/core/src/action.rs` tests, `crates/core/src/traits.rs` tests, `crates/core/src/plugin.rs` tests, `crates/engine/src/script.rs` (`EchoTool`, `DenyAction`), `crates/engine/src/turn.rs` (guard invocation), `crates/engine/tests/turn_loop.rs`, `crates/llm/src/schema.rs` tests, `crates/llm/src/emitter.rs` tests, `crates/components-std/src/time_tool.rs`, `crates/components-std/src/http_tool.rs`
- Test: inline in `traits.rs`

**Interfaces:**
- Produces in `crates/core/src/traits.rs`:

```rust
/// Typed view of session state for guards — replaces M1's serde_json::Value.
pub struct GuardCtx<'a> {
    /// Spec of the proposed action (synthetic actions get synthetic specs).
    pub spec: &'a crate::action::ActionSpec,
    pub turn: u32,
    /// A Confirmed event was appended this turn (unlocks SideEffectGate).
    pub confirmed_this_turn: bool,
    /// Action names that have ToolCalled at least once this session.
    pub fired_actions: &'a std::collections::HashSet<String>,
    /// Active (non-expired) pending confirmation, if any.
    pub pending_confirmation: Option<crate::event::EventId>,
}

pub trait Guard: Send + Sync {
    fn name(&self) -> &str;
    fn check(&self, p: &crate::action::ClassifiedProposal, ctx: &GuardCtx) -> Verdict;
}
```

- Produces in `crates/core/src/action.rs`: `ActionSpec` gains `pub dedupe_tag: Option<String>` (serde `#[serde(default)]` so stored M2 events still deserialize).
- HttpToolConfig (`components-std`) gains `#[serde(default)] pub dedupe_tag: Option<String>` passed through to its ActionSpec.
- Every other `ActionSpec` literal adds `dedupe_tag: None`.

- [ ] **Step 1: Write the failing test** — replace the Guard-related test in `crates/core/src/traits.rs` tests module (keep `tool_is_object_safe`):

```rust
    struct AlwaysDeny;
    impl Guard for AlwaysDeny {
        fn name(&self) -> &str { "always_deny" }
        fn check(&self, _p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
            Verdict::Deny { reason: format!("turn {}", ctx.turn) }
        }
    }

    #[test]
    fn guard_sees_typed_ctx() {
        let spec = ActionSpec {
            name: "x".into(), description: "d".into(),
            args_schema: serde_json::json!({}), side_effect: SideEffect::Pure,
            residual_policy: Default::default(), dedupe_tag: None,
        };
        let fired = std::collections::HashSet::new();
        let ctx = GuardCtx {
            spec: &spec, turn: 3, confirmed_this_turn: false,
            fired_actions: &fired, pending_confirmation: None,
        };
        let p = ClassifiedProposal {
            proposal: Proposal { rationale: "".into(), action: "x".into(), args: serde_json::json!({}) },
            args: vec![],
        };
        let g: Box<dyn Guard> = Box::new(AlwaysDeny);
        assert!(matches!(g.check(&p, &ctx), Verdict::Deny { reason } if reason == "turn 3"));
    }
```

(Imports for the test module: `use crate::action::{ClassifiedProposal, Proposal, SideEffect, Verdict};`.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-core`. Expected: compile error (`GuardCtx` missing, `dedupe_tag` missing).

- [ ] **Step 3: Implement + mechanical sweep.** Add `GuardCtx` and the new `Guard` trait to `traits.rs`; add `#[serde(default)] pub dedupe_tag: Option<String>` to `ActionSpec`. Then fix every compile error the workspace reports, in this shape:
  - `ActionSpec { ... }` literals: add `dedupe_tag: None,` (in components-std `HttpTool::new`, pass `cfg.dedupe_tag`).
  - `DenyAction` in `script.rs`: signature becomes `fn check(&self, p: &ClassifiedProposal, _ctx: &GuardCtx) -> Verdict` (logic unchanged).
  - `turn.rs` guard loop: temporarily build `GuardCtx { spec: <resolved spec>, turn, confirmed_this_turn: false, fired_actions: &state.fired_tags, pending_confirmation: state.pending_confirmation }` — the real values land in later tasks. Resolving `spec` for the ctx uses the tool found by name (legality is checked before guards, so the lookup succeeds; move the tool lookup above the guard loop).
  - `turn_loop.rs` `DenyAction` usage compiles unchanged; the `deny_guard_denies_only_named_action` test in `script.rs` updates its call site to build a `GuardCtx` like the core test.

- [ ] **Step 4: Run to verify pass** — `cargo test` (whole workspace). Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(core): typed GuardCtx for guards and dedupe_tag on ActionSpec"
```

---

### Task 2: ns-provenance — ValueIndex

**Files:**
- Create: `crates/provenance/Cargo.toml`, `crates/provenance/src/lib.rs`, `crates/provenance/src/index.rs`
- Modify: root `Cargo.toml` (add member)
- Test: inline in `index.rs`

**Interfaces:**
- Produces:

```rust
/// One JSON leaf of one tool output, addressable for CopiedOutput claims.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputLeaf {
    pub call: nscore::EventId,     // the ToolCalled event id
    pub path: String,              // "$" for whole summary, "$.a.b[0]" for leaves
    pub value: serde_json::Value,  // leaf value (string/number/bool)
    pub trust: nscore::Trust,      // trust recorded on the ToolOutput
}

/// Everything this session has legitimately seen, rebuilt from the log.
pub struct ValueIndex {
    pub user_texts: Vec<(u32, String)>,   // (turn, text), newest last
    pub output_leaves: Vec<OutputLeaf>,   // newest last
}

impl ValueIndex {
    pub fn from_events(events: &[nscore::Event]) -> ValueIndex;
}
```

Build rules for `from_events`:
1. `UserSaid { text }` → push `(event.turn, text)`.
2. `ToolReturned { call, outcome: Ok { output } }` →
   a. push `OutputLeaf { call, path: "$", value: json!(output.summary), trust: output.trust }`;
   b. if `output.summary` parses as JSON, walk it and push one leaf per scalar (string/number/bool) with a JSONPath-style path (`$.key`, `$.arr[0].k`); objects/arrays themselves are not leaves.
3. Everything else contributes nothing.

- [ ] **Step 1: Scaffold**

Root `Cargo.toml` members: add `"crates/provenance"` (keep the list sorted as in M2's end state).

`crates/provenance/Cargo.toml`:

```toml
[package]
name = "ns-provenance"
version = "0.1.0"
edition = "2021"

[lib]
name = "nsprovenance"

[dependencies]
ns-core = { path = "../core" }
serde_json = { workspace = true }
```

`crates/provenance/src/lib.rs`:

```rust
pub mod classify;
pub mod index;
```

(`classify.rs` starts as an empty file; Task 3 fills it.)

- [ ] **Step 2: Write the failing tests** (in `index.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn log_with_tool_output(summary: &str, trust: Trust) -> EventLog {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "check stock for widget".into() });
        let call = log
            .append(1, Timestamp(2), EventKind::ToolCalled { action: "check_stock".into(), args: vec![] })
            .id;
        log.append(1, Timestamp(3), EventKind::ToolReturned {
            call,
            outcome: ToolOutcome::Ok {
                output: ToolOutput { summary: summary.into(), artifact: None, trust },
            },
        });
        log
    }

    #[test]
    fn indexes_user_turns() {
        let log = log_with_tool_output("ok", Trust::System);
        let idx = ValueIndex::from_events(log.events());
        assert_eq!(idx.user_texts, vec![(1, "check stock for widget".to_string())]);
    }

    #[test]
    fn json_summary_yields_addressable_leaves() {
        let log = log_with_tool_output(r#"{"in_stock":true,"items":[{"id":42}]}"#, Trust::External);
        let idx = ValueIndex::from_events(log.events());
        let paths: Vec<(&str, &serde_json::Value)> =
            idx.output_leaves.iter().map(|l| (l.path.as_str(), &l.value)).collect();
        assert!(paths.iter().any(|(p, v)| *p == "$.in_stock" && **v == serde_json::json!(true)));
        assert!(paths.iter().any(|(p, v)| *p == "$.items[0].id" && **v == serde_json::json!(42)));
        assert!(idx.output_leaves.iter().all(|l| l.trust == Trust::External));
    }

    #[test]
    fn non_json_summary_yields_only_root_leaf() {
        let log = log_with_tool_output("current unix time (ms): 99", Trust::System);
        let idx = ValueIndex::from_events(log.events());
        assert_eq!(idx.output_leaves.len(), 1);
        assert_eq!(idx.output_leaves[0].path, "$");
        assert_eq!(idx.output_leaves[0].value, serde_json::json!("current unix time (ms): 99"));
    }

    #[test]
    fn err_outcomes_contribute_nothing() {
        let mut log = EventLog::new(SessionId("s".into()));
        let call = log
            .append(1, Timestamp(1), EventKind::ToolCalled { action: "x".into(), args: vec![] })
            .id;
        log.append(1, Timestamp(2), EventKind::ToolReturned {
            call,
            outcome: ToolOutcome::Err { kind: "network".into(), detail: "down".into() },
        });
        let idx = ValueIndex::from_events(log.events());
        assert!(idx.output_leaves.is_empty());
    }
}
```

- [ ] **Step 3: Run to verify failure** — `cargo test -p ns-provenance`. Expected: compile error.

- [ ] **Step 4: Implement**

```rust
use nscore::{Event, EventId, EventKind, ToolOutcome, Trust};

#[derive(Debug, Clone, PartialEq)]
pub struct OutputLeaf {
    pub call: EventId,
    pub path: String,
    pub value: serde_json::Value,
    pub trust: Trust,
}

pub struct ValueIndex {
    pub user_texts: Vec<(u32, String)>,
    pub output_leaves: Vec<OutputLeaf>,
}

fn walk_leaves(call: EventId, trust: Trust, path: &str, v: &serde_json::Value, out: &mut Vec<OutputLeaf>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                walk_leaves(call, trust, &format!("{path}.{k}"), child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk_leaves(call, trust, &format!("{path}[{i}]"), child, out);
            }
        }
        serde_json::Value::Null => {}
        leaf => out.push(OutputLeaf { call, path: path.to_string(), value: leaf.clone(), trust }),
    }
}

impl ValueIndex {
    pub fn from_events(events: &[Event]) -> ValueIndex {
        let mut user_texts = Vec::new();
        let mut output_leaves = Vec::new();
        for e in events {
            match &e.kind {
                EventKind::UserSaid { text } => user_texts.push((e.turn, text.clone())),
                EventKind::ToolReturned { call, outcome: ToolOutcome::Ok { output } } => {
                    output_leaves.push(OutputLeaf {
                        call: *call,
                        path: "$".into(),
                        value: serde_json::Value::String(output.summary.clone()),
                        trust: output.trust,
                    });
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output.summary) {
                        if parsed.is_object() || parsed.is_array() {
                            walk_leaves(*call, output.trust, "$", &parsed, &mut output_leaves);
                        }
                    }
                }
                _ => {}
            }
        }
        ValueIndex { user_texts, output_leaves }
    }
}
```

- [ ] **Step 5: Run to verify pass** — `cargo test -p ns-provenance`. Expected: 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(provenance): ValueIndex over user turns and tool-output json leaves"
```

---

### Task 3: ns-provenance — classification

**Files:**
- Create: `crates/provenance/src/classify.rs` (replace empty file)
- Test: inline in `classify.rs`

**Interfaces:**
- Consumes: `ValueIndex`/`OutputLeaf` (Task 2), `nscore::{ActionSpec, Provenance, TaggedValue, Trust}`.
- Produces:

```rust
/// Classify every top-level arg of a proposal against the session's history.
/// Match order per spec §5.4: tool-result paths → user spans → constants →
/// normalizers (trim, lowercase, over the first two sources) → Residual.
pub fn classify_args(
    args: &serde_json::Value,           // the proposal's args object
    spec: &nscore::ActionSpec,          // for enum constants in args_schema
    index: &ValueIndex,
    current_turn: u32,                  // this turn's UserSaid is searched first
) -> Vec<(String, nscore::TaggedValue)>;
```

Matching rules per arg value `v`:
1. **Tool-result path:** newest-first `OutputLeaf` whose `value == v` (exact JSON equality) → `CopiedOutput { call, path }`, trust = leaf trust.
2. **User span:** if `v` is a string, search `user_texts` — current turn first, then newest-first — for an exact substring; hit at byte offsets `[start, end)` → `UserInput { turn, start, end }`, trust `User`. Non-strings: match `v.to_string()` the same way.
3. **Constant:** `spec.args_schema["properties"][arg]["enum"]` contains `v` → `Constant`, trust `System`.
4. **Normalizers:** retry 1 and 2 with `trim` then `lowercase` applied to `v`'s string form; a hit wraps the underlying provenance: `Transform { func: "trim"|"lowercase", inputs: [<underlying>] }`, trust = underlying trust.
5. **Residual** otherwise, trust `System` (model-invented — decision record in Global Constraints).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{OutputLeaf, ValueIndex};
    use nscore::*;

    fn spec_with_enum() -> ActionSpec {
        ActionSpec {
            name: "order".into(),
            description: "d".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "priority": {"type": "string", "enum": ["low", "high"]},
                    "product": {"type": "string"},
                    "order_id": {"type": "integer"}
                },
                "required": ["product"]
            }),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    fn index() -> ValueIndex {
        ValueIndex {
            user_texts: vec![(1, "order a widget please".into()), (2, "make it so".into())],
            output_leaves: vec![OutputLeaf {
                call: EventId(7),
                path: "$.order.id".into(),
                value: serde_json::json!(4242),
                trust: Trust::External,
            }],
        }
    }

    fn classify_one(args: serde_json::Value) -> Vec<(String, TaggedValue)> {
        classify_args(&args, &spec_with_enum(), &index(), 2)
    }

    #[test]
    fn tool_output_leaf_wins_with_its_trust() {
        let out = classify_one(serde_json::json!({"order_id": 4242}));
        let (_, tv) = &out[0];
        assert!(matches!(&tv.prov, Provenance::CopiedOutput { call: EventId(7), path } if path == "$.order.id"));
        assert_eq!(tv.trust, Trust::External);
    }

    #[test]
    fn user_span_is_found_with_offsets() {
        let out = classify_one(serde_json::json!({"product": "widget"}));
        let (_, tv) = &out[0];
        match &tv.prov {
            Provenance::UserInput { turn, start, end } => {
                assert_eq!(*turn, 1);
                assert_eq!(&"order a widget please"[*start as usize..*end as usize], "widget");
            }
            other => panic!("expected UserInput, got {other:?}"),
        }
        assert_eq!(tv.trust, Trust::User);
    }

    #[test]
    fn enum_value_is_constant_system_trust() {
        let out = classify_one(serde_json::json!({"priority": "high"}));
        let (_, tv) = &out[0];
        assert!(matches!(tv.prov, Provenance::Constant));
        assert_eq!(tv.trust, Trust::System);
    }

    #[test]
    fn lowercase_match_is_a_transform_preserving_trust() {
        let out = classify_one(serde_json::json!({"product": "Widget"}));
        let (_, tv) = &out[0];
        match &tv.prov {
            Provenance::Transform { func, inputs } => {
                assert_eq!(func, "lowercase");
                assert!(matches!(inputs[0], Provenance::UserInput { .. }));
            }
            other => panic!("expected Transform, got {other:?}"),
        }
        assert_eq!(tv.trust, Trust::User);
    }

    #[test]
    fn unmatched_value_is_residual_system_trust() {
        let out = classify_one(serde_json::json!({"product": "flux capacitor"}));
        let (_, tv) = &out[0];
        assert!(matches!(tv.prov, Provenance::Residual));
        assert_eq!(tv.trust, Trust::System);
    }

    #[test]
    fn current_turn_user_text_is_searched_first() {
        let mut idx = index();
        idx.user_texts.push((2, "widget again".into()));
        let out = classify_args(
            &serde_json::json!({"product": "widget"}),
            &spec_with_enum(),
            &idx,
            2,
        );
        let (_, tv) = &out[0];
        assert!(matches!(&tv.prov, Provenance::UserInput { turn: 2, .. }));
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-provenance`. Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use crate::index::ValueIndex;
use nscore::{ActionSpec, Provenance, TaggedValue, Trust};

fn value_as_match_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn find_output_leaf(v: &serde_json::Value, index: &ValueIndex) -> Option<(Provenance, Trust)> {
    index.output_leaves.iter().rev().find(|l| l.value == *v).map(|l| {
        (Provenance::CopiedOutput { call: l.call, path: l.path.clone() }, l.trust)
    })
}

fn find_user_span(needle: &str, index: &ValueIndex, current_turn: u32) -> Option<(Provenance, Trust)> {
    if needle.is_empty() {
        return None;
    }
    let current = index.user_texts.iter().rev().filter(|(t, _)| *t == current_turn);
    let earlier = index.user_texts.iter().rev().filter(|(t, _)| *t != current_turn);
    for (turn, text) in current.chain(earlier) {
        if let Some(start) = text.find(needle) {
            return Some((
                Provenance::UserInput {
                    turn: *turn,
                    start: start as u32,
                    end: (start + needle.len()) as u32,
                },
                Trust::User,
            ));
        }
    }
    None
}

fn find_constant(arg: &str, v: &serde_json::Value, spec: &ActionSpec) -> Option<(Provenance, Trust)> {
    spec.args_schema["properties"][arg]["enum"]
        .as_array()
        .filter(|options| options.contains(v))
        .map(|_| (Provenance::Constant, Trust::System))
}

fn classify_value(
    arg: &str,
    v: &serde_json::Value,
    spec: &ActionSpec,
    index: &ValueIndex,
    current_turn: u32,
) -> (Provenance, Trust) {
    let needle = value_as_match_string(v);
    // 1–3: direct matches in spec order
    if let Some(hit) = find_output_leaf(v, index) {
        return hit;
    }
    if let Some(hit) = find_user_span(&needle, index, current_turn) {
        return hit;
    }
    if let Some(hit) = find_constant(arg, v, spec) {
        return hit;
    }
    // 4: normalizers over sources 1–2
    for (func, normalized) in [
        ("trim", needle.trim().to_string()),
        ("lowercase", needle.to_lowercase()),
    ] {
        if normalized == needle {
            continue; // normalization changed nothing; already tried
        }
        let as_value = serde_json::Value::String(normalized.clone());
        let hit = find_output_leaf(&as_value, index)
            .or_else(|| find_user_span(&normalized, index, current_turn));
        if let Some((prov, trust)) = hit {
            return (Provenance::Transform { func: func.into(), inputs: vec![prov] }, trust);
        }
    }
    // 5: the model made it up
    (Provenance::Residual, Trust::System)
}

pub fn classify_args(
    args: &serde_json::Value,
    spec: &ActionSpec,
    index: &ValueIndex,
    current_turn: u32,
) -> Vec<(String, TaggedValue)> {
    args.as_object()
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    let (prov, trust) = classify_value(k, v, spec, index, current_turn);
                    (k.clone(), TaggedValue { value: v.clone(), prov, trust })
                })
                .collect()
        })
        .unwrap_or_default()
}
```

- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-provenance`. Expected: 10 tests pass (4 index + 6 classify).

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(provenance): spec-ordered arg classification with trust propagation"
```

---

### Task 4: Engine — real classification + fold upgrades

**Files:**
- Modify: `crates/engine/Cargo.toml` (add `ns-provenance = { path = "../provenance" }`), `crates/engine/src/state.rs`, `crates/engine/src/turn.rs`
- Test: inline in `state.rs`; integration in `crates/engine/tests/turn_loop.rs`

**Interfaces:**
- `SessionState` changes: `pending_confirmation: Option<EventId>` stays; add `pub pending_turn: Option<u32>` (turn of the PendingConfirmation event, cleared together with it); fold now inserts the action name into `fired_tags` on every `ToolCalled`; add `pub confirmed_this_turn_of: Option<u32>` (turn of the most recent `Confirmed` event).
- `turn.rs`: the M1 stub (every arg `Residual`/`Trust::User`) is replaced by `nsprovenance::classify::classify_args(&proposal.args, spec, &index, turn)` with `index = nsprovenance::index::ValueIndex::from_events(log.events())` rebuilt once per loop iteration (before classification). GuardCtx now carries real values: `confirmed_this_turn: state.confirmed_this_turn_of == Some(turn)`, `fired_actions: &state.fired_tags`, `pending_confirmation: <active pending per expiry rule>`.

- [ ] **Step 1: Write the failing fold test** (append to `state.rs` tests)

```rust
    #[test]
    fn fold_tracks_fired_actions_pending_turn_and_confirmation_turn() {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "go".into() });
        log.append(1, Timestamp(2), EventKind::ToolCalled { action: "echo".into(), args: vec![] });
        let pending = log
            .append(1, Timestamp(3), EventKind::PendingConfirmation { proposal_of: EventId(1), staged: None })
            .id;
        let s = fold(log.events());
        assert!(s.fired_tags.contains("echo"));
        assert_eq!(s.pending_confirmation, Some(pending));
        assert_eq!(s.pending_turn, Some(1));
        assert_eq!(s.confirmed_this_turn_of, None);

        log.append(2, Timestamp(4), EventKind::UserSaid { text: "yes".into() });
        log.append(2, Timestamp(5), EventKind::Confirmed { pending });
        let s2 = fold(log.events());
        assert_eq!(s2.pending_confirmation, None);
        assert_eq!(s2.pending_turn, None);
        assert_eq!(s2.confirmed_this_turn_of, Some(2));
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine`. Expected: compile error (`pending_turn` missing).

- [ ] **Step 3: Implement.** In `state.rs`: add the two fields (`pending_turn: Option<u32>`, `confirmed_this_turn_of: Option<u32>`) to `SessionState` (Default derives keep working); fold match arms: `ToolCalled { action, .. }` inserts `action.clone()` into `fired_tags`; `PendingConfirmation` sets both `pending_confirmation = Some(id)` and `pending_turn = Some(event.turn)`; `Confirmed { pending }` clears both when it matches and sets `confirmed_this_turn_of = Some(event.turn)`.

In `turn.rs`: add at the top of each loop iteration `let index = nsprovenance::index::ValueIndex::from_events(log.events());`; replace the stub classification block with:

```rust
            let spec = tool.spec(); // tool lookup already moved above guards in Task 1
            let classified_args =
                nsprovenance::classify::classify_args(&proposal.args, spec, &index, turn);
            let classified =
                ClassifiedProposal { proposal: proposal.clone(), args: classified_args.clone() };
```

Active-pending rule (used for GuardCtx and later tasks): `let active_pending = state.pending_confirmation.filter(|_| state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false) || state.pending_turn == Some(turn));`

- [ ] **Step 4: Add the integration test** (append to `turn_loop.rs`) — classification is observable through the persisted `ToolCalled` args:

```rust
#[tokio::test]
async fn real_classification_tags_user_input_and_residual() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: "echo".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "say hi"}), // exact substring of the user turn
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("cls1".into());
    e.run_turn(Incoming { session: sid.clone(), text: "please say hi now".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    let called_args = events.iter().find_map(|ev| match &ev.kind {
        EventKind::ToolCalled { args, .. } => Some(args.clone()),
        _ => None,
    }).expect("a ToolCalled event");
    let (name, tv) = &called_args[0];
    assert_eq!(name, "text");
    assert!(
        matches!(tv.prov, Provenance::UserInput { .. }),
        "'say hi' comes from the user's words, got {:?}",
        tv.prov
    );
    assert_eq!(tv.trust, Trust::User);
}
```

- [ ] **Step 5: Run to verify pass** — `cargo test -p ns-engine`. Expected: all green (M2's tests still pass — none asserted on the stub's Residual tags).

- [ ] **Step 6: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(engine): real provenance classification and richer session fold"
```

---

### Task 5: Engine — pure guards (ResidualPolicy, TaintPolicy, DedupeGate)

**Files:**
- Create: `crates/engine/src/guards.rs`; modify `crates/engine/src/lib.rs` (`pub mod guards;`), `crates/engine/src/turn.rs` (built-in chain)
- Test: inline in `guards.rs`

**Interfaces:**
- Produces:

```rust
/// Denies proposals whose args are Residual where the spec forbids it.
pub struct ResidualPolicy;
// name() = "residual_policy". Deny when any arg has Provenance::Residual
// (directly, or anywhere inside a Transform's inputs) AND
// spec.residual_policy.get(arg) == Some(ResidualRule::Never).
// Deny reason MUST contain the marker "NeverResidual" and the arg name —
// the turn loop keys forced clarification off that marker.

/// Side-effectful actions and clarification questions may not be driven by
/// External-trust values without confirmation (spec §5.4).
pub struct TaintPolicy;
// name() = "taint_policy". Applies when spec.side_effect != Pure OR
// p.proposal.action == "ask_clarification". If any arg trust == External
// and !ctx.confirmed_this_turn -> Verdict::NeedsConfirmation { prompt:
// "This uses data from an external source (<args>). Proceed?" }. Else Allow.

/// (session, dedupe_tag) fires once.
pub struct DedupeGate;
// name() = "dedupe_gate". If ctx.spec.dedupe_tag.is_some() and
// ctx.fired_actions.contains(&p.proposal.action) -> Deny { "already done
// this session" }. Else Allow.
```

- `turn.rs`: builds the built-in chain once in `Engine::new`/`with_clock` — `builtin_guards: Vec<Box<dyn Guard>>` field on `Engine` holding `[ResidualPolicy, TaintPolicy, DedupeGate]` (SideEffectGate joins in Task 6); the guard loop checks built-ins first, then `parts.guards`.

- [ ] **Step 1: Write the failing tests** (in `guards.rs`; helpers build `ClassifiedProposal`/`GuardCtx` locally)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;
    use std::collections::HashSet;

    fn spec(effect: SideEffect, residual: &[(&str, ResidualRule)], dedupe: Option<&str>) -> ActionSpec {
        ActionSpec {
            name: "act".into(),
            description: "d".into(),
            args_schema: serde_json::json!({"type": "object", "properties": {}}),
            side_effect: effect,
            residual_policy: residual.iter().map(|(k, r)| (k.to_string(), r.clone())).collect(),
            dedupe_tag: dedupe.map(String::from),
        }
    }

    fn proposal(action: &str, args: Vec<(&str, Provenance, Trust)>) -> ClassifiedProposal {
        ClassifiedProposal {
            proposal: Proposal {
                rationale: "".into(),
                action: action.into(),
                args: serde_json::json!({}),
            },
            args: args
                .into_iter()
                .map(|(k, prov, trust)| {
                    (k.to_string(), TaggedValue { value: serde_json::json!("v"), prov, trust })
                })
                .collect(),
        }
    }

    fn ctx<'a>(spec: &'a ActionSpec, fired: &'a HashSet<String>, confirmed: bool) -> GuardCtx<'a> {
        GuardCtx {
            spec,
            turn: 1,
            confirmed_this_turn: confirmed,
            fired_actions: fired,
            pending_confirmation: None,
        }
    }

    #[test]
    fn residual_policy_denies_never_residual_args() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let p = proposal("act", vec![("order_id", Provenance::Residual, Trust::System)]);
        match ResidualPolicy.check(&p, &ctx(&s, &fired, false)) {
            Verdict::Deny { reason } => {
                assert!(reason.contains("NeverResidual"));
                assert!(reason.contains("order_id"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn residual_policy_allows_grounded_args_and_allowed_residuals() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let grounded = proposal(
            "act",
            vec![("order_id", Provenance::UserInput { turn: 1, start: 0, end: 2 }, Trust::User)],
        );
        assert!(matches!(ResidualPolicy.check(&grounded, &ctx(&s, &fired, false)), Verdict::Allow));
        let free_arg = proposal("act", vec![("note", Provenance::Residual, Trust::System)]);
        assert!(matches!(ResidualPolicy.check(&free_arg, &ctx(&s, &fired, false)), Verdict::Allow));
    }

    #[test]
    fn residual_policy_sees_residual_inside_transforms() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let p = proposal(
            "act",
            vec![(
                "order_id",
                Provenance::Transform { func: "trim".into(), inputs: vec![Provenance::Residual] },
                Trust::System,
            )],
        );
        assert!(matches!(ResidualPolicy.check(&p, &ctx(&s, &fired, false)), Verdict::Deny { .. }));
    }

    #[test]
    fn taint_policy_gates_external_trust_on_side_effects() {
        let s = spec(SideEffect::Reversible, &[], None);
        let fired = HashSet::new();
        let p = proposal(
            "act",
            vec![("target", Provenance::CopiedOutput { call: EventId(3), path: "$.x".into() }, Trust::External)],
        );
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
        // confirmed this turn -> allowed
        assert!(matches!(TaintPolicy.check(&p, &ctx(&s, &fired, true)), Verdict::Allow));
        // pure action -> not gated
        let pure = spec(SideEffect::Pure, &[], None);
        assert!(matches!(TaintPolicy.check(&p, &ctx(&pure, &fired, false)), Verdict::Allow));
    }

    #[test]
    fn taint_policy_gates_clarification_questions_too() {
        let s = spec(SideEffect::Pure, &[], None);
        let fired = HashSet::new();
        let mut p = proposal(
            "ask_clarification",
            vec![("question", Provenance::CopiedOutput { call: EventId(3), path: "$".into() }, Trust::External)],
        );
        p.proposal.action = "ask_clarification".into();
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn dedupe_gate_fires_once_per_tagged_action() {
        let s = spec(SideEffect::Pure, &[], Some("greeting"));
        let mut fired = HashSet::new();
        let p = proposal("act", vec![]);
        assert!(matches!(DedupeGate.check(&p, &ctx(&s, &fired, false)), Verdict::Allow));
        fired.insert("act".into());
        assert!(matches!(DedupeGate.check(&p, &ctx(&s, &fired, false)), Verdict::Deny { .. }));
        // untagged spec never gated
        let untagged = spec(SideEffect::Pure, &[], None);
        assert!(matches!(DedupeGate.check(&p, &ctx(&untagged, &fired, false)), Verdict::Allow));
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine`. Expected: compile error.

- [ ] **Step 3: Implement** the three guards per Interfaces:

```rust
use nscore::{ClassifiedProposal, Guard, GuardCtx, Provenance, ResidualRule, SideEffect, Trust, Verdict};

fn contains_residual(p: &Provenance) -> bool {
    match p {
        Provenance::Residual => true,
        Provenance::Transform { inputs, .. } => inputs.iter().any(contains_residual),
        _ => false,
    }
}

pub struct ResidualPolicy;

impl Guard for ResidualPolicy {
    fn name(&self) -> &str {
        "residual_policy"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        for (arg, tv) in &p.args {
            let forbidden = ctx.spec.residual_policy.get(arg) == Some(&ResidualRule::Never);
            if forbidden && contains_residual(&tv.prov) {
                return Verdict::Deny {
                    reason: format!("NeverResidual: arg '{arg}' has no grounding in this session"),
                };
            }
        }
        Verdict::Allow
    }
}

pub struct TaintPolicy;

impl Guard for TaintPolicy {
    fn name(&self) -> &str {
        "taint_policy"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        let gated = ctx.spec.side_effect != SideEffect::Pure
            || p.proposal.action == "ask_clarification";
        if !gated || ctx.confirmed_this_turn {
            return Verdict::Allow;
        }
        let tainted: Vec<&str> = p
            .args
            .iter()
            .filter(|(_, tv)| tv.trust == Trust::External)
            .map(|(k, _)| k.as_str())
            .collect();
        if tainted.is_empty() {
            Verdict::Allow
        } else {
            Verdict::NeedsConfirmation {
                prompt: format!(
                    "This uses data from an external source ({}). Proceed?",
                    tainted.join(", ")
                ),
            }
        }
    }
}

pub struct DedupeGate;

impl Guard for DedupeGate {
    fn name(&self) -> &str {
        "dedupe_gate"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        if ctx.spec.dedupe_tag.is_some() && ctx.fired_actions.contains(&p.proposal.action) {
            Verdict::Deny { reason: format!("'{}' already done this session", p.proposal.action) }
        } else {
            Verdict::Allow
        }
    }
}
```

In `turn.rs`: add `builtin_guards: Vec<Box<dyn Guard>>` to `Engine`, initialized in `with_clock` with the three guards; the guard loop iterates `self.builtin_guards.iter().chain(self.parts.guards.iter())`. (Borrow note: collect the needed `parts` references before the loop if the borrow checker objects.)

- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-engine`. All green (existing loop tests use Pure `echo` with grounded/free args — unaffected).

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(engine): residual, taint and dedupe guards wired ahead of plugin guards"
```

---

### Task 6: Engine — SideEffectGate + staging

**Files:**
- Modify: `crates/engine/src/guards.rs` (add SideEffectGate), `crates/engine/src/turn.rs` (staging on NeedsConfirmation)
- Test: inline in `guards.rs` + integration in `turn_loop.rs`

**Interfaces:**
- Produces:

```rust
/// Irreversible actions require a Confirmed event this turn; otherwise the
/// proposal is staged and the user is asked (spec §5.4, §9).
pub struct SideEffectGate;
// name() = "side_effect_gate".
// spec.side_effect == Irreversible && !ctx.confirmed_this_turn
//   -> NeedsConfirmation { prompt: "'<action>' is irreversible. Confirm to proceed." }
// else Allow.
```

- `turn.rs` NeedsConfirmation handling (any guard) becomes:
  1. resolve the tool (if the action is a real tool) and call `tool.stage(&proposal.args, &tool_ctx).await` → `staged: Option<StagedEffect>`;
  2. append `PendingConfirmation { proposal_of: pid, staged: staged.clone() }`;
  3. prompt text: guard's prompt, plus `"\nPlanned: <staged.description>"` when staging returned Some;
  4. `Settled { Verbatim { prompt } }`, break.
- SideEffectGate joins `builtin_guards` after DedupeGate.

- [ ] **Step 1: Write the failing tests**

Unit (in `guards.rs` tests):

```rust
    #[test]
    fn side_effect_gate_blocks_unconfirmed_irreversible() {
        let s = spec(SideEffect::Irreversible, &[], None);
        let fired = HashSet::new();
        let p = proposal("wipe", vec![]);
        assert!(matches!(
            SideEffectGate.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
        assert!(matches!(SideEffectGate.check(&p, &ctx(&s, &fired, true)), Verdict::Allow));
        let reversible = spec(SideEffect::Reversible, &[], None);
        assert!(matches!(SideEffectGate.check(&p, &ctx(&reversible, &fired, false)), Verdict::Allow));
    }
```

Integration (in `turn_loop.rs`) — needs an irreversible tool double with staging:

```rust
struct WipeTool {
    spec: ActionSpec,
}

impl WipeTool {
    fn new() -> Self {
        Self {
            spec: ActionSpec {
                name: "wipe".into(),
                description: "wipe the database".into(),
                args_schema: serde_json::json!({"type": "object", "properties": {}}),
                side_effect: SideEffect::Irreversible,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for WipeTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput { summary: "wiped".into(), artifact: None, trust: Trust::System })
    }
    async fn stage(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Option<StagedEffect> {
        Some(StagedEffect { description: "would delete 3 rows".into() })
    }
}

fn engine_with_tools(
    proposals: Vec<Proposal>,
    tools: Vec<Arc<dyn Tool>>,
    store: Arc<InMemoryStore>,
) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    for t in tools {
        b.add_tool(t);
    }
    Engine::with_clock(b.build().unwrap(), EngineConfig::default(), Box::new(|| Timestamp(42)))
}

#[tokio::test]
async fn irreversible_action_is_staged_not_executed() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with_tools(
        vec![Proposal { rationale: "wipe".into(), action: "wipe".into(), args: serde_json::json!({}) }],
        vec![Arc::new(WipeTool::new())],
        store.clone(),
    );
    let sid = SessionId("se1".into());
    let reply = e.run_turn(Incoming { session: sid.clone(), text: "wipe it".into() }).await.unwrap();
    assert!(reply.contains("irreversible"), "user is asked to confirm, got: {reply}");
    assert!(reply.contains("would delete 3 rows"), "staged effect is shown, got: {reply}");

    let events = store.load(&sid).await.unwrap();
    assert!(
        !events.iter().any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })),
        "the tool must NOT run before confirmation"
    );
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::PendingConfirmation { staged: Some(s), .. } if s.description == "would delete 3 rows"
    )));
    assert!(matches!(&events.last().unwrap().kind, EventKind::Replied { .. }));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine`. Expected: compile error (`SideEffectGate` missing).

- [ ] **Step 3: Implement** `SideEffectGate` per Interfaces; register it as the fourth built-in; rework the NeedsConfirmation arm in `turn.rs` per Interfaces (stage → PendingConfirmation with `staged` → enriched Verbatim prompt).

- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-engine`.

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(engine): side-effect gate stages irreversible actions behind confirmation"
```

---

### Task 7: Engine — rejection→narrowed schema + ask_clarification

**Files:**
- Modify: `crates/engine/src/turn.rs`
- Test: integration in `turn_loop.rs`

**Interfaces:**
- `turn.rs` gains (module-level):

```rust
pub const ASK_CLARIFICATION: &str = "ask_clarification";

fn ask_clarification_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: ASK_CLARIFICATION.into(),
        description: "Ask the user one short question to resolve missing or ungrounded \
                      information required by the next action."
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}
```

- Legal-set construction per iteration becomes:
  1. start from all tool specs;
  2. remove every action rejected this turn (`denied_this_turn: HashSet<String>`, fed by IllegalAction and GuardDenied rejections — narrowing per spec §2);
  3. append `ask_clarification_spec()`;
  4. if any rejection this turn contained the `NeverResidual` marker → legal set = `[ask_clarification_spec()]` ONLY (forced clarification; `respond_directly` stays available at schema level).
- Proposal handling: `action == ASK_CLARIFICATION` → classify args + run guards (TaintPolicy applies); on Allow → `Settled { Verbatim { text: question } }`, break (the question IS the reply; next turn's UserSaid answers it). Malformed/missing `question` → `Rejected { Malformed }`, continue.

- [ ] **Step 1: Write the failing tests** (append to `turn_loop.rs`)

```rust
#[tokio::test]
async fn denied_action_is_removed_from_next_legal_set() {
    // Emitter proposes "echo" twice; a plugin guard denies it. The second
    // proposal must be rejected as ILLEGAL (narrowed schema), not guard-denied.
    let store = Arc::new(InMemoryStore::new());
    let guard: Box<dyn Guard> = Box::new(DenyAction { action: "echo".into(), reason: "no".into() });
    let mut e = engine_with(
        vec![echo_proposal("a"), echo_proposal("b")],
        vec![guard],
        store.clone(),
    );
    let sid = SessionId("nar1".into());
    e.run_turn(Incoming { session: sid.clone(), text: "x".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    let reasons: Vec<&RejectReason> = events.iter().filter_map(|ev| match &ev.kind {
        EventKind::Rejected { reason, .. } => Some(reason),
        _ => None,
    }).collect();
    assert!(matches!(reasons[0], RejectReason::GuardDenied { .. }));
    assert!(
        matches!(reasons[1], RejectReason::IllegalAction { .. }),
        "second identical proposal must be illegal under the narrowed set, got {:?}",
        reasons[1]
    );
}

#[tokio::test]
async fn never_residual_rejection_forces_clarification() {
    // A tool whose arg may never be residual + an emitter that invents the arg,
    // then obediently asks a clarification question.
    let mut order_spec_tool = EchoTool::new(); // reuse echo's shape via a custom spec below
    let store = Arc::new(InMemoryStore::new());

    struct OrderTool { spec: ActionSpec }
    #[async_trait::async_trait]
    impl Tool for OrderTool {
        fn spec(&self) -> &ActionSpec { &self.spec }
        async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "ordered".into(), artifact: None, trust: Trust::System })
        }
    }
    let tool = OrderTool {
        spec: ActionSpec {
            name: "cancel_order".into(),
            description: "cancel an order".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {"order_id": {"type": "string"}},
                "required": ["order_id"]
            }),
            side_effect: SideEffect::Reversible,
            residual_policy: [("order_id".to_string(), ResidualRule::Never)].into_iter().collect(),
            dedupe_tag: None,
        },
    };
    let _ = &mut order_spec_tool; // silence unused warning if any

    let mut e = engine_with_tools(
        vec![
            Proposal {
                rationale: "cancel".into(),
                action: "cancel_order".into(),
                args: serde_json::json!({"order_id": "ORD-99"}), // invented: user never said this
            },
            Proposal {
                rationale: "need the id".into(),
                action: "ask_clarification".into(),
                args: serde_json::json!({"question": "Which order should I cancel?"}),
            },
        ],
        vec![Arc::new(tool)],
        store.clone(),
    );
    let sid = SessionId("clar1".into());
    let reply = e
        .run_turn(Incoming { session: sid.clone(), text: "cancel my order".into() })
        .await
        .unwrap();
    assert_eq!(reply, "Which order should I cancel?");

    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::GuardDenied { reason, .. }, .. }
            if reason.contains("NeverResidual")
    )));
    assert!(
        !events.iter().any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })),
        "cancel_order must not run on an invented id"
    );
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine`. Expected: first test fails on the reason assertion (second rejection is still GuardDenied), second fails because `ask_clarification` is an illegal action.

- [ ] **Step 3: Implement** per Interfaces: `denied_this_turn` set updated on every rejection append; legal-set construction rules 1–4; the `ASK_CLARIFICATION` proposal path (classify → guards → `Settled Verbatim(question)`); `never_residual_this_turn: bool` flag set when a GuardDenied reason contains `"NeverResidual"`.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-engine` and `cargo test` (workspace — ns-llm schema tests unaffected: synthetic specs flow through `build_tools` like any other).

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(engine): narrowed schema on rejection and forced ask_clarification"
```

---

### Task 8: Engine — confirmation flow (confirm_pending, re-execution, expiry)

**Files:**
- Modify: `crates/engine/src/turn.rs`
- Test: integration in `turn_loop.rs`

**Interfaces:**
- `turn.rs` gains:

```rust
pub const CONFIRM_PENDING: &str = "confirm_pending";

fn confirm_pending_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: CONFIRM_PENDING.into(),
        description: "The user has just confirmed the pending action; execute it.".into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}
```

- Legal set: when an ACTIVE pending confirmation exists (`state.pending_turn + 1 == turn`), append `confirm_pending_spec()`; the state summary line for the emitter gains `"\nPending confirmation: awaiting the user's yes/no on the staged action."`.
- Proposal handling: `action == CONFIRM_PENDING` →
  1. if no active pending → `Rejected { IllegalAction }`, continue;
  2. append `Confirmed { pending: <pending id> }`;
  3. look up the original proposal: the `PendingConfirmation.proposal_of` event's `Proposed { proposal }`; re-enter the normal pipeline for it this iteration (classify → guards, with the fold now yielding `confirmed_this_turn = true` → SideEffectGate/TaintPolicy pass) → perform.
- Expiry needs no code beyond the active-pending rule: a pending from turn N is simply not offered/accepted at turn N+2 or later.

- [ ] **Step 1: Write the failing tests** (append to `turn_loop.rs`)

```rust
#[tokio::test]
async fn confirmation_flow_executes_on_next_turn_yes() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("cf1".into());
    // Turn 1: propose wipe -> staged, asks for confirmation.
    {
        let mut e = engine_with_tools(
            vec![Proposal { rationale: "wipe".into(), action: "wipe".into(), args: serde_json::json!({}) }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "wipe it".into() }).await.unwrap();
    }
    // Turn 2: user says yes; emitter proposes confirm_pending.
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "user confirmed".into(),
                action: "confirm_pending".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        let reply = e.run_turn(Incoming { session: sid.clone(), text: "yes".into() }).await.unwrap();
        assert!(reply.contains("wiped"), "trace reply reports execution, got: {reply}");
    }
    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert!(kinds.contains(&"Confirmed"));
    assert!(kinds.contains(&"ToolCalled"), "the staged action ran after Confirmed");
    // paper trail order: PendingConfirmation before Confirmed before ToolCalled
    let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
    assert!(pos("PendingConfirmation") < pos("Confirmed"));
    assert!(pos("Confirmed") < pos("ToolCalled"));
}

#[tokio::test]
async fn pending_confirmation_expires_after_one_turn() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("cf2".into());
    {
        let mut e = engine_with_tools(
            vec![Proposal { rationale: "wipe".into(), action: "wipe".into(), args: serde_json::json!({}) }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "wipe it".into() }).await.unwrap();
    }
    // Turn 2: user changes the subject; scripted emitter falls through to respond_directly.
    {
        let mut e = engine_with_tools(vec![], vec![Arc::new(WipeTool::new())], store.clone());
        e.run_turn(Incoming { session: sid.clone(), text: "actually, what time is it?".into() })
            .await
            .unwrap();
    }
    // Turn 3: a late confirm_pending must be rejected as illegal and nothing runs.
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "late yes".into(),
                action: "confirm_pending".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "yes do it".into() }).await.unwrap();
    }
    let events = store.load(&sid).await.unwrap();
    assert!(!events.iter().any(|ev| matches!(ev.kind, EventKind::Confirmed { .. })));
    assert!(!events.iter().any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })));
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::IllegalAction { action }, .. }
            if action == "confirm_pending"
    )));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine`. Expected: confirm_pending is rejected as illegal in test 1 (not yet in the legal set).

- [ ] **Step 3: Implement** per Interfaces. Implementation notes:
  - Track `confirmed_this_iteration` locally after appending `Confirmed` so the same iteration's GuardCtx sees it (the fold also picks it up on the next `fold(log.events())` — re-fold after appending Confirmed and before classifying the original proposal).
  - The original proposal lookup: find the `PendingConfirmation` event by id (`active_pending`), read its `proposal_of`, then find that `Proposed` event and clone its proposal. Missing links → `Rejected { Malformed }` (log corruption is data, not a panic).
  - Re-execution reuses the same classify→guards→perform code path — factor the per-proposal pipeline into a helper method if duplication grows; keep the event sequence identical.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-engine`.

- [ ] **Step 5: Commit**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "feat(engine): two-turn confirmation flow with staged execution and expiry"
```

---

### Task 9: Workspace verification + live smoke

**Files:**
- No new files; `config.toml` (runtime only, gitignored) for the smoke run.

- [ ] **Step 1: Full workspace checks**

Run: `cargo test` (all crates) and `cargo clippy --workspace 2>&1 | tail -5`.
Expected: all green, no warnings. Fix anything that surfaces before proceeding.

- [ ] **Step 2: Live smoke (only if OPENROUTER_API_KEY is available)**

`cp config.example.toml config.toml`, set the model the user currently prefers, run `cargo run -p ns-app` and drive:
1. `what time is it?` → tool runs, real reply (M2 regression).
2. A prompt that tempts an invented argument (e.g. add a `[[http_component]]` with `order_id` + `residual_policy` once config supports it — if config plumbing for residual rules is not present, note it as follow-up and smoke only step 1).
Delete `ns.sqlite*` afterwards. Report transcript + event dump in the completion summary.

- [ ] **Step 3: Commit anything outstanding**

```bash
git add -A
git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz" commit -m "chore: M3 workspace verification"
```

(Skip the commit if the tree is already clean.)

---

## Self-review notes (performed at write time)

- **Spec coverage (M3 scope, §12):** provenance store + classification ✔ (T2–T3), trust propagation ✔ (T3, min-trust via Transform + leaf trust), ResidualPolicy/TaintPolicy/SideEffectGate/DedupeGate ✔ (T5–T6), rejection→narrowed-schema loop ✔ (T7), confirmation flow ✔ (T6+T8), ask_clarification mechanics ✔ (T7, incl. forced mode and TaintPolicy on questions).
- **Deliberate simplifications, documented in Global Constraints:** forced-clarification trigger approximates "no grounding source exists"; Residual trust = System; normalizers limited to trim/lowercase; per-arg residual rules for HttpTool config plumbing deferred (noted in T9 smoke); "off-list values dropped" delegated to strict schema.
- **Type consistency:** `GuardCtx` fields used by T5/T6 guards match T1's definition; `engine_with_tools`/`WipeTool` defined in T6 are used by T8; `ASK_CLARIFICATION`/`CONFIRM_PENDING` constants defined where used; `pending_turn`/`confirmed_this_turn_of` from T4 feed T6/T8's active-pending and confirmed-this-turn logic; `ResidualRule` derives `PartialEq` already (M1 T3 gave all action types `PartialEq`) — the `== Some(&ResidualRule::Never)` comparison in T5 is valid; `Provenance` already derives `Debug` for test `panic!` formatting.
- **Not in M3:** facts recall, artifacts, templates, replay harness, `dump` (M4); Telegram, evolution pass (M5+). The emitter-side prompt/schema already carry rejection reasons from M2 — no ns-llm changes needed in M3.
