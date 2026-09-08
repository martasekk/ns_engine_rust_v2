# What is best to pass as context in 2026, what Temporal actually solves, and what changes for a 3B model

**Date:** 2026-09-08. **Status:** findings.
**Feeds:** `2026-09-08-general-harness-design.md`, and a correction to M7 §3.

Three questions that look separate and are not: what goes in the prompt, what Temporal does
about state, and what a small model changes. The answer to the second is that Temporal does
not solve the first at all — and mistaking one for the other is where M7 §3 slipped.

---

## 1. The 2026 consensus on what to pass

The field has converged on four operations, and it is worth naming them because the engine
already does three:

| operation | meaning | ns-engine today |
|---|---|---|
| **write** | persist context *outside* the window | the event log, facts, digests |
| **select** | retrieve only what is relevant now | `recall`, `lexical_rank`, pinned + query-relevant facts |
| **compress** | summarize to save tokens | `SessionSummary`, the M7 fold |
| **isolate** | give sub-tasks their own clean context | **missing** — see §4 |

The governing principle, stated the same way across the 2026 practitioner literature: *the
context window is a budget to spend, not a bucket to fill.* Default to subtraction. Retrieve
just in time rather than preloading. Prefer structural retrieval for code. Keep the tool set
lean. Reserve headroom.

### 1.1 The number that should end the "more context is better" argument

Sourcegraph benchmarked coding agents on identical tasks: agents handed a **100K-token
codebase summary performed worse than agents handed 5K tokens of targeted retrieval.**
Twenty times the context, measurably worse results.

Alongside it, two figures that make context the primary lever rather than a tuning detail:
**token usage explains ~80% of performance variance** in agent evaluation, and a widely cited
industry figure attributes **65% of enterprise agent failures to context drift** — the model
reasoning over the wrong tokens — rather than to model capability.

This is the same shape as the context-rot result in
`2026-09-08-codegen-decomposition-findings.md` §3.1, where 299k characters of *relevant*
context scored identically to 299k of *irrelevant*: volume itself is the harm.

### 1.2 A reference allocation, and where this engine sits against it

The practitioner reference for a 128k window:

| block | tokens | share |
|---|---|---|
| system prompt | 2,000 | 1.5% |
| tool definitions | 3,000 | 2.3% |
| memory | 5,000 | 3.9% |
| retrieval | 15,000 | 11.7% |
| conversation state | 8,000 | 6.3% |
| reserved for output | 20,000 | 15.6% |
| **unallocated headroom** | ~75,000 | **~59%** |

ns-engine's `prompt_budget_tokens = 6000` is not a model window — it is a ceiling on the
blocks the engine controls — so the shares are what compare, not the absolute numbers. From
the recorded `cli` session via `ns-app budget`, per turn: window ≈ 290 tokens, summary ≈ 111,
sent trace ≈ 156. Facts, system prompt and tool schemas are not counted by that tool, and they
are exactly where the interesting number is.

**The one line item that looks wrong is the tools array.** The ten `pointer_*` schemas alone
are a *lower bound* of ~540 tokens in source form, before the JSON wrapper, the per-tool
envelope and the injected `_rationale` property; add the memory synthetic actions
(`remember_fact`, `forget_fact`, `recall`, `inspect_result`, `ask_clarification`,
`respond_directly`) and the time tool and the real figure is plausibly 800–1,200 tokens. Against
a 6,000-token ceiling that is **13–20%, against a reference share of 2.3%** — five to eight
times over. M7 saw this from the other side and wrote it down: "schemas are most of a `Task`
prompt and none of a `Chat` one."

Two things follow:

- The tier router is already the mitigation, and it is more valuable than it was credited
  with. Tiering the *tool set* is the largest single lever on prompt size.
- M7 built `Usage::tools_tokens` to measure precisely this, and **the recorded session predates
  it**, so the real share has never been read. It is one session away from being known, and it
  should be read before anything else is tuned.

