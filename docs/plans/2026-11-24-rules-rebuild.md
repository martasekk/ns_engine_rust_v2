# Rules rebuild: bring ns_engine_rust_v2 in line with the coding rules

**Date:** 2026-11-24. **State:** planned; reviewed 24 Nov (`2026-11-24-rules-rebuild-review.md`, fixes applied); nothing started. **Base:** `main` at `ab2c3c6` (audited on branch
`rules-setup`). **Rules:** `~/.claude/CLAUDE.md` "Writing code", `~/.claude/rules/rust.md`, and the project
`CLAUDE.md` that RR-0 creates. **Builds on:** `archive/2026-09-08-engine-structure.md` §5 (what was left open in the
engine: reify the turn, one dispatch pipeline, measure `fold()`).

Out of scope: stub lints (`todo`/`unimplemented`/`dbg_macro`; the main chat adds them on `rules-setup`), the flow-engine
merge (`MERGE_INSTRUCTIONS.txt`), prompt caching (`prompt-cache-phase1`), new features, behaviour changes other than
those named in a step. Done means every scheduled step below is `done` with its gates green, and the project
`CLAUDE.md` names the gates and the accepted exceptions.

Audit method: line counts, a brace-counting fn-length scan (false positives in `memory-sqlite` from SQL strings were
dropped; the sizes below come from `grep -n fn` boundaries), Grep for `HashMap`/`unsafe`/locks/`spawn_blocking`,
`cargo fmt --all -- --check` (no build); the review added the `Vec::remove(0)`, unbounded-channel, transaction and
blocking-file-I/O checks (F19-F22) that the first greps skipped. **Not verified:** the workspace clippy run and the test suite at `ab2c3c6`
(no build allowed during the audit; RR-0 establishes them; the main chat's clippy of `ns-core` alone is already red,
see RR-0 item 5); whether `ns-app eval` runs fully offline (it uses scripted doubles,
`app/src/eval.rs:508`; RR-0 checks). Checked in review: `flows` builds one `ActionSpec`, in `crates/engine/tests/flow.rs`.

## 1. Current state

### 1.1 Findings, ranked (rule; why it matters; size)

