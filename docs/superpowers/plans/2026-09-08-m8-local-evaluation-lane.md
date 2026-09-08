# M8 — The evaluation lane, paid for locally

**Status:** plan, 2026-09-08. Written against `m7-context-budget` @ `8d18018` (20 commits ahead
of `main` @ `a478d46`; `main` is fully contained in it, so the merge is a fast-forward).

**Evidence base:** `docs/research/2026-09-07-context-budget-findings.md` (the 2026 brief),
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

M6 Phase 5's tasks, built in its order, with one substitution at the judge.

| Task | Files | Test |
|---|---|---|
| T2.1 symbolic checks — re-ask, ungrounded reply, question-ignored. **No model, always on** | `crates/evolution/src/evaluate.rs` (new) | M6's own criterion: the recorded session yields `UserReask` for turns 52–60, 67, 90, 100, 105 and `UngroundedReply` for turn 69 |
| T2.2 `Evaluator` trait; `ScriptedEvaluator` (tests); `ClientEvaluator` (the paid judge, unchanged from M6 §8.3) | `evaluate.rs` | fence-tolerant parse; invalid JSON → `Err`, counted unverified |
| T2.3 `LocalEvaluator` over `/embed` + `/rerank`: it scores *similarity and ranking*, never a verdict — reply-vs-question relevance, reply-vs-shown-facts grounding, re-ask clustering | `evaluate.rs`, `crates/evolution/src/local.rs` (new) | server down → every signal `Unavailable`, pass completes, nothing counted; scores are ranks, never thresholds (nsmodels' own finding: e5-small's cosine range is compressed, true match 0.898 vs unrelated 0.862) |
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
tier. M7 built the classifier and the tiers (Phase 3 router). So:

| Task | Files | Test |
|---|---|---|
| T3.1 `embeddings` table keyed by event rowid; backfill in the idle pass, never in a turn | `crates/memory-sqlite/src/lib.rs`, `evolution/src/pass.rs` | backfill is resumable; a turn never blocks on an embedding |
| T3.2 hybrid `search_turns`: bm25 candidates ∪ vector candidates, fused by rank (not score), then `/rerank` over the union, top-k | `memory-sqlite/src/lib.rs` | with the server down, results are exactly today's bm25 results — same order, same count |
| T3.3 the hybrid path is reachable **only from the `Task`/deep tier**; `Chat` recall stays lexical | `crates/engine/src/router.rs`, `turn.rs` | a `Chat` turn issues no `/embed` call |
| T3.4 re-run `ns-app eval --paraphrase`; the miss rate is the exit criterion | `eval.rs` | miss rate below 20%, with the verbatim arm not regressed |

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
