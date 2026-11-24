# Review of `2026-11-24-rules-rebuild.md` (plan at `300fce4`)

**Date:** 2026-11-24. **Reviewer:** independent architect pass, no builds run (only `cargo fmt --check`, Grep, Read, git).

## Verdict: ready after must-fixes

The audit is mostly right and the step shape (Start here, read budget, gates, mutation proof) is good. Seven things
stop it from being run as written: the start conditions contradict the gates, one contract step cannot pass its own
gate, three blocks carry wrong line ranges or wrong file pointers, and two acceptance checks fail on today's code for
reasons that are not bugs. All seven are plan edits, no redesign.

What I confirmed against the code:
- F1: `cargo fmt --all -- --check` on `rules-setup` fails in exactly the 11 files, 50 hunks. The 11 uncommitted edits
  in the main checkout are **identical to `rustfmt` of `HEAD`** (hashed each file, line endings ignored), and the main
  working copy passes the fmt check. So they hold no hand edits: committing the formatting or discarding them loses
  nothing.
- F2: `tokio::sync::Mutex<Connection>` at `crates/memory-sqlite/src/lib.rs:8-11`; **25** `self.conn.lock().await`
  sites (524-1600), not ~35; none is held across an await. `rusqlite` is used only in this crate.
- F4: `run_turn` is `crates/engine/src/turn/run.rs:21-713`; `settled = Some(match ..)` at 207, 351, 637.
- F5: `fold` callers at `run.rs:38`, `run.rs:215`, `reply.rs:58`, `routing.rs:26`, `memory.rs:100`.
- F6: `crates/core/src/action.rs:71-81`, all fields `pub`, `HashMap` in a `Serialize` type. See S8 for its rank.
- F7, F9, F10, F11, F12: present at the stated places (corrections to ranges below).
- The Structure and Contract-first rules cited are in `~/.claude/CLAUDE.md:94-105`.

## Must-fix

**M1. Start conditions contradict the gates** (plan lines 74, 83-89, 94-96, 122-127, 587, 615-622).
- Problem: every step's gate includes `cargo clippy --workspace .. -D warnings`, which is red on main until RR-0b,
  but RR-0b has no Start here block, no lane row and no §5 row, and line 74 says "Free now: RR-2, RR-6, RR-7, RR-11"
  (the lane table says M and V start after RR-0). The plan, the stub lints and the future `CLAUDE.md` exist only on
  `rules-setup` (3 commits ahead of main), while the brief (line 587) creates lane worktrees from main, so a lane
  agent cannot read its own block. Every brief runs `cargo fmt --all` in write mode, which rewrites the 11 F1 files in
  every lane until the fmt commit is on main.
- Fix: add a "Gate zero" block before RR-1 and make every other step depend on it: (1) Q2 answered; RR-0 commits the
  fmt change; the 11 uncommitted edits in the main checkout are discarded with `git checkout -- <11 files>` before
  the merge (git refuses to merge over dirty files; the content is identical, verified above); (2) RR-0b as a real
  step with a Start here block and a §5 row; (3) `rules-setup` merged to main. Replace line 74 with "Free after gate
  zero: RR-2, RR-6, RR-7, RR-11". RR-0b rule: name the enum; if its variants are built or matched outside `ns-core`
  (likely `EventKind`, `crates/core/src/event.rs:15`), use `#[expect(clippy::large_enum_variant, reason = "..")]` now
  (one line, no collision with `flows` or prompt caching) and record the sizes; boxing a public variant is a
  cross-crate contract change and gets its own step later.

**M2. RR-12a cannot pass its own gate, and its dependencies are incomplete** (lines 471-507, §2 lines 69-71).
- Problem a: `#[non_exhaustive]` makes every struct literal outside `ns-core` a compile error. There are 31 such
  literals in 14 files, so after RR-12a the workspace clippy gate (line 506) is red, and "reviewed and merged before
  RR-13" (lines 476, 601) merges a broken workspace.
- Fix a: expand, migrate, contract. RR-12a = `BTreeMap` swap plus `ActionSpec::new`, `with_residual`,
  `with_dedupe_tag`, `SpecError` and their tests, no `#[non_exhaustive]` (source-compatible today: 33 literals write
  `residual_policy: Default::default()`, `engine/src/guards.rs:124` and `engine/tests/turn_loop.rs:814` use
  `.collect()`). RR-12b = migrate the sites. RR-12c = add `#[non_exhaustive]` last.
- Problem b: RR-12b edits `llm/src/schema.rs` (5 literals) and `provenance/src/classify.rs` (2), both F1 files, and
  `core/src/traits.rs` (3), `engine/src/script.rs` (2), `llm/src/emitter.rs` (1), all modified in the prompt-cache
  worktree. That branch has **no commits** (`git log main..prompt-cache-phase1` is empty); its work is uncommitted in
  `app/src/{config,factory,tenant}.rs`, `core/src/{budget,router,traits}.rs`, `engine/src/{script,trace}.rs`,
  `engine/src/turn/prompt.rs`, `llm/src/emitter.rs` (emitter: 487 lines changed).