| # | Where | Rule | Finding and why it matters | Size |
|---|---|---|---|---|
| F1 | 11 files, see RR-0 | gates | `cargo fmt --all -- --check` fails on `ab2c3c6`: 50 hunks in `components-std/src/transport.rs`, `core/src/value.rs`, `engine/src/turn/{accounting,memory,mod,retrieval}.rs`, `llm/src/schema.rs`, `memory-sqlite/src/lib.rs`, `pointer/tests/pointer.rs`, `provenance/src/{classify,index}.rs`. `HEAD` itself is unformatted. They are exactly the files with uncommitted edits in the main checkout; the review hashed each and they equal `rustfmt` of `HEAD` (no hand edits), and `MERGE_INSTRUCTIONS.txt:30` says the user means to undo them. Until settled (RR-G0), every step's fmt gate is red and every `cargo fmt --all` rewrites these files. | S |
| F2 | `memory-sqlite/src/lib.rs:8-11`, 25 `self.conn.lock().await` sites 524-1600 (6 in recall/facts, 19 in the `MemoryStore` impl; `schema.rs` has none) | rust.md: blocking in async | Every rusqlite call (FTS5 search, backfill over many rows, migrations) runs synchronously on a tokio worker under a `tokio::Mutex<Connection>`. One slow query stalls every task on that worker, across tenants. The locks are scoped and released before `enc.embed(..).await` (lines 911-917/932, 1160-1251/1267), so there is no lock across an await, only blocking. Precedent for the fix: `pointer/src/agent.rs:555` (`spawn_blocking`). | L |
| F3 | `memory-sqlite/src/lib.rs` | structure | One 2,775-line file (1,617 non-test): codecs, schema and migrations (282-509), lexical/vector/hybrid recall (510-812, 943), fact writes and embedding (813-942), and the `MemoryStore` impl (958-1617). Six jobs in one module. | M |
| F4 | `engine/src/turn/run.rs:21-713` | structure | `run_turn` is ~690 lines: prelude (routing, deep recall, applicability, 21-171), about ten `let mut` loop locals (175-190), the iteration loop (`for _ in 0..self.cfg.max_iterations` at 192-620) with three `settled = Some(match ..)` sites (207, 351, 637), and the reply tail (685). This is item 1 of archive §5, still open. `Bookkeeping` (`builtins.rs:47`) and `Ctx` (`builtins.rs:104`) already exist to build on. | L |
| F5 | `engine/src/state.rs:50` `fold`; callers `run.rs:38`, `run.rs:215` (inside the loop), `reply.rs:58`, `routing.rs:26`, `memory.rs:100` | rust.md: quadratic over growing data | Every turn folds the whole session log at least four times, plus once per loop iteration. Turn t costs O(t*e) for e events per turn, so a session of T turns costs O(T^2 * e). Archive §5.6 decided "measure before designing"; still unmeasured. | S (measure) |
| F6 | `core/src/action.rs:71-81` `ActionSpec` | structure: invariants where built; determinism | All fields `pub`, built by struct literal at ~45 sites in 17 files across 8 crates (31 literals in 14 files outside `ns-core`); `check_arg_names` runs separately (`HarnessBuilder::build`), so an unchecked spec can exist. `residual_policy: HashMap<String, ResidualRule>` sits in a `Serialize` type, so its JSON order would be random. Latent, not live: no spec has more than one residual key, nothing in `src` serializes a spec (the only use is a test probe, `engine/tests/turn_loop.rs:5071`), and both production paths are already checked (`core/src/plugin.rs:148`, `engine/src/turn/specs.rs:286`). The migration is the widest edit in the plan; Q7 asks whether to do it now. | M |
| F7 | `pointer/src/mcp.rs:77-220` `tools()`, `:481-705` `call()` (match arms 503-640) | structure: no match on content id | The MCP server lists each tool's JSON schema in one function and dispatches behaviour by matching the tool-name string in another (224 lines). A new tool means editing two places kept in step by hand. | M |
| F8 | none in any `Cargo.toml` | property tests | No proptest, bolero, loom or criterion anywhere. Risky pure code without property tests: `llm/src/salvage.rs:21` `salvage_arguments` (repairs model JSON by string surgery: fences, trailing commas, quotes, truncation), `pointer/src/wire.rs` (the pointer wire protocol: `Op`, `Request`, `Response`, `Key`), `core/src/validate.rs:6` `validate_args` (schema handling), `identity/src/jwt.rs` (token verification at a trust boundary). | M |
| F9 | `evolution/src/pass.rs:742-1172` `run_report` | structure | A 430-line fn with 15 phase markers written as comments ("1-2.", "3.", "4.", "4b" .. "9.", see RR-7). The same smell archive §1 fixed in `run_turn`. No in-flight work touches it. | M |
| F10 | `engine/src/turn/builtins.rs:204` `run_builtin` | structure: no match on content id | Six engine builtins dispatch on `proposal.action.as_str()`; their specs live separately in `turn/specs.rs`, which lists seven synthetic specs (`confirm_pending` has a spec and no handler) and a third name list, `is_engine_owned` (`specs.rs:239-244`). It is one deliberate place (its doc comment says so), so ranked below F7. | S |
| F11 | `identity/src/resolvers.rs:96,99` | unsafe / Miri | The only `unsafe` in the workspace: a test-only no-op waker and `Pin::new_unchecked`. `std::task::Waker::noop()` (stable since 1.85) and `std::pin::pin!` (1.68) remove it, so no Miri step is needed. | S |
| F12 | `memory-sqlite/src/lib.rs:746-770` `hybrid_hits` (and `hybrid_fact_hits` 654) | rust.md: algorithm at stated size | Exact kNN by brute force: loads every vector and text of the scope per query, cosine over all, full `sort_by` to take `coarse_k`. Fine at the assumed sizes (see RR-10); the full sort is O(N log N) where O(N) selection does. | S |
| F13 | `llm/src/emitter.rs:279` `emit` (243 lines) | structure | Long fn; `prompt-cache-phase1` plans edits to this file, so deferred. | M |
| F14 | `app/src/factory.rs:409` `build_engine` (267 lines), `:842` `build_pass` (145) | structure | Long assembly fns; `flows` and `prompt-cache-phase1` both touch `factory.rs`, so deferred. | M |
| F15 | `channel-hub/src/hub.rs:126`, `channel-hub/src/tenant.rs:49`, `pointer/src/client.rs:85`, `pointer/src/mcp.rs:501`, `components-std/src/pointer_tool.rs:468-623`, `components-std/src/desktop_channel.rs:58,96,137`, `channel-cli/src/lib.rs:59`, `app/src/registry/mod.rs:138` | rust.md: no lock across `.await` | `tokio::Mutex` held across an await. Each serializes one connection, one receiver or one per-tenant build, which is what an async mutex is for. Proposed as a named exception in the project `CLAUDE.md` (Q3), not a rewrite. | S (doc) |
| F16 | `channel-hub/src/hub.rs:150-248` | loom | The only hand-written coordination: ingress refcount (`AtomicUsize`, last `Drop` shuts down), `closed` flag checked under the queues lock, `tokio::sync::Notify`, wake sender taken on shutdown. loom cannot model tokio `Notify`/`mpsc`; the other atomics are counters or id sources (`pointer/src/client.rs:13-15`, `channel-tcp/src/listener.rs:47,237-251`, `engine/src/dispatch.rs:165`, `pointer/src/backoff.rs:185` test-only). **No code qualifies for a loom test as written** (Q4). | - |
| F17 | `app/src/config.rs` (1,759 non-test lines), `testkit/src/eval.rs` (2,739 non-test), `engine/tests/turn_loop.rs` (6,623) | structure | Large but each is one job (config sections; eval harness; one test binary). Not scheduled; listed so nobody re-audits them. | - |
| F18 | repo root | gates | No project `CLAUDE.md`, no CI, no `.cargo/config.toml`, no `rust-toolchain.toml` (audited with rustc 1.98.1). Clippy is red on `ns-core` (`large_enum_variant`, see RR-0b); the workspace state is unmeasured. | S |
| F19 | `memory-sqlite/src/lib.rs:959-986` `append`, `:857-863` `write_fact`; no `BEGIN`/`transaction(` in the file | data loss (global rules: not a shortcut) | `append` runs one autocommit `INSERT` per event: a crash leaves part of a turn in the log, and each event is its own commit (most of F2's blocking time). `write_fact` marks the old version superseded, then inserts the new one: a failure between leaves the key with no current value. Fix: RR-9c. | M |
| F20 | `engine/src/turn/memory.rs:112-118`; `core/src/budget.rs:255-256` and `362-363` | rust.md: quadratic over growing data | `Vec::remove(0)` in a loop that re-measures the whole window per removal: `memory.rs` re-renders every remaining record each pass (on a summary rebuild `from = 1`, so quadratic in turns); `budget.rs` the same twice. `memory.rs` is an F1 file; `budget.rs` is modified by prompt caching. Fix: RR-19. | S |
| F21 | `channel-hub/src/hub.rs:64`, `app/src/registry/mod.rs:635` | rust.md: unbounded channels | Unbounded channels. `hub.rs:64` is bounded by tenant count per its own comment: say so in the comment/`CLAUDE.md`; `registry/mod.rs:635` to be checked in RR-19 (decision: bound it or document why not). | S |
| F22 | `app/src/admin/mod.rs` (43 `std::fs` uses), `app/src/tenant.rs` (9) | rust.md: blocking in async | Blocking file I/O in async code was **not audited**. Recorded in "Not scheduled"; audit when `flows`/prompt caching free `app/`. | - |

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
| Main checkout, uncommitted (fmt edits, F1); equal to `rustfmt` of `HEAD`, no hand edits | the 11 files in F1 | RR-G0 (settled there: discarded, replaced by one fmt commit); RR-8, RR-9, RR-10 (memory-sqlite); RR-12b (`llm/src/schema.rs`, `provenance/src/classify.rs`); RR-13..RR-15 (`turn/mod.rs`); RR-19 (`turn/memory.rs`) |
| Branch `flows` (`../ns_engine_rust_v2-flows`): 11 commits, 0 behind main | `engine/src/turn/{run,prompt,legal,config}.rs`, `core/src/flow.rs` (new), `app/src/{config,factory,tenant,main}.rs`, `app/src/admin/*`. It touches `run.rs` in only 3 hunks (22 lines, at 169-190, the RR-13/RR-14 seam); one `ActionSpec` literal, in `crates/engine/tests/flow.rs` (`core/src/flow.rs` builds none); no overlap with the 11 F1 files | RR-12b (one literal), RR-13, RR-14 (the seam), RR-16, RR-18; rebasing `flows` over lane E is small (Q6) |
| Branch `prompt-cache-phase1` (plan untracked in main): **no commits** (`git log main..prompt-cache-phase1` is empty) | its work is uncommitted in 10 files of the main checkout: `app/src/{config,factory,tenant}.rs`, `core/src/{budget,router,traits}.rs`, `engine/src/{script,trace}.rs`, `engine/src/turn/prompt.rs`, `llm/src/emitter.rs` (487 lines changed) | RR-12b (`core/src/traits.rs` 3 literals, `engine/src/script.rs` 2, `llm/src/emitter.rs` 1), RR-17, RR-18, RR-19 (`core/src/budget.rs`) |
| `rules-setup` (main chat): stub lints (`5874024`), this plan, the future `CLAUDE.md`; 3 commits ahead of main | root `Cargo.toml`, every member `Cargo.toml`, `docs/plans/*` | everything: lanes branch from main, so they cannot read their own block until RR-G0 merges it; RR-1 waits on that merge |

Free after RR-G0 (gate zero): RR-2 (after RR-1, it edits the root `Cargo.toml`), RR-6, RR-7, RR-11. Free after RR-1:
RR-3, RR-4, RR-5.

## 3. Build order

Lanes (each lane is its own worktree; target dirs are **two slots**, `C:/Users/Acemagic/ns-target/slot-a` and
`slot-b`, that the main chat assigns at launch: seven per-lane dirs of 1-25 GB each would fill the drive; at most
**two** lanes build at once on this PC, and only one while another session is building). **No lane starts before
gate zero (RR-G0) is done**: lanes branch from main, and main holds the plan, `CLAUDE.md`, the stub lints and the fmt
commit only after it.

| Lane | Steps | Starts when |
|---|---|---|
| G gate zero | RR-G0 (fmt commit, RR-0, RR-0b, merge to main) | now; needs the user's yes on Q2 and on the merge |
| 0 infra | RR-1, RR-2 | after RR-G0; RR-2 after RR-1 (both edit the root `Cargo.toml`) |
| P property tests | RR-3, RR-4, RR-5 | after RR-1 |
| M mcp | RR-6 | after RR-G0 |
| V evolution | RR-7 | after RR-G0 |
| S store | RR-8a, RR-8b, RR-9a, RR-9b, RR-9c, RR-10 | after RR-G0 and RR-1 |
| E engine | RR-11, RR-11b (conditional), RR-12a, RR-12b, RR-12c, RR-13, RR-14, RR-14b, RR-15, RR-16, RR-19 | RR-11 after RR-G0; RR-12b+ after the `flows`/prompt-cache decision (Q6, Q7) |
| D deferred | RR-17, RR-18 | after `prompt-cache-phase1` (and `flows` for RR-18) |

Rules for every step: refactor-only commits come first and change no behaviour; a pure move needs no failing-first
test, its proof is the crate's suite green with the **same test count** before and after (the builder records both
counts in the commit message). When a property test fails on current code, the failing input becomes a plain
`#[test]` that asserts today's behaviour with a comment and goes in "Needs next:"; never `#[ignore]`, never a red
commit. Lane agents do not edit this plan, `docs/plans/README.md` or `CLAUDE.md`: the agent puts state and numbers in
its commit message and report, and the main chat writes §5, the README row and the `CLAUDE.md` line in the merge
(adjacent lines edited from parallel lanes would conflict on every second merge). Test-only steps prove "would fail without it" by mutation: the reviewer breaks one
line of the code under test and sees the property fail. Gates for every step unless stated:
`cargo fmt --all` then `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --target-dir <lane dir> -j 8 -- -D warnings`;
`cargo test -p <crate> --target-dir <lane dir> -j 8` (`<lane dir>` is the slot dir the main chat assigned).

