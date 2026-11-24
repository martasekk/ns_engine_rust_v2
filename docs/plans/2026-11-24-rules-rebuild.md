# Rules rebuild: bring ns_engine_rust_v2 in line with the coding rules

**Date:** 2026-11-24. **State:** planned, nothing started. **Base:** `main` at `ab2c3c6` (audited on branch
`rules-setup`). **Rules:** `~/.claude/CLAUDE.md` "Writing code", `~/.claude/rules/rust.md`, and the project
`CLAUDE.md` that RR-0 creates. **Builds on:** `archive/2026-09-08-engine-structure.md` §5 (what was left open in the
engine: reify the turn, one dispatch pipeline, measure `fold()`).

Out of scope: stub lints (`todo`/`unimplemented`/`dbg_macro`; the main chat adds them on `rules-setup`), the flow-engine
merge (`MERGE_INSTRUCTIONS.txt`), prompt caching (`prompt-cache-phase1`), new features, behaviour changes other than
those named in a step. Done means every scheduled step below is `done` with its gates green, and the project
`CLAUDE.md` names the gates and the accepted exceptions.

Audit method: line counts, a brace-counting fn-length scan (false positives in `memory-sqlite` from SQL strings were
dropped; the sizes below come from `grep -n fn` boundaries), Grep for `HashMap`/`unsafe`/locks/`spawn_blocking`,
`cargo fmt --all -- --check` (no build). **Not verified:** the workspace clippy run and the test suite at `ab2c3c6`
(no build allowed during the audit; RR-0 establishes them; the main chat's clippy of `ns-core` alone is already red,
see RR-0 item 5); whether `ns-app eval` runs fully offline (it uses scripted doubles,
`app/src/eval.rs:508`; RR-0 checks); whether the `flows` branch constructs `ActionSpec`s (RR-12 checks).

## 1. Current state

### 1.1 Findings, ranked (rule; why it matters; size)

| # | Where | Rule | Finding and why it matters | Size |
|---|---|---|---|---|
| F1 | 11 files, see RR-0 | gates | `cargo fmt --all -- --check` fails on `ab2c3c6`: 50 hunks in `components-std/src/transport.rs`, `core/src/value.rs`, `engine/src/turn/{accounting,memory,mod,retrieval}.rs`, `llm/src/schema.rs`, `memory-sqlite/src/lib.rs`, `pointer/tests/pointer.rs`, `provenance/src/{classify,index}.rs`. They are exactly the files with uncommitted edits in the main checkout, which `MERGE_INSTRUCTIONS.txt:30` says the user means to undo. Until settled, every step's fmt gate is red. | S |
| F2 | `memory-sqlite/src/lib.rs:8-11`, ~35 `self.conn.lock().await` sites 524-1600 | rust.md: blocking in async | Every rusqlite call (FTS5 search, backfill over many rows, migrations) runs synchronously on a tokio worker under a `tokio::Mutex<Connection>`. One slow query stalls every task on that worker, across tenants. The locks are scoped and released before `enc.embed(..).await` (lines 911-917/932, 1160-1251/1267), so there is no lock across an await, only blocking. Precedent for the fix: `pointer/src/agent.rs:555` (`spawn_blocking`). | L |
| F3 | `memory-sqlite/src/lib.rs` | structure | One 2,775-line file (1,617 non-test): codecs, schema and migrations (282-509), lexical/vector/hybrid recall (510-812, 943), fact writes and embedding (813-942), and the `MemoryStore` impl (958-1617). Six jobs in one module. | M |
| F4 | `engine/src/turn/run.rs:21-~714` | structure | `run_turn` is ~690 lines: prelude (routing, deep recall, applicability, 21-171), about ten `let mut` loop locals (175-190), the iteration loop with three `settled = Some(match ..)` sites (207, 351, 637), and the reply tail (685). This is item 1 of archive §5, still open. `Bookkeeping` (`builtins.rs:47`) and `Ctx` (`builtins.rs:104`) already exist to build on. | L |
| F5 | `engine/src/state.rs:50` `fold`; callers `run.rs:38`, `run.rs:215` (inside the loop), `reply.rs:58`, `routing.rs:26`, `memory.rs:100` | rust.md: quadratic over growing data | Every turn folds the whole session log at least four times, plus once per loop iteration. Turn t costs O(t*e) for e events per turn, so a session of T turns costs O(T^2 * e). Archive §5.6 decided "measure before designing"; still unmeasured. | S (measure) |
| F6 | `core/src/action.rs:71-80` `ActionSpec` | structure: invariants where built; determinism | All fields `pub`, built by struct literal at ~45 sites in 17 files across 8 crates; `check_arg_names` runs separately (`HarnessBuilder::build`), so an unchecked spec can exist. `residual_policy: HashMap<String, ResidualRule>` sits in a `Serialize` type, so its JSON order is random. | M |
| F7 | `pointer/src/mcp.rs:77-220` `tools()`, `:481-705` `call()` (match arms 503-640) | structure: no match on content id | The MCP server lists each tool's JSON schema in one function and dispatches behaviour by matching the tool-name string in another (224 lines). A new tool means editing two places kept in step by hand. | M |
| F8 | none in any `Cargo.toml` | property tests | No proptest, bolero, loom or criterion anywhere. Risky pure code without property tests: `llm/src/salvage.rs:21` `salvage_arguments` (repairs model JSON by string surgery: fences, trailing commas, quotes, truncation), `pointer/src/wire.rs` (the pointer wire protocol: `Op`, `Request`, `Response`, `Key`), `core/src/validate.rs:6` `validate_args` (schema handling), `identity/src/jwt.rs` (token verification at a trust boundary). | M |
| F9 | `evolution/src/pass.rs:742-1172` `run_report` | structure | A 430-line fn with nine numbered phases written as comments ("1-2.", "3.", "4.", "4b" .. "9."). The same smell archive §1 fixed in `run_turn`. No in-flight work touches it. | M |
| F10 | `engine/src/turn/builtins.rs:204` `run_builtin` | structure: no match on content id | Six engine builtins dispatch on `proposal.action.as_str()`; their specs live separately in `turn/specs.rs`. It is one deliberate place (its doc comment says so), so ranked below F7. | S |
| F11 | `identity/src/resolvers.rs:96,99` | unsafe / Miri | The only `unsafe` in the workspace: a test-only no-op waker and `Pin::new_unchecked`. `std::task::Waker::noop()` (stable since 1.85) and `std::pin::pin!` (1.68) remove it, so no Miri step is needed. | S |
| F12 | `memory-sqlite/src/lib.rs:746-770` `hybrid_hits` (and `hybrid_fact_hits` 654) | rust.md: algorithm at stated size | Exact kNN by brute force: loads every vector and text of the scope per query, cosine over all, full `sort_by` to take `coarse_k`. Fine at the assumed sizes (see RR-10); the full sort is O(N log N) where O(N) selection does. | S |
| F13 | `llm/src/emitter.rs:279` `emit` (243 lines) | structure | Long fn; `prompt-cache-phase1` plans edits to this file, so deferred. | M |
| F14 | `app/src/factory.rs:409` `build_engine` (267 lines), `:842` `build_pass` (145) | structure | Long assembly fns; `flows` and `prompt-cache-phase1` both touch `factory.rs`, so deferred. | M |
| F15 | `channel-hub/src/hub.rs:126`, `channel-hub/src/tenant.rs:49`, `pointer/src/client.rs:85`, `pointer/src/mcp.rs:501`, `components-std/src/pointer_tool.rs:468-623`, `components-std/src/desktop_channel.rs:58,96,137`, `channel-cli/src/lib.rs:59`, `app/src/registry/mod.rs:138` | rust.md: no lock across `.await` | `tokio::Mutex` held across an await. Each serializes one connection, one receiver or one per-tenant build, which is what an async mutex is for. Proposed as a named exception in the project `CLAUDE.md` (Q3), not a rewrite. | S (doc) |
| F16 | `channel-hub/src/hub.rs:150-248` | loom | The only hand-written coordination: ingress refcount (`AtomicUsize`, last `Drop` shuts down), `closed` flag checked under the queues lock, `tokio::sync::Notify`, wake sender taken on shutdown. loom cannot model tokio `Notify`/`mpsc`; the other atomics are counters or id sources (`pointer/src/client.rs:13-15`, `channel-tcp/src/listener.rs:47,237-251`, `engine/src/dispatch.rs:165`, `pointer/src/backoff.rs:185` test-only). **No code qualifies for a loom test as written** (Q4). | - |
| F17 | `app/src/config.rs` (1,759 non-test lines), `testkit/src/eval.rs` (2,739 non-test), `engine/tests/turn_loop.rs` (6,623) | structure | Large but each is one job (config sections; eval harness; one test binary). Not scheduled; listed so nobody re-audits them. | - |
| F18 | repo root | gates | No project `CLAUDE.md`, no CI, no `.cargo/config.toml`, no `rust-toolchain.toml` (audited with rustc 1.98.1). Clippy state unknown. | S |

