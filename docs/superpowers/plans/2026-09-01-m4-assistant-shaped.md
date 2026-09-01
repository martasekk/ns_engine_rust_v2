# M4 Assistant-Shaped Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The harness becomes an assistant — standing facts with lifecycle metadata and implicit recall into replies, a `remember_fact` action whose stored provenance comes from classification, content-addressed artifacts for oversized tool outputs, a real template renderer, the replay harness (the hard dependency of all future self-improvement), and the `dump` subcommand.

**Architecture:** `ToolCtx` gains an optional artifact-store handle so tools can persist oversized content (`HttpTool` first). The engine grows three assistant behaviors: a synthetic `remember_fact` action (classify → `put_fact` with real provenance, logged as ToolCalled/ToolReturned), implicit fact recall into `ReplyContext` with a `uses` bump, and a config-driven template registry (`cant_help` replaces the hardcoded fallback when registered). A new `nsengine::replay` module re-feeds a recorded log through the engine with scripted doubles reconstructed from the log itself and reports the first divergence. `app` gains `ns-app dump <session>`.

**Tech Stack:** No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-01-neuro-symbolic-harness-design.md` (§4 Fact/ArtifactId, §5 reply context, §7 dump, §11 replay, §12 M4 scope)

## Global Constraints

- Persona config already shipped in M2 — out of scope here.
- Decision record (settled at plan time):
  - `ToolCtx` gains `pub artifacts: Option<std::sync::Arc<dyn MemoryStore>>` (None in unit tests; the engine passes its memory slot). Narrowing to a dedicated trait is deferred until a second consumer exists.
  - `remember_fact` is an engine-owned synthetic action (like `ask_clarification`): `args {key, value}` both required strings; the stored `Fact.prov` is the classification of the `value` arg — a fact remembered from the user's words carries `UserInput` provenance.
  - Implicit recall: `facts("")` capped at 20, recalled only on the `Generate` path; each recalled fact gets `uses += 1` written back (lifecycle metadata for the future consolidation pass).
  - Templates: `{name}` placeholder substitution, no escaping, unknown placeholders left verbatim; unregistered template ids keep M1's `"[{id}] {vars}"` rendering; when a `cant_help` template is registered it replaces the hardcoded fallback settle text.
  - Replay compares a normalized line per event — kind plus salient payload (Proposed action, Rejected reason variant, ToolCalled action, Replied text) — not raw events (timestamps/hashes legitimately differ). Chain verification of the recorded log runs first.
- TDD per task; commit per task with the given message; git identity `git -c user.name="Martin" -c user.email="hrabal@jtjdreams.cz"` + executing model's Co-Authored-By trailer; isolated worktree; delete the worktree's `target/` before merging (disk).

## File Structure (end state of M4)

```
crates/core/src/traits.rs           # ToolCtx + artifacts handle
crates/components-std/src/http_tool.rs  # oversized bodies -> artifact store
crates/engine/src/turn.rs           # remember_fact, fact recall, templates, ToolCtx wiring
crates/engine/src/replay.rs         # replay_session + ReplayError
crates/engine/src/lib.rs            # + pub mod replay;
app/src/config.rs                   # + [templates] section
app/src/main.rs                     # + dump subcommand, templates into EngineConfig
```

---

### Task 1: ToolCtx artifact handle + HttpTool artifact storage

**Files:**
- Modify: `crates/core/src/traits.rs` (`ToolCtx`), `crates/components-std/src/http_tool.rs`, `crates/engine/src/turn.rs` (two `ToolCtx` literals), plus every `ToolCtx { session: ... }` literal in tests (`crates/core/src/traits.rs`, `crates/engine/src/script.rs`, `crates/components-std/src/time_tool.rs`, `crates/components-std/src/http_tool.rs`, `crates/engine/tests/turn_loop.rs` if any)
- Test: inline in `http_tool.rs`

**Interfaces:**

```rust
// core/traits.rs
pub struct ToolCtx {
    pub session: SessionId,
    /// Artifact store for oversized tool content; None in unit tests.
    pub artifacts: Option<std::sync::Arc<dyn MemoryStore>>,
}
```

- All existing literals gain `artifacts: None`; `turn.rs` passes `artifacts: Some(self.parts.memory.clone())` in both its `ToolCtx` constructions (stage + call).
- `HttpTool::call`: serialize body once; when it exceeds `MAX_SUMMARY` AND `ctx.artifacts` is present, `put_artifact(full_body_bytes)` and set `ToolOutput.artifact = Some(id)`; summary stays truncated either way. Store errors degrade gracefully (artifact stays None — the summary is still useful).

- [ ] **Step 1: Write the failing test** (append to `http_tool.rs` tests; a minimal in-memory `MemoryStore` double lives in the test module):

```rust
    struct ArtifactSink(std::sync::Mutex<Vec<Vec<u8>>>);
    #[async_trait]
    impl nscore::MemoryStore for ArtifactSink {
        async fn append(&self, _s: &nscore::SessionId, _e: &[nscore::Event]) -> Result<(), nscore::StoreError> { Ok(()) }
        async fn load(&self, _s: &nscore::SessionId) -> Result<Vec<nscore::Event>, nscore::StoreError> { Ok(vec![]) }
        async fn facts(&self, _p: &str) -> Result<Vec<nscore::Fact>, nscore::StoreError> { Ok(vec![]) }
        async fn put_fact(&self, _f: nscore::Fact) -> Result<(), nscore::StoreError> { Ok(()) }
        async fn artifact(&self, _id: &nscore::ArtifactId) -> Result<Vec<u8>, nscore::StoreError> { Err(nscore::StoreError::NotFound) }
        async fn put_artifact(&self, content: Vec<u8>) -> Result<nscore::ArtifactId, nscore::StoreError> {
            let id = nscore::ArtifactId::for_content(&content);
            self.0.lock().unwrap().push(content);
            Ok(id)
        }
    }

    #[tokio::test]
    async fn oversized_body_is_stored_as_content_addressed_artifact() {
        let big = "x".repeat(5000);
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"blob": big})))]);
        let sink = std::sync::Arc::new(ArtifactSink(std::sync::Mutex::new(Vec::new())));
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()), artifacts: Some(sink.clone()) },
            )
            .await
            .unwrap();
        assert!(out.summary.len() <= 2000, "summary still truncated");
        let stored = sink.0.lock().unwrap();
        assert_eq!(stored.len(), 1, "full body stored once");
        let expected_id = nscore::ArtifactId::for_content(&stored[0]);
        assert_eq!(out.artifact, Some(expected_id), "artifact id is the content hash");
        assert!(stored[0].len() > 5000, "the FULL body was stored");
    }

    #[tokio::test]
    async fn small_body_stores_no_artifact() {
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"ok": true})))]);
        let sink = std::sync::Arc::new(ArtifactSink(std::sync::Mutex::new(Vec::new())));
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()), artifacts: Some(sink.clone()) },
            )
            .await
            .unwrap();
        assert_eq!(out.artifact, None);
        assert!(sink.0.lock().unwrap().is_empty());
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-components-std`. Expected: compile error (`artifacts` field missing).
- [ ] **Step 3: Implement** — add the field to `ToolCtx`; sweep all literals (`artifacts: None` in tests, `Some(self.parts.memory.clone())` in `turn.rs`); rework `HttpTool::call`'s tail:

```rust
        let full = body.to_string();
        let mut artifact = None;
        if full.len() > MAX_SUMMARY {
            if let Some(store) = &ctx.artifacts {
                artifact = store.put_artifact(full.clone().into_bytes()).await.ok();
            }
        }
        let mut summary = full;
        if summary.len() > MAX_SUMMARY {
            let mut end = MAX_SUMMARY;
            while !summary.is_char_boundary(end) {
                end -= 1;
            }
            summary.truncate(end);
        }
        Ok(ToolOutput { summary, artifact, trust: Trust::External })
