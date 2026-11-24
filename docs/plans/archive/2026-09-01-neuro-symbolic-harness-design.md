# Neuro-Symbolic Assistant Harness — Design Spec

Date: 2026-09-01
Status: approved design, pre-implementation
Research base: `docs/research/2026-09-01-findings.md` (every claim below is sourced there)

## 1. Goal

A general-purpose chatbot-assistant harness in Rust with a strict neural/symbolic split:

- A **symbolic engine** owns all state, control flow, and side effects. It decides *what is
  legal* at every step.
- A **neural command emitter** (small, cheap cloud LLM) reads text and proposes typed
  commands. It decides *how* to interpret the user — and nothing else.
- A separate **reply LLM** (stronger cloud model) turns the turn's execution trace into the
  user-facing answer, from a focused, bounded context.
- Everything is modular: components (tools), guards, channels, memory backends, and both
  LLM roles are swappable plugins behind traits in a tiny `core` crate.

First deployment target after v1: replacing the "Tomáš" Telegram sales bot (currently n8n)
as a plugin/config set. The harness itself stays domain-agnostic.

### Non-goals (v1)

- Local/self-hosted inference (hardware-limited; `Emitter` trait reserves the slot for a
  future llguidance backend).
- Vector/semantic memory search (keyed facts only; trait allows a later impl).
- The evolution pass itself (mining, compilation, speculation, tool synthesis,
  consolidation jobs) — v1 only guarantees its inputs: complete logs, side-effect classes,
  lifecycle metadata, and plugin slots.
- Multi-user auth, multi-tenant sessions, WASM tool host.

## 2. Architecture

```
                        ┌────────────────────────────────────────────────┐
                        │                 SYMBOLIC ENGINE                │
                        │              (owns state + control)            │
 ┌─────────┐  UserSaid  │  ┌─────────┐   legal action set   ┌─────────┐  │
 │ Channel ├───────────►│  │ PROJECT │──────────────────────► SCHEMA  │  │
 │(CLI/TG) │            │  │ (state) │                       │ BUILDER │  │
 └────▲────┘            │  └─────────┘                       └────┬────┘  │
      │                 │                                        │ per-turn
      │                 │                                        │ JSON schema
      │                 │  ┌──────────────────────────┐    ┌────▼─────────┐
      │                 │  │ GUARD + PROVENANCE CHECK │◄───┤   EMITTER    │
      │                 │  │  args classified:        │    │  (small LLM, │
      │                 │  │  Constant/User/Copied/   │    │  constrained │
      │                 │  │  Transform/Residual      │    │   output)    │
      │                 │  │  × trust: User/Sys/Ext   │    └──────────────┘
      │                 │  └───────┬──────────────────┘
      │                 │     ok?  │ rejected → re-prompt with reason ↑
      │                 │          ▼
      │                 │  ┌─────────┐  ToolCalled/ToolReturned  ┌────────────┐
      │                 │  │ PERFORM ├───────────────────────────► COMPONENTS │
      │                 │  └────┬────┘   (events appended)       │ (plugins)  │
      │                 │       │ engine may loop: new state →   └────────────┘
      │                 │       │ new legal actions → emitter again
      │                 │       ▼
      │                 │  ┌─────────┐ focused context (trace     ┌────────────┐
      │                 │  │ SETTLE  ├─── + outcomes + refusals) ─► REPLY LLM  │
      │                 │  └────┬────┘                            │ (stronger) │
      │                 │       │                                 └─────┬──────┘
      │   Replied       │       ▼                                       │
      └─────────────────┼── event log ◄─────────────────────────────────┘
                        │  (append-only, hash-chained, per session)
                        └────────────────────────────────────────────────┘
```

The **bidirectional channel** engine→emitter is the per-turn strict JSON schema compiled
from the current `LegalActionSet`; the emitter physically cannot propose an illegal action.
Rejections flow back as diagnostic re-prompts with a *narrowed* schema (denied tool removed
from the enum). Everything crossing a box boundary is an event.

Core principles (each backed by the research file):

1. **Compliance-by-construction.** Constraints live in guards and schemas, never restated
   in prompts (prompt constraints erode over turns).
2. **The log is the truth.** Flat, append-only, typed, hash-chained event log per session;
   all state is a fold; all contexts are projections; replay is the test harness.
3. **Provenance over trust-the-model.** Every emitted argument is classified per instance
   against this session's history; classification is bookkeeping, not model self-report.
4. **Small legal sets.** The engine prunes to the fewest applicable actions per state —
   accuracy and safety point the same way.