### 1.2 What already complies (keep it that way)

- Determinism: `serde_json` runs without `preserve_order` and `Engine::call_key`/`event_hash` rely on it
  (`core/src/action.rs:93-97`); the evolution `Report` uses `BTreeMap` (`evolution/src/pass.rs:212`); the registry
  report sorts tenant ids (`app/src/registry/mod.rs:282`); the `HashMap`s in `evolution/src/mine.rs:333-505,719`,
  `engine/src/replay.rs:220-254` and `engine/src/state.rs:56-57` are lookup-only. `core/src/event.rs` has no
  `HashMap`.
- Encapsulation: `EventLog` keeps `session` and `events` private and verifies the hash chain (`core/src/event.rs:333`).
- Concurrency: the hub hands over with bounded backpressure, never a drop (`channel-hub/src/hub.rs:209-224`), recovers
  poisoned locks on purpose (`hub.rs:183-198`), and creates queues under the same lock as the shutdown flag
  (`hub.rs:151-156`). The pointer agent already moves blocking work to `spawn_blocking` (`pointer/src/agent.rs:555`).
- Structure: the engine turn is already 15 modules (`engine/src/turn/`); archive §2 records the earlier split.
  `pointer_tool.rs` matches on the closed `Act` enum, which is allowed.
- Algorithms: brute-force cosine in `hybrid_hits` is the right choice at the assumed sizes (RR-10 keeps it).

### 1.3 Input sizes (assumptions; Q1 asks the user to confirm)

Events per session E <= 2,000 (T <= 200 turns, ~10 events a turn); turns with vectors per scope N <= 10^4; facts per
scope <= 10^3; embedding dimension d in 384..1,024; tenants per process <= 50.

## 2. In-flight work and what it blocks

| In flight | Files | Blocks |
|---|---|---|
| Main checkout, uncommitted (fmt edits, F1) | the 11 files in F1 | RR-0 fmt baseline; RR-8, RR-9, RR-10 (memory-sqlite); RR-13..RR-15 (`turn/mod.rs`) |
| Branch `flows` (`../ns_engine_rust_v2-flows`) | `engine/src/turn/{run,prompt,legal,config}.rs`, `core/src/flow.rs` (new), `app/src/{config,factory,tenant,main}.rs`, `app/src/admin/*` | RR-12 (ActionSpec sites; `flow.rs` may build specs, not verified), RR-13..RR-16, RR-18 |
| Branch `prompt-cache-phase1` (plan untracked in main) | `llm/src/{emitter,client}.rs`, `engine/src/turn/prompt.rs`, `core/src/{usage,traits}.rs`, `app/src/{config,budget,factory}.rs` | RR-17, RR-18 |
| `rules-setup` (main chat): stub lints | root `Cargo.toml`, every member `Cargo.toml` | RR-1 (root `Cargo.toml`, `Cargo.lock`) |

Free now: RR-2, RR-6, RR-7, RR-11. Free after RR-1: RR-3, RR-4, RR-5.

## 3. Build order

Lanes (each lane is its own worktree and `--target-dir`; at most **two** lanes build at once on this PC, and only
one while another session is building):

| Lane | Steps | Starts when |
|---|---|---|
| 0 infra | RR-0, RR-1, RR-2 | RR-0 now (main chat); RR-1 after stub lints land |
| P property tests | RR-3, RR-4, RR-5 | after RR-1 |
| M mcp | RR-6 | after RR-0 |
| V evolution | RR-7 | after RR-0 |
| S store | RR-8a, RR-8b, RR-9a, RR-9b, RR-10 | after F1 is settled (Q2) and RR-1 |
| E engine | RR-11 (now), RR-11b (conditional), RR-12a, RR-12b, RR-13, RR-14, RR-15, RR-16 | RR-12+ after the `flows` decision (Q6) |
| D deferred | RR-17, RR-18 | after `prompt-cache-phase1` (and `flows` for RR-18) |