- Fix b: replace the file list in §2 line 71 with that list; add RR-12b to the "Blocks" of the F1 row and the
  prompt-cache row; add both to RR-12's "Depends on". `flows`: replace "not verified" with the fact: one literal, in
  `crates/engine/tests/flow.rs`; `core/src/flow.rs` builds none.
- Problem c: "spec JSON is read from config (a wire format)" (line 475) is not what the code does. Grep finds no
  non-test serialization or deserialization of `ActionSpec` or `LegalActionSet`; `app/src/config.rs` does not mention
  `ActionSpec`; the only use is a test probe, `engine/tests/turn_loop.rs:5071`.
- Fix c: drop `#[serde(try_from = "RawActionSpec")]` and the test `deserializing_an_unchecked_spec_fails` unless the
  builder finds a deserialization site; correct the tier reason.
- Fix d (state it): the existing checks stay while fields are writable: `core/src/plugin.rs:148`
  (`BuildError::BadArgName`), `engine/src/turn/specs.rs:286-296`, `llm/src/schema.rs:416-433`.

**M3. RR-13/14/15 have wrong boundaries, a wrong signature, and an acceptance they cannot meet** (lines 509-542).
- Problem: the loop is `for _ in 0..self.cfg.max_iterations {` at `run.rs:192` and closes at 620. The hint
  `grep -n "loop {" run.rs` (line 529) prints nothing. RR-14's range "171-~560" stops 60 lines early and misses the
  `Ctx { log: &mut log, book: &mut book, state: &state, .. }` block at 594-619, which is the borrow problem the plan
  warns about; RR-15's "~560-714" overlaps the loop. Moved whole, `iterate` is ~430 lines, so "no fn in run.rs > 200
  lines" (line 540) is unreachable. `enum Next { Continue, Settled }` cannot express the `break` at `run.rs:311`
  (emitter gave up; `settled` stays `None` and the fallback at 648-667 depends on that). `tier` and `selected_tools`
  are reassigned inside the loop (`run.rs:435`, `438`), so they cannot live in `&TurnPlan`. `log` and `usage` are
  created inside the prelude (`run.rs:30`, `36`), so `plan_turn(.., &mut EventLog, usage)` cannot receive them.
- Fix: RR-13: `plan_turn(&self, incoming: &Incoming) -> Result<(TurnPlan, EventLog), EngineError>`; `TurnPlan` also
  holds `n_loaded`, `usage`, `routed.cues` and the user text; `cap_reached()?` stays first. RR-14: range 172-620;
  `TurnState` also holds `tier`, `selected_tools` and `answer_with_action`; `enum Next { Again, Stop }` with `settled`
  in `TurnState`. Add RR-14b that splits `iterate` along its lettered comments (a project, b prompt, c propose,
  validate/guard/confirm, f2-i run), or change line 540 to "run_turn <= 60 lines; `iterate` split is RR-14b". RR-15:
  range 622-712.

**M4. RR-2's acceptance can never pass** (line 220).
- Problem: `grep -rn "unsafe" --include=*.rs crates app` also matches the comment at
  `crates/engine/src/dispatch.rs:555`.
- Fix: enforce it by lint: `[workspace.lints.rust] unsafe_code = "forbid"` in the root `Cargo.toml` (every member
  already has `[lints] workspace = true`); acceptance = workspace clippy green. That edits the root `Cargo.toml`, so
  run RR-2 after RR-1 in lane 0. The `CLAUDE.md` line then states something the compiler checks.

**M5. RR-5 points at a signer the test cannot reach** (lines 298-300, 306).
- Problem: `crates/identity/src/minting.rs` is `#[cfg(test)] mod minting` (`lib.rs:31-32`) with `pub(crate)` fns; an
  integration test in `crates/identity/tests/` cannot call it, and `grep -n "pub fn" minting.rs` prints nothing.
- Fix: use the public `nsidentity::mint_hs256` (`crates/identity/src/issue.rs:42`; read its signature, lines 24-86).
  If forged headers are needed, put that property in `src/jwt.rs`'s test module, where `crate::minting::mint` is
  visible. Define the tamper as "replace one character with a different one from the base64url alphabet or `.`" (a
  raw byte flip can make the `String` invalid UTF-8).

**M6. RR-3 property 3 is false on today's code without any bug** (lines 247-248).
- Problem: `parse_object` refuses empty objects on purpose (`crates/llm/src/salvage.rs:56-60`), and the generator
  includes the empty list, so `fenced_object_is_recovered` fails on `{}`.
