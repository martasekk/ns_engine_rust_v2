# M9 — Memory that earns its context

Date: 2026-09-11 · Approved 2026-09-11 · Branch `worktree-m9-memory`, cut from
`worktree-research-multi-conversation` @ `c195262` (main @ `206ea0d` + dispatcher). Evidence:
`docs/research/2026-09-11-child-session-memory-and-token-economy-findings.md` §5 (levers) and
§7.6 (the ranking the user chose: most efficiency and intelligence, lowest context cost, the
desktop line parked). Convention as M7/M8: phases, task tables, requests spent, "not built,
with the reason"; measured numbers land in §Results at the end as each task's exit criterion
is met.

## Context

The engine already has the memory shape the 2025–2026 literature converges on: a verbatim
event log, versioned scoped facts, a fixed-field summary, session digests, a bounded window,
a clipped trace, on-demand recall, and an idle evolution pass that learns gated rules and
notes. Two sweeps of the field found no architecture that replaces any of it. What they found
is that the engine **does not measure what it loads**: no cached-token count, no tool-schema
share on a real session, no record of whether a fact or note that sat in a prompt ever
changed an outcome. On a tier where every model call is one of ~50 a day, that is the gap.

So M9 is not "add a memory architecture". It is: **close the loop from outcome back to
selection, offline and at zero requests, and make every block in the prompt earn its place
with a number.** Four concepts are adopted from the sweeps — an activation prior on recall,
usefulness-fitness forgetting with a citation boost, strategy memory from successes, case
exemplars — plus prompt hygiene the earlier findings already specified (a cache breakpoint
that can hit, an obligations checklist, a cap on guidance). Everything that touches retrieval
or forgetting is decided inside the M8 evaluation lane, so finishing the minimal part of M8
Phase 2 is a phase of this plan, not a prerequisite left to chance.

Total request spend for the whole plan: **2–3**, in one live smoke run.

## What does not change

| Stays | Reason |
|---|---|
| Bounded window, fixed-field summary rebuilt from verbatim, ≤10 facts, clipped trace, no accumulated history | Composition §5, M7 §12; §7.6 rank 1 says the shape is already right |
| No model in the router, the fold, or the guard chain; no model-decided applies; a check becomes a gate only after a measured true-positive rate | M5 §2, M6 §13 — every new counter here *reports* before it *demotes* |
| Legal-set pruning per iteration, even though it defeats emitter prefix caching | Selection accuracy on a small model; the free tier does not bill the miss (§5 R3) |
| `TurnRecord` as a pure projection of the log (`crates/core/src/memory.rs:11`) | Nothing per-turn is persisted as a column; obligations are therefore a pure function of the log, like the trace |
| Recall results re-enter context only as `ToolReturned` events (`crates/engine/src/turn.rs:891`) | Trust, provenance, replay — exemplars obey the same rule |
| `verify_note`'s gate, regression budget and lowest-lift eviction (`crates/evolution/src/notes.rs:212`, `pass.rs:392`) | The success lane gets no privilege the failure lane lacks |
| Three roles, one action per emitter request | Multi-action batching needs a log measurement first (§5 R1); out of scope |
| M8 Phase 3 (T3.1 embeddings backfilled in the idle pass, T3.2 hybrid search with bm25 fallback) | Referenced, not redesigned; P5's exemplars wait for it |
| Graph memory, consolidated window, sub-agent framework, Temporal server, RL, fine-tuning | Contraindicated; unchanged |
| `fold_older_steps` (`crates/engine/src/trace.rs:184`) | Already HiAgent's mechanism with a fixed boundary; build a semantic boundary only if P2's measurement says so |

## Architecture after M9, in one page

**Forward arrow (selection → prompt), lightly changed.** `select_facts`
(`turn.rs:456`) still returns pinned facts plus `search_facts`, and both stores still bottom
out in the one shared ranker `lexical_rank` (`memory.rs:138`), which gains an additive
activation term with a knob that defaults to **0.0**. `EmitterContext` (`traits.rs:50`) gains
`obligations`, rendered by `render_context` (`emitter.rs:33`) directly above the window and
dropped after the window in `fit_emitter` (`budget.rs:180`). `guidance` loses its exemption:
a `guidance_max` clamp becomes a fit stage. The replier gets one breakpoint after the last
stable block, the emitter one after the window block — **only if the estimated stable prefix
clears the provider's 1,024-token floor; otherwise recorded as not applicable on this config.**

**Backward arrow (outcome → selection), entirely new, entirely offline.**
`EventKind::Graded { turn, verdict, by, revision }` persists a per-turn verdict (M8 T2.3a).
`ContextManifest` (`usage.rs:66`) keeps `guidance: usize` and gains `note_hashes`. A new
infrastructure event `ReplyCited { sources }` records which reference parts a reply's grounded
claims matched. In `run_report` step 4 (`pass.rs:193`), per session, every `ModelCall`
manifest is joined to the `Graded` and `ReplyCited` events with the same `turn`: each
`fact_keys[i]` gets `exposures += 1`, and `credits += 1` when the verdict is good **or** the
key is in `sources`. The same join over `note_hashes` fills per-note counters in the ledger's
free-form `numbers` (`ledger.rs:24`). `consolidate_facts` (`consolidate.rs:103`) reads
`exposures` / `credits` as a third demotion signal — reporting first, demoting only behind a
knob that stays off for one release. Once P4 exists, the activation prior's frequency term
reads `credits`, so recall ranks by *usefulness*, which is the Darwinian idea in one column.

**Zero-request throughout.** Every counter is computed in the idle pass from the log.

## Concepts adopted

