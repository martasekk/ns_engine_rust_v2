# Loading tools only when needed — and why that is a legality change here, not a prompt optimization

**Date:** 2026-09-08. **Status:** findings, with a design recommendation.
**Feeds:** `2026-09-08-general-harness-design.md` §1, and
`2026-09-08-context-composition-findings.md` §1.2 and §4.4, which found the tools array to be
this engine's largest controllable context line item.

---

## 1. The evidence

### 1.1 Tool count degrades selection accuracy, and the knee is low

| tools visible | reported selection accuracy |
|---|---|
| 10 | ~98% |
| 100 | ~88% |
| ~50 | 84–95% |
| ~200 | 41–83% |
| ~740 | 0–20% |

Across GPT-4o-mini, Claude 3.5 Haiku and Gemini 2.0 Flash, hit rates fall from ~98% at 10 tools
to ~88% at 100. Practitioner reports put the knee lower than the benchmarks do: accuracy
degrades measurably **past roughly 10–15 tools**, and noticeably past 15–20 in active rotation.

**This engine is at the knee.** With a desktop wired in there are ten `pointer_*` tools plus the
synthetic actions (`remember_fact`, `forget_fact`, `recall`, `inspect_result`,
`ask_clarification`, `respond_directly`) and the time tool — about seventeen. Not a future
problem at 200 tools; the measured degradation band starts where the engine already is.

### 1.2 On-demand loading works, and the savings are large

Anthropic's Tool Search (advanced tool use; MCP Tool Search for Claude Code, January 2026)
loads tool definitions on demand instead of upfront. Reported: **~85% reduction in tool token
overhead**, 191,300 tokens of context preserved in internal testing; one third-party
implementation reports 94.5%. On accuracy rather than tokens, Anthropic's MCP evals report a
move from **49% to 74%** on Opus 4 with tool search enabled.

The token number is the advertised one. The accuracy number is the important one: this is not
only a budget optimization, it makes selection *better*, which is consistent with §1.1 — fewer
candidates is a smaller haystack.

### 1.3 The right number is adaptive, and fixed-small is dangerous

*How Many Tools Should an LLM Agent See? A Chance-Corrected Answer* (arXiv:2605.24660) is the
most directly useful result, and it argues against a constant:

- **BFCL (370 tools):** an adaptive method reaches **90.3% success at 7.4 tools average depth**,
  against **90.8% at a fixed 50** — statistically the same accuracy for **7× less** context.
- **ToolBench (3,251 tools):** a fixed depth of 5 gets 64.7% on average **but 0% on hard
  queries**. Adaptive gets 61.9% average and **recovers 16.7% of the hard queries** by searching
  deeper (K ≈ 5.7).
- Recommendation: **1–3 tools on easy queries, 5–7 on harder ones**, chosen per query.

The "0% on hard queries" figure is the one to carry. Under-serving tools does not make hard
tasks harder; it makes them **impossible**, and it does so silently — the failure looks like a
model that did not think of the right approach.

Their metric is worth stealing on its own. **Bits-over-Random**, `BoR = log₂(P_obs / P_rand)`,
is chance-corrected: because random success rises as you show more candidates, BoR penalizes
breadth automatically instead of needing an arbitrary depth penalty. With a legal set of three,
guessing looks good; BoR is what stops that from reading as competence.

### 1.4 A caution from the retrieval side

*The 99% Success Paradox: When Near-Perfect Retrieval Equals Random Selection*
(arXiv:2605.18857) is the counterweight: a tool retriever can score near-perfectly on its own
metric while the end-to-end selection is no better than chance. Whatever retrieves tools here
must be graded on **whether the turn completed**, not on whether the right schema was fetched.

### 1.5 An orthogonal and cheaper lever

*Don't Adapt Small Language Models for Tools; Adapt Tool Schemas to the Models*
(arXiv:2510.07248) argues that transforming tool interface definitions — simplifying parameter
specifications, reducing schema complexity, reorganising descriptions — gives comparable or
better results than fine-tuning, and is specifically aimed at small models. For a 3B emitter
this is the cheapest available win and it composes with everything below: slimmer schemas make
every tool cheaper whether or not it is loaded on demand.

---

## 2. Why this is not a prompt optimization in *this* engine