Rules for every step: refactor-only commits come first and change no behaviour; a pure move needs no failing-first
test, its proof is the crate's suite green with the **same test count** before and after (the builder records both
counts in the commit message). Test-only steps prove "would fail without it" by mutation: the reviewer breaks one
line of the code under test and sees the property fail. Gates for every step unless stated:
`cargo fmt --all` then `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --target-dir <lane dir> -j 8 -- -D warnings`;
`cargo test -p <crate> --target-dir <lane dir> -j 8`.

---

### RR-0. Project CLAUDE.md and the gate baseline

Goal: the repo states its gates, layout, plan location and conventions, and the gates are known to pass on the base.
Lane 0. Depends on: the user's answer to Q2 (the 11 fmt files). Size S. Tier `worker`.

```
Start here (RR-0)
Files: CLAUDE.md (new, repo root); nothing else unless the fmt baseline is approved (below).
Read first: this plan §1, §2 and "Proposed CLAUDE.md" below; Cargo.toml lines 1-20 (members);
  the first `//!` line of each crate's src/lib.rs (core, engine, llm, memory-sqlite, provenance and
  components-std have none: read their lib.rs `pub mod` list instead) to check each layout line.
Steps:
 1. Write CLAUDE.md from "Proposed CLAUDE.md". Every line must be true at the commit: fix any
    crate description the lib.rs check contradicts. Do NOT add the "added by later steps" lines.
 2. If the user approved formatting (Q2): `cargo fmt --all`, commit ONLY the 11 F1 files as
    "fmt: format the files main left unformatted (no behaviour change)". Otherwise skip and
    record in §5 that fmt gates are red until Q2 is answered.
 3. Baseline, once, own --target-dir: `cargo clippy --workspace --all-targets -- -D warnings`,
    then tests per crate (`cargo test -p <crate>`, each < 10 min). Record pass/fail counts per
    crate in §5 of this plan.
 4. Check whether `ns-app eval` (app/src/eval.rs) runs without network; if it does, add its
    command and request count under "Invariants to re-check after refactors".
 5. If clippy is red: record the warnings in §5 and hand over (RR-0b = fix them, refactor-free).
    Known red already (main chat, 24 Nov): `cargo clippy -p ns-core --all-targets -- -D warnings` fails with
    `clippy::large_enum_variant` ("large size difference between variants") in `ns-core`, on `ab2c3c6` with and
    without the stub lints, so the workspace clippy gate is red on main. RR-0b fixes it: box the large variant (or
    justify an `#[expect(clippy::large_enum_variant, reason = ..)]` if the enum is matched on a hot path and boxing
    costs more), and record the enum and its variant sizes in the commit.
Gates: fmt check (or recorded red per Q2); the baseline runs above.
Stop after: CLAUDE.md commit [+ fmt commit]; update §5.
```

**Proposed CLAUDE.md** (what holds today; RR-0 writes it):

```
# ns_engine_rust_v2: project rules
A message-driven agent engine: a model proposes actions, the engine checks them (provenance,
guards, confirmation) and runs tools; memory in SQLite; an offline evolution pass learns rules.

