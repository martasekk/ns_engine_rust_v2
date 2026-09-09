# M8 — The evaluation lane, paid for locally

**Status:** plan, 2026-09-08. Written against `m7-context-budget` @ `8d18018` (20 commits ahead
of `main` @ `a478d46`; `main` is fully contained in it, so the merge is a fast-forward).

**Evidence base:** `docs/research/2026-09-08-local-retrieval-and-lane-findings.md` (the
sweep and the Temporal/judge reading behind §5 and §6),
`docs/research/2026-09-07-context-budget-findings.md` (the 2026 brief),
`docs/research/2026-09-02-memory-findings.md`, and the measured numbers in
`~/models` (nsmodels), a CPU-only local model service on `127.0.0.1:7374`.

**Parent specs:** M6 `docs/superpowers/specs/2026-09-02-memory-redesign-design.md` — Phases 0–4
built and merged; §11 Phases 5–7 nominally open. M7
`docs/superpowers/plans/2026-09-07-m7-context-budget.md` — every phase built, §13 records four
residuals.

**Recorded decisions this plan respects:** emitter escalation discarded (`2026-09-01-findings.md`
§6); the verbatim window is never consolidated (`2026-09-04-reply-entrainment.md` §9); a check
becomes a gate only after a measured true-positive rate (same, §8); **no model in the guard chain
and no model-decided applies** (M6 §13); vector search stays deferred until its trigger fires
(M6 §12.8); Temporal not adopted (M7 §3).

---

## 1. Where the work actually stands

Two corrections to the state the previous session's notes carried:

- **M7 is built, not planned.** All six phases are on `m7-context-budget`, 461 tests, clippy
  clean, `ns-app eval` reports 9/9 abilities passing offline. It is unmerged, and it carries
  `messages-client` underneath it. Nothing in this plan starts before that merge.
- **M6 Phases 6 and 7 were delivered by M7 under different names.** Phase 6 asked for `ModelCall`
  events with a context manifest and `Retry-After` cooldown: M7 T0.1 (`93bf508`) built the first,
  `messages-client` (`8837c33`) the second. Phase 7 asked for a six-ability memory suite run as
  harness replays with tools: that is `ns-app eval` (`f77e404`), which runs all six and three
  desktop abilities besides.

So the honestly open list is shorter than "Phases 5–7", and it is this:

| Open item | Source | Blocked on |
|---|---|---|
| M6 Phase 5 — evaluation lane, observations, κ calibration | M6 §8, §5.2 | a judge the free tier can afford |
| §12.8 vector trigger — never measured | M6 §12.8, Phase 7 exit | a paraphrase arm in the suite |
| Config pass — `tool_result_max_chars`, `trace_verbatim_lines` unreadable from `config.toml` | M7 §13 | nothing |
| `budget_mode = "enforce"` still off | M7 §13 | a no-impact rate over real sessions |
| Three new signatures are notes, not symbolic candidates | M7 §13 | `Patch` growing variants |
| `ns-app eval --live` refused; `eval_request_cap` unbuilt | M7 §13 | a seam; `ns-engine` does not depend on `ns-llm` |
| Scope digest | M7 §13 | its trigger (unchanged) |

The first two are the substance. The rest are finishing.

## 2. The constraint that shapes everything, and what changed

`openrouter/free` allows ~50 requests a day (`crates/llm/src/provider.rs`). M7 spent its whole
design budget on that number: clip, fold, fit, route, digest — all of it is "fewer and smaller
requests". M6 Phase 5 asks for the opposite: a **judge model call per graded turn**, plus a
`reply_probe` that calls the replier again. On fifty requests a day, an evaluation lane that
grades ten turns costs a fifth of the day and competes with the work being graded. That is why
Phase 5 has sat open since 2026-09-02, and no amount of prompt engineering fixes it.

**What changed on 2026-09-08:** `~/models` exists. Measured on this box, CPU-only, no CUDA:
e5-small at **337 texts/s**; a cross-encoder reranker; both served warm on `127.0.0.1:7374`
(`/health`, `/embed`, `/rerank`, `/vision`, `/ocr`) because a cold load is ~30 s and a
per-call subprocess is a thousand times the cost of a warm request. Its defaults are
multilingual by construction — engine traffic is Czech, and English-only encoders score it near
randomly.

The consequence for this plan: **the evaluation lane's marginal request cost can be zero.** Not
because a 33M-parameter encoder is a good judge — it is not, and this plan never treats it as
one — but because two of Phase 5's three jobs are *ranking and matching*, not judging, and those
are exactly what an encoder does well.

Two limits, stated up front because they bound every phase below:

- **nsmodels is Python on a port; the engine is Rust.** Every consumer built so far
  (`adgen-eval::local_vision`, `ns-pointerd`'s OCR tools) degrades rather than fails when the
  server is down. This plan does the same: off by default, feature-gated, and a `/health` miss
  is a skipped signal, never an error.
- **M6 §13 is not relaxed.** "No model in the guard chain, and no model-decided applies" does not
  acquire an exception because the model is local and free. Local output enters through the
  evaluation lane as **signals**, and becomes a gate only on a measured true-positive rate — the
  same bar every other check has cleared.

## 3. Phase 0 — Merge and finish M7 (½ day)

| Task | Files | Test |
|---|---|---|
| T0.1 fast-forward `main` to `m7-context-budget`; retire `messages-client` and `worktree-suspended-backoff` as contained | git only | `git merge-base --is-ancestor` for both; `cargo test --workspace` and `cargo clippy -- -D warnings` green on `main` |
| T0.2 config pass: `[memory] tool_result_max_chars`, `trace_verbatim_lines` readable from `config.toml`, defaults 1200 / 5 | `app/src/config.rs` | `full_config_parses()` grows both keys; absent keys keep the plan defaults |
| T0.3 `[models]` section: `enabled` (default `false`), `base_url` (default `http://127.0.0.1:7374`), `timeout_ms` (default 2000) | `app/src/config.rs` | absent section parses; `enabled = true` with no server reachable is a startup warning, not a failure |

M7 deferred T0.2 "to avoid two writers in `app/`". There is one writer now.

Exit: `main` builds and tests clean with every M7 phase on it, and both budget knobs are settable
without a rebuild.

## 4. Phase 1 — The paraphrase arm, and the measurement M6 has owed since September 2 (1 day)

M6 §12.8 says: stay lexical; add embeddings when the Phase 7 suite shows `recall` **miss rate
above 20% on paraphrased queries**. The suite now exists and nobody has run that arm — the six
memory abilities all ask in the words the fact was stated in. FTS5 bm25 over `events_fts` and
`session_digests_fts` cannot miss what it was handed verbatim, so the current 9/9 is silent about
the only question the trigger asks.

| Task | Files | Test |
|---|---|---|
| T1.1 paraphrase fixtures: for each of the six memory abilities, a second query form that shares intent and not vocabulary, Czech and English | `crates/engine/tests/memory_eval/` | each fixture's paraphrase shares ≤ 30% content words with the stated form, asserted in the fixture loader so a lazy paraphrase fails the test, not the run |
| T1.2 `ns-app eval --paraphrase` runs the arm and reports `recall` hit/miss per ability beside the verbatim arm | `crates/engine/src/eval.rs`, `app/src/main.rs` | the two arms report separately; the ledger row carries both |
| T1.3 offline lexical baseline: miss rate with today's bm25, no models involved | same | a number, written into this document's §9 |

**This phase deliberately builds no embeddings.** It builds the instrument that decides whether
embeddings are allowed. If the lexical miss rate lands at or under 20%, M6 §12.8 says stay
lexical, Phase 3 below is not built, and that is a real outcome worth a day.

Exit: `ns-app eval --paraphrase` prints a paraphrased-recall miss rate per ability and a total,
offline, spending no requests. The total is the §12.8 trigger, evaluated for the first time.

## 5. Phase 2 — The evaluation lane, with a local scorer (2–3 days)

M6 Phase 5's tasks, with one substitution at the judge — and, since
`2026-09-08-local-retrieval-and-lane-findings.md` §3, **a gate in the middle rather than one
straight run**.

**T2.1 ships and is measured before anything else in this phase is built.** M6's exit
criterion for Phase 5 is "`ns-app evolve --dry-run` reports ≥ 10 `UserReask` / `BadReply`
signatures where today it reports zero" — and `UserReask` is a *symbolic* signature. It may
already be met with no model in the lane at all. Finding that out first costs half a day and
decides whether the rest of this phase is the point or the enrichment; it also gives κ
something real to be computed against, which it would not have if the checks and the scorer
arrived together. The 2026 reliability literature pushes the same way: judges are unstable
on re-ask, and simple deterministic scorers can beat them on exactly the axis an evolution
gate needs.

| Task | Files | Test |
|---|---|---|
| T2.1 symbolic checks — re-ask, ungrounded reply, question-ignored. **No model, always on** | `crates/evolution/src/evaluate.rs` (new) | M6's own criterion: the recorded session yields `UserReask` for turns 52–60, 67, 90, 100, 105 and `UngroundedReply` for turn 69 |
| T2.2 `Evaluator` trait; `ScriptedEvaluator` (tests); `ClientEvaluator` (the paid judge, unchanged from M6 §8.3) | `evaluate.rs` | fence-tolerant parse; invalid JSON → `Err`, counted unverified |
| T2.3 `LocalEvaluator` over `/embed` + `/rerank`: it scores *similarity and ranking*, never a verdict — reply-vs-question relevance, reply-vs-shown-facts grounding, re-ask clustering | `evaluate.rs`, `crates/evolution/src/local.rs` (new) | server down → every signal `Unavailable`, pass completes, nothing counted; scores are ranks, never thresholds (nsmodels' own finding: e5-small's cosine range is compressed, true match 0.898 vs unrelated 0.862) |
| **T2.3a a grade is a recorded value.** The score, the scorer's id and its model revision are written into the log as one event; no replay path may call a model | `core/src/event.rs`, `evaluate.rs`, `pass.rs` | **grade a session, stop the service, replay — the grades are identical.** See below |
| **T2.3b the retry table**, so a refused connection is not retried like a timeout | `crates/evolution/src/local.rs` | connection refused disables the lane for the rest of the pass after one attempt; a timeout or 5xx gets exactly one retry; malformed JSON is `Err` and counted unverified, never `Unavailable` |
| T2.4 `observations` table + FTS5; observer writes from grades; dedupe; origin + trust; `relevance_count` on recall | `crates/memory-sqlite`, `crates/core/src/traits.rs`, `crates/evolution/src/pass.rs` | same turn observed twice writes once; External origin yields External trust |
| T2.5 new signatures, reply-scope notes, `guidance_for_reply`, `ReplyContext.guidance` | `evolution/src/mine.rs`, `core/src/learned.rs`, `llm/src/replier.rs` | a reply note renders after the current-turn block |
| T2.6 gate: `TurnOutcome::Graded`; live-replier probe behind `reply_probe`; Unverified path when off | `evolution/src/notes.rs`, `pass.rs` | with `reply_probe = false` a reply-scope candidate is Unverified and listed |
| T2.7 **[R2]** calibration: Cohen's κ of each evaluator against the symbolic proxies, 5% test–retest, `evaluator_min_kappa` | `pass.rs`, `ledger.rs` | κ from a table-driven contingency; below threshold → observations only, no candidates |
| T2.8 budget `evaluate_budget_turns`; graded turns remembered | `pass.rs` | a second run grades nothing new |

**Why the local scorer is admissible here and nowhere else.** T2.7 is the whole argument: κ is
computed for whichever evaluator ran, against the symbolic checks that need no model at all. A
local scorer that cannot agree with the proxies produces observations and no candidates —
exactly what a paid judge below threshold produces. The lane's contract does not change; only the
bill does. And the three jobs T2.3 takes are ranking jobs by their nature: "is this reply about
the question that was asked", "does this reply's content appear in what the prompt showed it",
"is this turn a re-ask of that one". None of them is a verdict.

**T2.3a is Temporal's `SideEffect`, and it prevents a real bug.** Temporal executes
non-deterministic work once, records the result in the event history, and returns the
recorded result on replay rather than re-executing. This engine has the same replay property
and the same exposure: `fold()` is the replay, and `verify_patch` replays recorded sessions
to decide whether a symbolic candidate earns its verdict. If a grade were recomputed by
calling a model at replay time, then replay would need a Python service running to reproduce
a Rust session; a weight change would silently rewrite history; and the same patch could pass
and then fail with no code change, which empties a ledger verdict of meaning. Recording the
scorer's identity beside the score is the `MutableSideEffect` half — drift shows up in a diff
instead of disappearing into a number. Cheap now, a migration of everything already graded
later.

**What T2.3 may not do**, recorded at the code: it may not decide an apply, may not enter the
guard chain, may not run in-turn (M6 §8.7's deferral is unchanged — this is the offline pass,
during the wait for the next message), and may not produce a signature the gate accepts without
κ.

Exit: `ns-app evolve --dry-run` on the current `ns-run/ns.sqlite` reports ≥ 10 `UserReask` /
`BadReply` signatures where today it reports zero, with κ printed per evaluator — **and, with
`[models] enabled = true`, spends zero requests doing it.** That last clause is the phase's real
claim.

## 6. Phase 3 — Conditional: coarse vector → rerank, deep tier only (1–2 days, only if Phase 1 fires)

Built **only if** T1.2 reports a paraphrased-recall miss rate above 20%. If it does not, this
section stays unbuilt and §9 records the number that kept it unbuilt.

The shape is not a free choice. The brief's traversal row (findings §391) prescribes: classify
intent first, then tier and budget; coarse vector → rerank; graph traversal only for the deep
tier. M7 built the classifier and the tiers (Phase 3 router).

**The settings below are measured, not guessed**
(`2026-09-08-local-retrieval-and-lane-findings.md` §1), and they correct what the first
version of this section said:

| retrieval | paraphrase miss | ms/query (CPU) |
|---|---|---|
| sqlite fts5 bm25 — what ships | **83%** | — |
| e5-small, top-5, no rerank | 33% | ~4 |
| e5-small, coarse 48 → rerank | 17% | 1865 |
| bge-m3, top-5, no rerank | 25% | ~33 |
| **bge-m3, coarse 10 → rerank** | **8%** | **509** |
| bge-m3, coarse 20 → rerank | 17% | 1035 |
| bge-m3, coarse 48 → rerank | 17% | 1979 |

Three corrections follow, and the earlier text had all three wrong:

- *"The coarse `k` has to be generous" was an artifact of the weaker embedder.* With bge-m3,
  **wider is worse**: k=10 beats k=20, 30 and 48 and is two to four times cheaper. Every
  candidate past the tenth is another chance for the cross-encoder to promote a distractor.
  This is the caveat the fusion literature names — widening the candidate set improves mean
  nDCG while making recall@5 worse on the hardest slice.
- *Fusion is a guard, not the fix.* bm25's hits are a strict subset of the vector arm's, and
  on ten of twelve paraphrases bm25 returns nothing at all, so there is no second list to
  fuse. T3.2 stays, but its justification is that an exact-match query must never lose to a
  semantic near-miss — and if RRF is used, its constant is k ∈ [1, 10], not the customary 60,
  which is tuned for lists of hundreds.
- *The embedder is `--model quality` (bge-m3), not the nsmodels default.* The default is
  chosen for throughput; this is a recall problem.

**And what is honest to claim from twelve cases:** the gap between 8% and 17% is one case,
inside the noise of a set this size. What survives a single case flipping is the direction —
bm25 at 83% against anything embedding-based at ≤33%, bge-m3 dominating e5-small at every
`k`, and coarse-10 being at least as good as wider while costing a fraction. The precise
optimum is not established and must not be hard-coded as though it were: `coarse_k` is a
config key with a default of 10 and a measurement behind it, not a constant.

So:

| Task | Files | Test |
|---|---|---|
| T3.1 `embeddings` table keyed by event rowid; backfill in the idle pass, never in a turn; the embedder is bge-m3 | `crates/memory-sqlite/src/lib.rs`, `evolution/src/pass.rs` | backfill is resumable; a turn never blocks on an embedding; a stored vector records which model produced it, so a model change invalidates rather than silently mixes |
| T3.2 hybrid `search_turns`: bm25 candidates ∪ vector candidates, fused by rank (not score), then `/rerank` over the union, top-k | `memory-sqlite/src/lib.rs` | with the server down, results are exactly today's bm25 results — same order, same count |
| T3.3 the hybrid path is reachable **only from the `Task`/deep tier**; `Chat` recall stays lexical | `crates/engine/src/router.rs`, `turn.rs` | a `Chat` turn issues no `/embed` call |
| T3.4 re-run `ns-app eval --paraphrase`; the miss rate is the exit criterion | `eval.rs` | miss rate below 20%, with the verbatim arm not regressed |
| T3.5 `[recall] coarse_k` (default 10) and the rerank latency budget | `app/src/config.rs`, `router.rs` | a deep-tier recall that would exceed the budget falls back to lexical rather than holding the turn |

Rank fusion rather than score fusion is deliberate and is nsmodels' own measured finding: e5-small's
cosine range is compressed enough that a threshold is meaningless, so **rank, never threshold**.

**Graph memory stays unbuilt regardless.** M6 §13's reason is unchanged: it did not close the
fidelity gap against verbatim text in the controlled ablation, and it needs state labels to avoid
ghost memory anyway. A vector index is not a graph, and firing §12.8 does not fire that trigger.

Exit: paraphrased miss rate under 20% with no regression in the verbatim arm, or the phase is
reverted and the attempt recorded.

## 7. Phase 4 — Finish the M7 residuals (1 day)

| Task | Files | Test |
|---|---|---|
| T4.1 `Patch` grows the two variants the M7 signatures need — a per-action `tool_result_max_chars` and a router cue — so `verify_patch` can replay against them | `crates/evolution/src/pass.rs`, `core/src/learned.rs` | a mined `ResultClippedThenInspected` signature becomes a symbolic candidate and survives replay verification |
| T4.2 `budget_mode = "enforce"` decision: run `ns-app budget` over every recorded session, report the would-drop and no-impact rates, and turn it on or record why not | `app/src/main.rs`, this document §9 | a number, and a decision written down |
| T4.3 `ns-app eval --live` seam: an `Evaluator`/emitter injection point so the task set can be driven by `ns-llm` from `app/`, with `eval_request_cap` enforced | `app/src/main.rs`, `engine/src/eval.rs` | `--live` without a key still refuses; with a cap of 5 it stops at 5 |

T4.3 is the one M7 called structurally blocked — "`ns-engine` does not depend on `ns-llm`". It is
blocked at that layer and not at `app/`, which depends on both; the seam belongs there, and this
is the first task that has needed it.

## 8. Order, configuration, metrics

**Order.** Phase 0 → Phase 1 → Phase 2 → (Phase 3 only if Phase 1 fires) → Phase 4. Phase 4 is
independent of 1–3 and can be pulled forward into a gap. Phase 1 gates Phase 3 by design; do not
build Phase 3 speculatively while Phase 1 is unrun.

**New configuration**, all defaulting to today's behaviour:

```toml
[memory]
tool_result_max_chars = 1200     # T0.2, was an EngineConfig field only
trace_verbatim_lines  = 5        # T0.2, likewise

[models]                          # T0.3 — the local lane
enabled    = false               # off by default; a /health miss degrades, never fails
base_url   = "http://127.0.0.1:7374"
timeout_ms = 2000

[evolution]
evaluator            = "symbolic" # symbolic | local | client
evaluator_min_kappa  = 0.4        # T2.7; below it, observations only
evaluate_budget_turns = 20        # T2.8
reply_probe          = false      # unchanged from M6

[recall]
hybrid = false                    # T3.2, and only ever read on the deep tier
```

**Metrics added to the ledger row:** paraphrased-recall miss rate per ability (Phase 1);
κ per evaluator and the count of unverified grades (Phase 2); requests spent by the pass, which
should read zero with the local evaluator (Phase 2); `/embed` and `/rerank` latency and
unavailability counts (Phase 3); would-drop and no-impact rates (T4.2).

## 9. Risks, and what is deliberately not built

- **A local scorer that agrees with nothing.** Then κ is below threshold, the lane produces
  observations and no candidates, and Phase 2 has still delivered the symbolic checks, the
  observations table and the calibration machinery — everything except a free judge. The paid
  `ClientEvaluator` remains behind the same trait.
- **The multilingual encoder scores Czech engine traffic badly.** nsmodels' defaults are
  multilingual precisely because of this, and the paraphrase fixtures (T1.1) are Czech and
  English both, so the arm measures it rather than assuming it.
- **The server is down when the pass runs.** Every signal reports `Unavailable`, the pass
  completes, the ledger records the gap. This is the behaviour `adgen-eval::local_vision` and
  `ns-pointerd` already have, and it is why they are usable.
- **A vector index becomes a second source of truth.** T3.2's guard is that with the server down
  the results are byte-identical to today's bm25 — the index is an *addition* to the candidate
  set, never a replacement for it.
- **Two writers in `app/`.** T0.2 and T0.3 touch the same file; they are one task in practice and
  are ordered before anything else for that reason.

**Not built, with the reason:** graph memory (M6 §13, trigger unchanged and not fired by §12.8);
a consolidated window (plan 2026-09-04 §9); a model in the guard chain, in the router, or in the
fold (M6 §8.7, M7 §12); in-turn model reply checks (M6 §12 item 5, still awaiting these numbers);
emitter escalation (`2026-09-01-findings.md` §6, and on the free tier there is no second model);
the scope digest (M7 §13, trigger unchanged); any RL or fine-tuning (M6 §13); Temporal (M7 §3);
sd-turbo, ASR and TTS from nsmodels — the engine has no use for them and this plan adds no
capability it cannot measure.

## 10. Status

- 2026-09-08: plan written against `m7-context-budget` @ `8d18018`. Nothing built yet; T0.1
  (the merge) is the first step. The two numbers this plan exists to produce — the paraphrased
  recall miss rate (§4) and κ for a local evaluator (§5) — are both unmeasured today.

- 2026-09-08, later: **Phase 0 built.** Branch `worktree-m8-plan`, based on
  `m7-context-budget` — the merge in T0.1 is a fast-forward and is left for a human to push,
  so this branch carries M7 forward rather than re-landing it.

  | Task | State | Note |
  |---|---|---|
  | T0.1 verify the merged tip | done | 465 tests pass, clippy clean with `-D warnings` |
  | T0.2 `tool_result_max_chars` settable | done | see the deviation below |
  | T0.2 `trace_verbatim_lines` settable | **already was** | see the deviation below |
  | T0.3 `[models]` section + startup probe | done | `app/src/models.rs` |

  **Two deviations, both from M7 §13 being half right about what it had deferred:**

  - *`trace_verbatim_lines` was never unwired.* It is an `EngineConfig` field, a `[memory]`
    key with a default, and is threaded at `app/src/main.rs` — it has been settable since
    `76663d9`. M7's note recorded both knobs as deferred; only one was.
  - *`tool_result_max_chars` was not an `EngineConfig` field at all*, but a private
    `const TRACE_LINE_MAX_CHARS = 1200` in `turn.rs`, read from six places. Making it
    settable was therefore a thread-it-through change rather than a config line: the const
    survives as `pub DEFAULT_TOOL_RESULT_MAX_CHARS`, the engine's single source for the
    default, and `trace_for_prompt`, `clipped_results`, `fold_older_steps`, `fold_descriptor`
    and `render_budget` all take the cap as an argument now.

  **Behaviour-preserving, checked against the recorded session rather than argued.**
  `ns-app budget cli` on a copy of `ns-run/ns.sqlite` reproduces M7's Phase 1 rows exactly at
  the default cap, and moves when the cap moves:

  ```
  turn      window  summary    trace     sent    chars  ~tokens  tools
  t6          1026        0     9742     1314    10768     2692      1     # cap 1200, = M7
  t7          1327        0    14108     1311    15435     3858      1     # cap 1200, = M7
  t6          1026        0     9742      513    10768     2692      1     # cap 400
  t7          1327        0    14108      510    15435     3858      1     # cap 400
  ```

  The session has grown from 7 turns to 18 since M7 measured it, so only the per-turn rows
  are comparable; t6 and t7 match to the character.

  **T0.3 note.** The probe is a `/health` GET over `TcpStream` rather than the `reqwest`
  already in the tree, because a probe should fail the way a probe fails — a pooled,
  redirect-following, TLS-capable client aimed at loopback answers a slightly different
  question than "is something on that port speaking HTTP right now". It reports the loaded
  model list, so "running but loaded nothing the lane needs" reads differently from "not
  running". Verified against the live service, which answers
  `{"ok": true, "loaded": ["embed", "ocr", "vision"]}` — note **no reranker**: nsmodels
  loads it only under `--rerank`, which Phase 2's T2.3 will need and should check for by
  name rather than assume.

  Still unmeasured, unchanged: the paraphrased recall miss rate (§4) and κ (§5).

- 2026-09-08, later still: **Phase 1 built, and the §12.8 trigger has fired.**

  | Task | State | Note |
  |---|---|---|
  | T1.1 paraphrase corpus, 12 cases, cs + en | done | `crates/engine/src/paraphrase.rs` |
  | T1.2 `ns-app eval --paraphrase`, both retrievers | done | `app/src/eval.rs` |
  | T1.3 lexical baseline | done | the table below |

  **The measurement M6 has owed since September 2:**

  ```
  retriever                    verbatim     miss paraphrase     miss
  ------------------------------------------------------------------
  in-memory (token hits)         12/12        0%      3/12       75%
  sqlite (fts5 bm25)             12/12        0%      2/12       83%
  ```

  The control arm is 12/12 on both, so the paraphrase number is about vocabulary and not
  about a broken harness. **83% ≫ 20%: M6 §12.8 permits embeddings.**

  **Two deviations from §4 as written, both making the number harder rather than easier:**

  - *Both retrievers, not one.* §4 said "today's bm25". The ability suite runs on
    `InMemoryStore`, whose `search_turns` counts token hits; what ships is `SqliteStore`'s
    FTS5 `bm25`. Those are different retrievers, and a trigger decided on the one that does
    not ship would be a decision about test scaffolding. They disagree — 75% against 83% —
    which is the argument for having measured both.
  - *One pooled session, not one session per case.* The first cut gave each case its own
    session, making the target one of four candidates with `recall_top_k` at 5 — so a
    retriever that returned everything would have scored 12/12 without ranking anything.
    The number survived only because lexical search drops zero-score lines and returns
    nothing at all. That is a real failure mode, but resting the measurement on it would
    have made this arm useless for comparing anything against. All twelve cases now share
    one session; every query is asked against all 48 lines.

  The corpus polices itself: `every_paraphrase_actually_paraphrases` fails the build if a
  paraphrase shares more than 30% of its tokens with the line, **or** if a verbatim control
  shares less than 40%. Two cases were caught by it while being written.

  **What Phase 3 is now allowed to be, measured rather than assumed.** With the trigger
  fired, the obvious next question is whether a local embedder actually fixes what bm25
  missed. Probed against the same corpus and the same 48-line pool, on the live nsmodels
  service (e5-small, and the cross-encoder loaded with `serve --rerank`):

  | retrieval | paraphrase miss |
  |---|---|
  | sqlite fts5 bm25 — what ships | **83%** |
  | e5-small vectors, top-5 | 33% |
  | vector top-20 → cross-encoder rerank → top-5 | 25% |
  | cross-encoder over the whole pool → top-5 | **17%** |

  Three things follow, and they change §6:

  1. **A plain union of bm25 and vector candidates buys nothing here.** bm25's two hits are
     a strict subset of the vector arm's eight. T3.2's fusion is still right for not
     *losing* lexical exact matches, but it must not be sold as the thing that closes the
     gap.
  2. ~~**The coarse retrieval is the bottleneck, not the reranker.** At `COARSE = 20` two
     targets never reach the reranker at all; widened to the whole pool, the same reranker
     takes the miss rate to 17%. So T3.2's coarse `k` has to be generous.~~
     **Superseded the same day** by the sweep in
     `2026-09-08-local-retrieval-and-lane-findings.md` §1: this was an artifact of probing
     e5-small only. With bge-m3 the relation inverts — coarse-10 beats 20, 30 and 48, and
     costs a quarter as much. Left visible rather than deleted, because "one model's recall
     ceiling read as a property of the pipeline" is the kind of mistake worth being able to
     recognise a second time.
  3. **The two irreducible misses are both the identity question** ("who am I, remind me" /
     "kdo jsem, připomeň mi to" against a line where the user introduced themselves). Those
     are exactly the cases the engine answers from *pinned facts*, not from `recall` — M6
     pins `user.*` — so in the running system they never depend on this path. Worth stating
     plainly: the residual failure of the retriever is concentrated where the retriever is
     not what answers.

  Still unmeasured: κ for a local evaluator (§5). That is Phase 2.

- 2026-09-08, research pass before Phase 2. No code; `2026-09-08-local-retrieval-and-lane-findings.md`
  and the revisions to §5 and §6 above. Prompted by the observation that §6 was about to be
  built on a conclusion drawn from one embedder.

  **What it changed:**

  - *§6's coarse `k`.* Reversed — see the strikethrough above. bge-m3 at coarse-10 reaches
    **8% miss at ~0.5 s/query**, against bm25's 83%. The deep-tier-only rule now has a
    latency number instead of an intuition.
  - *§6's fusion.* Re-cast from "the fix" to "a guard for the verbatim arm", because bm25
    returns nothing at all on ten of twelve paraphrases and there is no second list to fuse.
  - *§5's ordering.* T2.1 — the symbolic checks, no model — is now built and measured
    **before** the rest of Phase 2, because M6's own exit criterion for Phase 5 may already
    be met without a model, and κ needs a baseline to be computed against.
  - *§5 grew two tasks.* T2.3a (a grade is a recorded value; no replay path calls a model)
    and T2.3b (the retry table). Both come from reading Temporal against the *pass* rather
    than the turn loop, which is what M7 §3 had mapped.

  **The one that would have been a bug:** without T2.3a, `verify_patch` — which replays
  recorded sessions to settle a candidate — would have been verifying against a moving
  target, and a patch could pass and later fail with no code change. Temporal's `SideEffect`
  is the same problem with a name and a fix: execute once, record the result, return the
  record on replay.

  **Not adopted, with the reason:** Temporal itself (M7 §3's trigger is unchanged, and it is
  not this deployment); heartbeats (Temporal's own guidance excludes sub-second loopback
  calls); a second-stage LLM reranker (that is the request cost this plan exists to avoid);
  fine-tuning an embedder (no labelled set — twelve adversarial cases are an instrument, not
  training data).

- 2026-09-09: **T2.1 built and measured.** Research first —
  `docs/research/2026-09-09-symbolic-evaluation-findings.md` — then
  `crates/evolution/src/evaluate.rs`, wired into the pass beside `mine`.

  | Task | State | Note |
  |---|---|---|
  | T2.1 symbolic checks, no model, always on | done | 14 unit tests; `evolve --dry-run` now names the turns |

  **The run, on a copy of `ns-run/ns.sqlite`:**

  ```
  sessions: 1 (skipped broken: 0)
  signatures:
    FallbackReply: 3 (turns 2, 4, 12)
    IgnoredQuestion: 2 (turns 13, 17)
    UserReask: 7 (turns 4, 9, 10, 17, 18, 19, 20)
  candidates: 0
  ```

  **M6's exit criterion cannot be evaluated as written, and this is the first
  thing to say.** It names "turns 52–60, 67, 90, 100, 105" of the recorded
  session, and `ns.sqlite` now holds **one session of twenty turns** — the log
  those numbers were written against was reset between M6 and now (`echo.rs`
  still cites "session `cli`, turns 137–158", which are also gone). The
  ≥ 10 threshold was set against a log five times longer than the one that
  exists. What can be said is the rate: **7 `UserReask` in 20 turns, where
  today the pass reports zero**, with no model and no request spent. On the
  session the criterion was written for that is roughly 37.

  So T2.1 answers the question §5 asked it to answer — *how much of Phase 5
  lands with nothing in the lane that costs a request* — with: most of the
  volume. What it does not deliver is `BadReply`, which is a model signature
  by definition and belongs to T2.2/T2.3.

  **Three deviations from §5's one-line description, all from the research pass:**

  - *Re-ask reports a band, not a boolean.* M6's rule is normalized equality;
    findings §1 points out that this is the Jaccard = 1.0 corner of the
    standard query-reformulation feature set, and that M8 Phase 1 already
    measured what that corner costs here (75–83% of
    same-question-different-words missed). So `UserReask` carries
    `band: Repeat | Reformulated`, and **only `Repeat` is a κ proxy** for
    T2.7 — the reformulated band is counted while its true-positive rate is
    unmeasured, which is how it gets measured.
  - *A fourth check, `IgnoredRequest`.* Higashinaka et al.'s taxonomy splits
    "ignore" into five categories; M6 specified I5 (question) and called it
    weak. I6 (request) is the one **this** harness can detect precisely,
    because the router already recorded its own judgement that the turn wanted
    an action (`ModelCall.manifest.tier`) and the log shows whether one was
    taken. No text is compared. It is the mirror of `Misrouted` out of the
    same field. It reports **0 here** — this log predates `ModelCall`
    entirely, so no turn carries a tier — and is covered by unit tests only
    until a session recorded under M7 exists.
  - *`UngroundedReply` surfaces the recorded flag; it does not re-run the
    check.* That is T2.3a's invariant arriving one task early, and here it is
    not caution but correctness: `Material` is built from the persona, the
    fact *values* and the window, and the log carries neither the persona nor
    the values. A check run against a partly reconstructed context reports
    unsupported claims whose support was merely unrecoverable. M6 §8.3 says
    "judged against exactly what the replier saw"; exactly is the word. The
    gain is real anyway — `ReplyFlagged` reached the pass only as rendered
    text for a note proposer to read, and is now a signature that can be
    counted, gated and calibrated. It reports 0 here because the log has none.

  **`IgnoredQuestion` fired twice and was wrong twice**, and the two failures
  are worth more than the check:

  - turn 13, "what time is it?" answered "It is currently 09:36:12 UTC on
    Tuesday, 2026-09-08" — a correct answer states the **value**, not the
    question's words;
  - turn 17, whose question mark is not one: it is the console's rendering of
    "koš". Czech inflection then keeps "vysyp" from matching "vysypání".

  This is `echo.rs`'s situation exactly (21 firings, zero true positives), and
  it gets `echo.rs`'s treatment: counted, note-lane, **never a proxy and never
  acting**, with the 0-for-2 recorded at the enum. It is not tuned against
  n = 2.

  **What the re-ask check missed, and why it is the right miss.** Turn 15 asks
  in Czech what turn 14 asked in English; turn 16 is turn 15 mojibake'd past
  the Jaccard threshold. Neither is reachable lexically — they are the
  paraphrase problem in its two hardest forms — and reaching them is T2.3's
  job, not this check's. Turn 20 is the one arguable firing: it repeats turn
  19, which had just succeeded.

  **One finding that changes a later task.** T2.7 gates on
  `evaluator_min_kappa = 0.4` against these proxies, and the proxies are rare
  by construction. That is the regime of the κ **prevalence paradox** — high
  observed agreement, κ near zero or negative, and no comparability between
  sessions of different prevalence. `evaluator_min_kappa` as written is a
  threshold on a number that moves with how bad the graded session was.
  Findings §3 records what T2.7 should carry instead: κ beside its 2×2
  contingency, the prevalence and Gwet's AC1; an under-powered κ reported as
  `insufficient` and treated as below threshold; and the proxy set named in
  the ledger row, because changing it changes the number. M6 §8.5 already
  refused raw agreement for overstating by 33–41 points — this is the same
  error with the sign flipped.

  **Also in this change:** `[evolution] reask_jaccard` (default 0.6, a guess
  and labelled one), and `evolve --dry-run` now prints the turns each
  signature fired on — without which an exit criterion written in turn numbers
  cannot be read at all.

  Still unmeasured: κ (§5). That needs T2.2/T2.3, which are next.
