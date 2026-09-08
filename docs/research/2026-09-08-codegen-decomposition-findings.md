# Would this harness make code generation worse? Mostly yes — and the fix is not looser limits

**Date:** 2026-09-08. **Status:** findings. Revises §4 of
`docs/superpowers/specs/2026-09-08-general-harness-design.md`, which was too optimistic.

Written because the claim "the clip is exactly right for compiler output" was made without
measuring anything, and it is half wrong.

---

## 1. The measurement that settles it

Against this repo's own files, with the harness's shipped defaults
(`prompt_budget_tokens = 6000`, `tool_result_max_chars = 1200`):

| file | chars | ~tokens | × the whole prompt budget | × the tool-result clip |
|---|---|---|---|---|
| `crates/engine/src/turn.rs` | 135,980 | 33,995 | **5.7×** | **113×** |
| `crates/engine/src/eval.rs` | 88,034 | 22,008 | 3.7× | 73× |
| `crates/llm/src/salvage.rs` | 18,944 | 4,736 | 0.8× | 16× |
| `crates/core/src/action.rs` | 9,075 | 2,268 | 0.4× | 8× |

One source file in this workspace is nearly six times the *entire* context ceiling, and a
hundred times the cap on a single tool result. Add the rest of the shipped defaults —
`max_iterations = 5`, `trace_verbatim_lines = 5`, window records capped at 300 characters and
lines at 120 — and the picture is unambiguous.

**Yes: pointed at code as configured today, this harness would degrade generation, not
improve it.** Reading a file returns 1.1% of it. A compile-fix loop gets five iterations. The
fold collapses the edits made earlier in the same turn into a counted line, and in code
editing — unlike clicking — what happened three steps ago *is* the state.

None of this is a design flaw. M7 sized 6,000 tokens explicitly "so … a desktop turn's trace
sit inside it with room". These are desktop numbers doing exactly what they were measured to
do, on a workload that is nothing like a desktop turn. The tier system (M7 Phase 3) exists
precisely so different kinds of turn get different budgets; code is a tier that does not exist
yet.

## 2. The correction that matters more than the numbers

The obvious response is to raise the caps for a code tier. That is the wrong fix, and the
2026 evidence in §3 says so plainly.

> **A tool that returns 136 KB is the bug. The clip is a symptom-fixer.**

The engine's clip and fold exist because tool results are unbounded. For a desktop control
tree, the head is representative and the cap is honest. For a source file, no prefix is
representative — the thing you need is at line 400, and paging to it with `inspect_result`
costs a request per page on a fifty-a-day budget.

The fix is that code tools must return **symbol-scoped slices**: this function, this impl
block, this struct with its doc comment, addressed by name rather than by character offset.
Then a result is naturally 1–3 KB, the existing cap never fires, and the model gets a whole
unit of meaning instead of a truncated prefix of one. The engine already has the paging
primitive (`inspect_result`); what it lacks is *semantic addressing*.

This also makes the retrieval work in M8 pay off twice: a symbol index is the same shape as
the embedding index Phase 3 would build.

## 3. What the 2026 evidence says

### 3.1 Context rot in coding agents — the decisive study

A white-box study of coding agents under controlled context inflation
(arXiv:2607.17937) reports, on its main task:

- **11k characters (clean): 8/10 runs pass. 299k characters: 3/10** — and the same 3/10
  whether the added context is *relevant* or *irrelevant*. Volume itself is the harm.
- **Onset is unstable and non-monotonic**: a 90k condition fails once and passes on replay.
  There is no threshold below which you are safe, which means a fixed budget number cannot be
  the whole answer.
- **Strict success retention 37.5%, but requirement coverage retention 93–95%.** Degradation
  "removes a few decisive obligations rather than the whole artifact" — sparse, specific
  losses, not collapse.
- **38 of 44 failed runs end with a success claim despite unresolved defects.**
- Mitigations under the harmful 299k context: a generic self-check gets 5/10; **a detailed
  external checklist restating all 24 obligations gets 10/10** (p = 0.0325).

The authors' conclusion is nearly a specification for this engine: *keep rich background
retrievable, but keep pending critical obligations small, explicit, external, and
independently checkable.*

Two things follow, and they are the most useful sentences in this document.