## Layout (Cargo workspace; no rust-toolchain.toml, built with stable rustc 1.98)
core (ns-core): shared vocabulary - events, ActionSpec, traits (ports), errors. No I/O.
engine (ns-engine): the turn loop (src/turn/*), dispatch per session, guards, replay, state fold.
llm (ns-llm): providers, emitter (tool-call JSON), salvage of malformed JSON, summarizer.
memory-sqlite (ns-memory-sqlite): the MemoryStore on SQLite (events, FTS5, vectors, facts, digests).
provenance (ns-provenance): where each argument value came from, and its trust.
identity (ns-identity): who is on the other end of a connection, and the credential that proves it.
channel-{cli,http,tcp,hub}: transports; hub = per-tenant queues behind one ingress.
pointer (ns-pointer): drive another machine's pointer - agent, wire protocol, MCP server.
components-std: standard tools (http, time, pointer, desktop channel).
evolution (ns-evolution): offline pass - mine, grade, consolidate, propose learned rules.
testkit (ns-testkit): evaluation harnesses that drive the engine.  app (ns-app): the binary.

## Gates (every commit)
cargo fmt --all            # write first, then:
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p <each touched crate>
Full `cargo test --workspace --all-targets` only after a cross-crate change and before a
final report. Give every worktree its own --target-dir; at most two builds at once, -j 8.
The slowest test binary is `cargo test -p ns-engine --test turn_loop`; filter it when you can.

## Plans
docs/plans/YYYY-MM-DD-<topic>.md, index in docs/plans/README.md. Rules rebuild: 2026-11-24-rules-rebuild.md.

## Conventions
- Maps whose iteration order is observed (output, hashing, persistence) are BTreeMap.
- serde_json must not enable `preserve_order`: event hashes and call keys rely on byte order.
- Scaling tests for hot paths run in --release and assert roughly linear growth 1x -> 10x.
- Refactor-only commits keep every crate's test count unchanged and say so in the message.

## Invariants to re-check after refactors
- Test counts per crate unchanged by refactor-only commits.
```

**Lines added by later steps** (each in the same commit as the step, so `CLAUDE.md` never states what is not yet
true):
- RR-1, under Conventions: "Property tests: bolero 0.13 (workspace dev-dependency), always `with_iterations(N)`; a
  failing input becomes a plain #[test] before the fix."
- RR-2, under Conventions: "The workspace has no `unsafe`; adding any needs a Miri-run test."
- RR-9b, under Conventions: "SQLite is reached only through memory-sqlite's `Db::call` (spawn_blocking); never call
  rusqlite on an async worker."
- After the user answers Q3 (yes), whoever lands next adds under Conventions: "A tokio::Mutex may be held across
  .await only to serialize one connection, one receiver or one per-key build; the field's doc comment says which."
- RR-0 step 4 (if offline): the eval command and its request count under Invariants.

---

### RR-1. Test-tool dependencies, one commit

Goal: bolero is available to every crate that gets a property test, added once so parallel lanes never edit
`Cargo.toml`/`Cargo.lock` concurrently (contract-first for the build files). Lane 0. Depends on: stub lints landed on
`rules-setup` (root `Cargo.toml` free), RR-0. Size S. Tier `worker`.

```
Start here (RR-1)
Files: Cargo.toml ([workspace.dependencies], after line 35);
  crates/{llm,pointer,core,identity,engine,memory-sqlite}/Cargo.toml ([dev-dependencies]);
  Cargo.lock (generated); CLAUDE.md (one Conventions line, see the plan's "Lines added by later steps").
Read first: root Cargo.toml lines 1-60; each of the six member Cargo.toml files (each < 40 lines).
Steps:
 1. Root: `bolero = "0.13"` under [workspace.dependencies].
 2. Each of the six: `bolero = { workspace = true }` under [dev-dependencies].
 3. `cargo check -p ns-llm --tests` to resolve the lockfile; commit Cargo.toml files + Cargo.lock + CLAUDE.md.
Acceptance: `cargo tree -p ns-core -e dev | grep bolero` lists 0.13.x; `git diff Cargo.lock` only
  adds bolero's tree (no other dependency version moved).
Gates: fmt check; clippy workspace; `cargo test -p ns-core` (smoke).
```

### RR-2. Remove the only `unsafe` (test helper)

Goal: zero `unsafe` in the workspace, so no Miri lane exists. Lane 0. Depends on: none. Size S. Tier `worker`.

```
Start here (RR-2)
Files: crates/identity/src/resolvers.rs lines 80-110 (test module `block_on`); CLAUDE.md (one line,
  if RR-0 has landed; else leave the line to RR-0's author).
Read first: those lines only.
Steps: replace the hand-made RawWaker vtable with `std::task::Waker::noop()` (returns &Waker;
  `Context::from_waker(Waker::noop())`) and `Pin::new_unchecked(&mut fut)` with
  `let mut fut = std::pin::pin!(fut);`. Delete the safety comment.
Acceptance: `grep -rn "unsafe" --include=*.rs crates app` prints nothing; the identity resolver
  tests pass with the same count (they exercise this helper).
Gates: fmt check; clippy workspace; `cargo test -p ns-identity`.
```

### RR-3. Property tests: `salvage_arguments`

Goal: the JSON repair of model output never panics, never invents a key, and never changes a valid object. Lane P.
Depends on: RR-1. Size S. Tier `worker`. Collision: none (`salvage.rs` is not in flight; the test is a new file).

Contract (read at `salvage.rs:17-38`): it runs only after `serde_json::from_str` failed; each repair in `REPAIRS`
runs on the previous output, a repair that changes nothing is skipped, and it returns `Some((object, repair_name))`
from the first repair whose output parses, else `None`. So valid JSON that no repair rewrites yields `None`.

```
Start here (RR-3)
Files: crates/llm/tests/salvage_props.rs (new). Under test: crates/llm/src/salvage.rs:21
  `pub fn salvage_arguments(raw: &str) -> Option<(Value, &'static str)>` (check the path, e.g.
  nsllm::salvage::salvage_arguments; if the module is private add a `pub use` in
  crates/llm/src/lib.rs - the only src edit allowed).
Read first: salvage.rs 1-140 and the test names after line 373.
Tests (each `bolero::check!().with_type::<..>().with_iterations(2000)`):
 1. no_panic: any `String` (lengths 0..~512, multibyte included) -> returns.
    Edge: a multibyte char right at a truncation point (byte-index slicing).
 2. valid_object_is_never_altered: object from `Vec<(String, i64, bool, String)>` with keys
    filtered to [a-z_]{1,12}; result is `None` or `Some((obj, _))` - never a different value.
    Edge: string values holding `'`, `,}`, `True`, `None` (the quote and python repairs).
 3. fenced_object_is_recovered: the same object inside "```json\n..\n```" -> `Some((obj, _))`.
    Edge: a fence with no language tag.
 4. no_invented_keys: any `String` -> if `Some((Value::Object(m), _))`, every key of m occurs in
    the input text (the module doc's promise). Edge: input `{a` (truncation closes it).
 5. truncated_is_object_or_none: cut a serialized object at any char boundary -> `None` or
    `Some((Value::Object(_), _))`.
A failing input becomes a plain #[test] before anyone changes salvage.rs (rust.md); a real bug
found here is reported, not fixed in this step.
Acceptance (mutation): reviewer makes drop_trailing_commas (salvage.rs:142) also drop the char
  after each comma -> test 2 or 3 fails.
Gates: fmt check; clippy workspace; `cargo test -p ns-llm --test salvage_props`.
```

### RR-4. Property tests: pointer wire protocol

Goal: every `Request`/`Response` survives serialize -> deserialize, and the bytes on the wire match the documented
protocol. Lane P. Depends on: RR-1. Size S. Tier `worker`. Collision: `crates/pointer/tests/pointer.rs` is in the main
checkout's uncommitted edits: do not touch it; use a new file.

```
Start here (RR-4)
Files: crates/pointer/tests/wire_props.rs (new). Under test: crates/pointer/src/wire.rs
  (Step 25, Key 77, Button 138, Op 146, Request 204, ResultBody 212, TextBox 270,
  ErrorKind 279, WireError 316, Response 323).
Read first: wire.rs 1-380 (whole file); crates/pointer/examples/protocol_samples.rs, only the
  first 3-4 sample requests/responses (or the matching examples in docs/pointer-protocol.md).
Tests:
 1. pinned_samples (plain #[test]): 3-4 JSON samples copied from protocol_samples.rs /
    pointer-protocol.md parse into the expected values and serialize back to the same JSON
    value. This pins the other side of the wire.
 2. request_round_trip (bolero, 2000): generate `(u64, u8, i32, i32, String, u8)`, map the u8 to
    an `Op` variant (modulo the count), build `Request`; `from_str(to_string(req))` equals req
    (`==` if PartialEq is derived, else compare `serde_json::to_value` of both).
 3. response_round_trip: same for `Response::ok(id, ..)` and the error form.
 4. no_panic: any `String` into `serde_json::from_str::<Request>` returns Ok or Err.
Edges: id = u64::MAX, negative coordinates, emoji and quotes in typed text, `Key::f(1)` and
  `Key::f(24)`, `Key::ch` with a multibyte char. Do not add derives to src.
Acceptance (mutation): renaming one `Op` variant's serde name fails test 1;
  `#[serde(skip_serializing)]` on one `Request` field fails test 2.
Gates: fmt check; clippy workspace; `cargo test -p ns-pointer --test wire_props`.
```

### RR-5. Property tests: `validate_args` and JWT verification

Goal: schema validation and token verification never panic on hostile input and reject every tampered token.
Lane P. Depends on: RR-1. Size M. Tier `worker`. Collision: `core/src/value.rs` is in flight; do not touch it.

```
Start here (RR-5)
Files: crates/core/tests/validate_props.rs (new), crates/identity/tests/jwt_props.rs (new).
Under test: crates/core/src/validate.rs:6 `validate_args(schema, args) -> Result<(), String>`;
  crates/identity/src/jwt.rs (Hs256Verifier 45-165) and the token signer in
  crates/identity/src/minting.rs (`grep -n "pub fn" minting.rs`).
Read first: validate.rs 1-75; jwt.rs 1-165; the minting.rs signer fn only.
Tests (bolero, 2000 iterations each):
 1. validate_no_panic: depth-limited JSON (nest <= 4) built from primitives as both schema
    and args -> returns. Edge: `required` naming a missing key; `type` not a string.
 2. validate_required: schema {type: object, required: [k], properties: {k: {type: string}}};
    args without k -> Err; with k a string -> Ok. Edge: k = "" and k with unicode.
 3. jwt_round_trip: random 32-byte key, tenant id [a-z]{1,16}, exp after the verifier's clock
    -> mint -> verify Ok.
 4. jwt_tamper: flip one byte at any index of a valid token -> Err. Edge: each of the three
    segments; a separator `.`.
 5. jwt_no_panic: any String -> verify returns.
Acceptance (mutation): make the verifier skip the signature compare -> test 4 fails.
Gates: fmt check; clippy workspace; `cargo test -p ns-core --test validate_props`;
  `cargo test -p ns-identity --test jwt_props`.
```

### RR-6. MCP tools as one table (golden first, then refactor)

Goal: each MCP tool is one table row (name, schema, handler); no `match` on the tool-name string. Lane M. Depends
on: RR-0. Size M. Tier `worker` (behaviour is pinned by a golden). Collision: none (`pointer/src/mcp.rs` and
`pointer/tests/mcp.rs` are not in flight).

```
Start here (RR-6)
Files: crates/pointer/src/mcp.rs: tools() 77-220, call() 481-705 (arms 503-640), gate() (grep).
Read first: mcp.rs 1-76 (imports, key parsing), 77-220, 481-705 (~370 lines).
Commit 1 (test only): in crates/pointer/tests/mcp.rs add `tools_list_is_unchanged`: the
  tools/list JSON for every gate setting the server has, compared with a golden file
  crates/pointer/tests/golden/mcp_tools.json written from the current code; and
  `unknown_tool_is_the_same_error` (name "nope" -> today's exact error JSON).
Commit 2 (refactor, no behaviour change): one table, e.g. an enum `ToolId` with
  `const TOOLS: &[(&str, ToolId)]`, `ToolId::schema()` and `async fn run(..)` matched on the enum
  (or a struct of fn pointers if that is less code); tools() iterates the table; call() looks
  the name up once and runs the row. Keep `gate()` before dispatch.
Acceptance: both tests green before and after commit 2; existing mcp tests same count.
Gates: fmt check; clippy workspace; `cargo test -p ns-pointer --test mcp`.
```

### RR-7. `run_report` as phase methods (refactor only)

Goal: `run_report` reads as a list of named phases, each a method with explicit inputs and outputs; no behaviour
change. Lane V. Depends on: RR-0. Size M. Tier `worker`.

```
Start here (RR-7)
Files: crates/evolution/src/pass.rs: Report 202-230, run_report 742-1172.
Read first: pass.rs 195-240 and 742-1172 (~470 lines). Do not read the tests (1183+).
Steps (one commit): extract, in order, keeping the numbered comments as fn docs:
  load_sessions (750-772: "3."), mine_and_evaluate (772-803: "4."), grade (803-856: "4b"),
  calibrate_and_fitness (856-892: "4b'", "4c"), symbolic_lane (892-958: "5.", "6."),
  consolidate_and_digest (958-1019: "6b", "6c"), notes_lane (1019-1151: "7."),
  apply (1151-1164: "8."), record_spend (1164-1172: "9."). Each takes &self, &mut Report and
  only the locals it uses; each `dry_run` check stays inside its phase.
  Ranges are from the audit; confirm with `grep -n "        // [0-9]" pass.rs`.
Acceptance: `cargo test -p ns-evolution` green with the same test count (record before/after);
  run_report <= 80 lines; no phase method > 150 lines.
Gates: fmt check; clippy workspace; `cargo test -p ns-evolution`.
```

### RR-8a / RR-8b. Split `memory-sqlite/src/lib.rs` into modules (refactor only)

Goal: one module per job; `lib.rs` keeps the struct, the `MemoryStore` impl and the tests. Lane S. Depends on: F1
settled (this file is dirty in the main checkout), RR-0. Size M each. Tier `worker`.

```
Start here (RR-8a: codecs and schema)
Files: crates/memory-sqlite/src/lib.rs 1-509 -> new src/codec.rs (vector_blob 58, blob_vector 62,
  digest_text 90, fact_text 114, turn_text 124, hex/unhex32 136-150, trust_str/parse_trust
  151-170, row_to_fact 171, row_to_digest 201) and src/schema.rs (ensure_embeddings 282,
  ensure_events_fts 313, ensure_session_digests 362, migrate_facts 424, and the CREATE
  statements from open() 222-256).
Read first: lib.rs 1-509.
Rules: pure move; items become pub(crate); no renames; tests stay in lib.rs.
Acceptance: `cargo test -p ns-memory-sqlite` same count, green.

Start here (RR-8b: recall and fact writes)
Files: lib.rs 510-957 -> src/recall.rs (lexical_rows 510, turn_vectors 555, fact_vectors 616,
  hybrid_fact_hits 654, hybrid_hits 735, fts_query 943) and src/facts.rs (write_fact 819,
  embed_fact 903). The Recall struct (32-57) moves to recall.rs with a pub re-export.
Read first: lib.rs 1-60 and 510-957.
Acceptance: as RR-8a. Gates (both): fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`.
```

### RR-9a / RR-9b. SQLite off the async workers through one choke point

Goal: no rusqlite call runs on a tokio worker; the connection is reachable only through `Db::call`, which uses
`spawn_blocking`. Lane S. Depends on: RR-8b. Size M + M. Tier **`builder`**: async/blocking boundary, `'static`
closures force owned arguments at ~35 sites, poison handling, and a wrong move loses writes.

Design: `pub(crate) struct Db(Arc<std::sync::Mutex<Connection>>)` with
`async fn call<T: Send + 'static>(&self, f: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static) -> Result<T, StoreError>`
= `spawn_blocking(move || f(&mut lock_recovering_poison(&m)))`, a `JoinError` mapped to `StoreError::Io`.
Alternatives rejected: `block_in_place` (panics on the `current_thread` runtime that `#[tokio::test]` uses, as
`pointer/src/agent.rs:493` notes); a dedicated SQLite thread with a command channel (more code, same effect with one
connection); `tokio-rusqlite`/`deadpool-sqlite` (a new dependency for ~30 lines). Invariant kept by type privacy:
`SqliteStore` holds `Db`, `Db`'s field is private, so a forgotten site fails to compile. Writes stay serialized (one
connection, one mutex), so event-append order is unchanged.

```
Start here (RR-9a: Db, and the moved modules)
Files: crates/memory-sqlite/src/db.rs (new); lib.rs struct (10-31) and open() (222-256);
  schema.rs, facts.rs, recall.rs (from RR-8) - convert their lock sites.
Read first: lib.rs 1-60, 222-260; facts.rs; recall.rs; pointer/src/agent.rs 540-570 (precedent).
Steps: write Db and its unit tests first; switch the struct field; convert module sites, turning
  borrowed args into owned values before the closure.
Tests: db::tests::call_runs_off_the_runtime_thread - on a current_thread runtime the thread id
  inside `call` differs from the test's (a direct call runs on the same thread, so it fails
  before the change); db::tests::a_poisoned_lock_still_serves - a closure panics, the next call
  succeeds. No timing-based tests (this PC has load-sensitive flakes).
Acceptance: both tests; module sites converted; crate tests green.

Start here (RR-9b: the MemoryStore impl)
Files: lib.rs `impl MemoryStore for SqliteStore` (958-1617 before RR-8; `grep -n "impl MemoryStore" lib.rs`);
  CLAUDE.md (the Db::call Conventions line).
Read first: that impl (~600 lines) in two halves; convert each half before reading the next.
Tests: run the existing conformance suite (lib.rs `nsengine_conformance`, ~1823) under both
  `#[tokio::test(flavor = "current_thread")]` and `flavor = "multi_thread"` (add the second).
Acceptance: `grep -n "\.lock()" crates/memory-sqlite/src/*.rs` shows the connection locked only in
  db.rs (one site); crate tests green; `cargo test -p ns-engine` green.
Gates (both): fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`; RR-9b also
  `cargo test -p ns-app` (cross-crate user).
```

### RR-10. Hybrid recall: selection instead of full sort, and a scaling test

Goal: candidate selection is O(N), ties are broken deterministically, and growth is shown linear. Lane S. Depends on:
RR-9b (and RR-1 for bolero). Size S. Tier `worker`.

Algorithm: exact k-nearest-neighbour by brute force. Cost per query at N = 10^4, d = 1,024: cosine 10^7 multiply-adds
(~5-10 ms), vectors loaded from SQLite ~40 MB (the dominant cost), `select_nth_unstable_by` O(N) then a sort of
`coarse_k` (<= 100) items, versus today's O(N log N) full sort. Rejected: approximate NN (HNSW, `sqlite-vec`), not
needed below N ~ 10^5 and a known problem class that needs a research pass first; an in-memory vector cache per scope,
which needs invalidation on append and backfill, deferred until the scaling test shows load time matters. Tie-break:
equal cosine -> ascending row id; the builder checks `turn_vectors`' `ORDER BY` so the result matches today's stable
sort (if today's order is not row-ascending, keep today's order and say so in the commit).

```
Start here (RR-10)
Files: crates/memory-sqlite/src/recall.rs hybrid_hits and hybrid_fact_hits (moved by RR-8b).
Read first: those two fns and turn_vectors (~200 lines).
Tests: top_k_matches_full_sort (bolero, 2000) - for generated score lists with ties, the selected
  coarse_k equal the first coarse_k of a stable full sort with the same tie-break;
  recall_scales_linearly (#[ignore], --release): 1x = 1,000 and 10x = 10,000 stored vectors,
  time hybrid_hits with a fake encoder; assert the ratio < 15.
Gates: fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`;
  `cargo test -p ns-memory-sqlite --release -- --ignored recall_scales_linearly`.
```

### RR-11. Measure `fold()` (archive §5.6)

Goal: know whether folding the whole log per call matters at the stated sizes. Lane E. Depends on: none (`state.rs`
is not in flight; new test file). Size S. Tier `worker`.

```
Start here (RR-11)
Files: crates/engine/tests/fold_scaling.rs (new). Under test: crates/engine/src/state.rs:50 fold.
Read first: state.rs 1-60 and the test helpers at 280-340 (how a log is built).
Test (#[ignore], --release): sessions of 200 and 2,000 events (mix: UserSaid, ProposalMade,
  ToolCalled, ToolReturned); time 100 folds each; print ns/event; assert the 10x ratio < 15
  (linear per call). Print the session total for 5 folds per turn over T = 200 turns.
Acceptance: the numbers go into §5 with a decision: RR-11b proceeds only if a 200-turn session
  spends > 50 ms in fold (assumed threshold, Q1); otherwise RR-11b is dropped and archive §5.6
  is closed.
Gates: fmt check; clippy workspace; `cargo test -p ns-engine --release --test fold_scaling -- --ignored`.
```

RR-11b (conditional, `builder`, M): `SessionState::apply(&mut self, &Event)` with `fold = default + apply each`, and
an incremental state carried through `run_turn` instead of re-folding. Property test (bolero): for any event sequence
split at any point, `fold(a ++ b) == apply_all(fold(a), b)`. Depends on RR-15 (the turn state exists). Written in
detail only if RR-11 says so.

### RR-12a / RR-12b. `ActionSpec` checked where it is built (contract step)

Goal: an `ActionSpec` cannot exist unchecked, and its serialized form is deterministic. Lane E. Depends on: the
`flows` decision (Q6; `flows` may construct specs in `core/src/flow.rs`, not verified), RR-0. Size M + M. Tier
**`builder`**: public API of `ns-core` used by 8 crates, and spec JSON is read from config (a wire format).
**Reviewed and merged before RR-13 and RR-16 start.**

Design (hard to reverse, justified): `residual_policy: BTreeMap<String, ResidualRule>` (same JSON object shape, so
configs and stored specs still parse); `#[non_exhaustive]` on `ActionSpec` so other crates must use
`ActionSpec::new(name, description, args_schema, side_effect) -> Result<Self, SpecError>` plus `with_residual(..)` and
`with_dedupe_tag(..)`; `new` runs `check_arg_names`; `#[serde(try_from = "RawActionSpec")]` runs the same check when a
spec is deserialized (trust boundary: config files). Fields stay `pub` for reading: no `src` site assigns a spec field
after construction (Grep found none), so getters would be churn without a guarded invariant. The code gets
`// shortcut: fields stay pub and writable; switch to private fields + getters if a mutation site appears.`
Rejected: private fields + getters now (every `.name`/`.args_schema` read changes, hundreds of sites, for no invariant
today); leaving the check in `HarnessBuilder::build` only (specs built elsewhere, e.g. the pointer tool and tests, are
never checked).

```
Start here (RR-12a: core)
Files: crates/core/src/action.rs 64-140 (ResidualRule, ActionSpec, check_arg_names); core's own
  literal sites (`grep -n "ActionSpec {" crates/core/src`).
Read first: action.rs 1-140 and 340-380 (the existing check test).
Tests first: spec_new_rejects_arg_sorting_before_rationale (an arg "A" -> Err);
  deserializing_an_unchecked_spec_fails (JSON with arg "A" -> serde error);
  residual_policy_serializes_in_key_order (one spec, 12 keys inserted in reverse order -> the
  serialized keys come out sorted; a HashMap passes by chance with probability ~1/12!).
Acceptance: tests green; `cargo test -p ns-core`.

Start here (RR-12b: every other crate)
Files: the ~45 literal sites: `grep -rln "ActionSpec {" crates app` (engine 5 files, components-std 3,
  llm 2, evolution, provenance, testkit, app 1 each). Each becomes `ActionSpec::new(..)?`, or
  `.expect("static spec is valid")` for constant specs (with a comment saying so).
Read first: each site's surrounding fn only.
Acceptance: `cargo test --workspace --all-targets` green (cross-crate change).
Gates (both): fmt check; clippy workspace; tests as stated.
```

### RR-13 / RR-14 / RR-15. Reify the turn (`run_turn` into phases; refactor only)

Goal: `run_turn` becomes a short sequence of phase calls over one `TurnState`; no behaviour change. Archive §5.1.
Lane E. Depends on: the `flows` decision (it edits `run.rs`), the main checkout's `turn/*` edits settled (F1),
RR-12b. Size M, L, M. Tier **`builder`**: the archive warns the borrow checker resists (`&mut ctx`, `&mut log` and
`&self` at once), and this is the heart of the engine.

Invariant for all three: `cargo test -p ns-engine` (incl. `--test turn_loop`, `--test act_or_answer`) green with the
same count, and the RR-0 offline eval request count unchanged if RR-0 found one.

```
Start here (RR-13: the prelude)
Files: crates/engine/src/turn/run.rs 21-171; new type `TurnPlan` in crates/engine/src/turn/plan.rs.
Read first: run.rs 1-200; builtins.rs 47-130 (Bookkeeping, Ctx); turn/mod.rs 1-131.
Steps: move routing, deep recall and applicability (scope_holds_facts, recall_applies) into
  `async fn plan_turn(&self, incoming, &mut EventLog, usage) -> Result<TurnPlan, EngineError>`;
  TurnPlan holds sid, scope, turn, the rules snapshot, tier, selected_tools and the two
  applicability flags. run_turn calls it. One commit; run_turn shrinks by ~150 lines.

Start here (RR-14: the loop state and body)
Files: run.rs ~171-~560 (from `let mut book` to the loop end; confirm with `grep -n "loop {" run.rs`).
Read first: run.rs 171-560 (~390 lines); builtins.rs 47-130.
Steps: bundle book, emit_failures, last_emit_error, proposed_this_turn, settled, pre_draft and
  the held answer (186+) into `TurnState` (plan.rs); move the loop body into
  `async fn iterate(&self, plan: &TurnPlan, st: &mut TurnState, log: &mut EventLog) -> Result<Next, EngineError>`
  with `enum Next { Continue, Settled }`. Keep the three `settled = Some(match ..)` sites as they are.

Start here (RR-15: settle and reply)
Files: run.rs ~560-714 (fallback reason, ReplyPolicy handling at 685, the reply call).
Read first: run.rs 540-745; reply.rs 47-60.
Steps: move into `async fn finish(&self, plan, st, log) -> Result<String, EngineError>`.
Acceptance: run_turn <= 60 lines; no fn in run.rs > 200 lines.
Gates (each): fmt check; clippy workspace; `cargo test -p ns-engine`.
```

### RR-16. Engine builtins as one table

Goal: each builtin's name, spec and handler sit in one table row; `run_builtin` looks the name up once. Lane E.
Depends on: RR-12b (spec construction), RR-15. Size S. Tier `worker`.

```
Start here (RR-16)
Files: crates/engine/src/turn/builtins.rs 197-215 (run_builtin) and the six handler signatures;
  crates/engine/src/turn/specs.rs (the builtin specs; grep the six names).
Read first: builtins.rs 1-215; specs.rs (297 lines).
Steps: `enum Builtin { AskClarification, RememberFact, Recall, InspectResult, ForgetFact, ForgetAll }`
  with `const BUILTINS: [(&str, Builtin); 6]`, `Builtin::from_name`, `Builtin::spec(profile)`
  (delegating to specs.rs) and dispatch by enum. The builtin-spec list in specs.rs iterates BUILTINS.
Tests: every_builtin_has_a_spec_and_a_handler (for each row, spec name == row name) - fails if
  a row is added without a matching spec; existing turn_loop tests unchanged.
Gates: fmt check; clippy workspace; `cargo test -p ns-engine`.
```

### RR-17. `emit` into phases (deferred)

Goal: `llm/src/emitter.rs:279` `emit` (243 lines) split into request building, sending/retry, parsing/salvage;
refactor only. Lane D. Depends on: `prompt-cache-phase1` merged. Size M. Tier `worker`. Start-here block written when
unblocked: read emitter.rs 260-530; acceptance = `cargo test -p ns-llm` same count; emit <= 80 lines.

### RR-18. `build_engine` / `build_pass` into parts (deferred)

Goal: `app/src/factory.rs:409` (267 lines) and `:842` (145 lines) split by what they assemble (providers, memory,
tools, guards, channels); refactor only. Lane D. Depends on: `flows` and `prompt-cache-phase1` merged. Size M. Tier
`worker`. Start-here block written when unblocked; acceptance = `cargo test -p ns-app` same count.

### Not scheduled (recorded so nobody re-audits)

`app/src/config.rs` per-section split, `testkit/src/eval.rs`, `engine/tests/turn_loop.rs` split (F17); `core` shedding
policy (`core/src/budget.rs`, `core/src/router.rs`, archive §5.4); a narrow `engine::sim` surface for `ns-evolution`
(archive §5.5); one pointer-act catalogue shared by `pointer/src/mcp.rs` and `components-std/src/pointer_tool.rs`
(their schemas differ: pixels vs schema profiles; Q5); a loom test for the hub (Q4).

## 4. How to run it

**Brief template** (main chat hands one to each step's agent; fill the brackets):

```
Task RR-<n> of docs/plans/2026-11-24-rules-rebuild.md. Tier: <worker|builder>.
Worktree: C:/Users/Acemagic/ns_engine_rust_v2-rr-<lane> on branch rr-<lane> (create from main if missing);
--target-dir C:/Users/Acemagic/ns-target/rr-<lane> on every cargo command, -j 8.
Read ONLY: the plan's §3 "Start here (RR-<n>)" block and the sections it names. Do not read other steps.
One step per run. Tests first where the block names them. Refactor-only commit before any behaviour commit.
Gates before each commit: cargo fmt --all; cargo fmt --all -- --check;
  cargo clippy --workspace --all-targets -- -D warnings; cargo test -p <crate> (as the block says).
Commit with gates green; for refactor-only commits put the test counts before/after in the message.
Then update this plan's §5 row for RR-<n> (state, commit, date, numbers) on the same branch.
Do not touch files listed as in flight in §2 unless the block says so.
Near 120k context: stop, commit what is green, write the hand-over in §5 ("Needs next:" one line).
End your report with "Needs next:" and what you skipped or did not check.
```

**Review rule:** one review per finished step, against its acceptance list (each test would fail without the change;
refactor-only: same test count, no behaviour diff). Contract steps (RR-1, RR-12a, RR-12b) are reviewed and merged
before any step that depends on them starts. A lane merges to main after its review; the next lane step branches from
the new main.

**Waits on the user's yes:** Q2 before RR-0's fmt commit and lane S; the `flows` decision (Q6) before RR-12+; merging
any lane to main; RR-11b; anything in "Not scheduled".

**Builds:** at most two lanes building at once (one while another session builds). Delete a lane's `--target-dir`
with its worktree when the lane is merged or abandoned.

## 5. Step state

| Step | State | Commit | Notes / Needs next |
|---|---|---|---|
| RR-0 | planned | | waits on Q2 for the fmt commit; records clippy/test baseline here |
| RR-1 | planned | | waits on stub lints |
| RR-2 | planned | | free now |
| RR-3, RR-4, RR-5 | planned | | after RR-1 |
| RR-6 | planned | | after RR-0 |
| RR-7 | planned | | after RR-0 |
| RR-8a, RR-8b, RR-9a, RR-9b, RR-10 | planned | | after Q2 / F1 |
| RR-11 | planned | | free now; RR-11b conditional |
| RR-12a, RR-12b | planned | | after the flows decision |
| RR-13, RR-14, RR-15, RR-16 | planned | | after RR-12b and the flows decision |
| RR-17, RR-18 | deferred | | after prompt-cache-phase1 (and flows) |

## 6. Risks and cost

- Build time: `clippy --workspace` is a full check per step (minutes with sccache); lanes S and E rebuild the most.
  Per-lane target dirs are 1-25 GB each.
- RR-9 changes where SQLite work runs. A missed owned-argument conversion is a compile error, not a runtime bug
  (type privacy). Throughput under load is unmeasured; serialization is the same as today (one connection, one
  mutex), it only moves from the async mutex to a std mutex on the blocking pool.
- RR-12 changes the public API of `ns-core`; any branch that builds specs (`flows`) must rebase onto it.
- RR-13..RR-15 conflict with any open branch touching `run.rs`; run them when no such branch is open.
- Property tests add a few seconds per crate test run at 2,000 iterations.

## 7. Open questions for the user

- **Q1** The input sizes in §1.3 are assumptions (events per session, vectors per scope, embedding dimension).
  Confirm or give real numbers; RR-10's and RR-11's thresholds depend on them.
- **Q2** The 11 files that fail `cargo fmt --check` on main are the ones with uncommitted formatting edits in the main
  checkout (`MERGE_INSTRUCTIONS.txt:30` says you planned to undo them). Commit the formatting (RR-0, fmt-only) or undo
  it and format later?
- **Q3** Accept "tokio::Mutex across `.await` only to serialize one connection, receiver or build, documented on the
  field" as a project rule (F15), instead of rewriting those sites?
- **Q4** loom: nothing hand-written qualifies (F16); the hub's shutdown uses tokio `Notify`/`mpsc`, which loom cannot
  model. Leave it to the existing tokio tests, or extract the ingress-count + closed-flag protocol behind a small
  std-only type so loom can test it (an extra M step, builder tier)?
- **Q5** One shared pointer-act catalogue for MCP and the engine tool (not scheduled), or keep two adapters with RR-6's
  table on the MCP side only?
- **Q6** Should lane E wait for the `flows` merge decision, or run RR-12..RR-16 on main first and have `flows` rebase?
