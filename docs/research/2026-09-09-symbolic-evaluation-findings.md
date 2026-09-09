# The symbolic checks, against the literature that already names them

**Date:** 2026-09-09. **Status:** findings, feeding `2026-09-08-m8-local-evaluation-lane.md` §5 (T2.1).
**Method:** reading, not measurement. The measurements this feeds are T2.1's own, and they follow.

Written before building T2.1 — the three symbolic checks, no model, always on — because two of
the three have a literature with names and taxonomies for them, and one of the three turns out
to be five checks rather than one. A third question surfaced on the way and matters more than
either: **T2.7's κ is computed against these checks, and κ misbehaves exactly in the regime
these checks live in.**

---

## 1. Re-ask is query reformulation, and the plan's rule is its degenerate case

M6 §8.2 defines the check as: *normalized user text equals one of the previous 3 user texts →
`UserReask { times }`*.

The search-log literature has been mining this since long before chat harnesses. Query
reformulation mining classifies consecutive queries in a session into a small taxonomy —
generalization, specialization, error correction, parallel move — reporting ~92% accuracy over
a labelled set and applying it to ~17M reformulations from two geographic logs. The features
that carry that classification are lexical and cheap: **Jaccard coefficient, cosine, and size
of the intersection** between the two query strings.

Two things follow.

**First, M6's rule is the Jaccard = 1.0 corner of that feature.** It fires on a literal repeat
and on nothing else. That is not a criticism of the rule — it is a description of its recall.

**Second, this engine has already measured what that recall costs, in the same building.**
M8 Phase 1's paraphrase arm asked the *same question in different words* and watched retrieval
miss it:

```
retriever                    verbatim     miss paraphrase     miss
in-memory (token hits)         12/12        0%      3/12       75%
sqlite (fts5 bm25)             12/12        0%      2/12       83%
```

A user who re-asks because the answer was wrong does not usually re-paste their sentence; they
rephrase it. That is the same event the paraphrase arm constructed, measured at a 75–83% miss
rate for lexical matching. **A lexical-equality re-ask rule should be expected to miss most
re-asks**, by a mechanism this repository has already put a number on.

The tempting fix is the wrong one here. Clustering re-asks by embedding is T2.3 — the local
scorer — and pulling it into the always-on check would defeat the thing T2.1 exists to
establish: how much of M6's exit criterion lands with **no model in the lane at all**.

**What T2.1 does instead**, and why it is not a fudge: report the band, not a boolean.

| band | rule | used as a κ proxy? |
|---|---|---|
| `Repeat` | normalized token sequence equal to one of the previous 3 user turns | **yes** |
| `Reformulated` | Jaccard over content tokens ≥ `reask_jaccard` (default 0.6), and < 1.0 | **no, not yet** |

`Repeat` is the precise half and it is what T2.7 compares an evaluator against. `Reformulated`
is recorded, counted and visible in the report, and it earns proxy status the same way every
other check in this engine earns a gate: on a measured true-positive rate
(`2026-09-04-reply-entrainment.md` §8). Shipping it as a counted-but-not-trusted signal is how
that rate gets measured at all; shipping it as a proxy first would be asserting it.

The default 0.6 is a guess and is labelled one — a config key, not a constant, for the same
reason `coarse_k` is (`2026-09-08-local-retrieval-and-lane-findings.md` §1.2).

## 2. "Question ignored" is five failures in the taxonomy, and this engine can detect the *other* one precisely

M6 §8.2's third check — *user text is a question and the reply shares no content token with
it* → `IgnoredQuestion` — is marked "weak; feeds the model check" in the spec, and the spec is
right, for the reason §1 gives: content-token overlap is vocabulary, and vocabulary is the
thing the paraphrase arm showed does not survive rephrasing.

The named prior art is the Dialogue Breakdown Detection Challenge and the integrated error
taxonomy behind it (Higashinaka et al., SIGDIAL 2021). DBDC labels each system utterance
Breakdown / Possible Breakdown / Non-breakdown by 15–30 annotators; the taxonomy sorts the
breakdowns into **16 categories under a Form Violation / Content Violation split**. The
"ignore" family alone is five of the sixteen:

| code | category | shape |
|---|---|---|
| I5 | **Ignore question** | the utterance ignores the user's question |
| I6 | **Ignore request** | the system ignores the user's request to *do* something |
| I7 | Ignore proposal | the system ignores a proposal to do something |
| I8 | Ignore greeting | the system ignores a greeting |
| I9 | Ignore expectation | the intention was conveyed, the content is not what was expected |

M6's `IgnoredQuestion` is I5. The interesting one for this harness is **I6**, and it is
interesting for a structural reason: a chat corpus cannot see whether the system *did* anything,
so I6 has to be annotated from text. **This engine logs what it did.**

And it logs something better: **its own classification of what the turn was for.** M7 Phase 3's
router puts every turn in a tier and records the decision in the `ModelCall` manifest
(`tier`, `route_cues`). So I6 has a detector that compares no text at all:

> the router put the turn in the `Task` tier, and the turn produced no `Proposed` and no
> `ToolCalled` before `Settled { Respond }`.

That is the engine's own recorded judgement that the message wanted an action, against the log
of it not taking one. It is the mirror of `Misrouted`, which mining already reads out of the
same field in the other direction — there the tier was too narrow and the turn widened it; here
the tier was wide enough and nothing was done.

Both are built. `IgnoredQuestion` (I5) stays weak, note-lane, and is **not** a κ proxy.
`IgnoredRequest` (I6) is structural, and is one.

**Not built: I7, I8, I9 and the other eleven categories.** There is no symbolic detector for
them that is not a text-overlap heuristic wearing a taxonomy code, and a taxonomy with no
measurement behind each row is a longer list, not a better one. DBDC's middle label — *Possible
Breakdown* — is likewise not adopted: the gate downstream of these signatures consumes evidence,
and there is nothing for it to do with a maybe.

