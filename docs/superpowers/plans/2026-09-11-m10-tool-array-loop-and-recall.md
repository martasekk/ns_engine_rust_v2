# M10 — The tool array, the loop, and the recall that fires

Date: 2026-09-11 · Drafted after M9 (P0–P5, branch `worktree-m9-memory` @ `2b28d23`), **not yet
approved or executed** · Evidence: `docs/research/2026-09-11-child-session-memory-and-token-economy-findings.md`
§8 (what M9 measured), `2026-09-08-tool-loading-findings.md` §4.1, §5, §6 (the design and the
order it recommended), `2026-09-08-m8-local-evaluation-lane.md` §6 (Phase 3, the recall table),
M9 §Results (the instruments and their first readings). Convention as M7/M8/M9: phases, task
tables with file / test / exit criterion, requests spent, "not built, with the reason";
measured numbers land in §Results as each exit criterion is met.

## Context

M9 built the instruments and closed the outcome-to-selection loop, then read the live box for
the first time. The reading moved the target. The largest context cost on every emitter call is
not memory but the **tool array**: 45.9% of the prompt on a deep-tier chat turn, with a ~70-token
envelope floor per tool, a 106-character `_rationale` boilerplate repeated in every tool (25% of
tool tokens by itself), duplicated coordinate text, and synthetic tools sent when they cannot
apply (`forget_*` with zero facts in the store). The largest request cost the log shows is not
batching (5 of 81 requests, all on irreversible runs) but **repeat-gate loops**: 6 of 81 emitter
proposals re-issued a call already executed that turn. And every intelligence lever M9 shipped at
its default is gated on **fixtures** that do not exist: the scripted suite carries no summary, no
notes, no tied recall targets, and κ per evaluator cannot print because the local evaluator is
not wired into the pass. Meanwhile the largest measured intelligence gain on record, M8 Phase
3's paraphrase recall (83% miss → 8–25% with bge-m3 coarse-10 → rerank, zero requests), is still
unbuilt.

So M10 does four things, all at zero requests: shrink the tool array without narrowing
capability; give the router an adaptive depth guarded by a hard-query fixture; stop the
self-inflicted loops; build M8 Phase 3 and the fixtures that let every M9 knob be decided.
The user's priority is unchanged: most efficiency and intelligence, lowest context cost, the
desktop line parked (M10 touches desktop *tool schemas* because they are the cost, not because
it builds desktop behaviour).

Total request spend planned: **0**. Accuracy is monitored on whatever live sessions happen
anyway, through the pass's signature rates, which is the evidence-first instrument this repo
already has.

## What does not change

| Stays | Reason |
|---|---|
| Strict function calling (`strict: true`, `additionalProperties: false`, all fields required) | Schema-valid arguments; the envelope floor is the price, and it is the same for every design |
| JSON as the tool-call format | TRON/TOON save 18–27% of tokens for up to 14 pp accuracy loss (2605.29676) |
| Legal-set pruning per iteration, and *removing* rather than masking tools | Masking protects a prefix cache this tier does not bill; the free tier meters requests |
| The guard chain, including the repeat gate | It is doing its job; M10 stops the model from tripping it, it does not loosen it |
| One action per emitter request | Batching measured at 5/81, irreversible-only; not adopted |
| The grounding check and its one regeneration | 1 flag in 21 turns, a bilingual false positive; a lemma/bilingual match is a follow-up, not a redesign |
| Everything M9 left at its default (`activation_weight`, `fitness_demote`, `obligation_check`, `summary_guidelines`, `exemplars_max`) | M10 builds the fixtures that can decide them; it does not decide them |
| No tool-RAG over an embedding index for 17 tools; no `find_tool` action | Tool-loading §4.3, §4.2: escalation is the discovery path until the escalation rate says otherwise |
| No passive fact extraction | Zero `remember_fact` proposals in 21 turns is expected for a desktop log with no stated facts; no evidence of need |

## Concepts adopted