```

- [ ] **Step 4: Run to verify pass** — `cargo test` (whole workspace).
- [ ] **Step 5: Commit** — `feat(core,components-std): tools store oversized content as content-addressed artifacts`

---

### Task 2: remember_fact + implicit recall with uses bump

**Files:**
- Modify: `crates/engine/src/turn.rs`
- Test: integration in `crates/engine/tests/turn_loop.rs`

**Interfaces:**

```rust
pub const REMEMBER_FACT: &str = "remember_fact";

fn remember_fact_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: REMEMBER_FACT.into(),
        description: "Store one durable fact about the user or task as key/value \
                      (dotted keys, e.g. user.name)."
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" },
                "value": { "type": "string" }
            },
            "required": ["key", "value"]
        }),
        side_effect: nscore::SideEffect::Reversible,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}
```

- Legal set: `remember_fact_spec()` joins `ask_clarification_spec()` in the non-forced arm.
- New branch **f4** (after f2, before g): `action == REMEMBER_FACT` → missing/non-string `key` or `value` → `Rejected { Malformed }` + continue. Else classify args against `remember_fact_spec()`; build `Fact { key, value: json!(value), confidence: 1.0, uses: 0, last_validated: now(), prov: <classified prov of the "value" arg, Residual if absent> }`; `put_fact` (store error → `ToolOutcome::Err`); append `ToolCalled { action: REMEMBER_FACT, args: <classified> }` + `ToolReturned { outcome: Ok { ToolOutput { summary: format!("remembered {key}"), artifact: None, trust: Trust::System } } }` (or the Err); `continue` the loop (emitter decides what's next).
- `Generate` reply path: before building `ReplyContext`, `let mut facts = self.parts.memory.facts("").await.unwrap_or_default(); facts.truncate(20);` then for each, write back `uses + 1` via `put_fact` (ignore errors) and pass the bumped copies as `ReplyContext.facts`.

- [ ] **Step 1: Write the failing tests** (append to `turn_loop.rs`):

```rust
struct FactsProbe;
#[async_trait::async_trait]
impl Replier for FactsProbe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        let lines: Vec<String> =
            ctx.facts.iter().map(|f| format!("{}={} uses={}", f.key, f.value, f.uses)).collect();
        Ok(format!("FACTS[{}]", lines.join(";")))
    }
}