5. **The reply model narrates only the trace.** Outcomes and refusal reasons included;
   entities absent from the trace are out of bounds.

## 3. Workspace layout

```
rust_engine/
├── Cargo.toml                  # workspace
├── crates/
│   ├── core/                   # contract crate: types + traits only, serde-only deps
│   │   ├── event.rs            # Event, EventKind, EventLog, SessionId
│   │   ├── value.rs            # TaggedValue, Provenance, Trust
│   │   ├── action.rs           # ActionSpec, SideEffect, LegalActionSet, Proposal
│   │   ├── traits.rs           # Tool, Guard, Emitter, Replier, Channel,
│   │   │                       #   MemoryStore, Projection, Consolidator
│   │   └── plugin.rs           # HarnessPlugin, HarnessBuilder
│   ├── engine/                 # fold/project/perform/settle loop; schema builder
│   ├── provenance/             # value index, matching, binding classification
│   ├── llm/                    # cloud clients; CloudEmitter, CloudReplier
│   ├── components-std/         # built-in tools; HttpTool (Windmill/n8n escape hatch)
│   ├── memory-sqlite/          # EventLog + facts + artifacts persistence
│   ├── channel-cli/            # REPL
│   └── channel-telegram/       # later (teloxide)
└── app/                        # binary: config + plugin assembly, the only wiring point
```

Rules:

- `core` depends on nothing in the workspace and almost nothing outside (serde,
  async-trait). Every other crate depends on `core` and never on each other.
  ("Almost": if a type needs it, `thiserror` is acceptable; nothing heavier.)
- The tool boundary is JSON-shaped in both directions (WASM-later guarantee; cloud
  tool-calling requires it anyway).
- `app` is the only crate that reads config and the only crate that knows which plugins
  exist.

## 4. Core types

```rust
// event.rs
struct Event {
    id: EventId,               // monotonic u64 per session
    parent: Option<EventId>,   // fork point; None = linear
    prev_hash: [u8; 32],       // hash chain: tamper-evident, verified on replay
    turn: u32,
    at: Timestamp,
    kind: EventKind,
}

enum EventKind {
    UserSaid { text: String },
    Proposed { proposal: Proposal },
    Rejected { proposal_id: EventId, reason: RejectReason },
    ToolCalled { action: String, args: Vec<TaggedValue> },
    ToolReturned { call: EventId, outcome: ToolOutcome },   // Ok | Err — errors are data
    PendingConfirmation { proposal_id: EventId, staged: Option<StagedEffect> },
    Confirmed { pending: EventId },
    Corrected { target: Option<EventId>, text: String },    // user contradicts/fixes us
    Settled { policy: ReplyPolicy },
    Replied { text: String },
}

// value.rs
enum Trust { User, System, External }     // min-trust propagates along chains

enum Provenance {
    Constant,
    UserInput   { turn: u32, span: Range<u32> },
    CopiedOutput{ call: EventId, path: JsonPath },
    Transform   { func: String, inputs: Vec<Provenance> },
    Residual,                              // matched nothing: the model made it up
}

struct TaggedValue { value: serde_json::Value, prov: Provenance, trust: Trust }

// action.rs
enum SideEffect { Pure, Reversible, Irreversible }

struct ActionSpec {
    name: String,
    args: ArgSchema,                       // JSON schema + per-arg policies
    side_effect: SideEffect,
    residual_policy: HashMap<ArgName, ResidualRule>,  // e.g. OrderId: NeverResidual
    guards: Vec<GuardId>,
}

struct Proposal {
    rationale: String,        // FIRST field in the emitted schema (think-then-commit)
    action: String,           // enum over the legal set + "ask_clarification"
    args: serde_json::Value,  //   + "respond_directly"
}

// Proposal after provenance classification — what guards actually see:
struct ClassifiedProposal { proposal: Proposal, args: Vec<TaggedValue> }
```

`ArtifactId` = SHA-256 of content (dedup; verifiable `CopiedOutput` claims).

## 5. The turn pipeline

Per user turn, the engine loop (max `N_iter`, config, default 5):

1. **PROJECT** — fold log (cached snapshot + new events) → `SessionState`; derive
   `LegalActionSet` (aggressively pruned). `ask_clarification` enters the set when any
   plausible next action has a required arg unresolvable by provenance; it is *forced*
   (sole action besides `respond_directly`) when the engine has rejected on
   `NeverResidual` grounds and no grounding source exists.
2. **SCHEMA BUILD** — `LegalActionSet` → strict JSON schema; `rationale` first, action
   enum, per-action arg schemas.