| Concept | Source (findings) | Form here |
|---|---|---|
| Measure before load | §5 R2–R3 | `cached_tokens` per call; `tools_tokens` finally read; both in `ns-app budget` |
| Ablation as the arbiter | §7.4 S8 (ATMem's idea, not its RL) | `ns-app eval --ablate <block>` prints each block's marginal effect |
| A breakpoint that can hit | §1.5, §5 R3 | Breakpoint after the last stable block, conditional on the 1,024 floor, verified by `cached_tokens` |
| Obligations checklist | §2 (codegen §3.1b, 5/10 → 10/10) | Symbolic extraction from the user's text; a checklist block; one regeneration if the reply leaves one unaddressed |
| Activation prior | §7.4 S3 (SuperLocalMemory, Hindsight; vstash's caution) | Recency × usefulness term in `lexical_rank`, default 0, decided by the suite |
| Usefulness-fitness forgetting + citation boost | §7.1 N7 (Darwinian, RMM, DeMem) | `exposures` / `credits` on facts and notes from the manifest ∩ grade ∩ citation join |
| Strategy memory from successes | §7.1 N2 (ReasoningBank) | `SignatureKind::Succeeded` into the *same* notes gate |
| Case exemplars | §7.1 N1 (Memento, Synapse) | ≤2 nearest digests as a `ToolReturned` in the Deep tier, after M8 T3.1, default off |
| Failure-derived summary guidelines | §3.4 (ACON) | `summary_guidelines` appended to the summarizer prompt, measured by the ablation arm |

## Phases

Dependency order: **P0 → P1 → P4** is the critical path (§7.6 rank 3 hangs on the manifest
carrying hashes and the log carrying a grade). P2 and P3 are independent of it. P5 needs P1
(T5.1) and M8 T3.1 (T5.3).

### P0 — Instruments (0 requests, ~1 day)

Goal: no later phase is arguable without a number this phase produces.

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T0.1 | `Usage.cached_tokens: u32` (`#[serde(default)]`), parsed from `usage.prompt_tokens_details.cached_tokens` beside `tools_tokens` | `crates/core/src/usage.rs:15`; `crates/llm/src/client.rs:211-236`; the five `Usage` literals in tests (`app/src/budget.rs:452`, `crates/testkit/src/eval.rs:244`, `crates/evolution/src/mine.rs:810`, `crates/evolution/src/evaluate.rs:607`, `crates/core/src/usage.rs:160`) | `client.rs` inline `cached_tokens_is_read_from_prompt_tokens_details`, `a_usage_block_without_details_reports_zero_cached` | fixture with `cached_tokens = 2048` → `Usage.cached_tokens == 2048`; absent → 0 and `estimated` unchanged |
| T0.2 | `cached` column in `Measured::add_call` and `MEASURED_COLUMNS`; a footer line "cached: X of Y prompt tokens" | `app/src/budget.rs:147, 225, 245-272` | `budget.rs` inline `cached_column_sums_only_measured_calls` | `ns-app budget cli` prints the column; estimated calls contribute 0; `RECONSTRUCTED_COLUMNS` untouched |
| T0.3 | `ContextManifest.note_hashes: Vec<String>` (`#[serde(default)]`; keep `guidance: usize`); `LearnedRules::guidance_for` / `guidance_for_reply` return `(hash, text)` so the manifest builders can fill it | `crates/core/src/usage.rs:66`; `crates/core/src/learned.rs:170, 179`; `crates/engine/src/trace.rs:445, 469` (`emitter_manifest`, `reply_manifest`) | `usage.rs` inline `an_old_manifest_json_without_note_hashes_still_parses`; `crates/engine/tests/turn_loop.rs` `manifest_records_a_hash_per_rendered_guidance_note` | old manifests round-trip; `note_hashes.len() == guidance` on every new call |
| T0.4 | `ns-app eval --ablate <facts\|summary\|guidance\|obligations>`: a flag on `Harness` beside `desktop: bool` (`eval.rs:687`) that blanks one block inside `run` (`eval.rs:752`); report modelled on `paraphrase::Arm` (`paraphrase.rs:224`) | `crates/testkit/src/eval.rs`; new `crates/testkit/src/ablate.rs`; `app/src/eval.rs` beside `run_paraphrase` (:303) | `ablate.rs` inline `blanking_facts_lowers_the_fact_dependent_abilities_only` | prints per-ability pass counts for the full and ablated arms plus a marginal delta; exit 0 whatever the number |
| T0.5 | stable-prefix estimate: a `ns-app budget` footer line giving, per role, the estimated tokens of the blocks before the candidate breakpoint (persona+facts+summary for the replier; facts+summary+window for the emitter), from the existing manifest and `estimate_tokens` | `app/src/budget.rs` | inline `prefix_estimate_uses_the_manifests_block_sizes` | the number P2 reads before moving any breakpoint |

Not built: `session_id` sticky routing (needs P2's measurement first).

### P1 — Finish M8 Phase 2, minimally (0 requests, ~1 day)

Goal: a grade that survives replay, and a gate that consumes it. **M9 needs T2.3a, T2.6 and
T2.8 and nothing else from Phase 2.** T2.4's observations table is a new store whose only
consumer (`relevance_count` on recall) is on no §7.6 rank and costs a migration across the
four `MemoryStore` impls for no measured effect; deferred. The paid `ClientEvaluator` is
deferred: the local evaluator's held-out κ is 0.628 against the symbolic 0.501, and a paid
judge spends requests. (S4/EARM matters only once it exists.)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T1.1 | `EventKind::Graded { turn, verdict, by, revision }`, classed infrastructure in `replay.rs:295/:372`, inert in `fold` (`state.rs:50`); written by `run_report` step 4 | `crates/core/src/event.rs:15`; `crates/engine/src/replay.rs`; `crates/evolution/src/pass.rs:193` | `crates/engine/tests/replay.rs` `grading_a_session_then_replaying_it_yields_identical_grades_without_a_service` | grade → stop nsmodels → replay: verdicts byte-identical; fold output unchanged |
| T1.2 | `TurnOutcome::Graded` in `verify_note`: positives and negatives are graded turns, not signature presence alone | `crates/evolution/src/notes.rs:89-105, 212` | `notes.rs` inline `a_candidate_is_unverified_when_no_turn_in_the_session_is_graded` | a candidate over sessions with ≥1 graded turn reports `improved`/`regressed`; with none it is Unverified, never accepted |
| T1.3 | `evaluate_budget_turns` in `PassConfig.evaluate` (the nested block reserved for it, `pass.rs:40`); graded turns remembered by `(session, turn)` | `pass.rs`; `app/src/config.rs` (no `[eval]` section exists — add `evaluate_budget_turns` under `[models]`) | inline `a_second_pass_grades_nothing_already_graded` | a second `ns-app evolve --dry-run` reports 0 newly graded turns |

Exit for the phase, inherited from M8: `ns-app evolve --dry-run` on `ns-run/ns.sqlite` prints
κ per evaluator and spends zero requests with `[models] enabled = true`.

### P2 — Prompt hygiene (2–3 requests in one smoke run, ~1 day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T2.1 | Obligations: a pure function `obligations_for(user_text) -> Vec<String>` (questions → "answer: …", imperatives → "do: …", capped at `obligations_max`, default 5); rendered as a checklist block above the window in `render_context`; new `EmitterContext.obligations` and `ReplyContext.obligations` (also added to `ground::reference_parts`); the existing in-turn symbolic interceptor checks the draft reply against them, reusing `evaluate.rs`'s ignored-question logic, and regenerates **once** with `do_not_state`-style guidance ("not yet addressed: …") — same shape as `ReplyFlagged` (`turn.rs:2191-2200`) | `crates/core/src/memory.rs` (new fn beside `query_tokens`); `traits.rs:50, 125`; `emitter.rs:33`; `replier.rs:79`; `ground.rs:16`; `budget.rs:180` (dropped after the window, before facts); `turn.rs` reply path | `memory.rs` inline `two_questions_yield_two_answer_obligations`; `crates/engine/tests/turn_loop.rs` `a_reply_leaving_an_obligation_unaddressed_regenerates_once`, `obligations_drop_after_the_window_and_before_facts` | the interceptor fires at most once per turn; the block is dropped only after every window record; no new event kind, no migration |
| T2.2 | `guidance_max` knob (default 6) and a fit stage clamping guidance before the summary clamp; `BudgetReport.dropped` names the dropped notes | `app/src/config.rs:279` + `TurnConfig` (`turn.rs:49-52`); `budget.rs:202-205` | `budget.rs` inline `guidance_is_clamped_before_the_summary_is` | a context with 12 notes renders 6 and reports the rest dropped |
| T2.3 | Replier breakpoint moves from after the persona to after `<summary>`; emitter gets a breakpoint after the window block — **both gated on T0.5's estimate being ≥ 1,024 tokens for that role**; if under, the change is not made and the plan's Results record "not applicable on this config, prefix N tokens" | `crates/llm/src/replier.rs:139-146`; `crates/llm/src/emitter.rs` (content-parts form, `prompt_cache` gate mirrored from the replier) | `replier.rs` inline `the_breakpoint_falls_after_the_summary_block_not_the_persona`; `emitter.rs` inline `the_emitter_breakpoint_precedes_the_trace_block` and `the_breakpoint_prefix_is_byte_identical_across_iterations` | breakpoint index identical across iterations 2..12 of one recorded turn |
| T2.4 | **Live smoke**: one real turn with 2–3 emitter iterations, then `ns-app budget cli` | — | manual; result recorded in the plan's Results | `cached > 0` on the second emitter call if T2.3 was made; `tools_tokens` share, `trace_lines` median and requests-per-turn read for the first time since the log reset. **Spends 2–3 of the day's 50 requests** |

Decisions: a new obligations list, not `SessionSummary.open` — `open` is model-written prose
rebuilt by the summarizer; obligations must be symbolic, per-turn and checkable without a
model. One replier breakpoint, after the summary — facts change whenever the user states one;
the summary is stable for four turns. Obligations need no event: they are a function of
`UserSaid`, exactly as `trace_entries` is a function of the turn's events.

Not built: model-set obligations (a request per turn), block-order reordering (T0.4's arm
decides it; findings §7 item 11), `session_id` routing.

### P3 — Recall (0 requests, ~½ day plus one measurement run)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T3.1 | Activation term in `lexical_rank`: `score = hits + w · ln(1 + freq) · exp(−Δdays / half_life)`, `w = activation_weight` (default **0.0**), `half_life = 7.0`; `freq` is `uses` until P4 lands, then `credits` (one closure, one line to switch); `Δ` from `last_used` | `crates/core/src/memory.rs:138`; knob in `MemorySection` + `TurnConfig` | `memory.rs` inline `activation_weight_zero_reproduces_todays_order_exactly`, `a_recently_credited_fact_outranks_an_equal_hit_count_at_weight_one` | at `w = 0.0` the output is byte-identical to today's |
| T3.2 | Recency term in `search_turns` behind the same knob (post-hoc rescoring before truncate, both stores) | `crates/memory-sqlite/src/lib.rs:425-459`; `crates/engine/src/store.rs` | `memory-sqlite` inline `search_turns_order_is_unchanged_at_weight_zero` | same |
| T3.3 | Decide `w`: `ns-app eval --paraphrase` and `--ablate facts` at `w ∈ {0, 0.5, 1.0}` | `app/src/eval.rs` | — | `w` moves off 0 only if the verbatim arm does not regress; the number goes into Results |

Why not full ACT-R base-level (`ln Σ tⱼ^−d`): it needs per-use timestamps, which no table
has; the closed form uses exactly the two columns that exist. vstash's negative result on
BEIR is why the default is 0 and the suite decides.

### P4 — Fitness (0 requests, ~1½ days)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T4.1 | `Fact.exposures: u32`, `Fact.credits: u32` — `#[serde(default)]`, `Default`, `ALTER TABLE facts ADD COLUMN … NOT NULL DEFAULT 0` via the `table_info` sniff (`lib.rs:255`); added to **both** inlined conformance suites | `crates/core/src/action.rs:206, 239`; `crates/memory-sqlite/src/lib.rs:245-292, 826`; `crates/engine/src/store.rs:276` | both suites: `exposures_and_credits_survive_a_put_and_reload` | a pre-M9 `.sqlite` opens and reports 0/0 |
| T4.2 | `ReplyCited { sources: Vec<String> }` infrastructure event beside `ReplyEchoed`; `ground::reference_parts` returns `(source_id, text)` with ids `fact:<key>`, `summary`, `window:<turn>`, `guidance:<hash>`; `Material` reports which ids the grounded claims matched | `crates/engine/src/ground.rs:16-60`; `crates/engine/src/echo.rs`; `turn.rs:2176-2198`; `event.rs`; `replay.rs` classification | `turn_loop.rs` `a_reply_quoting_a_fact_value_records_that_fact_as_cited` | `sources` names the fact key; `Material::from_context`'s substring semantics unchanged |
| T4.3 | The join in `run_report` step 4: per session, `ModelCall.manifest.fact_keys` / `note_hashes` × `Graded{turn}` × `ReplyCited{turn}` on equal `turn`; credits the fact version current at pass time (recorded caveat: versions superseded between call and pass are not credited) | `crates/evolution/src/pass.rs:193` (new step 4b) | `pass.rs` inline `a_fact_in_two_graded_good_turns_gets_two_credits_and_two_exposures`, `a_cited_fact_in_a_bad_turn_still_gets_a_credit`, `an_ungraded_turn_moves_neither_counter` | counters match a hand-computed fixture; a graded session with `Σ exposures == 0` is printed as an alarm |
| T4.4 | Third demotion signal in `consolidate_facts`: `exposures ≥ fitness_min_exposures` (default 8) and `credits == 0` → Cold. Pinned prefixes excluded before the branch. Knob `fitness_demote` default **false**; the dry-run prints the would-demote set | `crates/evolution/src/consolidate.rs:103`; `PassConfig` | inline `a_pinned_fact_is_never_demoted_for_low_fitness`, `a_fact_with_credits_is_not_demoted`, `demotion_is_reported_not_applied_while_the_knob_is_off` | with the knob off, the report lists candidates and the store is unchanged |
| T4.5 | Per-note counters in the ledger's `numbers` (`{exposures, credits, lift}`); reporting lines in `ns-app evolve --dry-run`: top-10 zero-credit facts, per-note credits beside lift | `pass.rs`; `ledger.rs:24` | inline `the_dry_run_prints_exposures_and_credits_per_fact_and_note` | the numbers are readable without opening SQLite |

Rule kept: fitness **reports before it demotes**. `fitness_demote` flips on only after one
release's dry-runs show the would-demote set is not full of facts that were simply never
queried.

### P5 — Pass (0 requests, ~1 day; T5.3 blocked on M8 T3.1)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T5.1 | `SignatureKind::Succeeded` mined from turns with a good `Graded` verdict that share an action pattern; `lane()` routes it to `"note"`; `verify_note` unchanged | `crates/evolution/src/mine.rs:11, 127, 373` | `mine.rs` inline `a_repeated_good_turn_pattern_mines_a_succeeded_signature_into_the_note_lane` | no new accept path; candidates show lift and regression like any note |
| T5.2 | `[memory] summary_guidelines: Vec<String>` appended to the summarizer system prompt after the fixed-field instructions; hand-filled from graded failures at first | `app/src/config.rs:279`; `crates/llm/src/summarizer.rs:10, 41-61` | `summarizer.rs` inline `guidelines_render_after_the_fixed_field_instructions`; `ns-app eval --ablate summary` on vs off | the arm reports summary-dependent ability deltas; guidelines stay only if the delta is ≥ 0 |
| T5.3 | Case exemplars in the Deep tier's pre-emptive step (`turn.rs:893-918`): ≤ `exemplars_max` (default **0**) nearest `session_digests` by local embedding, emitted as one `ToolReturned` with the digests' min trust; embeddings from M8 T3.1's table | `turn.rs:893-918`; `crates/evolution/src/consolidate.rs:32` (embed at digest write) | `turn_loop.rs` `exemplars_enter_as_a_tool_returned_not_as_a_context_block`, `a_chat_tier_turn_retrieves_no_exemplars` | default 0 until `--ablate` shows a gain; blocked until T3.1 |

**HiAgent subgoal-collapsed trace: measure first, expect to drop.** `fold_older_steps`
already collapses all but the last N outcomes; T2.4's `trace_lines` median is the number.
Build a semantic boundary only if the median exceeds 8 lines on a real session; otherwise
record "not built, median N".

## Minimal cut (three days)

Day 1: all of P0. Day 2: P1 T1.1 + T1.2, and P2 T2.2, T2.3 (if T0.5 allows), T2.4 smoke.
Day 3: P4 T4.1, T4.3, T4.5 in report-only mode. Cut: P5 whole, P3 whole, P2's obligations
(T2.1), P4's citation signal (T4.2) and demotion (T4.4). That still yields the two numbers
M9 exists for — the cached-token hit rate and per-fact credits — and every cut item is
additive on top.

## Risks and where each is caught

- **Manifest and event fields change event JSON.** Events are hash-chained over serialized
  JSON (`event.rs:122`). Adding `note_hashes` to new events is safe; rewriting an old one
  breaks the chain. Rule: **never backfill a manifest.** Pre-M9 sessions simply have zero
  exposures. Caught by `an_old_manifest_json_without_note_hashes_still_parses` and a replay of
  the existing `ns-run/ns.sqlite`.
- **New `EventKind`s (`Graded`, `ReplyCited`) and replay.** Inert in `fold` unless matched,
  rejected by replay unless classed infrastructure (`replay.rs:295/:372`). Caught by T1.1's
  replay test.
- **The stable prefix never reaches 1,024 tokens on this config** (persona ~120 tokens, ≤10
  facts, an 800-char summary). T0.5 measures it first; T2.3 is skipped and recorded if it
  doesn't. Caching then stays a latency lever for a paid tier, as §3.6 already says.
- **Breakpoint × block order** (findings §7 item 11). T2.3 lands after T0.4's arm has run; if
  the arm later reorders blocks, the breakpoint test fails loudly instead of caching a moving
  prefix.
- **Activation prior vs vstash.** Default 0.0 with a byte-identity test; only T3.3 may move
  it, and only if the verbatim arm holds.
- **Demoting pinned facts.** Pinned prefixes are excluded before the fitness branch; a test
  guards it; `fitness_demote` is off for a release.
- **The join silently matches nothing** (an off-by-one between `ModelCall.turn` and
  `Graded.turn` would zero every credit and, with demotion on, demote everything). Caught by
  T4.3's hand-computed fixture and by the `Σ exposures == 0` alarm line.
- **Four `MemoryStore` impls** (`store.rs:27`, `memory-sqlite/lib.rs:320`, `plugin.rs:237`,
  `http_tool.rs:202`): M9 adds struct fields and one optional method (T5.3's nearest-digests)
  which gets a default trait body (the `search_turns_in` pattern, `traits.rs:283-306`). The
  conformance suite is inlined twice; both copies gain the same tests.
- **SQLite migration.** No `user_version`; follow the `table_info` sniff + `ADD COLUMN`
  pattern. "A pre-M9 `.sqlite` opens and reports 0/0" is the test that matters.
- **Obligation extraction over-fires on Czech/English mixed text** and regenerates replies
  needlessly. The interceptor regenerates at most once, and `--ablate obligations` measures the
  block; if the arm shows no gain the block stays but the interceptor is disabled by knob.

## Verification

| Phase | Checks |
|---|---|
| P0 | `cargo test -p nscore -p nsllm -p nsapp -p nstestkit`; `ns-app budget cli` shows `cached`, `tools_tokens`, the prefix estimate; `ns-app eval --ablate facts\|summary\|guidance` prints three marginal-effect tables |
| P1 | `cargo test -p nsevolution -p nsengine`; `ns-app evolve --dry-run` on `ns-run/ns.sqlite` prints κ per evaluator, ≥10 signatures, spends 0 requests; a second run grades 0 new turns |
| P2 | `cargo test -p nscore -p nsllm -p nsengine`; full `ns-app eval` unregressed; **one live turn, 2–3 requests**, then `ns-app budget cli` shows `cached > 0` on the second emitter call (if T2.3 was made) and the four first-time numbers |
| P3 | `cargo test -p nscore -p nsmemory-sqlite`; `ns-app eval --paraphrase` and `--ablate facts` at three weights, verbatim arm not regressed |
| P4 | both conformance suites green; `ns-app evolve --dry-run` prints exposures/credits, the would-demote set and per-note counters; replay of a pre-M9 session yields identical grades |
| P5 | `cargo test -p nsevolution -p nsllm -p nsengine`; `--ablate summary` with guidelines on/off; the dry-run lists `Succeeded` candidates with lift and regression counts |

Box notes that apply to every phase (from memory): export `~/.cargo/bin` onto the Git Bash
PATH first; redirect full `cargo test` output to a file rather than piping through `tail`;
pass only changed files to `rustfmt`; commit with `-c user.name=Martin -c user.email=…`.

## How to execute

- One worktree branch, `worktree-m9-memory`, from `main` after the multi-conversation branch
  merges (P4's join reads `ModelCall` events that only exist on `main` since M7).
- One chapter per phase under `/plan-execute`; each task test-first where a suite exists;
  each task's exit criterion pasted into the plan's Results section with its number, in the
  M7/M8 style, including the "not built" lines (T2.3 if the prefix is short, HiAgent if the
  median is short).
- Every knob added here defaults to today's behaviour (`activation_weight = 0`,
  `fitness_demote = false`, `exemplars_max = 0`, `obligations_max = 5` with the interceptor
  behind `obligation_check = true` only after T0.4's arm). A fresh checkout with an old config
  behaves exactly as before M9 except for the extra columns in `ns-app budget`.
- After the branch lands: `graphify <root> --update`, then update
  `docs/research/2026-09-11-…-findings.md` §7.6 with the measured numbers.

## Appendix — substrate facts the plan relies on (verified read-only)

- `EmitterContext` `crates/core/src/traits.rs:50-79`, filled `crates/engine/src/turn.rs:1030-1042`;
  `fit_emitter` `crates/core/src/budget.rs:180` drops window, then unpinned facts, then clamps
  the summary; `user_text`, `trace_so_far`, `rejections_this_turn`, `guidance` exempt and
  uncapped (:202-205). Render order `crates/llm/src/emitter.rs:33-82`.
- `ReplyContext` `traits.rs:125-150`; built `turn.rs:2120-2133`; `fit_reply` `budget.rs:268`;
  `turn_trace` exempt by design. Reply blocks `replier.rs:47` (`render_reference`), `:79`
  (`render_task`); anything new must also enter `ground::reference_parts` (`ground.rs:16`).
- `select_facts` `turn.rs:456-477`, `pinned_facts` `:439-454`; both stores' `search_facts`
  call `nscore::lexical_rank` (`store.rs:227`, `memory-sqlite/lib.rs:668`): token hits, ties
  by `last_validated`. `Fact` `action.rs:206` has `uses`, `last_used` (bumped only on the reply
  path, `turn.rs:2109-2114`), identity `(scope, key, valid_from)`.
- `ContextManifest` `usage.rs:66-111` (`fact_keys` in render order; `guidance` a count) on
  `EventKind::ModelCall` (`event.rs:86`) via `record_model_calls` (`turn.rs:833-850`); read by
  `app/src/budget.rs:225`. The summarizer records one too (`turn.rs:544-549`).
- Trace derived from the log (`trace.rs:67-133`), `fold_older_steps` `:184-245`,
  `clip_trace_line` `:293`; finish is `respond_directly` (`turn.rs:1134`).
- Grounding: `Material` flattens all parts (`ground.rs:42-44`); `ReplyEchoed` / `ReplyFlagged`
  `turn.rs:2176-2198`, one regeneration with `do_not_state`.
- Summarizer prompt `summarizer.rs:10` (`topic / established / open`), `render_input` `:41-61`,
  `maybe_summarize` `turn.rs:491-588`.
- Guidance `learned.rs:170, 179`, file order, no cap. Notes carry `hash` (`learned.toml`).
- Recall `recall_outcome` `turn.rs:695-815`; Deep tier pre-emptive call `:893-918`; results
  enter only as `ToolReturned` (`:891-892`).
- Pass `run_report` `pass.rs:193` (eight steps; symbolic lane `:243`, notes lane `:335`,
  apply `:434`); miner `mine.rs:373`, `SignatureKind::lane()` `:127`; `verify_note`
  `notes.rs:212`; `consolidate_facts` `consolidate.rs:103`; `write_session_digests` `:32`;
  ledger `numbers` free-form (`ledger.rs:24`); `CandidateReport.lane` open string (`pass.rs:71`).
- Eval `Harness::run` `eval.rs:752` (fresh engine per turn over a shared `InMemoryStore`),
  `desktop: bool` `:687` as the flag precedent, `Ability` `:1036`; arm precedent
  `paraphrase.rs:224`; CLI `app/src/eval.rs:303`; grading `grading.rs`, κ `kappa.rs`.
- M8 status: T2.1, T2.2 (minus `ClientEvaluator`), T2.3, T2.3b, T2.7 done; T2.3a, T2.4,
  T2.5, T2.6, T2.8 open; Phase 3 all open with the trigger fired (bm25 miss 83%, vector→rerank
  25%); held-out κ symbolic 0.501, local 0.628. Local client `crates/evolution/src/local.rs`;
  `[models]` `config.rs:57`; no embeddings table anywhere.
- Store: trait `MemoryStore` `traits.rs:277`, four impls; migrations by `table_info` sniff
  (`memory-sqlite/lib.rs:245-292`); `search_turns` bm25 only (`:425-459`); conformance inlined
  at `store.rs:276` and `lib.rs:826`. `TurnRecord` `memory.rs:11` never persisted. `Usage`
  `usage.rs:15`; `tools_tokens` is the precedent for a new field. `MemorySection`
  `config.rs:279`, defaults as free fns, validated post-parse, mirrored into `TurnConfig`
  `turn.rs:49-52, 146-148`.

## Results

### P0 — done 2026-09-11 (commits `72048d9`, `d0bc08f`, `b40f8bd`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T0.1 | fixture `cached_tokens = 2048` → `Usage.cached_tokens == 2048`; absent → 0 | met; seven `Usage` literals updated (two in `turn_loop.rs` the plan had not listed) |
| T0.2 | `ns-app budget` prints `cached`; estimated calls contribute 0 | met; footer `cached: X of Y prompt tokens on measured calls (Z%)` |
| T0.3 | old manifests round-trip; `note_hashes.len() == guidance` | met; plus `facts_chars/summary_chars/window_chars`, `ablated`, and `EngineConfig.ablate` (blanks after fitting, budget report unchanged) |
| T0.4 | per-ability table for full and ablated arms, exit 0 | met. **Without `facts`: 9/9 → 4/9** (information extraction, multi-session reasoning, temporal reasoning, knowledge updates, selective forgetting fail; abstention and the three desktop tasks hold). **`summary` and `guidance`: 9/9 → 9/9, not measurable** — the scripted suite carries no summary and no notes; fixtures that do are a follow-up before T5.2's arm or T3.3 can read anything |
| T0.5 | a stable-prefix number per role | footer built and unit-tested (emitter median/max from block sizes, replier includes the real persona from config). On the recorded `cli` session it is not reached: that session predates `ModelCall` entirely and takes the reconstruction path. **First real reading comes from P2's smoke run.** |

Deviations recorded: `app/src/main.rs` gained `ablate: None` (exhaustive `EngineConfig`
literal) and the `--ablate` dispatch; `Harness::new()`/`desktop()` were replaced by
`Harness::ablating(..)`/`desktop_ablating(..)` once nothing called the old ones. rustfmt
collateral in `paraphrase.rs` was reverted; `turn.rs`, `config.rs`, `budget.rs` keep their
pre-existing format diffs.

### P1 — done 2026-09-11 (commit `0869bc8`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T1.1 | grade → stop the service → replay: verdicts byte-identical; fold unchanged | met: `grading_a_session_then_replaying_it_yields_identical_grades_without_a_service` grades with a stoppable scorer, flips it to Unavailable, re-runs — `Graded` JSON identical, `verify_chain` ok, `fold` and `replay::normalize` byte-equal. `Grade { ok, issues: Vec<String> }` lives in core; `Graded` is infrastructure in `replay.rs:48/:61`, `state.rs:204`, `mine.rs:327` |
| T1.2 | reply-quality candidate Unverified without a graded turn; emitter-side unchanged | met: `NoteGate { require_graded, authoritative }`; `SignatureKind::from_grades()` = UserReask, UngroundedReply, IgnoredQuestion, IgnoredRequest; an ungraded reply-quality candidate costs zero probe budget |
| T1.3 | second `evolve --dry-run` grades 0 new | met on a copy of `ns-run/ns.sqlite`: run 1 "20 newly graded, 0 read back", run 2 "0 newly graded, 20 read back", chain verifying. `[models] evaluate_budget_turns` default 40 |
| exit | κ per evaluator, ≥10 signatures, zero requests | signatures 12 (FallbackReply 3, IgnoredQuestion 2, UserReask 7), unchanged from M8's numbers; **zero HTTP requests** (only the refused loopback to ns-pointerd during tool registration, before the pass). **κ not printed**: `build_pass` constructs only `SymbolicEvaluator`; `[models] enabled = true` dials nothing because `LocalEvaluator` is not added via `with_evaluator` yet. That wiring is M8 T2.7's gate, deferred by this plan — recorded as the one open piece of the M8 exit line |

Deviation recorded: the replay test lives in `crates/evolution/src/pass.rs` (there is no
`crates/engine/tests/replay.rs`, and the test needs `ScriptedEvaluator` and
`EvolutionPass`, which sit above the engine).

### P2 — done 2026-09-11 (commit `1d6db32` + this record), **4 requests** (the smoke turn)

| Task | Exit criterion | Measured |
|---|---|---|
| T2.1 | interceptor fires at most once; block dropped after the window, before facts; no new event, no migration | met: `obligations_for` (en+cs, 26+32 imperative openers, question clauses), block above `Recent turns:`, `ContextManifest.obligations`, grounding material; `obligation_check` default **false**; the overlap predicate moved down from the evaluator into `nscore::addresses` (one definition) |
| T2.2 | 12 notes → 6 rendered, the rest reported dropped | met: `clamp_guidance` runs first in both fits, guidance now counts toward the total. **Only under `budget_mode = enforce`**; in the default `report` mode the clamp is reported, not made — the crate's report-before-enforce rule |
| T2.3 | breakpoint only if the stable prefix ≥ 1,024 tokens | **not applicable on this config**: emitter prefix 538 tokens (facts+summary+window), replier 225 tokens (persona+facts+summary). No breakpoint moved; `cached: 0 of 5696` confirms the persona-only breakpoint hits nothing. Caching stays a lever for a paid tier with a bigger window, as §3.6 said |
| T2.4 | one live turn, then `ns-app budget cli` | turn 21 on the live `cli` session, question "what did we talk about last time, and what is my name?": **4 requests** (emitter 2, replier 2 — the second replier call is one grounding regeneration), prompt 5,696 tokens, peak 1,601, completion 1,441, trace 2,238 chars sent, 0 clipped. **Tool schemas: 1,462 of 3,186 prompt tokens on the two emitter calls — 45.9%**, against the composition doc's 13–20% estimate. The pointer agent was down, so the legal set was the chat set; with desktop tools registered the share is higher still |

**The number that reorders the rest of the plan:** nearly half of every emitter prompt on
this box is tool schema. Slimming the schemas (`2026-09-08-tool-loading` §1.5) is the
largest context lever measured so far and is not an M9 task; it goes on the follow-up list
ahead of any retrieval work. HiAgent's trace boundary stays measure-first: 2,238 trace chars
on a two-iteration chat turn says nothing about a twelve-iteration desktop turn.

Follow-ups recorded, not built: schema slimming with `tools_tokens` as the before/after;
fixtures carrying a summary and notes so `--ablate summary|guidance` can read anything;
the reply regeneration cost (a second replier request on a plain chat turn) as a signature
for the pass to count.

### P3 — done 2026-09-11, 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T3.1 | at `w = 0.0` output byte-identical to today's | met: `Activation { weight, half_life_days, now }`, `bonus()` returns a hard `0.0` at weight 0 (also rules out NaN from a zero half-life); zero-hit filter runs before the bonus; `activation_freq(f) = f.uses` is the one line P4 flips to `credits` |
| T3.2 | `search_turns` order unchanged at weight 0 | met in both stores; candidates `max(4k, 20)`, `RECENCY_HALF_LIFE_TURNS = 20`, trait signature untouched via `with_activation` builders. Finding: in `InMemoryStore` scores are whole token counts, so a fractional recency term can only confirm the newest-first tiebreak; it reorders only against fractional bm25 (SQLite) |
| T3.3 | `w` moves off 0 only if the verbatim arm holds | **suites insensitive**: at `w ∈ {0, 0.5, 1.0}` paraphrase verbatim miss 0%/0%, paraphrase-arm miss 75% (in-mem) / 83% (SQLite), ablate-facts 9/9 → 4/9 — identical lists at every weight. The verbatim arm does not regress, and nothing is evidence *for* moving. **Default stays 0.0.** A corpus where recall targets tie on hits is the follow-up before this knob can be decided |

`ns-app eval --activation <w>` exists for that day.

### P4 — done 2026-09-11, 0 requests

Two decisions taken at build time, both stricter than the plan text: **counters are derived,
never incremented** — each pass recomputes `exposures` and `credits` for every current fact
and note from the whole store and *sets* them, so the join is idempotent and auditable and
pre-M9 sessions contribute zero; and **setting a counter never supersedes a version** — a
`set_fact_fitness` trait method with a default body updates the current version in place
(one UPDATE on the primary key in SQLite). A new-value fact version starts at 0/0; the
restatement arm keeps the counters (inheriting would credit "Peter" for calls that showed
"Martin"). Per-note counters live in a `fitness` map in the ledger, not in `numbers`, because
hand-written notes have no entry.

| Task | Exit criterion | Measured |
|---|---|---|
| T4.1 | a pre-M9 `.sqlite` opens and reports 0/0 | met (`a_pre_m9_database_opens_and_reports_zero_exposures_and_credits`); `ALTER TABLE … ADD COLUMN` via the `table_info` sniff; `activation_freq` flipped to `credits` |
| T4.2 | `sources` names the fact key | met: `reference_parts` → `(id, text)` with `persona`, `trace`, `fact:<key>`, `summary`, `window:<turn>`, `guidance:<hash>`, `obligations`; `Material` text byte-identical; `ReplyCited` emitted on the final draft of all three grounding exits, infrastructure everywhere `Graded` is |
| T4.3 | counters match a hand-computed fixture; `Σ exposures == 0` on a graded session is an alarm | met (`crates/evolution/src/fitness.rs`, six tests incl. idempotence and the alarm) |
| T4.4 | knob off → reported, store unchanged; pinned never demoted | met: `[memory] fitness_min_exposures = 8`, `fitness_demote = false`; pinned prefixes threaded into `PassConfig` and excluded before the branch |
| T4.5 | numbers readable without opening SQLite | met: `fitness: N facts derived, M with exposures, Z zero-credit`, top-10 zero-credit, would-demote count + knob state, the alarm line, per-note `hash lift exposures credits` |

**First reading, on a copy of the live log (21 turns):** `fitness: 0 facts derived … alarm:
graded session cli exposes no fact` — true, not a bug: turn 21's four manifests carry
`fact_keys: []` because **the live store holds no facts at all** (which is also why the
smoke reply said it did not know the user's name). The one note, `sha256:contextmenu`,
scores **2 exposures, 2 credits** (turn 21 graded ok by `symbolic`); a non-dry pass persists
`"fitness": {"sha256:contextmenu": {"exposures": 2, "credits": 2}}` and a following dry run
re-derives it identically with `0 newly graded, 21 read back`.

Caveat recorded: the manifest carries no scope, so counters are keyed by fact key and applied
to whichever scope's current version holds it — exact on this deployment (every session maps
to `global`); a multi-scope channel needs the scope in the manifest before `fitness_demote`
may be turned on there. **Lifted by follow-up 7, recorded after the status below.**

### P5 — done 2026-09-11 (T5.1, T5.2), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T5.1 | no new accept path; candidates show lift and regression like any note | met: `SignatureKind::Succeeded { actions, times }`, mined **cross-session** in a step after grading (mining beside step 4 would see no grades until a second pass — hit on the first dry run and moved), requires a recorded `Graded{ok}` by the authoritative evaluator and the same action sequence in ≥ 2 graded-ok turns, `recall` and `inspect_result` excluded from sequences as bookkeeping; `Ask::Strategy` phrases the proposer for a reusable strategy, `verify_note` untouched; `a_succeeded_candidate_takes_the_same_gate_as_a_failure_candidate` asserts the verdict arithmetic is identical |
| T5.2 | guidelines stay only if the arm's delta ≥ 0 | knob and render built (`[memory] summary_guidelines`, `CloudSummarizer::with_guidelines`, empty list → prompt byte-identical); **unmeasured and shipped empty** — `--ablate summary` is blind on the scripted suite (T0.4) |
| T5.3 | default 0 until `--ablate` shows a gain | **not built**: blocked on M8 T3.1, no embeddings table, and a turn must never block on an embedding |
| HiAgent | build only if `trace_lines` median > 8 on a real desktop session | **not built**: turn 21's manifests report `trace_lines` 1, 3, 6, 6 (median 4.5, max 6) — a chat turn; the desktop trigger is still unevaluated |

**Reading on the recorded session** (dry run, no key, zero requests): `Succeeded: 2 (turns 6,
7)` — both ran the single-action sequence `pointer_ui_read` and both graded ok, so
`times = 2`; the failure signatures are unchanged (FallbackReply 3, IgnoredQuestion 2,
UserReask 7) plus one `UngroundedReply` on the new turn 21. `candidates: 0` because the notes
lane needs a key; the first keyed pass will propose from those two and gate them like any
failure note.

### Status after P0–P5

Built and green across the workspace, one live turn spent (4 requests, against the plan's
2–3: one grounding regeneration). Every knob defaults to today's behaviour:
`activation_weight = 0`, `fitness_demote = false`, `obligation_check = false`,
`summary_guidelines = []`, `guidance_max` and `obligations_max` biting only under
`budget_mode = enforce`. What changed for a user of an old config: three new columns and
three footer lines in `ns-app budget`, `Graded` and `ReplyCited` events in the log after the
next idle pass, and a fitness report in `ns-app evolve --dry-run`.

Follow-ups, in the order the numbers rank them: (1) slim the tool schemas — 45.9% of the
emitter prompt — with `tools_tokens` as the before/after; (2) wire `LocalEvaluator` into
`build_pass` so κ per evaluator prints (M8 T2.7); (3) fixtures carrying a summary and notes
so `--ablate summary|guidance` and T5.2 can be read; (4) a recall corpus whose targets tie on
hits so `activation_weight` can be decided; (5) M8 T3.1 embeddings, which unblocks T5.3;
(6) a desktop-session `trace_lines` reading for the HiAgent trigger; (7) the scope in the
manifest before `fitness_demote` is enabled on a multi-scope channel — **done below**, after
M10 and M11 had closed (1), (2), (4) and (5) and parked (6) with the desktop line.

### Follow-up 7 — done 2026-09-11, 0 requests

The scope rides in the manifest, so the fitness join is exact on a multi-scope channel.
`ContextManifest.scope: Option<String>` (`crates/core/src/usage.rs`) names the scope the
call's session maps to — `EngineConfig::scope_for(sid)`: `global` on the CLI, one per
session under `ns-app serve` — and every builder fills it: `emitter_manifest` and
`reply_manifest` (`crates/engine/src/trace.rs`) take it as their first argument, and the
summarizer's inline manifest in `maybe_summarize` sets it. Serialized **absent when `None`**,
the M10 T0.1 rule: a manifest read from an old log re-serializes to its recorded bytes and the
chain still hashes. `fitness::derive` keys exposures and credits by `(manifest.scope, key)`;
a fact in scope S reads the `(Some(S), key)` count plus the `(None, key)` count, so a manifest
written before the field — never backfilled — still counts in every scope holding the key,
which is what the join did before and exact wherever every session maps to one scope.
`ReplyCited` sources stay `fact:<key>`: a session maps to one scope, so the manifest's scope
covers the citation too.

| Exit criterion | Measured |
|---|---|
| an old manifest parses with no scope and re-serializes without one | met: `usage.rs` `an_old_manifest_without_a_scope_parses_as_none_and_serializes_to_nothing` |
| every call of a turn, on all three roles, names the session's scope | met: `turn_loop.rs` `every_manifest_of_a_turn_names_the_session_scope` (`scope_for = sid`, `window_turns = 0` so the summarizer fires inside the same test); `the_default_engine_names_the_global_scope` — the CLI records `global`, never nothing, so a manifest written today never takes the fallback path |
| a key held in two scopes is credited only where the manifest says | met: `fitness.rs` `a_key_held_in_two_scopes_is_credited_only_in_the_scope_the_manifest_names` (`a`: 1/1, `b`: 0/0) |
| a scope-less manifest counts as before | met: `a_manifest_without_a_scope_still_counts_in_every_scope_holding_the_key` (both 1/1); the six P4 fitness tests pass unchanged on that path |

Workspace suite: 706 passed, 0 failed, 1 ignored. The exhaustive `ContextManifest` literals
elsewhere (`mine.rs`, `pass.rs`, `evaluate.rs`) build on `..Default::default()` and compiled
untouched. The P4 caveat is lifted; `fitness_demote` stays `false` under the release rule.
On the recorded `cli` log nothing moves: its old manifests take the fallback path and every
new one carries `global`, so the next dry run derives the numbers the last one did.