**(a) "38 of 44 failed runs end with a success claim" is the strongest argument this
repo has ever had for its own central rule.** The dominant failure mode of a coding agent is
not writing bad code — it is *reporting that it is done when it is not*. A judge asked "did
this go well" is asking the failing component to grade itself. A compiler is not. M6 §13's
"no model in the guard chain, and no model-decided applies" is not conservatism here; it is
the only thing that catches the modal failure.

**(b) The engine has two thirds of the winning mitigation and is missing the third.** Facts
are already small, explicit and external; guards are already independently checkable. What
does not exist is a notion of *the obligations of the current task* — a checklist that
survives the fold, the clip and the budget, because it is the one block that must never be
dropped. That is a small, concrete thing to build, and the study measures it as the difference
between 5/10 and 10/10.

### 3.2 The format tax — and a correction to the BAML section

Earlier work measured 10–30% degradation under hard structural constraints. The 2026 refinement
matters more: separating the prompt-level format request from the decoder-level sampling
constraint shows **the degradation originates in the prompt — the instruction to format — not
in the decoder where the constraint lives.**

For this engine that is good news and one action item:

- **Keep constrained decoding.** The illegality guarantee is nearly free; the tax is not where
  it was assumed to be. The prior spec's reasoning was right by luck.
- **Audit the templates for format instructions.** Any "respond with JSON", "output only the
  object" phrasing in a prompt is now the measured cost, and it is redundant with a schema the
  provider is already enforcing.
- **Code payloads are the exception that proves it.** Emitting a source file as a JSON string
  argument is the worst case: escaping, newline handling, and the highest chance of a malformed
  emission burning one of `max_emit_retries`. The right shape is to **split the emission** —
  the *action choice* stays constrained (symbolic, the legality guarantee is untouched), while
  the *code payload* is emitted as free text and recovered by a schema-aligned parse.
  That is exactly BAML's SAP, applied to the one field where it earns its keep, rather than
  adopted wholesale.

### 3.3 Cascades and routing — why confidence is the wrong signal

The routing literature (FrugalGPT-style cascades; 2026 surveys on cascade routing) converges
on cheap-first with escalation on a confidence threshold, and reports that cascade routing —
choosing at each step rather than fixing an order — beats both pure routing and pure
cascading. But it also reports the limitation that matters here:

> the confidence check is what makes cascades work, and for open-ended generation, where
> models are confidently wrong regularly, cascades are trickier.

And, from the same body of work, the rule this engine should actually adopt:

> skip the model when one cheap deterministic check can decide correctness.

**So the engine should not route on model confidence.** It has something better and rarer: it
has verifiers. The question is not "does the model think it knows" but "does a decision
procedure exist for this". See §4.

### 3.4 Decomposition and context isolation

The decomposition literature converges on sub-agents with isolated contexts: a subtask gets its
own context window and tool set, its intermediate tool calls stay private, and only the final
result returns to the parent. The failure it prevents is named directly — instructions for one
subtask contaminating the context of the next, and subtask histories accumulating in the main
context.

This engine already has the machinery and does not know it: **a subtask is a child session,
and its digest is the return value.** M7 Phase 4 built session digests and cross-session recall
precisely so a closed session becomes a small searchable summary. A subtask that runs as its
own session gets context isolation for free, returns a digest to the parent, and stays
inspectable in the log rather than vanishing into a sub-agent's private scratchpad.

## 4. Deterministic or model: the decision rule

Three tiers, above the two the router already has. The test is the *existence of a decision
procedure*, never confidence.

| tier | when | who decides correctness | example |
|---|---|---|---|
| **0 — no model** | the answer is a function of state the engine already holds | replay (`verify_patch`) | a learned rule fires; a fact answers it; a command form parses; the next step is entailed by the last tool result |
| **1 — model proposes, symbolic verifier decides** | a total checker exists for the output | compiler, test suite, schema validation, path containment | code, structured edits, config changes |
| **2 — model proposes, a person confirms** | irreversible and no checker exists | `SideEffectGate` + owner identity | sending a message, clicking Buy |

Tier 0 is the one that does not exist yet and is worth the most. On fifty requests a day, a
turn answered by a rule instead of a model is 2% of the day recovered, and the engine already
reports requests per completed task, so the share is directly measurable.

The practical discriminator for Tier 0 vs Tier 1, in this engine's terms: **is the required
output already determined by the log plus the learned rules?** If yes, derive it. That is
checkable by replaying the sessions the rule was mined from — the machinery M5 built.