3. **EMIT** — `Emitter::propose(ctx, legal)` with focused `EmitterContext` (state summary,
   last few turns, this turn's prior rejections with reasons). Temperature 0.
4. **CLASSIFY + GUARD** — provenance classification of every arg (order: tool-result paths
   → user spans → constants → registered normalizers → Residual), min-trust propagation;
   then guards: `ResidualPolicy`, `TaintPolicy` (side-effectful actions and clarification
   questions may not be driven by External-trust values without confirmation),
   `SideEffectGate` (Irreversible ⇒ `Confirmed` event this turn, else stage +
   `PendingConfirmation`; shows `Tool::stage()` output when available), `DedupeGate`
   ((session, action_tag) fires once; off-list values dropped, never coerced), plus
   plugin-registered guards. Verdict: `Allow | Deny{reason} | NeedsConfirmation{prompt}`.
   Deny → append `Rejected`, narrow schema, back to 3.
5. **PERFORM** — execute the tool; append `ToolCalled`/`ToolReturned` (errors are data).
   Interface note: `perform` takes a *set* of calls so a later speculation pass can run
   `Pure` tools in parallel with emission — v1 always passes exactly one.
   Loop to 1 if the engine's policy wants another action this turn.
6. **SETTLE** — choose `ReplyPolicy`:
   - `Verbatim(String)` — engine knows the answer (confirmations, canned flows); no LLM.
   - `Template(id, vars)` — deterministic fill-in.
   - `Generate` — build `ReplyContext` and call `Replier`.
7. **REPLY** — append `Replied`; channel delivers.

Every turn terminates in `Replied` — after `N_iter` or emitter failure ×3, settle falls
through to `Template(cant_help)` with the rejection trail in the log.

### Reply context layout (cache-aware, drift-resistant)

`ReplyContext` blocks in fixed order: persona (static) → standing facts (stable) →
session summary (bounded length, append-mostly) → this turn's trace with outcomes and
refusal reasons (dynamic). Stable-first ordering targets provider prefix caching
(85–95% input savings on hits); fresh persona per call + bounded projection is the
structural mitigation for long-session persona drift. The replier never sees raw
accumulated history, tool schemas, or the raw log.

### Emitter context

Deliberately tiny; cache losses are pennies, so per-turn narrowing wins (tool-set bloat
measurably degrades accuracy). Narrowing lives in the schema parameter, not prose.

## 6. Traits (contract surface, `core/traits.rs`)

```rust
#[async_trait] trait Tool {
    fn spec(&self) -> &ActionSpec;
    async fn call(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput>;
    async fn stage(&self, args: &serde_json::Value, ctx: &ToolCtx)
        -> Option<StagedEffect> { None }          // dry-run; default: not stageable
}
struct ToolOutput { summary: String, artifact: Option<ArtifactRef>, trust: Trust }
// Tools fetching external content MUST return trust: External.

trait Guard {
    fn check(&self, p: &ClassifiedProposal, s: &SessionState) -> Verdict;
}

#[async_trait] trait Emitter {
    async fn propose(&self, ctx: EmitterContext, legal: &LegalActionSet)
        -> Result<Proposal, EmitError>;
}

#[async_trait] trait Replier {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError>;
}

#[async_trait] trait Channel {
    async fn recv(&mut self) -> Result<Incoming>;
    async fn send(&self, session: SessionId, text: &str) -> Result<()>;
    // send() is callable WITHOUT a pending incoming turn: outbound-initiated
    // messages are required for future proactive behavior (reminders, alerts).
}

#[async_trait] trait MemoryStore {
    async fn append(&self, session: SessionId, events: &[Event]) -> Result<()>;
    async fn load(&self, session: SessionId) -> Result<Vec<Event>>;
    async fn facts(&self, scope: FactScope) -> Result<Vec<Fact>>;
    async fn put_fact(&self, fact: Fact) -> Result<()>;
    async fn artifact(&self, id: ArtifactId) -> Result<Artifact>;
    async fn put_artifact(&self, a: Artifact) -> Result<ArtifactId>;
}
struct Fact {
    key: String, value: serde_json::Value,
    confidence: f32, uses: u32, last_validated: Timestamp, prov: Provenance,
}   // lifecycle metadata: required by the future consolidation pass

#[async_trait] trait Consolidator {      // idle-time jobs; v1 ships NoopConsolidator
    async fn run(&self, store: &dyn MemoryStore) -> Result<()>;
}

trait HarnessPlugin { fn build(&self, app: &mut HarnessBuilder); }
```

Builder slots: exactly-one — `Emitter`, `Replier`, `MemoryStore`, `Channel`,
`Consolidator`; collections — `Tool`, `Guard`, `Projection`. `build()` fails loudly on
missing/duplicate exactly-one slots.

## 7. Persistence

SQLite via `rusqlite`, WAL mode, one file. Tables: `events(session_id, id, parent,
prev_hash, turn, at, kind_json)`, `facts`, `artifacts(id = content hash)`. In memory:
`Vec<Event>` + cached fold per active session. Snapshots (persist fold every N events):
deferred until sessions are long enough to hurt. Value index: in-memory `HashMap`
rebuilt on session load (`aho-corasick` upgrade path documented in `provenance`).
A `dump` subcommand prints any session as JSONL for eyeballing.

## 8. Configuration

One `config.toml`, read only by `app`:

```toml
[llm.emitter]  model = "claude-haiku-4-5"   # cheap; temperature 0
[llm.replier]  model = "claude-sonnet-5"    # defaults; any provider via llm crate
[engine]       max_iterations = 5, max_emit_retries = 3
[persona]      text = "..."
[[http_component]]                    # Windmill/n8n scripts as tools, no recompile
name = "check_stock"; url = "..."; side_effect = "pure"; args_schema = "..."
[guards.residual]  OrderId = "never" # per-arg-type residual rules
```

Crates receive typed config structs from `app`; none read files themselves.

## 9. Error handling

- The engine never panics on model output; every failure becomes an event; every turn ends
  in `Replied`.
- Malformed emitter output → `Rejected{Malformed}` → retry (≤3) → template fallback.
- Tool errors are `ToolOutcome::Err` events; the loop decides (retry / alternative /
  settle with failure in reply context — the replier reports failure truthfully).
- Cloud transport failures: bounded backoff retries inside `llm`; if exhausted, nothing is
  appended and the channel surfaces a retry message — the log never records a half-turn.
- `NeedsConfirmation` → `PendingConfirmation` (+ staged effect when available); next user
  turn carries `Confirmed` or the proposal expires. Irreversible actions therefore always
  have a two-event paper trail.

## 10. Observability

`tracing` spans in the engine loop mirroring event kinds, attribute names following the
OpenTelemetry GenAI semantic conventions (model, token counts, tool name, outcome).
OTLP exporter = later plugin; no vendor SDKs.

## 11. Testing

1. **Engine tests (majority):** `ScriptedEmitter` + `ScriptedTool` drive full turns; assert
   on the event log. No network, no LLM, milliseconds.
2. **Replay tests:** recorded sessions re-fed after refactors; the log is the fixture.
   Hash-chain verification runs here. NOTE: replay is the hard dependency of all future
   self-improvement (verified-before-write: a distilled rule commits only if replaying the
   logged failure with the patch flips the outcome).
3. **Live smoke tests:** small suite, real cheap models, manual/nightly; asserts schema
   compliance and guard behavior only.

## 12. Milestones

- **M1 — Skeleton.** Workspace; `core` complete; builder + plugins; CLI channel; engine
  loop with `ScriptedEmitter`; in-memory store. *Proves the architecture.*
- **M2 — Real loop.** SQLite persistence; `CloudEmitter` (per-turn schema);
  `CloudReplier`; 2–3 tools incl. `HttpTool`; `Verbatim`/`Generate` paths. *First real
  conversation.*
- **M3 — Neuro-symbolic core.** Provenance store + classification; trust propagation;
  `ResidualPolicy`/`TaintPolicy`/`SideEffectGate`/`DedupeGate`; rejection→narrowed-schema
  loop; confirmation flow; `ask_clarification` mechanics. *The thesis becomes testable.*
- **M4 — Assistant-shaped.** Facts (+lifecycle metadata) with implicit recall;
  content-addressed artifacts; templates; persona config; replay harness; `dump`.
- **M5+ — Evolution & reach** (order by value, each independent): Telegram channel;
  Tomáš component/config pack; the evolution pass (trace mining → compiled flows,
  speculative `Pure` prefetch, consolidation jobs incl. sleep-time projection precompute,
  correction/failure distillation with replay-verified-before-write, human-gated tool
  synthesis); local llguidance backend; WASM tool host; OTLP exporter; vector memory.

## 13. Deferred decisions (explicitly parked, with their trigger)

- Snapshots — when fold time is measurable in a real session.
- `aho-corasick` value index — when span matching exceeds ~hundreds of values.
- Emitter model escalation — DISCARDED by user decision (see findings §6).
- Merkle DAG sync — when a second replica exists.
- Any RL/fine-tuning — out of scope; harness-level loops only.