In an ordinary agent, withholding a tool schema is a context-budget decision. The model might
still hallucinate the tool's name, and the harness rejects the call afterwards. Nothing about
the system's *semantics* changed.

Here it is different, and the difference is the engine's central mechanism:

> `build_tools` compiles `LegalActionSet` into a strict `tools` array with
> `additionalProperties: false`, so **the emitter physically cannot name an illegal action**
> (`llm/src/schema.rs`).

**The tools array *is* the legal set.** A tool omitted from the array is not merely unmentioned;
it is unproposable. So "load tools on demand" is a change to what is legal on that turn — a
semantic change, not a formatting one — and it lands directly on M5 §2's standing invariant,
restated in M6 §13: *no new lane may relax a guard, **widen a legal set**, or change a
side-effect class.*

Three consequences that a naïve port of Anthropic's design would violate:

1. **Whatever decides the depth must be recorded in the log**, because the legal set on a turn
   must be reconstructable at replay. If a model-facing search chose the tools, then `fold()`
   over the same events must produce the same legal set, or `verify_patch` is settling
   candidates against a legality that no longer exists. This is the same rule M8 §T2.3a
   established for grades: a non-deterministic choice is a **recorded value**, never a
   recomputed one.
2. **A widening must be the engine's own rule, or an explicit recorded action** — never a
   silent side effect of a retrieval. The invariant does not forbid the *engine* computing a
   legal set; it forbids a lane widening one behind the guards' back.
3. **The residual risk M7 already named becomes quantified.** M7 §12 wrote: *"A model that
   never proposes the tiered-out tool because it cannot see it is the residual risk, and the
   reason `Task` is the default whenever the previous turn used a tool."* §1.3's "0% on hard
   queries at fixed-5" is that risk with a number on it. The mitigation M7 chose — default wide
   after a tool turn — is the right shape and is now evidence-backed.

---

## 3. What the engine already has

More than it looks, which is why the recommendation below is small:

| needed for on-demand tools | already built |
|---|---|
| a per-turn legal set computed by rules | `LegalActionSet`, recomputed every turn |
| intent classification before the first call | M7 Phase 3 router (`Chat` / `Task` tiers) |
| a way to widen mid-turn when the tier was wrong | escalate-on-misroute, **already implemented** |
| the cost of widening, measured | the `escalations` column in `ns-app eval` |
| the size of the tools array, measured | `Usage::tools_tokens` (M7 T2.4) — **built, never read** |
| a retrieval stack to rank candidates | M8's embedder + reranker, if it is ever needed |

The engine is roughly one design decision away, not one milestone.

---

## 4. Two designs, and the recommendation

### 4.1 Design A — adaptive depth in the router (recommended)

Keep the decision in the engine. Replace the two coarse tiers with a **per-turn depth**: the
router already scores a message for intent; extend it to select *which* tools rather than which
tier, targeting §1.3's 1–3 easy / 5–7 harder band.

- **Cost: zero extra requests.** The decision happens before the first call, from cues the
  router already reads.
- **Replay-safe by construction.** The legal set is a pure function of the log plus the router's
  config, exactly as it is today.
- **Invariant-clean.** The engine computes legality; no lane widens anything.
- **Fits the free tier.** On fifty requests a day, a design whose discovery step costs a request
  is a design that spends 2% of the day asking which tools exist.

This is Tier 0 thinking from `2026-09-08-codegen-decomposition-findings.md` §4 applied to tool
selection: prefer the deterministic answer where one exists.

### 4.2 Design B — a `find_tool` action (the escalation path, not the default)

The Anthropic shape: a discovery action in the legal set whose effect is to widen the legal set
for the next iteration.

- Costs **one iteration = one request**, and is therefore the fallback, not the norm.
- Must emit an event recording *which* tools were admitted, so replay reproduces the legal set.
- Is subject to the same guards; discovery itself is `SideEffect::Pure`.

**The key observation: B is nearly already built.** M7's escalate-on-misroute already widens the
legal set mid-turn and already counts the cost. `find_tool` is that mechanism with a model-chosen
trigger instead of an illegality-driven one. So the sequencing is: do A, and let the existing
escalation be B until measurement says a model-chosen trigger beats an automatic one.

### 4.3 What not to do