| Concept | Source | Form here |
|---|---|---|
| Deterministic schema compression | TsCG 2605.26165 (44–50% at the conservative profile, no accuracy loss), tool-loading §1.5 (2510.07248), OpenAI/Gemini guidance | `schema_profile = "slim"`: one-clause `_rationale`, deduped `xy_schema`, shortened descriptions, never-used parameters behind the profile |
| Applicability pruning of synthetic tools | M9 §Results P2 (forget tools sent with zero facts) | `forget_*` only when the scope holds facts; `recall` only when there is history to recall |
| Adaptive depth in the router | Tool-loading §4.1 (Design A), 2605.24660 (7 tools vs 50 at equal coverage), §1.3's 1–3 / 5–7 band | Per-turn tool selection from the router's cues, replay-safe, removing not masking; Bits-over-Random reported |
| Hard-query fixture before narrowing | Tool-loading §5.3 ("0% on hard queries fails silently") | A desktop fixture whose target action a narrow set would withhold, in the suite before P2 ships |
| Argument examples on mis-parameterised tools | Anthropic advanced tool use (72% → 90% on complex parameters) | One example each on the ≤ 3 tools the Malformed/Illegal counters name, only if they name any |
| Loop prevention prompt-side | M9 log: 6/81 repeat-gate rejections | A "done, identical call denied" marker on executed calls in the trace; measured by a rejections-by-reason counter |
| Recall that fires | M8 Phase 3 table (83% → 8–25%), M6 §12.8 trigger fired | T3.1–T3.5 exactly as M8 specifies; unblocks M9 T5.3 |
| Fixtures as the arbiter | LongMemEval's paraphrase + knowledge-update + abstention triple (2410.10813); 2606.09376 (precision-only scoring rewards abstention) | ~50 scripted sessions carrying summaries, notes, updates, paraphrased targets and unanswerable items; a tie-heavy recall corpus |

## Phases