- Fix: require at least one key in tests 3 and 5, and name `{}` -> `None` as the edge case. Add one sentence to the
  step rules (line 91): when a property fails on current code, the input becomes a plain `#[test]` that asserts
  today's behaviour with a comment and goes in "Needs next:"; never `#[ignore]`, never a red commit.

**M7. RR-16 would change the tool array** (lines 544-560).
- Problem: there are seven synthetic specs but six handlers: `synthetic_specs` (`turn/specs.rs:15-25`) also returns
  `confirm_pending_spec`, in the order ask, confirm, remember, forget_fact, forget_all, recall, inspect. "The
  builtin-spec list iterates BUILTINS" either drops `confirm_pending` or reorders the array the model is sent. The
  third name list, `is_engine_owned` (`specs.rs:239-244`), is not in the block.
- Fix: commit 1 (test only) pins the names and order of `synthetic_specs(profile)` for both profiles. The table has
  seven rows in today's order with `handler: Option<Builtin>` (`None` for `CONFIRM_PENDING`), and `is_engine_owned`
  and `run_builtin` both read it. Add `specs.rs:239-244` to Files.

## Should-fix

**S1. Missing finding: multi-statement writes have no transaction.** `lib.rs` contains no `BEGIN` or `transaction(`.
`append` (959-986) runs one autocommit `INSERT` per event: a crash leaves part of a turn in the log, and each event is
its own commit, which is most of F2's blocking time. `write_fact` (857-863) marks the old version superseded, then
inserts the new one: a failure between leaves the key with no current value. `~/.claude/CLAUDE.md:92-93` puts
data-loss handling out of bounds for shortcuts. Add RR-9c after RR-9b (`Db::call` hands `&mut Connection`, so
`conn.transaction()` fits): behaviour commit, tests `append_is_all_or_nothing` (a test-only trigger raises on the
third row; `load` returns none of the batch) and `a_failed_fact_insert_keeps_the_previous_current_version`.

**S2. RR-9's goal is wider than its scope** (line 385). `SqliteStore::open` stays sync and runs migrations and the
FTS rebuild (222-246) on a worker through `app/src/models.rs:171` <- `app/src/factory.rs:486`. Either say "after
`open`" in the goal and in the `CLAUDE.md` line, with a `// shortcut:` at `models.rs:171`, or wrap that call once
`factory.rs` is free. Correct "~35 sites" to 25 (6 in recall/facts for RR-9a, 19 in the impl for RR-9b);
`schema.rs` has no lock sites (its fns take `&Connection`).

**S3. RR-10: the selection is not worth a step; the scaling test is** (lines 423-446). Sorting 10^4 pairs is under
1 ms beside the 40 MB load the plan calls dominant and two network round trips. `recall_scales_linearly` also passes
before the change (N log N at 10x is a ratio of 13.3, under the 15 limit), so it proves nothing about selection.
Keep the scaling test and the deterministic tie-break; make `select_nth_unstable_by` optional. In either case use
`f32::total_cmp` (`partial_cmp(..).unwrap_or(Equal)` at 701 and 769 is not a total order when a score is NaN, and
std sorts may panic on that), and add `ORDER BY e.rowid` to `turn_vectors` (568-574 has none, so "today's order" for
ties is whatever SQLite returns).

**S4. RR-11's threshold measures the wrong thing** (lines 457-462). "A 200-turn session spends > 50 ms in fold" is a
session total: about 10^6 event visits can exceed it while costing under 1 ms per turn next to a model call. Use a
per-turn threshold: RR-11b only if five folds of 2,000 events exceed 5 ms, or 1% of a measured turn. `ProposalMade`
does not exist; the variants are `UserSaid`, `Proposed`, `ToolCalled`, `ToolReturned`, `Settled`, `Replied`
(`crates/core/src/event.rs:15`).

**S5. The audit's greps (lines 13-15) skipped three rust.md rules.** Add findings with a decision each:
- `Vec::remove(0)` in a loop that re-measures the whole window per removal: `engine/src/turn/memory.rs:112-118`
  (re-renders every remaining record each pass; on a summary rebuild `from = 1`, so quadratic in turns),
  `core/src/budget.rs:255-256` and `362-363`. `memory.rs` is an F1 file; `budget.rs` is modified by prompt caching.
- Unbounded channels: `channel-hub/src/hub.rs:64` (bounded by tenant count per its comment; say so),
  `app/src/registry/mod.rs:635`.
- Blocking file I/O in async code was not audited: `app/src/admin/mod.rs` (43 `std::fs` uses), `app/src/tenant.rs`
  (9). Record "not audited" or audit it.

