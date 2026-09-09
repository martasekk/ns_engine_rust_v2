# What a local scorer has to prove before it is allowed to grade anything

**Date:** 2026-09-09. **Status:** findings, feeding `2026-09-08-m8-local-evaluation-lane.md` §5
(T2.2, T2.3, T2.7). **Method:** reading, plus a sample-size calculation. The measurements are
T2.3's and follow this.

Written between T2.1 and T2.3, because T2.1's result exposed the thing this phase is actually
short of. T2.1 found seven re-asks in twenty turns with no model, and it found them against a
log holding **one session**. Every number M8 has produced so far rests on n = 12 (the paraphrase
corpus) or n = 20 (the recorded session). κ is next, and κ is a statistic — asking it to decide
whether a scorer may propose candidates, on twenty turns, would be asking a coin flip to
arbitrate.

So this pass answers two questions before any evaluator is built: **how many labelled cases does
a decisive κ need**, and **what does the 2026 literature say happens to cheap automatic scorers
when they are trusted without that measurement.** The second answer is worse than the plan
assumed, and it makes T2.7 load-bearing rather than a refinement.

---

## 1. The finding that changes the phase: automatic scorers do not transfer

The 2026 cross-dataset audit of attribution metrics
(*Do LLM Attribution Metrics Transfer?*, arXiv:2606.23915) takes eight automatic scorers —
lexical overlap, embedding similarity, BERTScore and three NLI variants — and asks whether a
metric that works on one dataset works on another. It does not:

| observation | number |
|---|---|
| a clean MNLI scorer on short-form AttributedQA | **0.904 AUROC** |
| the same scorer on long-form LFQA | **0.531 AUROC** — chance |
| BERTScore on the same two, in the opposite direction | rises to 0.909 on LFQA |
| concordance of per-dataset metric rankings | **Kendall W = 0.07**, p = 0.029 |
| ranking inversion between the two datasets | Kendall τ = **−0.64**, p = 0.031 |
| regret of picking the best-on-average metric, leave-one-dataset-out | **0.172 AUROC** |
| LLM judges against each other | κ = 0.832 |
| LLM judges against humans | κ ≈ **0.47**, and dataset-dependent (0.93 → 0.58) |

The authors' conclusion is one sentence: *no single automatic scorer transfers across datasets
without target-dataset validation.*

**What that means here.** M8 §5's argument for `LocalEvaluator` is that its three jobs — is this
reply about the question, does its content appear in what the prompt showed it, is this turn a
re-ask of that one — are *ranking* jobs, which an encoder does well. That argument is still
right, and it is not sufficient. "An encoder does this kind of job well" is a claim about a
literature; the audit says such claims invert between datasets of the same kind. The only claim
that survives is one measured **on this engine's traffic**.

Which is exactly what T2.7 already specifies. The change is its status: κ is not a safety rail
bolted on after the scorer works. **It is the experiment that decides whether the scorer works
at all**, and the plan's ordering — build T2.1's proxies first, so κ has something to be
computed against — turns out to be the only order in which the phase is answerable.

Two consequences the plan did not carry:

- **A held-out split.** Naive best-on-average selection carried 0.172 AUROC of regret in
  leave-one-dataset-out. Any threshold, coarse `k` or field weighting chosen by looking at the
  corpus must be chosen on a *development* half and reported on a *held-out* half, or the number
  reported is the number that was fitted.
- **Report the construct, not just the score.** The audit's first recommendation is to name the
  dataset and the evaluation construct together, because "grounded" on short factoid answers and
  "grounded" on long-form answers are different constructs that the same metric ranks
  differently. The ledger row should say which corpus and which check produced a κ.

## 2. How big is decisive: n ≈ 100, not n = 20

The sample size for a κ with a usable confidence interval follows Donner and Eliasziw (1992),
the standard reference for κ interval construction and power. The published worked numbers
bracket what this phase needs:

| target | n |
|---|---|
| 95% CI with a margin of error of 0.2, positive rate ≈ 20% | **96** |
| H₀: κ = 0.3 against H₁: κ = 0.5, α = 0.05, 90% power, p₁ = 0.40 | **173** |

The gate M8 §8 sets is `evaluator_min_kappa = 0.4`, and the question a run has to answer is
"is this evaluator's κ above 0.4 or not". A ±0.2 interval is exactly the resolution that
question needs, and it costs ~96 cases. The stricter 173 buys the power to separate 0.3 from
0.5, which is a finer distinction than the gate makes.

So: **a corpus of about a hundred labelled turns**, and the twenty-turn recorded session is not
a substitute for it — not because it is unrepresentative, but because it is too small for the
statistic that has to be computed over it. This is the same lesson `2026-09-08-…-lane-findings`
§1.2 recorded about the retrieval sweep ("the difference between 8% and 17% is one case"),
arriving a second time at a place where it can be fixed instead of caveated.

**Prevalence is chosen, and the choice has to be stated.** κ's variance is worst when one class
is rare, which is the paradox `2026-09-09-symbolic-evaluation-findings.md` §3 records against
T2.7. A corpus built to *calibrate* a scorer should therefore be near-balanced — that is what
makes κ estimable at this n — while live traffic will be nothing like balanced. Those are two
different numbers about two different things, and a run must not print one and mean the other:
the corpus κ measures **discrimination**, the live-pass κ measures **agreement in situ**, and
only the first is worth gating on at n = 20.