The 2026 answer beyond tiering is tool discovery on demand — send a handful of schemas plus a
lookup rather than the whole surface. That is a real option here because the legal set is
already computed per turn.

---

## 2. How Temporal does it — and the category error worth avoiding

**Temporal does not manage context. It manages history.** These are different problems and
Temporal solves only the second. In the durable-agent write-up of a production Temporal agent,
the section on what the LLM receives each turn simply is not there; what is specified is how
*state* is checkpointed. Any answer of the form "do what Temporal does for context" is
answering a question Temporal never asks.

What Temporal actually provides:

| mechanism | detail |
|---|---|
| event history is the state | workflow progress is persisted as events and replayed to recover |
| hard limits | **51,200 events or 50 MB**, with warnings from **10,240 events or 10 MB**; the docs advise staying under a few thousand events per execution |
| Continue-As-New | checkpoint the state, close the run, start a fresh run with that state — the workflow id is preserved, so callers never see the rollover |
| aggressive pruning | in the production design, **only the latest checkpoint is retained**, "with the understanding that Temporal's Workflow replay can restore an agent to any previous state"; pruning runs at every checkpoint and at every rollover |

That last row is worth pausing on, because it is ns-engine's own architecture stated in
Temporal's vocabulary: *keep one small bounded projection, and rely on the log for anything
else.* The engine reached the same design independently — `fold()` is the replay, the window
and summary are the retained projection.

### 2.1 Where M7 §3 was wrong, and it matters for Tomáš

M7 §3 dismissed Continue-As-New with: *"not needed — the window and summary already bound what
any turn reads; the log is unbounded and stays so."*

The first clause is true and the conclusion does not follow, because Temporal's limits are not
about what a turn **reads**; they are about what a run must **replay**. And ns-engine has that
cost too:

```
turn.rs:869   let stored = self.parts.memory.load(&sid).await?;   // the whole session
turn.rs:873   let turn = fold(log.events()).turn + 1;
turn.rs:937   let state = fold(log.events());
turn.rs:2075  let state = fold(log.events());
```

Every turn loads **all** of the session's events and folds them — several times within a single
turn. For an 18-turn CLI session this is free and invisible. For the WhatsApp target, where one
thread with one person runs for months, it is O(n) per turn with n growing without bound, done
three or four times a turn. This is precisely Temporal's history-bloat trap, and Temporal's
answer to it is Continue-As-New.

**The good news: ns-engine has already built the payload.** M7 Phase 4's session digest *is*
the checkpoint — a fixed-size summary of a closed session, searchable, with provenance. Facts
are scoped rather than session-bound, so they cross a rollover untouched (M6 §6.6). And
cross-session recall (`search_turns_in`, `recall_sessions = 3`) already reaches back across
the boundary. What is missing is only the **trigger and the link**: close the session at a
threshold, write the digest, open a successor that names its predecessor.

So the correction is narrow and the work is small: Continue-As-New is not "not needed", it is
"not needed *yet*, and nearly built". Its trigger is the same trigger M7 §3 already recorded
for revisiting Temporal itself — a channel where conversations outlive a process.

---

## 3. What changes with a small model

The free-tier emitter is a 3B-class model, so this is the operative constraint, not a footnote.

**Capacity is not fidelity.** 2026 SLMs advertise large windows — Ministral 3 (3B/8B) at 256k,
Gemma 3 4B at 128k, SmolLM3-3B at 32k — but evaluations report reasoning degrading
substantially well before those limits, and the distinction the literature draws is between
*context capacity* (what a model accepts) and *context fidelity* (how well it uses it). The
specific finding that bites: **the sheer volume of surrounding distractor text degrades a
model's ability to apply retrieved evidence.** Not to find it — to *apply* it.

**Small models struggle with multi-intent synthesis.** They degrade on open-ended queries and
on answers requiring synthesis across long context. The practical rule that follows is: *one
intent per model call, and put the answer next to the question.*

This is a strong endorsement of three decisions the engine already made, and one it has not:

- **The tiered router** (M7 Phase 3) — routing to the smallest legal set per turn is the
  single most SLM-appropriate thing in the design.
- **The bounded window and fixed-field summary** — a 3B model gains nothing from turn 40 of a
  conversation and loses fidelity to it.
- **`_rationale` first** — a free-text field before the constrained ones, which the 2026
  format-tax work now says was the right call for a reason M7 did not have.
- **Not yet done: block ordering by position.** The lost-in-the-middle effect is a U-shape —
  accuracy is highest at the beginning and end of the context and degrades by more than 30% in
  the middle. `ReplyContext` documents its order as "stable-first for prefix caching: persona →
  facts → summary → window → current turn", which optimises cache hits and puts *the current
  user message last* — correctly, at the strong end. But it also leaves **facts** — the block
  most often carrying the answer — in the weak middle. There is a real tension here between
  prefix caching and positional fidelity, and it has never been measured. On a fifty-request
  budget the caching matters; on a 3B model the position may matter more. `ns-app eval` can
  settle it offline.

---

## 4. What to change, in order of value

1. **Read `tools_tokens` from a real session.** The instrument exists; the number does not.
   Everything else about prompt composition is guesswork until it is read. One session.
2. **Isolate — the missing fourth operation.** A subtask should be a child session whose digest
   returns to the parent (`2026-09-08-codegen-decomposition-findings.md` §3.4). This is the one
   context-engineering primitive the engine lacks entirely, and it is mostly assembly of parts
   that exist.
3. **Session rollover (Continue-As-New).** Close at a threshold, write the digest, link the
   successor. Required before Tomáš, not before the desktop.
4. **Tool-set reduction beyond tiering** — schemas are the largest controllable line item and
   sit five to eight times over the reference share.
5. **Measure block order against prefix caching** — a cheap offline arm, and the answer may
   differ between a 3B emitter and a larger replier.

## 5. What not to change

The bounded window, the fixed-field summary, and the refusal to accumulate history: §1.1 and
§3 are an argument *for* them, not against. The engine's instinct — that a turn should read a
small projection rather than a growing transcript — is the 2026 consensus, arrived at two
milestones early.

## 6. Sources

- *Context Engineering: A Practical Guide for AI Agents (2026)*, Sourcegraph —
  https://sourcegraph.com/blog/context-engineering
- *Context Engineering for Production LLM Agents (2026)* —
  https://appscale.blog/en/blog/context-engineering-production-llm-agents-token-budget-compaction-2026
- *Context Engineering for Coding Agents 2026: What Works* —
  https://www.heyuan110.com/posts/ai/2026-06-16-context-engineering-2026/
- *Externalization in LLM Agents: A Unified Review of Memory, Skills, Protocols and Harness
  Engineering*, arXiv:2604.08224 — https://arxiv.org/pdf/2604.08224
- Temporal, *Continue-As-New* — https://docs.temporal.io/workflow-execution/continue-as-new
- Temporal, *Workflow Execution limits* — https://docs.temporal.io/workflow-execution/limits
- Temporal, *Events and Event History* — https://docs.temporal.io/workflow-execution/event
- Temporal, *The thread is the Workflow: Durable AI agents without changing Agent code* —
  https://temporal.io/blog/manetu-the-thread-is-the-workflow
- *Temporal History Bloat: 10 State-Growth Traps* (Mar 2026) —
  https://medium.com/@bhagyarana80/temporal-history-bloat-10-state-growth-traps-000c4d349136
- *Can Small Language Models Handle Context-Summarized Multi-Turn Customer-Service QA?*,
  arXiv:2602.00665 — https://arxiv.org/abs/2602.00665
- *Best Small Language Models 2026: Top SLMs Ranked (1B–14B)* —
  https://localaimaster.com/blog/small-language-models-guide-2026
- *LLM Context Length & Context Window Explained (2026)* —
  https://datanorth.ai/blog/context-length