---

### RR-G0. Gate zero: land `rules-setup` on a main that every lane can start from

Goal: lane worktrees (created from main) contain this plan, the stub lints and `CLAUDE.md`; `cargo fmt --all` leaves
the tree unchanged; the workspace clippy gate is green. Without it every step's start condition contradicts its gate
(fmt and clippy are red on main, and `cargo fmt --all` in write mode rewrites the 11 F1 files in every lane).
Lane G. Depends on: the user's answers to Q2 and to the merge. Size S. Tier: the main chat does the git; the steps
RR-0 and RR-0b are `worker`. **No other step starts before this one is done.**

```
Start here (RR-G0)
Files: the 11 F1 files (fmt commit); RR-0's and RR-0b's files; no other source file.
Steps, in order:
 1. Q2 answered (recommended: commit the formatting). `git status` in the main checkout: list what else is dirty
    (the 10 prompt-cache files must stay untouched).
 2. On `rules-setup`: `cargo fmt --all`; commit ONLY the 11 F1 files as
    "fmt: format the files HEAD left unformatted (no behaviour change)".
 3. RR-0 (CLAUDE.md and the baseline), then RR-0b (clippy green), each its own commit.
 4. With the user's yes: in the main checkout `git checkout -- <the 11 F1 files>` (git refuses to merge over dirty
    files; their content equals `rustfmt` of `HEAD`, which step 2 commits, so nothing is lost; if Q2 was "undo it",
    skip step 2 and the files stay unformatted, which keeps the fmt gate red: not recommended). Then merge
    `rules-setup` into main and `graphify update` the main checkout.
Acceptance (on main): `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`
  both pass; `CLAUDE.md` and this plan exist on main; the baseline counts are in §5.
Gates: the two acceptance commands.
```

### RR-0. Project CLAUDE.md and the gate baseline

Goal: the repo states its gates, layout, plan location and conventions, and the gates are known to pass on the base.
Part of gate zero (RR-G0 step 3); runs after the fmt commit. Size S. Tier `worker`.

```
Start here (RR-0)
Files: CLAUDE.md (new, repo root); nothing else (the fmt commit is RR-G0 step 2, before this).
Read first: this plan §1, §2 and "Proposed CLAUDE.md" below; Cargo.toml lines 1-20 (members);
  the first `//!` line of each crate's src/lib.rs (core, engine, llm, memory-sqlite, provenance and
  components-std have none: read their lib.rs `pub mod` list instead) to check each layout line.
Steps:
 1. Write CLAUDE.md from "Proposed CLAUDE.md". Every line must be true at the commit: fix any
    crate description the lib.rs check contradicts. Do NOT add the "added by later steps" lines.
 2. (The fmt commit already landed in RR-G0 step 2; check `cargo fmt --all -- --check` passes.)
 3. Baseline, once, own --target-dir: `cargo clippy --workspace --all-targets -- -D warnings`,
    then tests per crate (`cargo test -p <crate>`, each < 10 min). Record pass/fail counts per
    crate in §5 of this plan, and the wall time of each test binary; write CLAUDE.md's "slowest test
    binary" line from those times (it was not measured when the plan was written).
 4. Check whether `ns-app eval` (app/src/eval.rs) runs without network; if it does, add its
    command and request count under "Invariants to re-check after refactors".
 5. If clippy is red: record the warnings in §5 and hand over to RR-0b (next block; refactor-free fixes).
    Known red already (main chat, 24 Nov): `cargo clippy -p ns-core --all-targets -- -D warnings` fails with
    `clippy::large_enum_variant` in `ns-core`, on `ab2c3c6` with and without the stub lints.
Gates: fmt check; the baseline runs above.
Stop after: CLAUDE.md commit; record the baseline in the commit message and the report (the main chat writes §5).
```

### RR-0b. Make clippy green (gate zero, refactor-free)

Goal: `cargo clippy --workspace --all-targets -- -D warnings` passes on the base. Lane G. Depends on: RR-0 (so the
baseline lists every red item, not only the known one). Size S. Tier `worker`.

```
Start here (RR-0b)
Files: crates/core/src/event.rs (likely the enum; `EventKind` is at event.rs:15) or whichever enum the clippy
  message names; any other file the baseline's warnings name (fix only what clippy names).
Read first: the clippy output for the named enum; the enum; Grep its variants' use sites outside ns-core.
Rule: name the enum and the variant sizes. If its variants are built or matched outside `ns-core` (likely for
  `EventKind`), use `#[expect(clippy::large_enum_variant, reason = "..")]` NOW (one line, no collision with
  `flows` or prompt caching) and record the sizes in the commit. Boxing a public variant is a cross-crate contract
  change and gets its own step later (add a §5 row). If the variants are used only inside `ns-core`, box the large one.
Acceptance: workspace clippy (command above) passes; `cargo test -p ns-core` same count as the RR-0 baseline.
Gates: fmt check; clippy workspace; `cargo test -p ns-core`.
```

**Proposed CLAUDE.md** (what holds today; RR-0 writes it):

```
# ns_engine_rust_v2: project rules
A message-driven agent engine: a model proposes actions, the engine checks them (provenance,
guards, confirmation) and runs tools; memory in SQLite; an offline evolution pass learns rules.

## Layout (Cargo workspace; no rust-toolchain.toml, built with stable rustc 1.98)
Package names are `ns-<dir>`; library names are `ns<dir>` with underscores (`nscore`, `nsmemory_sqlite`).
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
The slowest test binary is <RR-0 step 3 measures it and writes this line>; filter it when you can.
Scaling tests are `#[ignore]` and no gate runs them: run them on a change to `fold` or hybrid recall, and
before the final report: `cargo test -p <crate> --release -- --ignored <name>` (names in RR-10, RR-11).

## Plans
docs/plans/YYYY-MM-DD-<topic>.md, index in docs/plans/README.md. Rules rebuild: 2026-11-24-rules-rebuild.md.

## Conventions
- Maps whose iteration order is observed (output, hashing, persistence) are BTreeMap.
- serde_json must not enable `preserve_order`: event hashes and call keys rely on byte order.
- Scaling tests for hot paths run in --release and assert roughly linear growth 1x -> 10x.
- Refactor-only commits keep every crate's test count unchanged and say so in the message.