#[tokio::test]
async fn remember_fact_stores_classified_fact_and_recall_bumps_uses() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("facts1".into());
    // Turn 1: remember the user's name (value is a span of the user's words).
    {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "durable".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": "user.name", "value": "Martin"}),
        }])));
        b.set_replier(Box::new(FactsProbe));
        b.set_memory(store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        let reply = e
            .run_turn(Incoming { session: sid.clone(), text: "my name is Martin".into() })
            .await
            .unwrap();
        // Generate path recalls the just-stored fact (uses already bumped to 1)
        assert!(reply.contains("user.name=\"Martin\" uses=1"), "got: {reply}");
    }
    let stored = store.facts("user").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        matches!(stored[0].prov, Provenance::UserInput { .. }),
        "fact provenance comes from classification, got {:?}",
        stored[0].prov
    );
    assert_eq!(stored[0].uses, 1, "recall bumped lifecycle metadata");

    // The log carries the paper trail.
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::ToolCalled { action, .. } if action == "remember_fact"
    )));
}

#[tokio::test]
async fn remember_fact_without_value_is_malformed() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: "bad".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": "user.name"}),
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("facts2".into());
    e.run_turn(Incoming { session: sid.clone(), text: "hi".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::Malformed { .. }, .. }
    )));
    assert!(store.facts("").await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ns-engine --test turn_loop`. Expected: remember_fact rejected as illegal → assertions fail.
- [ ] **Step 3: Implement** per Interfaces (constant + spec fn + legal-set line + f4 branch + Generate-path recall/bump).
- [ ] **Step 4: Run to verify pass** — `cargo test -p ns-engine`.
- [ ] **Step 5: Commit** — `feat(engine): remember_fact action and implicit fact recall with lifecycle bump`

---

### Task 3: Template renderer

**Files:**
- Modify: `crates/engine/src/turn.rs`
- Test: unit in `turn.rs` + integration in `turn_loop.rs`

**Interfaces:**

```rust
// EngineConfig gains:
pub templates: std::collections::HashMap<String, String>,   // Default: empty

// turn.rs module fn:
fn render_template(template: &str, vars: &serde_json::Value) -> String;
// replaces "{name}" with vars["name"] (string form without quotes for strings);
// unknown placeholders left verbatim.
```

- `ReplyPolicy::Template { id, vars }` arm: registered id → `render_template`; unregistered → keep `format!("[{id}] {vars}")`.
- Fallback settle: `templates.contains_key("cant_help")` → `Settled { Template { id: "cant_help", vars: {} } }` else the current Verbatim.

- [ ] **Step 1: Failing tests.** Unit (in `turn.rs` `#[cfg(test)]`):

```rust
    #[test]
    fn render_substitutes_known_placeholders_only() {
        let vars = serde_json::json!({"name": "Martin", "n": 3});
        assert_eq!(
            render_template("Hi {name}, {n} items, {missing} stays", &vars),
            "Hi Martin, 3 items, {missing} stays"
        );
    }
```

Integration (in `turn_loop.rs`; `FailingEmitter` always errors, driving the fallback):

```rust
struct FailingEmitter;
#[async_trait::async_trait]
impl Emitter for FailingEmitter {
    async fn propose(
        &self,
        _ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        Err(EmitError::Transport("down".into()))
    }
}

#[tokio::test]
async fn registered_cant_help_template_replaces_fallback() {
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(FailingEmitter));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        templates: [("cant_help".to_string(), "Promiň, to nezvládnu.".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let reply = e
        .run_turn(Incoming { session: SessionId("tpl1".into()), text: "x".into() })
        .await
        .unwrap();
    assert_eq!(reply, "Promiň, to nezvládnu.");
}
```

- [ ] **Step 2: Verify failure** (compile error: `templates` field). 
- [ ] **Step 3: Implement**; `EngineConfig::default` gains `templates: Default::default()`.
- [ ] **Step 4: Verify pass** — `cargo test` (app compiles: it uses struct construction with all fields — update `app/src/main.rs` EngineConfig literal with `templates: Default::default()` placeholder until Task 5 wires config).
- [ ] **Step 5: Commit** — `feat(engine): config-driven template renderer with cant_help fallback`

---

### Task 4: Replay harness

**Files:**
- Create: `crates/engine/src/replay.rs`; modify `crates/engine/src/lib.rs` (`pub mod replay;`)
- Test: inline in `replay.rs`

**Interfaces:**

```rust
use nscore::{Event, EventKind, Guard, SessionId};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("recorded chain broken: {0}")]
    ChainBroken(String),
    #[error("divergence at line {at}: expected `{expected}`, got `{got}`")]
    Divergence { at: usize, expected: String, got: String },
    #[error("replay produced {got} lines, recording has {expected}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("engine error during replay: {0}")]
    Engine(String),
}

/// Re-feed a recorded session through the engine: the recorded proposals
/// drive a scripted emitter, recorded tool outcomes drive replay tools,
/// recorded replies drive a queue replier. Any behavioral change in the
/// engine (guards, narrowing, settling) surfaces as a Divergence.
/// `extra_guards` reproduces the plugin guards the recording ran with.
pub async fn replay_session(
    session: SessionId,
    recorded: &[Event],
    extra_guards: Vec<Box<dyn Guard>>,
) -> Result<(), ReplayError>;

/// One normalized line per event: kind + salient payload. Timestamps and
/// hashes are legitimately different on replay and excluded.
pub fn normalize(events: &[Event]) -> Vec<String>;
```

`normalize` lines: `UserSaid <text>` / `Proposed <action>` / `Rejected <variant-name>` / `ToolCalled <action>` / `ToolReturned <Ok|Err>` / `PendingConfirmation` / `Confirmed` / `Corrected` / `Settled <Verbatim|Template|Generate>` / `Replied <text>`.

`replay_session` algorithm:
1. `EventLog::from_events(session, recorded.to_vec()).verify_chain()` → `ChainBroken` on error.
2. Collect from the recording: user inputs `Vec<String>` (UserSaid order), proposals `Vec<Proposal>` (Proposed order), per-action outcome queues `HashMap<String, VecDeque<ToolOutcome>>` (ToolCalled action paired with its following ToolReturned via `call` id), reply texts `VecDeque<String>` (Replied order), plus the set of action names seen in ToolCalled (→ one `ReplayTool` each, spec: permissive object schema, `SideEffect::Pure`, empty residual policy).
3. Assemble a fresh harness: `ScriptedEmitter::new(proposals)`, `QueueReplier` (pops reply texts; error when empty), `InMemoryStore`, `NullChannel`-style closed channel, `NoopConsolidator`, the replay tools, `extra_guards`. `EngineConfig::default()` with fixed clock.
4. Run `run_turn` per user input; map engine errors to `ReplayError::Engine`.
5. `normalize(recorded)` vs `normalize(&new_log)`; first differing line → `Divergence`; length mismatch → `LengthMismatch`.

Note: `ReplayTool`, `QueueReplier`, and the closed channel are private types inside `replay.rs` implementing the core traits with the obvious 10-line bodies (`ReplayTool::call` pops its queue: `Ok{output}` → `Ok(output)`, `Err{kind,detail}` → `Err(ToolError::Failed{kind,detail})`, empty queue → `Err(ToolError::Failed{kind:"replay", detail:"outcome queue exhausted"})`).

- [ ] **Step 1: Failing tests** (in `replay.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{DenyAction, EchoTool, ScriptedEmitter, ScriptedReplier};
    use crate::store::{InMemoryStore, NoopConsolidator};
    use crate::turn::{Engine, EngineConfig};
    use nscore::*;
    use std::sync::Arc;

    struct ClosedChannel;
    #[async_trait::async_trait]
    impl Channel for ClosedChannel {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    async fn record_session(guards: Vec<Box<dyn Guard>>) -> (SessionId, Vec<Event>) {
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("rec".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "echo".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "replay me"}),
        }])));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        for g in guards {
            b.add_guard(g);
        }
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "please replay me".into() })
            .await
            .unwrap();
        let events = store.load(&sid).await.unwrap();
        (sid, events)
    }

    #[tokio::test]
    async fn faithful_replay_of_a_recorded_session_passes() {
        let (sid, events) = record_session(vec![]).await;
        replay_session(sid, &events, vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn replay_detects_behavioral_divergence() {
        // Recorded WITH a guard that denied echo; replayed WITHOUT it,
        // the engine now executes echo instead of rejecting -> divergence.
        let guard: Box<dyn Guard> =
            Box::new(DenyAction { action: "echo".into(), reason: "no".into() });
        let (sid, events) = record_session(vec![guard]).await;
        let err = replay_session(sid, &events, vec![]).await.unwrap_err();
        assert!(matches!(err, ReplayError::Divergence { .. } | ReplayError::LengthMismatch { .. }));
    }

    #[tokio::test]
    async fn replay_refuses_a_tampered_recording() {
        let (sid, mut events) = record_session(vec![]).await;
        if let EventKind::UserSaid { text } = &mut events[0].kind {
            *text = "TAMPERED".into();
        }
        let err = replay_session(sid, &events, vec![]).await.unwrap_err();
        assert!(matches!(err, ReplayError::ChainBroken(_)));
    }
}
```

- [ ] **Step 2: Verify failure** (compile error). 
- [ ] **Step 3: Implement** per algorithm. 
- [ ] **Step 4: Verify pass** — `cargo test -p ns-engine`.
- [ ] **Step 5: Commit** — `feat(engine): replay harness — recorded logs re-run and diffed as fixtures`

---

### Task 5: App — [templates] config + dump subcommand

**Files:**
- Modify: `app/src/config.rs`, `app/src/main.rs`
- Test: config test inline; `render_dump` unit test in `main.rs`

**Interfaces:**
- `AppConfig` gains `#[serde(default)] pub templates: std::collections::HashMap<String, String>` (TOML `[templates]` table of `id = "text"`); wired into `EngineConfig.templates`.
- `main`: `ns-app dump <session_id>` → read config (store path), open `SqliteStore`, `load(SessionId(arg))`, print `render_dump(&events)` (one `serde_json::to_string(event)` JSONL line per event) and exit; unknown/absent subcommand → run the engine as before.

```rust
fn render_dump(events: &[nscore::Event]) -> String;  // JSONL, one event per line
```

- [ ] **Step 1: Failing tests.** Config (append to `config.rs` tests):

```rust
    #[test]
    fn templates_section_parses_and_defaults_empty() {
        let cfg = AppConfig::parse("[templates]\ncant_help = \"Sorry.\"\n").unwrap();
        assert_eq!(cfg.templates.get("cant_help").map(String::as_str), Some("Sorry."));
        assert!(AppConfig::parse("").unwrap().templates.is_empty());
    }
```

Dump (in `main.rs` `#[cfg(test)]`):

```rust
    #[test]
    fn render_dump_is_one_json_line_per_event() {
        let mut log = nscore::EventLog::new(nscore::SessionId("d".into()));
        log.append(1, nscore::Timestamp(1), nscore::EventKind::UserSaid { text: "hi".into() });
        log.append(1, nscore::Timestamp(2), nscore::EventKind::Replied { text: "ho".into() });
        let out = render_dump(log.events());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"]["type"], "UserSaid");
    }
```

- [ ] **Step 2: Verify failure.** 
- [ ] **Step 3: Implement** (`render_dump`, the `dump` arg branch before the API-key check — dumping needs no key —, config plumbing `templates: cfg.templates.clone()` into `EngineConfig`).
- [ ] **Step 4: Verify pass** — `cargo test` + `cargo clippy --workspace`.
- [ ] **Step 5: Commit** — `feat(app): templates config and dump subcommand`

---

### Task 6: Verification + live smoke

- [ ] Full `cargo test` + clippy clean.
- [ ] Live smoke (needs `OPENROUTER_API_KEY`): `config.toml` with the user's current model; drive `cargo run -p ns-app` with `my name is Martin, remember it` then `/quit`; then a second run with `what do you know about me?` (same sqlite file) — expect recall; `ns-app dump cli` prints the JSONL log. Clean up `ns.sqlite*`/`config.toml` after; report transcript + dump excerpt.
- [ ] Commit anything outstanding: `chore: M4 workspace verification`.

## Self-review notes (performed at write time)

- **Spec coverage (M4, §12):** facts + lifecycle + implicit recall ✔ (T2: prov from classification, uses bump on recall), content-addressed artifacts ✔ (T1: HttpTool oversized bodies, id = content hash), templates ✔ (T3), persona config ✔ (M2, noted), replay harness ✔ (T4: chain check + scripted re-run + normalized diff; divergence test removes a guard), dump ✔ (T5, JSONL per §7).
- **Type consistency:** `ToolCtx.artifacts` type matches turn.rs `self.parts.memory` (`Arc<dyn MemoryStore>`); `remember_fact` uses `now()` clock for `last_validated`; `FailingEmitter`/`FactsProbe`/`QueueReplier` defined where used; `normalize` covers all 10 EventKind variants.
- **Deliberate simplifications:** recall prefix `""` cap 20 (scoped recall is a later projection); uses-bump write-per-recall accepted at CLI scale; replay compares normalized lines not full events (timestamps/hashes differ by design); `remember_fact` is Reversible (facts can be overwritten) so TaintPolicy gates external-driven values automatically.
- **Not in M4:** vector memory, consolidation jobs, Telegram, evolution pass (M5+).
