# Local retrieval, and what Temporal has left to teach the evaluation lane

**Date:** 2026-09-08. **Status:** findings, feeding `2026-09-08-m8-local-evaluation-lane.md`.
**Method:** measured on this box against the twelve-case corpus in
`crates/engine/src/paraphrase.rs` and its 48-line pool — the same corpus
`ns-app eval --paraphrase` runs, so every number here is comparable to the ones in the
plan. Models served warm by nsmodels on loopback, CPU only.

Written because the M8 plan's §6 was about to be built on a conclusion that turned out
to be an artifact of the model I happened to probe with first.

---

## 1. The retrieval sweep

Coarse retrieval by embedding, then a cross-encoder rerank of the top `k`, then top-5 —
the shape the 2026 brief prescribes for the deep tier
(`2026-09-07-context-budget-findings.md` §391: "coarse vector → rerank → graph traversal
only for the deep tier"). Both nsmodels embedders, both against the same pool. The
verbatim control is 12/12 everywhere, so every number below is about vocabulary.

| retrieval | paraphrase miss | ms/query (CPU) |
|---|---|---|
| sqlite fts5 bm25 — **what ships today** | **83%** | — |
| e5-small, vector top-5, no rerank | 33% | ~4 |
| e5-small, coarse 10 → rerank → top-5 | 25% | 458 |
| e5-small, coarse 48 → rerank → top-5 | 17% | 1865 |
| bge-m3, vector top-5, no rerank | 25% | ~33 |
| **bge-m3, coarse 10 → rerank → top-5** | **8%** | **509** |
| bge-m3, coarse 20 → rerank → top-5 | 17% | 1035 |
| bge-m3, coarse 48 → rerank → top-5 | 17% | 1979 |

### 1.1 The conclusion this overturns

The plan's §6, written after probing e5-small only, said the coarse `k` "has to be
generous" — because with e5-small, two targets never reached the reranker at k=20 and
only k=48 got under 20%. That reads as a property of the pipeline. It is not. It is a
property of a 33M-parameter embedder with poor recall, and it inverts with a better one:

**With bge-m3, wider is worse.** k=10 beats k=20, k=30 and k=48, and it is two to four
times cheaper. Every candidate past the tenth is one more chance for the cross-encoder to
promote a distractor, and past k=10 that costs more than the extra recall pays for. This
is the caveat the RRF literature names directly — hybrid and fusion methods routinely
improve mean nDCG while making recall@5 *worse* on the hardest slice, because widening
the candidate set flattens exactly the rank distinctions the top-5 depends on.

### 1.2 What is honest to claim from n = 12

Twelve cases. The difference between 8% and 17% is **one case**, which is inside the
noise of a set this size. The robust claims are the ones a single case cannot flip:

- bm25 at 83% against anything embedding-based at ≤33% is a five-fold gap, not noise.
- bge-m3 dominates e5-small at every coarse `k` measured.
- coarse-10 is *at least as good as* wider settings and 2–4× cheaper — the monotone
  "more candidates is better" assumption is wrong, which is enough to stop the plan from
  hard-coding a generous `k`.

The precise optimum is not established and should not be written into code as though it
were. What is established is the shape and the direction.

### 1.3 Fusion is insurance, not improvement

M8 §6's T3.2 proposed fusing bm25 and vector candidates. Measured: bm25's two hits are a
**strict subset** of the vector arm's eight, and on ten of the twelve paraphrases bm25
returns *nothing at all* — a lexical search drops zero-score lines, so there is no ranking
to fuse. Reciprocal-rank fusion cannot improve a list it has no second list for.

That does not make fusion wrong. It makes its purpose different from the one the plan
gave it: fusion exists so the **verbatim** arm cannot regress — an exact-match query must
never lose to a semantic near-miss. It is a guard, and it should be justified and tested
as one. If it is kept, note that the standard RRF constant k=60 is wrong here: it is tuned
for lists of hundreds, and for a top-5 list the literature's own advice is k ∈ [1, 10].

### 1.4 Where the residual failure lives

With bge-m3 at coarse-10 the single remaining miss is `cs/name` — "kdo jsem, připomeň mi
to" against "jmenuji se Martin a dělám v tiskárně". The English twin, which e5-small also
missed, is now found.

This sharpens the observation from the plan rather than changing it: the residue is the
**identity question**, and identity is the one thing this engine does not answer from
`recall`. M6 pins `user.*`; `user.name` is in front of both models on every turn by
construction (§4.6, the recorded turn 85 failure). The retriever's worst case is
concentrated exactly where the retriever is not what answers.

### 1.5 What this means for the hot path

Rerank latency is the real constraint, and it is per query, on CPU:

- coarse-10 → rerank: ~0.5 s
- coarse-48 → rerank: ~2.0 s

Embedding is not the cost (48 documents and 24 queries in 0.3 s for e5-small, 2.4 s for
bge-m3; documents are embedded once, offline). The reranker is. Half a second inside a
turn that is already waiting on a model call is affordable; two seconds is not, and
neither is either of them when the service is down. This is the quantitative argument for
the plan's existing rule that hybrid recall is reachable **only from the deep tier** — and
it now has a number instead of an intuition.

---

## 2. Temporal, again — for the pass this time

M7 §3 mapped Temporal onto the **turn loop** and concluded, correctly, not to adopt it.
That table is not re-litigated here. But it maps the workflow, and M8 Phase 2 builds
something M7 did not have: an **offline pass that calls a model and writes its output back
into the log**. Four of Temporal's constructs bear directly on that, and one of them
prevents a real bug.

### 2.1 SideEffect — a grade is a recorded value, never a recomputed one

Temporal's rule: non-deterministic work inside a workflow is executed once, its **result
recorded into the event history**, and on replay the recorded result is returned without
executing anything. `MutableSideEffect` extends it — the value is re-read but a new marker
is written only when it changes, which is how a workflow reads drifting config without
breaking determinism.

The engine has the same replay property and the same exposure. `fold()` is the replay, and
`verify_patch` **replays recorded sessions** against a patched `LearnedRules` — that is how
a symbolic candidate earns its verdict. So:

> **Invariant for Phase 2: an evaluator's output is a recorded event, and no replay path
> may call a model.** The grade, the scorer's identity and its model revision go into the
> log together; replay reads them.

Without it, three things break at once, and none of them loudly:

1. Replay needs a Python service running to reproduce a Rust session.
2. A model or weight change silently rewrites history — yesterday's grade is not
   reproducible, and nothing says so.
3. `verify_patch` verifies against a moving target: the same patch can pass and then fail
   with no code change, which destroys the meaning of a verdict in the ledger.

It is cheap to get right and expensive to retrofit, because the retrofit is a migration of
everything already graded. Recording the scorer's identity beside the score is the
`MutableSideEffect` half: drift becomes visible in a diff rather than invisible in a
number.

**Testable:** grade a session, stop the model service, replay — the grades must be
identical. That is a test Phase 2 can carry, and it is the one that would have caught this.

### 2.2 Non-retryable error types — "the server is off" must not be retried

Temporal separates errors that retry from errors that must not, and lets either the
activity or the caller decide. Its default policy is exponential backoff, 2× coefficient,
1 s initial, 100 s cap.

For the local lane the distinction is sharp and the plan did not make it:

| condition | Temporal shape | lane behaviour |
|---|---|---|
| connection refused — service not running | non-retryable | mark `Unavailable` **once**, skip every remaining signal this pass |
| timeout mid-request | retryable, bounded | one retry, then `Unavailable` |
| HTTP 5xx | retryable, bounded | one retry, then `Unavailable` |
| malformed JSON, unknown model | non-retryable | `Err`, counted unverified, and say so |

The plan said "degrades rather than fails", which is true and insufficient. Retrying a
refused connection twelve times with backoff would spend the whole idle window discovering
something the first refusal already established. The `[models] timeout_ms = 2000` default
from Phase 0 is the right order of magnitude; what was missing is that the *first* refusal
should disable the lane for the rest of the pass.

### 2.3 Local Activities — the shape the local scorer actually is

Temporal's local activities are for "fast, cheap operations", trading durability for
latency and history size; the docs are explicit that regular activities remain right for
most work, because a local activity is retried from the workflow-task boundary rather than
from where it was scheduled.

That is exactly the local scorer: loopback, sub-second, idempotent, and losing one costs
nothing because an ungraded turn is simply graded next pass. And the contrast is the
argument for keeping the *paid* judge in the regular-activity shape — recorded, retried
under policy — since a spent request is not recoverable by re-running.

### 2.4 Heartbeats — not adopted, and Temporal says why

Heartbeats detect a stalled worker on long activities. Temporal's own guidance: "quick
scenarios like making a quick API call or reading a small file from disk are not suitable
for Heartbeating." A 0.5 s loopback call is that scenario. The timeout is the whole
mechanism needed. Recorded as not-adopted so it is not rediscovered.

---

## 3. The judge, and whether it should be a model at all

The 2026 reliability literature has moved against the assumption M6 §8.3 was written
under. The finding that matters: LLM judges are **unstable on re-ask** — the same judge
gives materially different scores to the same response across runs, with position and
ordering effects and a large random-error component in the variance decomposition. The
authors' own recommendation is to prefer non-LLM scorers where budget is constrained or
reproducibility matters. Meanwhile calibration against a labelled sample — Cohen's κ, with
periodic re-sampling for drift — has become table stakes rather than a refinement.

Two consequences for Phase 2, both of which make it smaller:

1. **M6's design was right for a reason it did not have yet.** §8.2's symbolic checks
   (re-ask, ungrounded reply, question-ignored) need no model, are perfectly stable across
   runs, and are exactly the kind of "simpler automated scorer" the literature now says can
   beat a judge on reliability. §12.7's `evaluator_min_kappa` and the 5% test–retest check
   are the drift instrument the field has converged on. Nothing in the spec needs revising.

2. **The ordering should change.** M6 §11 Phase 5 lists the symbolic checks first and then
   the evaluator, but its exit criterion — "`ns-app evolve --dry-run` reports ≥ 10
   `UserReask` / `BadReply` signatures where today it reports zero" — does not obviously
   need a model to meet. `UserReask` is a symbolic signature. **Build T2.1 alone, run the
   exit criterion, and find out how much of it lands with no model in the lane at all.**
   If the symbolic half meets it, the local scorer becomes enrichment measured against a
   working baseline rather than the thing the phase depends on — and κ has something real
   to be computed against, which it would not have if both arrived together.

---

## 4. What changes in the plan

- **§6 T3.2** — coarse `k` starts at 10, not "generous"; fusion is re-cast as a guard for
  the verbatim arm, not as the thing that closes the paraphrase gap; RRF's constant, if
  fusion is kept, is k ∈ [1, 10] and not 60.
- **§6 T3.1** — the embedder is bge-m3 (`nsmodels --model quality`), not the default. The
  default is chosen for throughput; this is a recall problem.
- **§6 T3.3** — the deep-tier-only rule now has its number: ~0.5 s per reranked query.
- **§5 Phase 2** — split T2.1 out and gate the rest on its result; add the recorded-grade
  invariant (§2.1) and the retry table (§2.2) as things the code must carry.

## 5. Not pursued

Graph memory (M6 §13, unchanged — a vector index is not a graph and §12.8 firing does not
fire that trigger). Fine-tuning an embedder on this traffic — no labelled set, and the
corpus here is twelve adversarial cases, which is an instrument, not training data.
Adopting Temporal (M7 §3, unchanged; the trigger there is a turn that must outlive the
process, which is still not this deployment). Replacing the reranker with a second-stage
LLM — that is the request cost this whole plan exists to avoid.

## 6. Sources

- Temporal, *Side Effects (Go SDK)* — https://docs.temporal.io/develop/go/workflows/side-effects
- Temporal, *Local Activity* — https://docs.temporal.io/local-activity
- Temporal, *Retry Policies* — https://docs.temporal.io/encyclopedia/retry-policies
- Temporal, *Detecting Activity failures* — https://docs.temporal.io/encyclopedia/detecting-activity-failures
- *The Coin Flip Judge? Reliability and Bias in LLM-as-a-Judge Evaluation*, arXiv:2606.13685
- *LLM-as-Judge Best Practices in 2026: Calibration, Bias, and Cost* — https://futureagi.com/blog/llm-as-judge-best-practices-2026/
- *Reciprocal Rank Fusion: Why k=60 Buries Your Best BM25 Hit* — https://dev.to/ji_ai/reciprocal-rank-fusion-why-k60-buries-your-best-bm25-hit-1k25
- *Reciprocal Rank Fusion (RRF): How It Works and When to Use It* — https://bigdataboutique.com/blog/reciprocal-rank-fusion-how-it-works-and-when-to-use-it