The practical discriminator for Tier 1 vs Tier 2: **is there something that can say "no"
without asking a model?** A compiler can. A schema can. A path-containment check can. "Is this
email polite" cannot, and that is Tier 2 or it is not done at all.

## 5. How to split a big task

Two rules, in priority order.

**(a) Prefer a decomposition a tool already produced.** A compiler emitting 11 errors has
decomposed the task into 11 subtasks, each with a file, a line and a message — for free,
deterministically, with no model call and no risk of an invented step. The same is true of a
failing test list, a lint report, a file list from a glob, a checklist the user wrote. Asking a
model to invent subtasks when a tool has already enumerated them is spending a request to get
a worse answer.

This is Tier 0 decomposition, and it covers most of code work.

**(b) When nothing enumerated it, split by obligation, not by step.** §3.1's result is that
what degrades is a small set of decisive obligations, and that restating them externally
recovers the whole loss. So the durable artifact of a split is not an ordered plan — plans go
stale the moment a step fails — it is a **checklist of obligations with a check for each**,
carried outside the context that rots:

- each obligation is a row in the log, not a line in a prompt;
- each carries how it will be checked (a test name, a compile target, a guard);
- the block is rendered into every prompt of the task and is the *last* thing the budget may
  drop, ahead of the window and the trace;
- a turn cannot be reported complete while an obligation is unchecked — which is the direct
  countermeasure to "38 of 44 failed runs end with a success claim".

**And the container for a subtask is a child session** (§3.4), so its trace, its clip and its
fold are its own, and only its digest reaches the parent.

## 6. What this changes

1. **A code tier**, with its own budget, iteration cap and caps — not raised globally.
2. **Symbol-scoped code tools** (§2), so results arrive already the right size. This is the
   real fix; the tier's budget is the safety net.
3. **An obligations block** (§5b) with drop-priority above the window, and a completion rule
   that reads it.
4. **Split emission for code payloads** (§3.2): constrained action choice, SAP-parsed payload.
5. **Tier 0 derivation** (§4), which is the symbolic-derivation proposal in the design spec,
   now with a decision rule and a measurable share.
6. **Subtask = child session** (§3.4), reusing M7's digests rather than building sub-agents.
7. **Audit prompt templates for format instructions** (§3.2) — cheap, and it is where the
   format tax is actually paid.

## 7. Not changed

Constrained decoding stays (§3.2). No model in the guard chain — §3.1(a) strengthens this
rather than weakening it. No judge on generated code: the compiler is better, free and stable.
No sub-agent framework: child sessions already do it with provenance the log can audit.

## 8. Sources

- *When and How Context Rot Appears in Coding Agents: A White-Box Study of Agent Skills in Code
  Auditing*, arXiv:2607.17937 — https://arxiv.org/html/2607.17937
- *Harness as an Asset: Enforcing Determinism via the Convergent AI Agent Framework (CAAF)*,
  arXiv:2604.17025 — https://arxiv.org/pdf/2604.17025
- *Let Me Speak Freely? A Study on the Impact of Format Restrictions on Performance of LLMs*,
  arXiv:2408.02442 — https://arxiv.org/pdf/2408.02442
- *The Hidden Cost of Structured Generation in LLMs: Draft-Conditioned Constrained Decoding*,
  arXiv:2603.03305 — https://arxiv.org/pdf/2603.03305
- *JSONSchemaBench: A Rigorous Benchmark of Structured Outputs for Language Models*,
  arXiv:2501.10868 — https://arxiv.org/pdf/2501.10868
- *Dynamic Model Routing and Cascading for Efficient LLM Inference: A Survey*,
  arXiv:2603.04445 — https://arxiv.org/pdf/2603.04445
- *Is Escalation Worth It? A Decision-Theoretic Characterization of LLM Cascades*,
  arXiv:2605.06350 — https://arxiv.org/pdf/2605.06350
- *Diagnosing and Mitigating Context Rot in Long-horizon Search*, arXiv:2606.29718 —
  https://arxiv.org/pdf/2606.29718
- *CodeCompass: Navigating the Navigation Paradox in Agentic Code Intelligence*,
  arXiv:2602.20048 — https://arxiv.org/pdf/2602.20048
- *Sema Code: Decoupling AI Coding Agents into Programmable, Embeddable Infrastructure*,
  arXiv:2604.11045 — https://arxiv.org/pdf/2604.11045