## 3. What the scorer may be, given §1

Three candidate shapes, and the audit's discipline applied to each.

**Embedding similarity (`/embed`, bge-m3).** Cheap — 33 ms a query on this box — and already
measured on this engine's own material: 25% paraphrase miss at top-5 against bm25's 83%
(`2026-09-08-…-lane-findings` §1). Multilingual by construction, which matters because half the
recorded session is Czech and the cross-lingual re-ask (turn 14 English, turn 15 Czech) is
precisely what T2.1's lexical check could not reach. **Rank, never threshold** — nsmodels'
own finding, that e5-small's cosine range is compressed enough (0.898 true match against 0.862
unrelated) that an absolute cut-off is meaningless.

**Cross-encoder rerank (`/rerank`, bge-reranker-v2-m3).** The 2026 reranking literature places
exactly this in the slot the plan wants: cheap recall first, a cross-encoder over the top
candidates, and an LLM only where one is unavoidable. Deterministic and reproducible, which is
the property an LLM judge does not have — and the property T2.3a's recorded-grade invariant
depends on. ~0.5 s per query at coarse-10 on this box, which is affordable in an offline pass
and is the reason the same path is deep-tier-only in a turn.

**A dedicated fact-checker.** MiniCheck-FT5 is the one the audit itself names as lowest-regret
(0.044 AUROC against 0.172 for best-on-average): 770M parameters, GPT-4-level grounding accuracy
at ~400× lower cost. **Not adoptable here, and the reason is not size.** It is English; the
paper lists multilingual as future work. This engine's traffic is Czech, and an English-only
checker on Czech scores near-randomly — the same asymmetry nsmodels' README already flags for
CLIP. Recorded as rejected so it is not rediscovered as an obvious win.

**And the judge is not being replaced, only unblocked.** `ClientEvaluator` stays behind the same
trait with the same contract. The audit's own note is that prompt-based judges avoid the
catastrophic collapses the automatic scorers show, at ~100× the cost and without determinism —
on fifty requests a day that trade is not available, but the trait is where it lives when it is.

## 4. What T2.3 must therefore carry

Nothing here changes the plan's task list; it changes what counts as those tasks being done.

1. **A labelled corpus of ~100 turns**, Czech and English, near-balanced, self-policing the way
   the paraphrase corpus is, split development / held-out. Without it there is no κ, and
   without κ there is no admissible local scorer (§1).
2. **Every configurable number chosen on the development half only**, and the reported κ
   computed on the held-out half (§1).
3. **κ reported with its contingency table, prevalence, Gwet's AC1 and a confidence interval**
   — the interval especially, since the whole argument for n ≈ 100 is that the interval is what
   makes the number decisive (§2, and `2026-09-09-symbolic-evaluation-findings.md` §3).
4. **Ranks, never thresholds, out of `/embed`**; the cross-encoder is what turns a ranking into
   a decision, and even then the decision is a signal (§3).
5. **The scorer's identity and revision recorded beside every grade** (T2.3a). §1 is the reason
   this stops being pedantry: a metric whose behaviour inverts between datasets will certainly
   move when its weights change, and a grade that cannot say which model produced it cannot be
   compared to a later one.

## 5. Not pursued

MiniCheck (§3, English-only). A second-stage LLM reranker (the request cost this plan exists to
avoid). Fine-tuning any of these on this traffic — the corpus below is an instrument, and using
it as training data would destroy the only held-out set there is. Replacing κ with AUROC: the
audit reports AUROC because it has graded probabilities, and the gate here is a binary agreement
against a symbolic proxy, which is κ's shape; AC1 comes along as the paradox-resistant companion
(`2026-09-09-symbolic-evaluation-findings.md` §3), not as a replacement.

## 6. Sources

- *Do LLM Attribution Metrics Transfer? Auditing Retrieval-Augmented Generation Evaluation
  Across Datasets and Constructs*, arXiv:2606.23915 — https://arxiv.org/html/2606.23915v1
- Donner, A. & Eliasziw, M. (1992), *A goodness-of-fit approach to inference procedures for the
  kappa statistic*, Statistics in Medicine 11, 1511–1519 — sample-size treatment summarised at
  https://real-statistics.com/reliability/interrater-reliability/cohens-kappa/cohens-kappa-sample-size/
  and https://www.ime.usp.br/~abe/lista/pdfGSoh9GPIQN.pdf
- *Guidelines of the minimum sample size requirements for Cohen's Kappa* —
  https://www.researchgate.net/publication/320148141
- *MiniCheck: Efficient Fact-Checking of LLMs on Grounding Documents*, EMNLP 2024 —
  https://aclanthology.org/2024.emnlp-main.499/
- *Should You Use LLMs for Reranking? Pointwise, Listwise, and Cross-Encoders* —
  https://zeroentropy.dev/articles/should-you-use-llms-for-reranking-a-deep-dive-into-pointwise-listwise-and-cross-encoders/