## Invariants to re-check after refactors
- Test counts per crate unchanged by refactor-only commits.
- The order and bytes of the tool array (`synthetic_specs`, `engine/src/turn/specs.rs`) and the event hash chain.
```

**Lines added by later steps** (written by the main chat in the merge of the step, never by a lane agent, so
`CLAUDE.md` never states what is not yet true and parallel lanes do not conflict on it):
- RR-1, under Conventions: "Property tests: bolero 0.13 (workspace dev-dependency), always `with_iterations(N)`; a
  failing input becomes a plain #[test] before the fix."
- RR-2, under Conventions: "`unsafe_code = \"forbid\"` in `[workspace.lints.rust]`; adding any `unsafe` needs a
  Miri-run test and lifting the lint for that crate."
- RR-9b, under Conventions: "SQLite is reached only through memory-sqlite's `Db::call` (spawn_blocking); never call
  rusqlite on an async worker."
- After the user answers Q3 (yes), whoever lands next adds under Conventions: "A tokio::Mutex may be held across
  .await only to serialize one connection, one receiver or one per-key build; the field's doc comment says which."
- RR-0 step 4 (if offline): the eval command and its request count under Invariants.

---

### RR-1. Test-tool dependencies, one commit

Goal: bolero is available to every crate that gets a property test, added once so parallel lanes never edit
`Cargo.toml`/`Cargo.lock` concurrently (contract-first for the build files). Lane 0. Depends on: RR-G0 (the stub
lints already landed on `rules-setup` as `5874024`; RR-1 waits for the merge to main). Size S. Tier `worker`. Add
bolero to `engine` only when RR-11b is approved.

```
Start here (RR-1)
Files: Cargo.toml ([workspace.dependencies], after line 35);
  crates/{llm,pointer,core,identity,engine,memory-sqlite}/Cargo.toml ([dev-dependencies]);
  Cargo.lock (generated); crates/core/tests/json_order.rs (new guard test, step 4). The CLAUDE.md Conventions
  line is written by the main chat in the merge (see "Lines added by later steps").
Read first: root Cargo.toml lines 1-60; each of the six member Cargo.toml files (each < 40 lines).
Steps:
 1. Root: `bolero = "0.13"` under [workspace.dependencies].
 2. Each of the six: `bolero = { workspace = true }` under [dev-dependencies].
 3. `cargo check -p ns-llm --tests` to resolve the lockfile.
 4. Guard test in ns-core (the convention "serde_json without `preserve_order`" had no test behind it):
    `json_key_order_is_sorted`: `json!({"b":1,"a":2}).to_string() == r#"{"a":2,"b":1}"#`.
    Commit Cargo.toml files + Cargo.lock + the test.
Acceptance: `cargo tree -p ns-core -e dev | grep bolero` lists 0.13.x; `git diff Cargo.lock` only
  adds bolero's tree (no other dependency version moved; `Cargo.lock` does not record features, so also:)
  `cargo tree -e features -i serde_json | grep preserve_order` prints nothing; the guard test passes.
Gates: fmt check; clippy workspace; `cargo test -p ns-core` (smoke).
```

### RR-2. Remove the only `unsafe` (test helper)

Goal: zero `unsafe` in the workspace, enforced by the compiler, so no Miri lane exists. Lane 0. Depends on: RR-1
(both edit the root `Cargo.toml`; run RR-2 after it in lane 0). Size S. Tier `worker`.

```
Start here (RR-2)
Files: crates/identity/src/resolvers.rs lines 80-110 (test module `block_on`); root Cargo.toml
  (`[workspace.lints.rust]`: add `unsafe_code = "forbid"`; every member already has `[lints] workspace = true`).
  The CLAUDE.md line is written by the main chat in the merge.
Read first: those lines, and the root Cargo.toml lints table.
Steps: replace the hand-made RawWaker vtable with `std::task::Waker::noop()` (returns &Waker;
  `Context::from_waker(Waker::noop())`) and `Pin::new_unchecked(&mut fut)` with
  `let mut fut = std::pin::pin!(fut);`. Delete the safety comment.
Acceptance: workspace clippy green with `unsafe_code = "forbid"` (a grep for "unsafe" also matches the comment at
  `crates/engine/src/dispatch.rs:555`, so the lint is the check); the identity resolver tests pass with the same
  count (they exercise this helper).
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
Read first: salvage.rs 1-372 (the repairs are 72-372, not 1-140) and the test names after line 373.
Inputs: a random `String` almost never reaches a repair, so drive tests 1 and 4 from a valid object plus random
  corruptions (fence, single quotes, trailing comma, bare keys, Python literals, a cut), as well as raw strings.