Do not adopt a tool-RAG stack over an embedding index for seventeen tools. §1.4's paradox is
exactly this trap — a retriever that scores well and changes nothing end to end. Vector tool
retrieval earns its place somewhere north of a hundred tools; the engine is at seventeen, and
the router's cues are cheaper, deterministic and replayable.

---

## 5. Measure first — three numbers, none of which exist yet

1. **`tools_tokens` on a real session.** The instrument shipped with M7 T2.4 and the recorded
   session predates it, so the share this whole document is about has never been read. One
   session.
2. **Selection accuracy against depth, chance-corrected.** Add a BoR-style metric to `ns-app
   eval`: for each fixture, the legal set size and whether the right action was proposed. With
   small legal sets, raw hit rate flatters; BoR is the correction. Offline, no requests.
3. **The hard-query arm.** §1.3's warning only shows up on tasks that *need* the rarely-offered
   tool. The desktop half of the task set is the place for it: a fixture whose target action is
   one a narrow tier would have tiered out, so "0% on hard queries" would show as a failure
   here rather than as a silent capability loss in production.

Without (3), a narrower legal set will look like a pure win on every existing fixture, because
none of them needs a tool the router would withhold.

---

## 6. Recommended order

1. **Slim the schemas** (§1.5). Cheapest, helps the 3B emitter, independent of everything else,
   and lowers the cost of every tool whether or not it is loaded on demand.
2. **Read `tools_tokens`** (§5.1). One session; everything else is guesswork until then.
3. **Build the hard-query fixture** (§5.3) *before* narrowing anything, so the risk is visible
   from the first measurement rather than discovered in production.
4. **Adaptive depth in the router** (§4.1), with BoR reported per run.
5. **Leave escalation as the discovery path** (§4.2); revisit `find_tool` only if the
   escalation rate says the router's cues are not enough.

## 7. Not proposed

Vector tool retrieval at this scale (§4.3). A model deciding the legal set without a recorded
event (§2.1). Fine-tuning the emitter for tool use — §1.5 is the cheaper answer and M6 §13 rules
out fine-tuning anyway. Removing the `Task`-after-a-tool-turn default: §1.3 says that
conservatism is load-bearing.

## 8. Sources

- *How Many Tools Should an LLM Agent See? A Chance-Corrected Answer*, arXiv:2605.24660 —
  https://arxiv.org/html/2605.24660v1
- *The 99% Success Paradox: When Near-Perfect Retrieval Equals Random Selection*,
  arXiv:2605.18857 — https://arxiv.org/pdf/2605.18857
- *Don't Adapt Small Language Models for Tools; Adapt Tool Schemas to the Models*,
  arXiv:2510.07248 — https://arxiv.org/pdf/2510.07248
- *Semantic Tool Discovery for Large Language Models: A Vector-Based Approach to MCP Tool
  Selection*, arXiv:2603.20313 — https://arxiv.org/html/2603.20313v1
- *HumanMCP: A Human-Like Query Dataset for Evaluating MCP Tool Retrieval Performance*,
  arXiv:2602.23367 — https://arxiv.org/pdf/2602.23367
- *Beyond the Leaderboard: A Synthesis of Tool-Use, Planning, and Reasoning Failures in LLM
  Agents*, arXiv:2607.05775 — https://arxiv.org/pdf/2607.05775
- *Anthropic Tool Search Explained: BM25, Regex & the Advanced Tool Use Header* —
  https://growthmethod.com/anthropic-tool-search/
- *What is MCP Tool Search?* —
  https://www.atcyrus.com/stories/mcp-tool-search-claude-code-context-pollution-guide
- *Hermes Agent Ships Tool Search for MCP: Anthropic Evals Show 49% to 74% Accuracy Gain on
  Opus 4* —
  https://www.marktechpost.com/2026/05/29/hermes-agent-ships-tool-search-for-mcp-anthropic-evals-show-49-to-74-accuracy-gain-on-opus-4/
- *Tool RAG: The Next Breakthrough in Scalable AI Agents*, Red Hat Emerging Technologies —
  https://next.redhat.com/2025/11/26/tool-rag-the-next-breakthrough-in-scalable-ai-agents/
- *Effective context engineering for AI agents*, Anthropic —
  https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