**S6. RR-7's phase table is wrong** (lines 348-351). Actual markers in `evolution/src/pass.rs`: 743 "1-2.", 749 "3.",
764 "4.", 788 "4b", 809 "4b°" (embeddings backfill), 851 "4b′", 863 "4b''" (success lane), 884 "4c", 912 "5.",
957 "6.", 980 "6b", 1001 "6c", 1013 "7.", 1149 "8.", 1160 "9."; the fn ends at 1169. Two phases have no name in the
plan: add `backfill_embeddings` (809-850) and `mine_success_lane` (863-883) and replace the ranges.

**S7. RR-6 misses the third per-tool list** (lines 324-333). `irreversible()` at `pointer/src/mcp.rs:262-267` matches
tool names and feeds `gate()` (358). The row should be (name, schema, irreversible, handler). `tools()` takes no
argument, so "for every gate setting" needs one golden, not several. Add 222-267 and 358-412 to Read first (about 510
lines in total).

**S8. F6 is ranked and sized too high; ask the user before RR-12b.** No spec has more than one residual key, and
nothing in `src` serializes a spec, so the random-order half is latent. Both production paths are already checked
(`plugin.rs:148`, `specs.rs:286`). The constructor migration is the widest edit in the plan (17 files, 8 crates) and
collides with both branches. Recommend RR-12a now (cheap, removes the `HashMap`), RR-12b and RR-12c after both
branches land; add this as Q7.

**S9. Lanes edit the same lines of shared files** (lines 174-183, 594). §5 rows, the README row and the `CLAUDE.md`
Conventions list are adjacent lines edited from parallel lanes, so every second merge conflicts. Lanes should not
edit the plan, the README or `CLAUDE.md`: the agent puts state and numbers in its commit message and report, and the
main chat writes §5, the README and the `CLAUDE.md` line in the merge.

**S10. Target dirs per lane will fill the drive** (lines 78, 588, 608). Seven lanes at 1-25 GB each. Only two builds
run at once, so use two slot dirs (`ns-target/slot-a`, `slot-b`) that the main chat assigns at launch.

**S11. RR-1's acceptance cannot see feature changes** (lines 203-204). `Cargo.lock` does not record features. Add
`cargo tree -e features -i serde_json | grep preserve_order` prints nothing, and a guard test in `ns-core`
(`json!({"b":1,"a":2}).to_string() == r#"{"a":2,"b":1}"#`): the convention at line 166 has no test behind it. Add
bolero to `engine` only when RR-11b is approved.

**S12. Property-test inputs.** RR-3: a random `String` almost never reaches a repair; drive tests 1 and 4 from a
valid object plus random corruptions (fence, single quotes, trailing comma, bare keys, Python literals, cut). Read
`salvage.rs` 1-372 (the repairs are 72-372, not 1-140). Test 4 compares keys after JSON unescaping, or generates
escape-free keys. RR-4 test 3: `TextBox.confidence` is `f32` (`pointer/src/wire.rs:272`); generate finite values.

**S13. Proposed `CLAUDE.md`** (lines 135-172). Correct on crates, gates and plans. Add: package `ns-<dir>`, lib
`ns<dir>` with underscores (`nscore`, `nsmemory_sqlite`); when the `#[ignore]` scaling tests run (on a change to
fold or recall, and before the final report) with the command, since no gate runs them; under Invariants, "the order
and bytes of the tool array (`synthetic_specs`) and the event hash chain". "The slowest test binary is turn_loop"
(line 159) was not measured: RR-0 step 3 records times and writes that line from them.

**S14. Q6 evidence.** `flows` is 11 commits, 0 behind main; it touches `run.rs` in 3 hunks (22 lines, at 169-190,
the RR-13/RR-14 seam), has one `ActionSpec` literal, and no overlap with the 11 F1 files. Put that in Q6 so the
user decides on facts: rebasing `flows` over lane E is small.

## Nits

- Lines 3 and 26: the fmt gate is red because `HEAD` is unformatted; the uncommitted edits are the fix.
- Line 34: `run_report` has 15 phase markers, not nine.
- Line 72: the stub lints already landed on `rules-setup` (`5874024`); RR-1 waits on the merge to main.
- RR-8a (lines 366-372): the schema fns are associated fns of `impl SqliteStore` (282-509); the move is a second
  `impl SqliteStore` block in `schema.rs`.
- RR-5 test 1: `serde_json::Value` has no bolero generator; build it from a `Vec<u8>` driver.
- Brief (line 587): after a lane merges, reset it with `git switch -C rr-<lane> main`.
- RR-9: `spawn_blocking` needs a tokio runtime; no other executor drives the store today (grep), so this is safe.

## Not verified

- Clippy and test state on any commit (no builds), and which enum trips `large_enum_variant`.
- bolero 0.13 on this toolchain and what features it turns on.
- Whether `ns-app eval` runs offline; all timing estimates (cosine ms, fold cost, slowest binary).
- The F15 list beyond `hub.rs:126` and `mcp.rs:501`; the content of the prompt-cache edits beyond their file list.