Tests (each `bolero::check!().with_type::<..>().with_iterations(2000)`):
 1. no_panic: any `String` or corrupted object (lengths 0..~512, multibyte included) -> returns.
    Edge: a multibyte char right at a truncation point (byte-index slicing).
 2. valid_object_is_never_altered: object from `Vec<(String, i64, bool, String)>` with keys
    filtered to [a-z_]{1,12}; result is `None` or `Some((obj, _))` - never a different value.
    Edge: string values holding `'`, `,}`, `True`, `None` (the quote and python repairs).
 3. fenced_object_is_recovered: the same object (at least one key: `parse_object` refuses `{}` on purpose,
    `salvage.rs:56-60`) inside "```json\n..\n```" -> `Some((obj, _))`.
    Edge: a fence with no language tag; the empty object `{}` -> `None` (named plain #[test]).
 4. no_invented_keys: any `String` or corrupted object -> if `Some((Value::Object(m), _))`, every key of m
    occurs in the input text (the module doc's promise); compare keys after JSON unescaping, or generate
    escape-free keys. Edge: input `{a` (truncation closes it).
 5. truncated_is_object_or_none: cut a serialized object (at least one key) at any char boundary -> `None` or
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
 3. response_round_trip: same for `Response::ok(id, ..)` and the error form. `TextBox.confidence` is `f32`
    (`wire.rs:272`): generate finite values only (NaN does not round-trip through JSON).
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
  crates/identity/src/jwt.rs (Hs256Verifier 45-165) and the public signer `nsidentity::mint_hs256`
  (`crates/identity/src/issue.rs:42`). `minting.rs` is `#[cfg(test)] mod minting` with `pub(crate)` fns
  (`lib.rs:31-32`): an integration test in `crates/identity/tests/` cannot call it. If forged headers are
  needed, put that property in `src/jwt.rs`'s test module, where `crate::minting::mint` is visible.
Read first: validate.rs 1-75; jwt.rs 1-165; issue.rs 24-86 (the `mint_hs256` signature).
Tests (bolero, 2000 iterations each):
 1. validate_no_panic: depth-limited JSON (nest <= 4) built from primitives as both schema
    and args -> returns (`serde_json::Value` has no bolero generator: build it from a `Vec<u8>` driver).
    Edge: `required` naming a missing key; `type` not a string.
 2. validate_required: schema {type: object, required: [k], properties: {k: {type: string}}};
    args without k -> Err; with k a string -> Ok. Edge: k = "" and k with unicode.
 3. jwt_round_trip: random 32-byte key, tenant id [a-z]{1,16}, exp after the verifier's clock
    -> mint -> verify Ok.
 4. jwt_tamper: replace one character of a valid token with a different one from the base64url alphabet or `.`
    -> Err (a raw byte flip can make the `String` invalid UTF-8). Edge: each of the three segments; a separator `.`.
 5. jwt_no_panic: any String -> verify returns.
Acceptance (mutation): make the verifier skip the signature compare -> test 4 fails.
Gates: fmt check; clippy workspace; `cargo test -p ns-core --test validate_props`;
  `cargo test -p ns-identity --test jwt_props`.
```

### RR-6. MCP tools as one table (golden first, then refactor)

Goal: each MCP tool is one table row (name, schema, irreversible, handler); no `match` on the tool-name string. Lane
M. Depends on: RR-G0. Size M. Tier `worker` (behaviour is pinned by a golden). Collision: none (`pointer/src/mcp.rs` and
`pointer/tests/mcp.rs` are not in flight).

```
Start here (RR-6)
Files: crates/pointer/src/mcp.rs: tools() 77-220, irreversible() 262-267 (a third per-tool name list: it
  matches tool names and feeds gate()), gate() 358-412, call() 481-705 (arms 503-640).
Read first: mcp.rs 1-76 (imports, key parsing), 77-220, 222-267, 358-412, 481-705 (~510 lines).
Commit 1 (test only): in crates/pointer/tests/mcp.rs add `tools_list_is_unchanged`: the
  tools/list JSON (`tools()` takes no argument, so one golden, not one per gate setting) compared with a
  golden file crates/pointer/tests/golden/mcp_tools.json written from the current code; and
  `unknown_tool_is_the_same_error` (name "nope" -> today's exact error JSON).
Commit 2 (refactor, no behaviour change): one table, e.g. an enum `ToolId` with
  `const TOOLS: &[(&str, ToolId)]`, `ToolId::schema()` and `async fn run(..)` matched on the enum
  (or a struct of fn pointers if that is less code); tools() iterates the table; call() looks
  the name up once and runs the row; `irreversible()` reads the row's flag. Keep `gate()` before dispatch.
Acceptance: both tests green before and after commit 2; existing mcp tests same count.
Gates: fmt check; clippy workspace; `cargo test -p ns-pointer --test mcp`.
```

### RR-7. `run_report` as phase methods (refactor only)

Goal: `run_report` reads as a list of named phases, each a method with explicit inputs and outputs; no behaviour
change. Lane V. Depends on: RR-G0. Size M. Tier `worker`.

```
Start here (RR-7)
Files: crates/evolution/src/pass.rs: Report 202-230, run_report 742-1172.
Read first: pass.rs 195-240 and 742-1172 (~470 lines). Do not read the tests (1183+).
Steps (one commit): extract, in order, keeping the marker comments as fn docs. Actual markers in pass.rs (the fn
  ends at 1169): 743 "1-2.", 749 "3.", 764 "4.", 788 "4b", 809 "4b°" (embeddings backfill), 851 "4b′",
  863 "4b''" (success lane), 884 "4c", 912 "5.", 957 "6.", 980 "6b", 1001 "6c", 1013 "7.", 1149 "8.", 1160 "9.".
  Phases: load_sessions (749-763: "3."), mine_and_evaluate (764-787: "4."), grade (788-808: "4b"),
  backfill_embeddings (809-850: "4b°"), calibrate (851-862: "4b′"), mine_success_lane (863-883: "4b''"),
  fitness (884-911: "4c"), symbolic_lane (912-979: "5.", "6."), consolidate_and_digest (980-1012: "6b", "6c"),
  notes_lane (1013-1148: "7."), apply (1149-1159: "8."), record_spend (1160-1169: "9."). Each takes &self,
  &mut Report and only the locals it uses; each `dry_run` check stays inside its phase.
  Ranges are from the review; confirm with `grep -n "        // [0-9]" pass.rs` (cut at each marker's line).
Acceptance: `cargo test -p ns-evolution` green with the same test count (record before/after);
  run_report <= 80 lines; no phase method > 150 lines.
Gates: fmt check; clippy workspace; `cargo test -p ns-evolution`.
```

### RR-8a / RR-8b. Split `memory-sqlite/src/lib.rs` into modules (refactor only)

Goal: one module per job; `lib.rs` keeps the struct, the `MemoryStore` impl and the tests. Lane S. Depends on:
RR-G0 (F1 settled: this file was dirty in the main checkout), RR-1. Size M each. Tier `worker`. The schema fns are
associated fns of `impl SqliteStore` (lib.rs 282-509): the move is a second `impl SqliteStore` block in `schema.rs`.

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

Goal: no rusqlite call runs on a tokio worker after `open`; the connection is reachable only through `Db::call`,
which uses `spawn_blocking`. `SqliteStore::open` stays sync and runs migrations and the FTS rebuild (222-246) on a
worker through `app/src/models.rs:171` <- `app/src/factory.rs:486`: it gets a `// shortcut: open() runs sync on the
caller's worker; wrap it in spawn_blocking once factory.rs is free` at `models.rs:171`, and the `CLAUDE.md` line
says "after `open`". Lane S. Depends on: RR-8b. Size M + M. Tier **`builder`**: async/blocking boundary, `'static`
closures force owned arguments at 25 sites (6 in recall/facts for RR-9a, 19 in the impl for RR-9b; `schema.rs`
has none, its fns take `&Connection`), poison handling, and a wrong move loses writes. `spawn_blocking` needs a
tokio runtime; no other executor drives the store today (grep), so this is safe.

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
Files: lib.rs `impl MemoryStore for SqliteStore` (958-1617 before RR-8; `grep -n "impl MemoryStore" lib.rs`).
  The CLAUDE.md Db::call line is written by the main chat in the merge.
Read first: that impl (~600 lines) in two halves; convert each half before reading the next.
Tests: run the existing conformance suite (lib.rs `nsengine_conformance`, ~1823) under both
  `#[tokio::test(flavor = "current_thread")]` and `flavor = "multi_thread"` (add the second).
Acceptance: `grep -n "\.lock()" crates/memory-sqlite/src/*.rs` shows the connection locked only in
  db.rs (one site); crate tests green; `cargo test -p ns-engine` green.
Gates (both): fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`; RR-9b also
  `cargo test -p ns-app` (cross-crate user).
```

### RR-9c. Transactions around multi-statement store writes (behaviour change)

Goal: a crash or failed statement never leaves part of a turn in the log or a fact key with no current value (F19;
data-loss handling is not a shortcut). Lane S. Depends on: RR-9b (`Db::call` hands `&mut Connection`, so
`conn.transaction()` fits). Size S. Tier `builder` (behaviour commit on the write path).

```
Start here (RR-9c)
Files: crates/memory-sqlite/src/lib.rs `append` (959-986 before RR-8) and `write_fact` (857-863; in facts.rs
  after RR-8b); tests in the same crate.
Read first: those two fns and their callers in the `MemoryStore` impl.
Steps: tests first, then wrap each in one `conn.transaction()` committed at the end.
Tests: append_is_all_or_nothing (a test-only trigger raises on the third row of a batch; `load` returns none of the
  batch); a_failed_fact_insert_keeps_the_previous_current_version (the new insert fails; the old version stays
  current). Both fail before the change.
Gates: fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`; `cargo test -p ns-engine`.
```

### RR-10. Hybrid recall: selection instead of full sort, and a scaling test

Goal: ties are broken deterministically with a total order, and growth is shown near-linear; candidate selection by
`select_nth_unstable_by` is optional (sorting 10^4 pairs takes under 1 ms next to the ~40 MB vector load and two
network round trips; the scaling test also passes before the change, since N log N at 10x is a ratio of ~13.3, under
the limit of 15, so it guards the load cost, not the selection). Lane S. Depends on: RR-9b (and RR-1 for bolero).
Size S. Tier `worker`.

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
Changes (all in recall.rs): use `f32::total_cmp` at the two compares (`partial_cmp(..).unwrap_or(Equal)` at 701 and
  769 is not a total order when a score is NaN, and std sorts may panic on that); add `ORDER BY e.rowid` to
  `turn_vectors` (568-574 has none, so "today's order" for ties is whatever SQLite returns); selection with
  `select_nth_unstable_by` only if the builder wants it.
Tests: top_k_matches_full_sort (bolero, 2000; includes NaN scores) - for generated score lists with ties, the selected
  coarse_k equal the first coarse_k of a stable full sort with the same tie-break;
  recall_scales_linearly (#[ignore], --release): 1x = 1,000 and 10x = 10,000 stored vectors,
  time hybrid_hits with a fake encoder; assert the ratio < 15.
Gates: fmt check; clippy workspace; `cargo test -p ns-memory-sqlite`;
  `cargo test -p ns-memory-sqlite --release -- --ignored recall_scales_linearly`.
```

### RR-11. Measure `fold()` (archive §5.6)

Goal: know whether folding the whole log per call matters at the stated sizes. Lane E. Depends on: RR-G0 (`state.rs`
is not in flight; new test file). Size S. Tier `worker`.

```
Start here (RR-11)
Files: crates/engine/tests/fold_scaling.rs (new). Under test: crates/engine/src/state.rs:50 fold.
Read first: state.rs 1-60 and the test helpers at 280-340 (how a log is built).
Test (#[ignore], --release): sessions of 200 and 2,000 events (mix of the real `EventKind` variants:
  UserSaid, Proposed, ToolCalled, ToolReturned, Settled, Replied; `crates/core/src/event.rs:15`); time 100 folds
  each; print ns/event; assert the 10x ratio < 15 (linear per call). Print the time of five folds of 2,000 events
  (one turn's worth) and the same as a share of a measured turn if one is available.
Acceptance: the numbers go into the report (the main chat writes §5) with a decision: RR-11b proceeds only if five
  folds of 2,000 events exceed 5 ms, or 1% of a measured turn (per-turn threshold, assumed, Q1: a session total
  over 10^6 event visits can pass 50 ms while costing under 1 ms per turn next to a model call); otherwise RR-11b
  is dropped and archive §5.6 is closed.
Gates: fmt check; clippy workspace; `cargo test -p ns-engine --release --test fold_scaling -- --ignored`.
```

RR-11b (conditional, `builder`, M): `SessionState::apply(&mut self, &Event)` with `fold = default + apply each`, and
an incremental state carried through `run_turn` instead of re-folding. Property test (bolero): for any event sequence
split at any point, `fold(a ++ b) == apply_all(fold(a), b)`. Depends on RR-15 (the turn state exists) and adds bolero to
`engine`'s dev-dependencies (not in RR-1). Written in detail only if RR-11 says so.

### RR-12a / RR-12b / RR-12c. `ActionSpec` checked where it is built (contract step: add, migrate, seal)

Goal: an `ActionSpec` cannot exist unchecked, and its serialized form is deterministic. Lane E. Depends on: RR-G0;
for RR-12b, the `flows` decision (Q6: one literal in `crates/engine/tests/flow.rs`) and the prompt-cache work
landed or its 10 files committed (§2: `core/src/traits.rs` 3 literals, `engine/src/script.rs` 2,
`llm/src/emitter.rs` 1, all modified in that worktree), and Q7 answered. Size S + M + S. Tier **`builder`**: public
API of `ns-core` used by 8 crates (31 literals in 14 files outside `ns-core`; the tier reason is the API breadth,
not a wire format: no non-test code serializes or deserializes `ActionSpec` or `LegalActionSet`). **Each step is
reviewed and merged before the next; RR-12a-c before RR-13 and RR-16 start.**

Why three steps: `#[non_exhaustive]` makes every struct literal outside `ns-core` a compile error, so adding it in
the same step as the constructors leaves the workspace clippy gate red. Expand (12a: source-compatible today, since
33 literals write `residual_policy: Default::default()` and `engine/src/guards.rs:124` and
`engine/tests/turn_loop.rs:814` use `.collect()`), migrate (12b), then seal (12c).

Design (hard to reverse, justified): `residual_policy: BTreeMap<String, ResidualRule>` (same JSON object shape, so
configs still parse); constructor `ActionSpec::new(name, description, args_schema, side_effect) -> Result<Self,
SpecError>` plus `with_residual(..)` and `with_dedupe_tag(..)`; `new` runs `check_arg_names`; after RR-12c,
`#[non_exhaustive]` on `ActionSpec` so other crates must use them. No `#[serde(try_from = "RawActionSpec")]` unless
the builder finds a deserialization site (none found). The existing checks stay while fields are writable:
`core/src/plugin.rs:148` (`BuildError::BadArgName`), `engine/src/turn/specs.rs:286-296`,
`llm/src/schema.rs:416-433`. Fields stay `pub` for reading: no `src` site assigns a spec field after construction
(Grep found none), so getters would be churn without a guarded invariant. The code gets
`// shortcut: fields stay pub and writable; switch to private fields + getters if a mutation site appears.`
Rejected: private fields + getters now (every `.name`/`.args_schema` read changes, hundreds of sites, for no invariant
today); leaving the check in `HarnessBuilder::build` only (specs built elsewhere, e.g. the pointer tool and tests, are
never checked).

**Collision list for RR-12b** (31 literals in 14 files outside `ns-core`): `llm/src/schema.rs` (5 literals) and
`provenance/src/classify.rs` (2), both F1 files (settled by RR-G0); `core/src/traits.rs` (3), `engine/src/script.rs`
(2) and `llm/src/emitter.rs` (1), modified in the prompt-cache worktree (no commits yet); `engine/tests/flow.rs` (1)
on `flows`. RR-12b is the widest edit in the plan (17 files, 8 crates); S8/Q7 recommends RR-12a now and RR-12b/c
after both branches land.

```
Start here (RR-12a: add, in core; no `#[non_exhaustive]`)
Files: crates/core/src/action.rs 64-140 (ResidualRule, ActionSpec, check_arg_names); core's own
  literal sites (`grep -n "ActionSpec {" crates/core/src`); the two `.collect()` sites
  (`engine/src/guards.rs:124`, `engine/tests/turn_loop.rs:814`) must still compile (BTreeMap collects).
Read first: action.rs 1-140 and 340-380 (the existing check test).
Tests first: spec_new_rejects_arg_sorting_before_rationale (an arg "A" -> Err);
  residual_policy_serializes_in_key_order (one spec, 12 keys inserted in reverse order -> the
  serialized keys come out sorted; a HashMap passes by chance with probability ~1/12!).
Acceptance: tests green; `cargo test -p ns-core`; the workspace still compiles and clippy is green (nothing outside
  core changes).

Start here (RR-12b: migrate every other crate)
Files: the 31 literals outside ns-core: `grep -rln "ActionSpec {" crates app` (engine 5 files, components-std 3,
  llm 2, evolution, provenance, testkit, app 1 each; see the collision list). Each becomes `ActionSpec::new(..)?`,
  or `.expect("static spec is valid")` for constant specs (with a comment saying so).
Read first: each site's surrounding fn only.
Acceptance: `cargo test --workspace --all-targets` green (cross-crate change).

Start here (RR-12c: seal)
Files: crates/core/src/action.rs (add `#[non_exhaustive]` to `ActionSpec`); any literal still left (the compiler
  lists them).
Acceptance: workspace clippy and `cargo test --workspace --all-targets` green.
Gates (all three): fmt check; clippy workspace; tests as stated.
```

### RR-13 / RR-14 / RR-14b / RR-15. Reify the turn (`run_turn` into phases; refactor only)

Goal: `run_turn` becomes a short sequence of phase calls over one `TurnState`; no behaviour change. Archive §5.1.
Lane E. Depends on: the `flows` decision (it edits `run.rs` in 22 lines at 169-190, the RR-13/RR-14 seam; Q6), RR-G0
(the `turn/*` fmt edits settled), RR-12c. Size M, L, L, M. Tier **`builder`**: the archive warns the borrow checker resists (`&mut ctx`, `&mut log` and
`&self` at once), and this is the heart of the engine.

Invariant for all three: `cargo test -p ns-engine` (incl. `--test turn_loop`, `--test act_or_answer`) green with the
same count, and the RR-0 offline eval request count unchanged if RR-0 found one.

```
Start here (RR-13: the prelude)
Files: crates/engine/src/turn/run.rs 21-171; new type `TurnPlan` in crates/engine/src/turn/plan.rs.
Read first: run.rs 1-200; builtins.rs 47-130 (Bookkeeping, Ctx); turn/mod.rs 1-131.
Facts: `log` and `usage` are created inside the prelude (`run.rs:30`, `36`), so they cannot be parameters.
Steps: move routing, deep recall and applicability (scope_holds_facts, recall_applies) into
  `async fn plan_turn(&self, incoming: &Incoming) -> Result<(TurnPlan, EventLog), EngineError>` (the
  `cap_reached()?` check stays its first statement). TurnPlan holds sid, scope, turn, the rules snapshot,
  `n_loaded`, `usage`, `routed.cues`, the user text, the two applicability flags and the INITIAL `tier` and
  `selected_tools` (they are reassigned inside the loop at `run.rs:435` and `438`, so RR-14 copies them into
  `TurnState`; the plan keeps the initial values). run_turn calls it. One commit; run_turn shrinks by ~150 lines.

Start here (RR-14: the loop state and body)
Files: run.rs 172-620 (from `let mut book` at 175 to the loop's closing brace at 620; the loop is
  `for _ in 0..self.cfg.max_iterations {` at 192, so `grep -n "loop {"` prints nothing: use
  `grep -n "max_iterations" run.rs`). It includes the `Ctx { log: &mut log, book: &mut book, state: &state, .. }`
  block at 594-619: that is the borrow problem (`&mut log`, `&mut book` and `&self` at once); the builder reads it first.
Read first: run.rs 171-620 (~450 lines); builtins.rs 47-130.
Steps: bundle book, emit_failures, last_emit_error, proposed_this_turn, settled, pre_draft, the held
  `answer_with_action` (190), and `tier` and `selected_tools` into `TurnState` (plan.rs); move the loop body into
  `async fn iterate(&self, plan: &TurnPlan, st: &mut TurnState, log: &mut EventLog) -> Result<Next, EngineError>`
  with `enum Next { Again, Stop }` and `settled` kept in `TurnState`: `Stop` covers both the settled breaks (the
  held-answer break at 211 and the others) and the break at `run.rs:311` (emitter gave up: `settled` stays `None`
  and the fallback at 648-667 depends on that), so `Next` carries no policy. `continue` (313) -> `Again`. Keep the
  three `settled = Some(match ..)` sites (207, 351, 637) as they are. Moved whole, `iterate` is ~430 lines: the
  200-line limit is met by RR-14b.

Start here (RR-14b: split `iterate`)
Files: run.rs `iterate` (as left by RR-14).
Read first: `iterate` whole, once.
Steps: split along its lettered comments: a. project (`fold`, active_pending, legal actions), b. prompt
  (`compose_prompt`), c. propose (emitter call and the failure arm), d..e. validate/guard/confirm,
  f2..i. run and record. Each phase takes `(&self, &TurnPlan, &mut TurnState, &mut EventLog)` plus the locals it
  needs by value; `Next` stays the only control signal. Alternative if the split fights the borrow checker: state in
  the commit message that the limit becomes "run_turn <= 60 lines" and leave `iterate` whole (record the line count).
  The builder confirms this design on a read of `run.rs` 172-620 before moving code; if it does not hold, stop and
  report (re-plan by the planner) rather than invent a new one.

Start here (RR-15: settle and reply)
Files: run.rs 622-712 (fallback reason at 648-667, ReplyPolicy handling at 685, the reply call; the fn ends at 713).
Read first: run.rs 600-713; reply.rs 47-60.
Steps: move into `async fn finish(&self, plan, st, log) -> Result<String, EngineError>`.
Acceptance: run_turn <= 60 lines; no fn in run.rs > 200 lines after RR-14b.
Gates (each): fmt check; clippy workspace; `cargo test -p ns-engine`.
```

### RR-16. Engine builtins as one table

Goal: each builtin's name, spec and handler sit in one table row; `run_builtin` looks the name up once. Lane E.
Depends on: RR-12c (spec construction), RR-15. Size S. Tier `worker`.

```
Start here (RR-16)
Files: crates/engine/src/turn/builtins.rs 197-215 (run_builtin) and the six handler signatures;
  crates/engine/src/turn/specs.rs: `synthetic_specs` (15-25) and `is_engine_owned` (239-244).
Read first: builtins.rs 1-215; specs.rs (297 lines).
Facts: there are seven synthetic specs but six handlers: `synthetic_specs` also returns `confirm_pending_spec`, in
  the order ask, confirm, remember, forget_fact, forget_all, recall, inspect. Iterating a six-row table would drop
  `confirm_pending` or reorder the tool array the model is sent.
Commit 1 (test only): pin the names AND order of `synthetic_specs(profile)` for both profiles
  (`synthetic_specs_are_unchanged`).
Commit 2 (refactor): `enum Builtin { AskClarification, RememberFact, Recall, InspectResult, ForgetFact, ForgetAll }`
  and a table of SEVEN rows in today's order, `(name, handler: Option<Builtin>)` with `None` for `CONFIRM_PENDING`;
  `Builtin::from_name`, `Builtin::spec(profile)` (delegating to specs.rs). `synthetic_specs` iterates the table,
  and `is_engine_owned` and `run_builtin` both read it.
Tests: every_builtin_has_a_spec_and_a_handler (for each row, spec name == row name; a row with `handler: Some` has
  a handler) - fails if a row is added without a matching spec; commit 1's test unchanged; turn_loop tests unchanged.
Gates: fmt check; clippy workspace; `cargo test -p ns-engine`.
```

### RR-19. `Vec::remove(0)` loops (F20) and the unbounded channels (F21)

Goal: no loop removes from the front of a `Vec` while re-measuring the whole window; the unbounded channel at
`app/src/registry/mod.rs:635` is bounded or its bound documented, and `channel-hub/src/hub.rs:64`'s comment says it
is bounded by tenant count. Lane E. Depends on: RR-G0 for 19a; the prompt-cache merge for 19b. Size S + S. Tier `worker`.

```
Start here (RR-19a: engine memory window)
Files: crates/engine/src/turn/memory.rs 112-118 (an F1 file, settled by RR-G0).
Read first: memory.rs 90-140.
Steps: find the cut with one pass (cumulative sizes, or a binary search over a monotone measure) instead of
  `remove(0)` plus a re-render per pass; on a summary rebuild `from = 1`, so today it is quadratic in turns.
Tests: first a test that pins today's output for windows of 0, 1, many records and a window that fits; then the change.
Acceptance: `cargo test -p ns-engine` same count plus the new tests; same output on every pinned window.

Start here (RR-19b: core budget; deferred)
Files: crates/core/src/budget.rs 255-256 and 362-363 (modified by prompt caching: wait for that merge).
Steps and acceptance: as 19a, in `cargo test -p ns-core`.

Start here (RR-19c: channels)
Files: app/src/registry/mod.rs:635 (check what feeds it and how fast it drains), channel-hub/src/hub.rs:64 (comment only).
Steps: bound the channel if a producer can outrun the consumer, else add a comment naming the bound; `app/` is
  edited by `flows` and prompt caching: do it with RR-18 if still blocked.
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
(their schemas differ: pixels vs schema profiles; Q5); a loom test for the hub (Q4); the blocking-file-I/O audit of
`app/src/admin/mod.rs` (43 `std::fs` uses) and `app/src/tenant.rs` (9) in async code (F22): **not audited**.

## 4. How to run it

**Brief template** (main chat hands one to each step's agent; fill the brackets):

```
Task RR-<n> of docs/plans/2026-11-24-rules-rebuild.md. Tier: <worker|builder>.
Worktree: C:/Users/Acemagic/ns_engine_rust_v2-rr-<lane> on branch rr-<lane> (create from main if missing; after a
lane merges, reset it with `git switch -C rr-<lane> main`);
--target-dir C:/Users/Acemagic/ns-target/<slot-a|slot-b> (assigned by the main chat) on every cargo command, -j 8.
Read ONLY: the plan's §3 "Start here (RR-<n>)" block and the sections it names. Do not read other steps.
One step per run. Tests first where the block names them. Refactor-only commit before any behaviour commit.
Gates before each commit: cargo fmt --all; cargo fmt --all -- --check;
  cargo clippy --workspace --all-targets -- -D warnings; cargo test -p <crate> (as the block says).
Commit with gates green; for refactor-only commits put the test counts before/after in the message.
Do NOT edit this plan, docs/plans/README.md or CLAUDE.md: put state, commit and numbers in the commit message and
the report; the main chat writes §5, the README row and the CLAUDE.md line in the merge.
Do not touch files listed as in flight in §2 unless the block says so.
Near 120k context: stop, commit what is green, write the hand-over in §5 ("Needs next:" one line).
End your report with "Needs next:" and what you skipped or did not check.
```

**Review rule:** one review per finished step, against its acceptance list (each test would fail without the change;
refactor-only: same test count, no behaviour diff). Contract steps (RR-1, RR-12a, RR-12b, RR-12c) are reviewed and merged
before any step that depends on them starts. A lane merges to main after its review; the next lane step branches from
the new main.

**Waits on the user's yes:** Q2 and the merge of `rules-setup` to main (RR-G0) before any other step; the `flows`
decision (Q6) and Q7 before RR-12b+; merging any lane to main; RR-11b; anything in "Not scheduled".

**Builds:** at most two lanes building at once (one while another session builds), each in one of the two slot dirs.
Delete a slot's contents when its lane is merged or abandoned.

## 5. Step state

| Step | State | Commit | Notes / Needs next |
|---|---|---|---|
| RR-G0 | planned | | gate zero; waits on Q2 and the user's yes to merge `rules-setup`; nothing else starts before it |
| RR-0 | planned | | part of RR-G0; records the clippy/test baseline and test-binary times |
| RR-0b | planned | | part of RR-G0; `large_enum_variant` (`#[expect]` if the enum is public API) |
| RR-1 | planned | | after RR-G0 (merge to main) |
| RR-2 | planned | | after RR-1 (root `Cargo.toml`) |
| RR-3, RR-4, RR-5 | planned | | after RR-1 |
| RR-6 | planned | | after RR-G0 |
| RR-7 | planned | | after RR-G0 |
| RR-8a, RR-8b, RR-9a, RR-9b, RR-9c, RR-10 | planned | | after RR-G0 and RR-1 |
| RR-11 | planned | | after RR-G0; RR-11b conditional |
| RR-12a | planned | | after RR-G0 (cheap; removes the `HashMap`) |
| RR-12b, RR-12c | planned | | after RR-12a, Q6, Q7 and both branches landing |
| RR-13, RR-14, RR-14b, RR-15, RR-16 | planned | | after RR-12c and the flows decision; RR-14b may need a re-plan by the planner |
| RR-19a | planned | | after RR-G0; 19b after prompt-cache, 19c with RR-18 if `app/` is busy |
| RR-17, RR-18 | deferred | | after prompt-cache-phase1 (and flows) |

## 6. Risks and cost

- Build time: `clippy --workspace` is a full check per step (minutes with sccache); lanes S and E rebuild the most.
  The two slot target dirs are 1-25 GB each; per-lane dirs would be seven times that.
- RR-9 changes where SQLite work runs. A missed owned-argument conversion is a compile error, not a runtime bug
  (type privacy). Throughput under load is unmeasured; serialization is the same as today (one connection, one
  mutex), it only moves from the async mutex to a std mutex on the blocking pool.
- RR-12b/c change the public API of `ns-core`; `flows` (one literal in `engine/tests/flow.rs`) and the prompt-cache
  files must rebase onto or land before them.
- RR-13..RR-15 conflict with any open branch touching `run.rs`; `flows` touches it in 22 lines at 169-190 (the
  RR-13/14 seam), so rebasing `flows` over lane E is small, but still do it when no such branch is open.
- RR-14b's design is unproven against the borrow checker; the builder is told to stop and report rather than improvise.
- RR-G0's merge discards the main checkout's 11 fmt edits (identical to `rustfmt` of `HEAD`); the 10 prompt-cache
  files in the same checkout must stay untouched.
- Property tests add a few seconds per crate test run at 2,000 iterations.

## 7. Open questions for the user

- **Q1** The input sizes in §1.3 are assumptions (events per session, vectors per scope, embedding dimension).
  Confirm or give real numbers; RR-10's and RR-11's thresholds depend on them.
- **Q2** The 11 files that fail `cargo fmt --check` on main are the ones with uncommitted formatting edits in the main
  checkout (`MERGE_INSTRUCTIONS.txt:30` says you planned to undo them). The review hashed each: the uncommitted edits
  are **identical to `rustfmt` of `HEAD`**, so they hold no hand edits, and committing the formatting or discarding
  them loses nothing. Recommended: commit the formatting on `rules-setup` (RR-G0 step 2) and discard the main-checkout
  copies before the merge; undoing it instead leaves the fmt gate red for every step.
- **Q0** (RR-G0) May `rules-setup` (the plan, stub lints, `CLAUDE.md`, fmt commit, clippy fix) be merged to main once
  gate zero is green? Lanes cannot start before it.
- **Q3** Accept "tokio::Mutex across `.await` only to serialize one connection, receiver or build, documented on the
  field" as a project rule (F15), instead of rewriting those sites?
- **Q4** loom: nothing hand-written qualifies (F16); the hub's shutdown uses tokio `Notify`/`mpsc`, which loom cannot
  model. Leave it to the existing tokio tests, or extract the ingress-count + closed-flag protocol behind a small
  std-only type so loom can test it (an extra M step, builder tier)?
- **Q5** One shared pointer-act catalogue for MCP and the engine tool (not scheduled), or keep two adapters with RR-6's
  table on the MCP side only?
- **Q6** Should lane E wait for the `flows` merge decision, or run RR-12..RR-16 on main first and have `flows` rebase?
  Facts: `flows` is 11 commits and 0 behind main; it touches `run.rs` in 3 hunks (22 lines, at 169-190, the
  RR-13/RR-14 seam), has one `ActionSpec` literal (`engine/tests/flow.rs`) and no overlap with the 11 F1 files, so
  rebasing `flows` over lane E is small.
- **Q7** (F6) Do RR-12a now (cheap: removes the `HashMap`) and RR-12b/RR-12c only after both `flows` and prompt
  caching have landed? No spec has more than one residual key and nothing in `src` serializes a spec, so the
  random-order half is latent; RR-12b is the widest edit in the plan (17 files, 8 crates). Recommended: yes.