## 3. The finding that changes T2.7: κ punishes exactly this shape of proxy

T2.7 computes Cohen's κ of each evaluator against the checks in §1 and §2, with
`evaluator_min_kappa = 0.4` as the gate: below it, observations and no candidates.

The proxies are, by construction, **rare**. `Repeat` and `IgnoredRequest` are meant to fire on a
handful of turns in a session of a hundred and fifty. That is the regime where κ is known to
misbehave — the **prevalence paradox**: when one class dominates the marginals, observed
agreement can be high while κ collapses toward zero or goes negative, and two κ values computed
over sessions with different prevalence are not comparable to each other. The recommended
practice is to report κ *with* the contingency table and prevalence, and beside a
paradox-resistant coefficient — Gwet's AC1, or PABAK — rather than alone.

This matters because `evaluator_min_kappa = 0.4` as written is a threshold on a number that
moves with **how bad the graded session happened to be**, not with how good the evaluator is. A
clean session drives the proxy prevalence toward zero and can fail an evaluator that agreed with
every proxy there was.

M6 already made this argument once, in the opposite direction: §8.5 refuses **raw agreement**
because it overstates judge reliability by 33–41 points. The symmetric hazard — κ *understating*
agreement when positives are rare — is the same class of error and deserves the same treatment.

**Recorded for T2.7, not built here:**

- report κ beside the 2×2 contingency, the prevalence, and Gwet's AC1;
- refuse to *emit* a κ below a minimum positive count rather than emitting a number that cannot
  carry the decision — an under-powered κ should read `insufficient` in the ledger, and an
  evaluator with an `insufficient` κ is treated as below threshold (observations, no
  candidates), which is the conservative direction;
- κ is per-evaluator **and** per-proxy-set; changing which checks are proxies changes the number,
  so the proxy set belongs in the ledger row beside it.

This is why §1 keeps `Reformulated` out of the proxy set. Every band added to the proxies moves
κ for reasons that have nothing to do with the evaluator.

## 4. Groundedness: precision-only, which the engine had already worked out

The 2026 coverage-aware work states the limitation plainly: **reference-free faithfulness
metrics measure only precision** — are the claims that were made supported? — and therefore
reward abstention, since a model scores near-perfectly by saying almost nothing. Their oracle
experiment makes it concrete: the most precise frontier system covers under half the relevant
facts and **ranks last by F1**, while 1B–7B models fine-tuned against complete oracles reach
F1 ≈ 0.98.

`crates/engine/src/ground.rs` is exactly a precision-only check, and `crates/engine/src/echo.rs`
is its lower bound — written because `ReplyFlagged` never fired across turns 137–158 of session
`cli` while the replies were being lifted wholesale out of their own prompt. The pair was built
for this reason without the citation. What the citation forbids is reading either count as a
**quality score**: `UngroundedReply` going to zero is as consistent with a reply that says
nothing as with a reply that is right.

Two consequences for T2.1's second check:

- **Reuse `ground::extract_claims` and `ground::Material` verbatim.** `ns-evolution` already
  depends on `ns-engine`. A second implementation in the pass would be a second definition of
  "grounded", and the first divergence between them would be invisible.
- **Judge against what the replier saw**, per M6 §8.3 — reconstructed from the `ModelCall`
  manifest (`fact_keys`, `window`, `summary_through`) and the log, not from a fresh context
  built at pass time. A turn graded against material it was never shown is a false positive
  by construction.

And one double-count to avoid: a turn that fired `ReplyFlagged` live was **already regenerated**
with the spans named, so its logged reply is the second draft. The live flag is the evidence for
that turn; re-mining the final reply would count the same failure twice and, worse, count the
interceptor's success as a failure.

## 5. Not pursued

Embedding or cross-encoder re-ask clustering (that is T2.3, and putting it in the always-on
check would destroy the baseline T2.1 exists to establish). An LLM judge for I5 (T2.2/T2.3).
The remaining eleven taxonomy categories (§2). DBDC's *Possible Breakdown* label (§2). Replacing
κ outright — the spec's instrument stays, it gains the companions the literature says it needs
(§3).

## 6. Sources

- *Query reformulation mining: models, patterns, and applications*, Discover Computing —
  https://link.springer.com/article/10.1007/s10791-010-9155-3
- *A term-based methodology for query reformulation understanding*, Discover Computing —
  https://link.springer.com/article/10.1007/s10791-015-9251-5
- Higashinaka et al., *Integrated taxonomy of errors in chat-oriented dialogue systems*,
  SIGDIAL 2021 — https://aclanthology.org/2021.sigdial-1.10/ (annotation manual:
  https://github.com/ryuichiro-higashinaka/taxonomy-of-errors)
- *Dialogue Breakdown Detection Challenge 5* — https://chateval.org/dbdc5
- *Detect, Explain, Escalate: Sustainable Dialogue Breakdown Management for LLM Agents*,
  arXiv:2504.18839 — https://arxiv.org/html/2504.18839v1
- *Precision Is Not Faithfulness: Coverage-Aware Evaluation of Grounded Generation with a
  Complete Oracle*, arXiv:2606.09376 — https://arxiv.org/abs/2606.09376
- *A comparison of Cohen's Kappa and Gwet's AC1 when calculating inter-rater reliability
  coefficients* — https://www.ncbi.nlm.nih.gov/pmc/articles/PMC3643869/
- *Quantifying Interrater Agreement and Reliability Between Thoracic Pathologists: Paradoxical
  Behavior of Cohen's Kappa* — https://www.jtocrr.org/article/S2666-3643(23)00161-3/fulltext