Dependency order: **P0 → P1 → P2** is the tool-array line (P2 needs P0's fixture and metric and
P1's slim profile so BoR is measured on the array that ships). **P3** (recall) and **P4**
(loop) are independent of it. **P5** (fixtures, κ) is independent and is the precondition for
re-reading every M9 knob; it can start on day one.

### P0 — Instruments and the fixture that guards narrowing (0 requests, ~1 day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T0.1 | `ContextManifest.tool_names: Vec<String>` (`#[serde(default)]`; keep `tools: usize`) filled from the legal set at `turn.rs:1126`; `ns-app budget` prints a per-tool table for the session: tool · calls it rode on · schema tokens (computed at report time by `build_tools` over the registry, `schema.rs:230`) · share of the session's `tools_tokens` | `crates/core/src/usage.rs`, `crates/engine/src/turn.rs`, `crates/llm/src/schema.rs` (a `schema_tokens(name)` helper), `app/src/budget.rs` | `budget.rs` inline `the_tool_table_sums_to_the_sessions_tools_tokens`; `usage.rs` `an_old_manifest_without_tool_names_still_parses` | the table's total equals `Σ tools_tokens` within `estimate_tokens` rounding; old logs print `n/a` |
| T0.2 | rejections by reason in `ns-app budget` and in `ns-app evolve --dry-run`: `Rejected` events bucketed by the guard that fired (`repeat_gate`, `IllegalAction`, `Malformed`, others), as counts and as a rate per 100 `Proposed` | `app/src/budget.rs`, `crates/evolution/src/pass.rs` (report), `mine.rs` (the reason is already in the event's `reason`) | `budget.rs` `rejections_are_bucketed_by_reason_and_rated_per_hundred_proposals` | on the recorded 21-turn copy: `repeat_gate 6, IllegalAction 2, Malformed 1 — 11.1 per 100 proposals` |
| T0.3 | the hard-query fixture (tool-loading §5.3): a desktop ability in `crates/testkit/src/eval.rs` whose only correct action is one a narrow set would withhold (`pointer_scroll` or `pointer_clipboard_write`, never called in the log), scored like the other desktop abilities | `crates/testkit/src/eval.rs` | the ability passes on the full set today | a run with the tool removed from the legal set **fails** the ability (proves it guards) |
| T0.4 | Bits-over-Random in `ns-app eval`: per fixture, legal-set size and whether the target action was proposed; report BoR per run beside the pass counts (tool-loading §5.2) | `crates/testkit/src/eval.rs`, `app/src/eval.rs` | `eval.rs` `bits_over_random_is_zero_at_chance_and_positive_when_the_target_is_chosen` | the full suite prints a BoR column; today's number is the baseline |

Not built: any change to what is sent. P0 only reads.

### P1 — Slim the tool array (0 requests, ~1½ days)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T1.1 | `_rationale` shrinks to one clause per tool (e.g. description `"why, in one clause"`), and the full instruction moves once into the emitter's system prompt (`crates/llm/src/emitter.rs:7`, four lines today) | `crates/llm/src/schema.rs:209-213, :241`, `emitter.rs` | `schema.rs` `the_rationale_property_costs_under_forty_chars_per_tool`; `emitter.rs` `the_system_prompt_carries_the_rationale_instruction_once` | turn-21 array reproduced: 2,927 → ≤ 2,250 chars |
| T1.2 | `xy_schema` (`crates/components-std/src/pointer_tool.rs:79-98`) keeps one sentence of coordinate convention in each tool's description and one short phrase per axis; `pointer_click` and `pointer_move` share it | `pointer_tool.rs` | `pointer_tool.rs` `click_and_move_share_one_coordinate_convention_under_two_hundred_chars` | `pointer_click` ≤ 130 tokens, `pointer_move` ≤ 100 (from 218 / 174) |
| T1.3 | `[llm] schema_profile = "full" \| "slim"` (default **`full`** until T1.6 reads a session): `slim` shortens every description to the TsCG conservative profile (imperative, one sentence, no restated parameter names), collapses enums the log never used (`button=middle`, `modifiers`) behind the profile, and keeps every parameter that changes capability (`count`, `dx`, `screen` stay; only their descriptions shrink) | `schema.rs`, `pointer_tool.rs`, the synthetic tools' specs in `crates/engine/src/turn.rs` (`ask_clarification`, `remember_fact`, `recall`, `forget_*`, `inspect_result`, `respond_directly`), `app/src/config.rs` | `schema.rs` `the_slim_profile_cuts_the_desktop_array_by_at_least_thirty_five_percent_and_keeps_every_action_name`; a snapshot test of both profiles so a change is a visible diff | desktop array ≤ 1,350 tokens (from ~2,100); chat floor ≤ 420 (from ~650); every action and every required parameter still present |
| T1.4 | applicability pruning of synthetic tools: `forget_fact`/`forget_all` only when `memory.facts(scope)` is non-empty; `recall` only when `turn > window_turns` or `recall_sessions > 0` with earlier sessions present; `inspect_result` already conditional | `turn.rs:1001-1037` | `crates/engine/tests/turn_loop.rs` `forget_tools_are_absent_while_the_scope_holds_no_facts`, `recall_is_absent_on_the_first_turn_of_a_fresh_store` | turn 1 of an empty store sends `ask_clarification`, `remember_fact`, `respond_directly` only (≤ 300 tokens) |
| T1.5 | argument examples on the ≤ 3 tools T0.2's Malformed/Illegal buckets name, one example each, ≤ 40 tokens | `pointer_tool.rs` or the synthetic specs | snapshot | only if a bucket is non-zero on the recorded log (it is: 1 Malformed, 2 Illegal — read which tools) |
| T1.6 | monitoring, not a task: after the first live sessions under `slim`, `ns-app evolve --dry-run` rates per 100 proposals for `Malformed`, `IllegalAction`, `repeat_gate` against the recorded baseline (11.1). A rise of more than 3 per 100 on Malformed or Illegal reverts the profile to `full` and records why | — | — | the number goes into §Results; `slim` becomes the default only when it holds |

Decisions: the profile is a knob because selection accuracy cannot be measured offline (the
scripted emitter does not choose by schema) and a live A/B would spend a day's quota; the M5
signature miner is the accuracy instrument, slow and honest. Parameters are never removed,
only their descriptions shrink — the paper's saving is in descriptions and structure, and a
removed parameter is a removed capability the model cannot ask for.

Not built: TRON/TOON or any non-JSON format; key renaming; dropping `strict`.

### P2 — Adaptive depth in the router (0 requests, ~1 day, after P0 and P1)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T2.1 | the router scores a message for intent already (`crates/core/src/router.rs`, M7 Phase 3); extend it to select *which* registered tools ride on the first iteration: cue → tool groups (`pointer_ui_*` + `pointer_click` for "find/click/open", `pointer_type` + clipboard for "type/paste", `pointer_scroll` for "scroll/down/up", …), target 1–3 tools on easy cues, 5–7 on hard, the full set when no cue matches. Replay-safe: the selection is a pure function of the text and the config | `router.rs`, `turn.rs:976-1037` | `router.rs` `an_open_and_click_cue_selects_the_ui_and_click_tools_only`, `no_cue_selects_the_full_set` | BoR (T0.4) on the desktop abilities does not fall; the hard-query fixture (T0.3) still passes |
| T2.2 | escalation stays the discovery path: an `IllegalAction` on a withheld tool widens the set for the next iteration (M7's escalate-on-misroute), and the widening is recorded in the manifest (`tool_names` grows) so replay reproduces it | `turn.rs` | `turn_loop.rs` `a_withheld_tool_is_admitted_after_one_illegal_action_and_the_manifest_records_it` | one request per escalation, counted in T0.2's buckets |
| T2.3 | `[router] depth = "full" \| "adaptive"`, default **`full`**; `adaptive` flips on when T1.6's monitoring holds and the escalation rate on live sessions stays under 5 per 100 proposals | `app/src/config.rs` | inline parse test | recorded in §Results with the escalation rate |

Not built: `find_tool` (tool-loading §4.2), vector tool retrieval (§4.3).

### P3 — Recall that fires: M8 Phase 3 (0 requests, ~2 days, local CPU)

Built exactly as `2026-09-08-m8-local-evaluation-lane.md` §6 specifies; restated here only as
the checklist. Requires the nsmodels service (`cd ~/models && ./.venv/Scripts/python.exe -m
nsmodels serve --model quality --rerank`).

| id | change | exit criterion |
|---|---|---|
| T3.1 | `embeddings` table keyed by event rowid; backfill in the idle pass, never in a turn; bge-m3; a stored vector records its model so a model change invalidates rather than mixes | backfill resumable; a turn never blocks on an embedding |
| T3.2 | hybrid `search_turns`: bm25 ∪ vector candidates fused by rank (RRF k ∈ [1, 10]), `/rerank` over the union, top-k; with the service down the result is exactly today's bm25 list | same order, same count with the server down |
| T3.3 | hybrid path reachable only from the `Task`/deep tier; `Chat` recall stays lexical | a `Chat` turn issues no `/embed` call |
| T3.4 | `ns-app eval --paraphrase` re-run | **miss rate under 20%**, verbatim arm not regressed, or revert and record |
| T3.5 | `[recall] coarse_k` (default 10) and a rerank latency budget; over budget falls back to lexical | a slow rerank never holds a turn |
| T3.6 | M9 T5.3 exemplars become buildable: ≤ `exemplars_max` (default 0) nearest digests as one `ToolReturned` in the deep tier | `--ablate` decides the default, as M9 wrote |

### P4 — The loop (0 requests, ~½ day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T4.1 | executed calls render in "This turn so far" with an explicit marker — `done; an identical call is denied` — and the emitter system prompt gains one line: never re-propose a call already marked done | `crates/engine/src/trace.rs` (`trace_entries`, `fold_descriptor`), `emitter.rs:7` | `trace.rs` `an_executed_call_carries_the_done_marker`; `turn_loop.rs` `the_marker_survives_folding` | the marker is on every executed call, folded or verbatim |
| T4.2 | monitoring: `repeat_gate` per 100 proposals on live sessions vs the recorded 7.4 | T0.2 | — | a fall to under 3 per 100 keeps T4.1; no change after three sessions records "prompt-side is not enough" and re-opens the design (a symbolic pre-check that returns the recorded outcome for an identical proposal without a request — Temporal's `SideEffect` again — is the next candidate, and it is a guard change, so it needs its own true-positive number first) |

### P5 — Fixtures and κ: the arbiter every M9 knob is waiting for (0 requests, ~2 days, independent)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T5.1 | ~50 scripted sessions in the testkit generated from hand-authored seeds (persona, timeline, events), each carrying: a **summary** built by the real summarizer path over scripted turns, **notes** in the learned rules, one **knowledge update** (a fact restated with a new value), **paraphrased recall targets**, and **unanswerable** items scored as abstention; cs and en; deterministic | `crates/testkit/src/` (a `fixtures` module), `eval.rs` abilities | each ability has ≥ 5 sessions; a fixture regenerated from its seed is byte-identical | `--ablate summary` and `--ablate guidance` report non-zero deltas (or a measured zero on a suite that carries them) |
| T5.2 | the tie-heavy recall corpus: sessions where ≥ 3 candidate turns tie on lexical hits and only recency/credits separate them | `paraphrase.rs` or a new arm | the arm's target is reachable at `w = 1` and not at `w = 0` on at least one fixture | `ns-app eval --activation` returns different numbers at 0 and 1 — the precondition for M9 T3.3 |
| T5.3 | `LocalEvaluator` wired into `build_pass` via `with_evaluator` when `[models] enabled`, κ per evaluator printed per M8 T2.7, `evaluator_min_kappa` honoured (below threshold: observations only, no candidates) | `app/src/main.rs:293-340`, `crates/evolution/src/pass.rs` | `pass.rs` `a_second_evaluator_prints_kappa_against_the_symbolic_one` | `ns-app evolve --dry-run` prints κ for `local` vs `symbolic` on the recorded copy (held-out κ 0.628 vs 0.501 per M8 grading — the pass should reproduce the direction) |
| T5.4 | re-read every M9 default on the new fixtures: `--ablate summary|guidance`, `--activation {0, 0.5, 1}`, the summary-guidelines arm (M9 T5.2), the obligations arm (`obligation_check` on vs off) | — | — | each knob gets its number in §Results; a knob moves off its default only on a non-regressing verbatim arm, the M9 rule |

## Minimal cut (two days)

Day 1: P0 T0.1, T0.2 (the instruments), then P1 T1.1, T1.2, T1.4 (the three exact cuts:
rationale, coordinates, applicability) — a measured token reduction on every call with no
knob and no risk to capability. Day 2: P0 T0.3 (the hard-query fixture) and P5 T5.3 (κ prints).
Cut: T1.3's slim profile, all of P2, P3, P4, T5.1–T5.2. That still lowers every emitter call by
roughly a third of its tool tokens and restores the M8 exit line.

## Ordering rationale

P1's three exact cuts are the highest gain per hour in the plan: they are subtractions of text
the model never needed, verified by a snapshot test, with no accuracy question to answer. P2 is
where accuracy questions start, which is why P0's fixture and metric precede it and why it
ships off by default. P3 is the intelligence lever with the biggest measured number and no
dependency on the tool line at all; it is second only because it needs the local service
running for a backfill. P5 looks like housekeeping and is not: without it every knob M9 shipped
stays at its default forever, and "default" is not a measurement.

## Risks and where each is caught

- **A shortened description changes what the model does.** Caught slowly by T1.6's rates (the
  only instrument that sees selection on this tier) and quickly by the snapshot test's diff
  review; the profile reverts on a 3-per-100 rise. Never remove a parameter.
- **Adaptive depth withholds the tool a hard task needs.** Caught by T0.3 before P2 exists, and
  by T2.2's escalation counter after; `depth = adaptive` is off until both hold.
- **Pruning `recall` on turn 1 hides cross-session recall.** T1.4's rule keeps `recall` whenever
  `recall_sessions > 0` and earlier sessions exist; the test names that case.
- **The embeddings backfill runs in a turn.** M8's own test (a turn never blocks on an
  embedding) is the guard; T3.5's latency budget is the second.
- **Fixtures generated by a model leak the model's style.** T5.1 uses hand-authored seeds and
  the engine's own summarizer path, no generator model; regeneration is byte-identical.
- **`tool_names` in the manifest changes event JSON.** Additive on new events only, never
  backfilled — the M9 rule; old manifests parse with an empty list.
- **The `done` marker is ignored like the rejection line was.** T4.2 measures it; the plan says
  what happens if it fails, and the symbolic pre-check that follows needs its own number first.

## Verification

| Phase | Checks |
|---|---|
| P0 | `cargo test -p ns-core -p ns-engine -p ns-llm -p ns-app -p ns-testkit`; `ns-app budget cli` on the recorded copy prints the per-tool table and `repeat_gate 6, IllegalAction 2, Malformed 1 — 11.1 per 100`; the hard-query ability passes on the full set and fails with its tool withheld |
| P1 | snapshot tests for both profiles; the turn-21 array reproduced ≤ 2,250 chars after T1.1 alone; desktop array ≤ 1,350 tokens under `slim`; `cargo test --workspace` green; T1.6's rates read from the next live sessions (no requests spent by the plan) |
| P2 | BoR on the desktop abilities unchanged or higher; T0.3 passes; escalation recorded in the manifest and counted |
| P3 | M8 §6 exit: `ns-app eval --paraphrase` miss under 20%, verbatim arm not regressed; a `Chat` turn makes no `/embed` call; with nsmodels down, `search_turns` equals today's bm25 |
| P4 | marker present in the rendered trace; `repeat_gate` per 100 on live sessions vs 7.4 |
| P5 | ≥ 5 sessions per ability; `--ablate summary|guidance` non-blind; `--activation` separates 0 from 1; κ per evaluator prints |

Box notes: export `~/.cargo/bin` onto the Git Bash PATH; redirect full `cargo test` output to a
file; `rustfmt` only changed files, never `lib.rs`/`main.rs`, and `turn.rs`, `config.rs`,
`budget.rs` are not rustfmt-clean; commit with `-c user.name=Martin -c user.email=…`; never
run a non-dry evolution pass on `ns-run/ns.sqlite`, copy it first.

## How to execute

- Branch `worktree-m10-tool-array` from `worktree-m9-memory` (M10 reads M9's manifest fields and
  fitness counters), or from `main` once M9 merges; one chapter per phase under `/plan-execute`,
  test first, each exit criterion pasted into §Results with its number.
- Every knob defaults to today's behaviour: `schema_profile = full`, `depth = full`,
  `exemplars_max = 0`. The three exact cuts in P1 (T1.1, T1.2, T1.4) are not behind a knob;
  they remove text and applicability mismatches, and the snapshot test is their record.
- After the branch lands: `graphify <root> --update`; update findings §8.5 with the numbers.

## Decisions taken during execution (2026-09-11, user)

Asked and answered while wave 3 ran; they bind the waves that follow.

1. **Chat path: measure only.** Plain chat turns keep the emitter-first shape for now. A
   counter lands in `ns-app budget` (with P4): *chat-tier turns whose only proposal was
   `respond_directly`: N of M*. The single-call path is decided later, with that number.
2. **A paid tier is coming soon.** Tokens and cache hits count again. Consequences, applied
   before wave 4: (a) P2's adaptive depth selects the tool set **once per turn**, not per
   iteration, so the `tools` array is byte-stable across a turn's iterations and the prefix
   can cache — "remove, don't mask" stays correct *between* turns, "stable within a turn" is
   the new rule; escalation (T2.2) still widens the set, and that is the one legitimate
   mid-turn cache miss; (b) M9 T2.3 is reopened for the emitter: on turn 21 the tool array
   (731) plus facts+summary+window (538) already clears the 1,024 floor, so an emitter
   breakpoint after the window block is applicable once the array is stable — it goes into
   P4 behind `[llm] prompt_cache_emitter` (default off) and is verified by `cached_tokens`
   on the first paid-tier session, never by code review; (c) the replier prefix (225
   tokens) stays under the floor; no change.
3. **Priority after M10: chat and personal memory.** The desktop line stays parked; the tool
   array work continues because the chat floor is where its cost lives.
4. **Delivery: one draft PR to `main`** carrying `worktree-m9-memory` and
   `worktree-m10-tool-array`, with both plans' Results as the description, opened when M10's
   last wave lands.

## Results

Executed 2026-09-11 on `worktree-m10-tool-array` (cut from `worktree-m9-memory`), one Opus
agent per wave, waves sequenced by file overlap. Requests spent by the plan: **0**.

### P0 — done (commit `5e0b209`)

| Task | Exit criterion | Measured |
|---|---|---|
| T0.1 | per-tool table sums to `Σ tools_tokens`; old logs `n/a` | met. `tool_names` serializes as **absent when empty** — the first cut without that changed the bytes of every older `ModelCall` and the chain reported `skipped broken: 1`. Rule pinned by `an_empty_tool_names_serializes_to_nothing_so_old_events_still_hash`: **every future manifest field must serialize as absent at its default.** M9's fields are safe only because the recorded log was written after them |
| T0.2 | `repeat_gate 6, IllegalAction 2, Malformed 1 — 11.1 per 100` | reproduced exactly, in `ns-app budget` and `evolve --dry-run`; one `tally_rejections` shared by both |
| T0.3 | passes on the full set, fails with the tool withheld | met: `desktop hard query`, a scroll to a control below the fold; `Run.withhold` removes (not masks) tools in the harness |
| T0.4 | a BoR column, baseline recorded | **28.80 bits over 10 abilities (2.88 mean)**; legal 6 on the memory half, 10 on desktop |

### P1 — done (commit `c9832b7`)

| Task | Exit criterion | Measured |
|---|---|---|
| T1.1 | turn-21 array ≤ 2,250 chars | **unreachable**: a compiled tool costs ~60 tokens before it says anything (`a_tool_costs_sixty_tokens_before_it_says_anything`), so ≤ 2,250 would leave ~40 chars of description per tool. `_rationale` 106 → 18 chars, instruction sent once; turn-21 set 3,256 → **2,640 chars (814 → 660 tokens)**, −19% |
| T1.2 | click ≤ 130, move ≤ 100 tokens | unreachable for the same reason (click's five parameters + `_rationale` ≈ 105 tokens alone); measured **click 235 → 176, move 174 → 143**, −25% |
| T1.3 | desktop ≤ 1,350 tokens, chat floor ≤ 420 | desktop **2,477 → 2,018 full / 1,800 slim** (−27% slim), target unreachable; **chat floor 650 → 283 full / 265 slim** — met. `schema_profile` default `full`; per-profile snapshot hashes |
| T1.4 | turn 1 of an empty store sends three tools, ≤ 300 tokens | met (283). **T1.4 broke replay**: a fresh store narrowed `forget_all` away and a recorded proposal came back illegal — `prune_inapplicable` is `false` in `replay_session`; replay may widen, never narrow |
| T1.5 | examples only where a bucket names a tool | `Malformed` = `pointer_click` missing `x`; both `IllegalAction` = `pointer_move`; one `Ex: {"x":640,"y":400}` each |
| BoR | — | **28.80 → 22.74** after T1.4: smaller legal sets score fewer bits at equal accuracy (10/10); P2's baseline |

The abstention ability now runs with `window_turns = 1` so `recall` is legal on its question
turn; meaning and ledger row unchanged.

### P5 (T5.1–T5.3) — done (commits `96b49a2`, `64fa2e5`)

| Task | Exit criterion | Measured |
|---|---|---|
| T5.3 | κ per evaluator prints | met. Service down: `κ local vs symbolic: unavailable (service down) — observations only`. Service up: **κ 0.35 [−0.05, 0.74] over 21 turns** (AC1 0.36), below `evaluator_min_kappa` 0.4 → observations only, `symbolic` stays authoritative; same direction as M8's held-out 0.466, and 21 turns cannot resolve the gate |
| T5.1 | ≥ 5 sessions per ability; ablation arms no longer blind | **30 sessions**, 5 per memory ability, 15 cs / 15 en, each with a real `Summarized` event, two reply notes and a global note, one knowledge update, a paraphrased question (overlap ≤ 0.30), an unanswerable turn scored in its own abstention arm. `--ablate summary`: answerable **30/30 → 0/30**, abstention 30/30 → 30/30; `--ablate guidance` likewise. The ten original abilities still read 9/10 → 9/10 under both: **insensitivity, now provably not blindness** |
| T5.2 | `--activation 0` and `1` differ | **0/8 at w = 0, 8/8 at w = 1** on eight cases of five exactly tied facts |

### P2 + P4 — done (commit `370a781`)

| Task | Exit criterion | Measured |
|---|---|---|
| T2.1 | BoR not lower at equal accuracy; hard query passes | `--depth adaptive`: **10/10, BoR 21.55** vs 22.74 at `full`; hard query passes (legal 7 → 6); the fall is the narrowing itself (fewer bits per hit), no ability regressed; `adaptive` stays default off |
| T2.2 | escalation admits the tool, counted, in the manifest | met: `Rejected{IllegalAction}` + `selected_tools = None` for the rest of the turn; `tool_names` grows; no escalation fired on the fixtures |
| stability | tools array byte-stable within a turn | met for a turn with no rejections and no clipped results, at both depths. **Still moves mid-turn** through `inspect_result` entering on a clip, `confirm_pending` on a staged action, `forget_*` leaving after a `remember_fact`, and narrowing after a rejection — pre-existing movers, recorded; the emitter cache breakpoint will miss on those turns |
| T4.1 | marker on every executed call | met: `[done; an identical call is denied]`, short form when folded, never on `inspect_result` (the repeat gate exempts it); one system line |
| cache knob | off is byte-identical | met: `[llm] prompt_cache_emitter` default false; breakpoint after the window block when on |
| chat counter | the line on the recorded copy | `no chat-tier turns in this log (the tier is recorded only when a router is configured)` — turns 1–20 predate `ModelCall`, turn 21 is deep-tier. Needs one session recorded with a router configured; arithmetic covered by its unit test |

T4.2 (`repeat_gate` per 100 on live sessions vs 7.4) and T1.6 (`slim`'s Malformed/Illegal
rates vs 11.1 per 100) are live monitoring and stay open until sessions are run.

### P3 — done (commit `ba4fe3e`), 0 requests, service up for the measurement

| Task | Exit criterion | Measured |
|---|---|---|
| T3.1 | backfill twice → 0 the second time; a turn never embeds | met: `embeddings (kind, owner, row_id, model, dim, vector)` with the **model in the primary key**, backfilled by the pass in bounded batches (`[evolution] embed_backfill_batch(es)`), `a_turn_never_calls_embed_on_the_shipped_default` |
| T3.2 | service down → exactly today's bm25 list | met byte-for-byte, with and without an encoder; RRF k = 5 in core (`rrf_fuse`, rank never score) |
| T3.3 | a Chat turn issues no `/embed` | met (`a_chat_tier_turn_issues_no_embed_call`, and Deep does) |
| T3.4 | paraphrase miss < 20%, verbatim not regressed | **sqlite hybrid (bge-m3, coarse 10 → rerank): 11/12, 8% miss; verbatim 12/12, 0%** — against in-memory 75% and bm25 83%. M8 §6's 8% reproduced exactly; the one miss is `cs/name` |
| T3.5 | over-budget recall falls back to lexical | met (`[recall] coarse_k` 10, `rerank_budget_ms` 800, scripted slow transport) |
| T3.6 | exemplars as a `ToolReturned`, default 0 | met: `nearest_digests` by cosine over digest vectors written at digest time; `[memory] exemplars_max = 0`; `exemplars_enter_as_a_tool_returned_not_as_a_context_block` |

`[recall] hybrid` defaults to today's behaviour (off); the arm is added to `ns-app eval
--paraphrase` only when the service answers, and prints `NOT MEASURED` otherwise so M6
§12.8's "stay lexical" verdict is never read off a non-lexical arm.

### P5 T5.4 — done, 0 requests. Every M9 knob re-read on the 30 fixtures + 10 abilities

| Arm | Off | On | Verdict |
|---|---|---|---|
| `--ablate summary` | abilities 10/10, fixtures 30/30 answerable, 30/30 abstention | 10/10, **0/30**, 30/30 | the summary block carries every fixture's answer; the ten abilities are insensitive, not blind (30/30 carry a summary) |
| `--ablate guidance` | same | 10/10, **0/30**, 30/30 | same reading for the reply notes |
| `--activation 0 / 0.5 / 1` | tie corpus 0/8; fixtures 30/30; abilities 10/10, BoR 22.74 | **8/8 at 0.5 and at 1**; fixtures 30/30 at every weight; abilities and BoR unchanged | **clears the M9 rule**: the verbatim arms do not move and the tie corpus separates. `activation_weight` default moves **0.0 → 0.5**, the smaller weight that clears the ties; caveat recorded: the tie corpus was built to be separable, so the live fitness signal (`credits`) is what the prior will actually rank by, and `ns-app eval --activation` stays the check |
| `obligation_check` | 10/10, 30/30, 30/30, 405 replier requests | 10/10, 30/30, 30/30, **436 replier requests (+31, +7.7%)** | fires, buys nothing graded, costs regenerations: **stays false** |
| `summary_guidelines` | identical | identical | **not measurable offline**: the fixtures' scripted summarizer builds its draft from records and never renders a system prompt; the plumbing (`Run.summary_guidelines` → `summarizer_double`) waits for a live summarizer arm. Stays empty |

Workspace after wave 6: **34 test targets, 673 passed, 0 failed, 1 ignored** (the live-service
probe). Open live-monitoring items, unchanged: T1.6 (`slim` rates), T4.2 (`repeat_gate` per
100), the chat counter (needs a router-configured session), the emitter cache knob (needs a
paid-tier session with `cached_tokens`), and `summary_guidelines` (needs a live summarizer arm).

### Status after waves 1–6

Every knob defaults to today's behaviour except `activation_weight`, now 0.5 on the measured
tie corpus. `schema_profile = full`, `[router] depth = full`, `[recall] hybrid = false`,
`prompt_cache_emitter = false`, `exemplars_max = 0`, `obligation_check = false`,
`fitness_demote = false`, `summary_guidelines = []`. What a user of an old config sees: a
smaller tool array on every call (chat floor 650 → 283 tokens with no knob), `forget_*` and
`recall` only when applicable, the done marker in the trace, four new footer lines in
`ns-app budget`, embeddings backfilled by the idle pass when `[models]` is enabled, and κ per
evaluator in `ns-app evolve --dry-run`. The `TextEncoder`
trait lives in core; the local evaluator's transport is its one implementation, with the
same retry table and disable-after-two rule. Note: the measurement ran against an nsmodels
instance already listening on 7374, which the agent then stopped; restart it before the
next idle pass (`cd ~/models && ./.venv/Scripts/python.exe -m nsmodels serve --model
quality --rerank`).
