# Handing memory to a child session, and the token economy of a turn — findings

Date: 2026-09-11 · Measured against branch `worktree-research-multi-conversation` @ `489bd2c`
(main @ `206ea0d` plus the dispatcher, `channel-tcp` and `ns-app serve`) · Live config
`~/ns-run/config.toml` (all three roles on `google/gemini-3.8-flash` via OpenRouter,
`window_turns = 6`, `facts_in_context = 10`, `summary_max_chars = 800`, `max_iterations = 12`).

Sources: the code (file:line below), the repo's own findings and plans (§2), the Anthropic
prompt-caching and cost-optimization references shipped with the `claude-api` skill
(2026-06-24 revision), and the web sweep in §7. Numbers are the sources' own; nothing here was
re-measured on this box except the file sizes and config values quoted.

The question was: *how does the engine paste memory to subagents, what has the knowledge graph
already researched, what does the field say is optimal, and how do we conserve the most tokens
while keeping the model performing?* The answer, in one paragraph:

> **The engine has no subagents and no child sessions.** Its units of work are sibling sessions
> under a dispatcher, tools that receive only a session id and an artifact sink, and three
> model roles whose prompts are assembled from the same capped blocks. Nothing passes context
> from one session to another except a read-only lexical `recall` over earlier sessions'
> digests. The "isolate" operation — subtask as child session, digest returned to the parent —
> is proposed in three docs and built nowhere. On the token side the engine already has the
> shape the 2025–2026 literature converges on (bounded window, fixed-field summary, ≤10 facts,
> clipped trace, on-demand recall); what it lacks is **measurement it already instruments but
> never reads** (`tools_tokens`), **a cache breakpoint that can actually hit** (the persona is
> ~120 tokens; every provider's minimum cacheable prefix is ≥1,024), and **the cached-token
> count in `Usage`**, without which no caching change can be verified. On `openrouter/free`
> the scarce unit is the request, not the token, and every lever below is ranked by that.

Legend: **[DONE]** already in the engine · **[PARTIAL]** shape exists, a piece is missing ·
**[MEASURE]** instrument or read a number before deciding · **[CANDIDATE]** worth a plan once
its trigger fires · **[LATER]** designed-for, built later · **[NOT ADOPTED]** with the reason ·
**[STANDS]** an earlier decision this sweep re-confirms.

## Disposition against ns-engine (2026-09-11)

| # | Finding | Where the engine stands | Disposition |
|---|---|---|---|
| 01 | Subagents should receive a fixed-schema brief (objective, output format, tool boundaries) and return a 1–2k-token digest, never the transcript (§3.1) | No child unit exists. `ToolCtx { session, artifacts }` is the only handoff (`crates/core/src/traits.rs:9-13`). Session digests exist and are searchable (`crates/engine/src/store.rs:100,119`) but never return to a parent. | **[CANDIDATE]** — §4 specifies the handoff; trigger in §4.4 |
| 02 | Reasoning belongs at the orchestrator; reasoning inside sub-agents is limited or negative (`2026-09-07` §2 finding 11; Cognition) | Three roles on one model; no escalation (discarded 2026-09-01). | **[STANDS]** — a child runs the same or a cheaper model, never a larger one |
| 03 | Parallel sub-agents make conflicting implicit decisions the merger cannot reconcile (Cognition) | `worker_slots` overlaps sessions, not subtasks (`crates/engine/src/dispatch.rs`). | **[STANDS]** — children, when built, run sequentially inside the parent turn |
| 04 | Context degrades at every length increment, relevant or not (Chroma; codegen §3.1) | Window, facts, summary, trace all capped; §5 of `2026-09-08-context-composition` says don't touch them. | **[DONE]**, **[STANDS]** |
| 05 | An explicit obligations checklist restores 10/10 from 5/10 on false-success runs (codegen §3.1b) | Specified, unbuilt. | **[CANDIDATE]** — cheapest quality lever in this doc; §5 R2 |
| 06 | Tool schemas are the largest unmeasured block; selection accuracy falls past 10–15 tools (`2026-09-08-tool-loading` §1.1, §1.2) | `tools_tokens` recorded on the wire (`crates/llm/src/client.rs:222-225`), printed by `ns-app budget`, **never read on a real session** (the `cli` log predates it). ~17 tools. | **[MEASURE]** — one `ns-app budget cli` after one real desktop turn |
| 07 | Prompt caching is the largest single cost lever on every agent loop Anthropic measured: 2.5–3.7× at 81–90% hit rate (cost-optimization §2.1) | One breakpoint, on the persona only (`crates/llm/src/replier.rs:139-146`); emitter sets none; OpenRouter is the only preset that forwards it (`crates/llm/src/provider.rs:45,214`). | **[PARTIAL]** — as placed it cannot hit; §5 R3 |
| 08 | Caching cannot be verified from code review; read `cached_tokens` from usage (prompt-caching § Verifying) | `record_usage` reads `prompt_tokens` and `completion_tokens` only (`client.rs:208-218`); `nscore::Usage` has no cached field (`crates/core/src/usage.rs:15-31`). | **[MEASURE]** — add one field; §5 R3 |
| 09 | Gemini and Anthropic via OpenRouter need explicit `cache_control`; OpenAI/DeepSeek cache automatically; `:free` endpoints bill nothing, so caching there is latency and routing only (§3.6) | Config names `google/gemini-3.8-flash` **without `:free`**; the 50-requests/day comment assumes the free variant. | **[MEASURE]** — `GET /api/v1/key` says which tier the key is on |
| 10 | Requests are the scarce unit on the free tier; a retry is a request (`usage.rs:27-31`; multi-conversation §2.9) | `attempts` counted per HTTP request; `max_emit_retries = 3`; exponential backoff at `client.rs:134`; no `Retry-After` handling. | **[DONE]** for counting; **[CANDIDATE]** for fewer emitter iterations per turn, §5 R1 |
| 11 | Block order: facts sit in the lost-in-the-middle trough; ordering for cache vs for attention was never measured (composition §3) | Unchanged. `ns-app eval` can run both orders offline. | **[MEASURE]** — offline, zero requests |
| 12 | Mask tools rather than remove them, to keep the prefix stable (Manus) | Legal-set pruning changes the `tools` array per iteration by design (`2026-09-01-findings` §Pruning). | **[NOT ADOPTED]** — pruning buys selection accuracy (06) and the free tier does not bill the miss; revisit only if 08 shows the emitter never hits |
| 13 | Graph or vector memory | Vector trigger fired (M8 §Phase 1, 83% paraphrase miss); graph trigger has not. | **[STANDS]** — M8 owns it; nothing here changes it |
| 14 | Consolidated (paraphrased) window; a model in the router or the fold; Temporal server; sub-agent framework | Contraindicated in `2026-09-04 §9`, M7 §12, codegen §7, `2026-09-10` §6. | **[STANDS]** |

---

## 0. Context for a fresh session

- "Memory" in this engine means four things, all in SQLite: the verbatim turn log (primary),
  facts (versioned, scoped, ≤10 in context), the fixed-field session summary (regenerated
  every four turns from verbatim), and session digests (one per finished session, FTS-indexed).
  Summary and facts are indexes over the log, never the source of truth (`2026-09-02-memory-findings`).
- "Subagent" has three possible referents here and none exists: (a) a child session spawned for
  a subtask, (b) a worker process, (c) a second model consulted mid-turn. (a) is proposed, (b)
  is what the dispatcher's `worker_slots` overlaps — whole sessions, not subtasks — and (c) is
  contraindicated.
- The binding resource is stated in the code itself: *"Tokens are not the scarce resource on
  every tier: `openrouter/free` allows fifty requests a day, and a call that succeeded on its
  third attempt spent three of them"* (`crates/core/src/usage.rs:27-31`). A desktop turn can
  spend ~14 requests (M7 §2).

## 1. How the engine hands context to a unit of work today

### 1.1 The three units, and what each receives

| Unit | Receives | Where |
|---|---|---|
| Session | Its own mailbox and task; one turn at a time; a `worker_slots` permit | `crates/engine/src/dispatch.rs:1-115` |
| Tool | `ToolCtx { session, artifacts }` — no facts, no window, no summary | `crates/core/src/traits.rs:9-13` |
| Model call | A role-specific prompt assembled from capped blocks (§1.2) | `crates/engine/src/turn.rs:1029-1052` (emitter), `:2121-2145` (replier) |

Fact scope is `global` for the CLI and one scope per session id under `ns-app serve`
(`app/src/main.rs:660-671`). A child session would inherit isolation for free there, and would
need an explicit rule to *see* its parent's facts.

### 1.2 One model call's prompt, block by block

**Emitter** (`crates/llm/src/emitter.rs`): `system` is one fixed four-line string
(`emitter.rs:7`). The `user` message is `render_context` (`emitter.rs:31-80`) in this order:
facts → summary → recent turns (window) → current turn → this-turn trace → budget line →
pending confirmation → rejections → guidance → "Propose the next action." Then
`tools = build_tools(legal)`, `tool_choice = "required"`, `temperature = 0`. The context is
trimmed by `nscore::fit_emitter` under the tier budget (`turn.rs:1046-1052`). Up to twelve of
these per turn, each re-sending the whole thing.

**Replier** (`crates/llm/src/replier.rs`): `system` = persona, then a fenced `<reference>`
block holding `<facts>` → `<summary>` → `<transcript>` (`replier.rs:34-72`). `user` =
`render_task`: the user message, the `<did>` trace, guidance, do-not-state / do-not-repeat,
closing instruction (`replier.rs:75-115`). No tool schemas. One per turn.

**Summarizer**: one request every `summary_every_turns = 4`, input capped at
`summary_input_max_chars = 6000`, output at 800 chars.

### 1.3 The only cross-session channel is read-only

`recall` searches earlier turns, then facts, then the digests of *earlier* sessions
(`turn.rs:688-800`, `store.rs:100` `session_digests`, `store.rs:119` `search_digests`). This is
the return half of the isolate operation — a digest is exactly what §3.4 of the codegen doc
wants a child to hand back — but nothing writes a digest *to* a waiting parent turn, and no
turn waits.

### 1.4 What is measured today

`Usage` per call: `role, model, prompt_tokens, completion_tokens, estimated, attempts,
latency_ms, tools_tokens` (`crates/core/src/usage.rs:15-31`, recorded at `client.rs:196-233`).
`ns-app budget` renders per turn `reqs, calls, e/r/s, prompt, compl, peak, schema, schema%,
trace, clip, tools` and a footer with requests per turn and requests by role
(`app/src/budget.rs:150-163, 245-272`). Provider numbers are used when a `usage` block comes
back, otherwise an estimate flagged `estimated = true`. `tools_tokens` is estimated from the
serialized `tools` array, envelope included.

Not measured: **cached prompt tokens**. OpenRouter returns them under
`usage.prompt_tokens_details.cached_tokens`; the client never reads that path
(`client.rs:208-218`). M7 §13 already recorded that the reconstruction is a floor that counts
neither facts, nor system prompt, nor schemas; this is the other blind spot.

### 1.5 Caching as built

- `prompt_cache: true` only for the OpenRouter preset (`provider.rs:45`), asserted by
  `only_openrouter_forwards_prompt_cache_breakpoints` (`provider.rs:214`); wired at
  `app/src/main.rs:587`.
- With it on, the replier sends `system` as content parts and marks the **persona** part
  `cache_control: ephemeral`; the reference block (facts, summary, window) follows the
  breakpoint and is never cached (`replier.rs:139-146`). The comment there calls the persona
  "the only part that never changes", which is true, and also why the breakpoint cannot hit:
  the live persona is ~120 tokens, and the minimum cacheable prefix is 1,024 tokens on Gemini
  Flash and on Anthropic's smaller models, 4,096 on the larger ones.
- The emitter — the role that makes up to twelve calls per turn over a byte-identical
  facts → summary → window prefix — carries no breakpoint at all.

So the cache is switched on, forwarded correctly, and placed where it can do nothing. Nobody
could have noticed, because §1.4.

## 2. What the docs already settled — do not re-litigate

Condensed; each line is a pointer, not a restatement.

- **Less, better-chosen context beats history**: 71%→92% completion and 1.48M→553k tokens in
  the cited system (`2026-09-07` §2 finding 02). Bounded window, fixed-field summary, refusal
  to accumulate history are **not to change** (`2026-09-08-context-composition` §5).
- **Volume itself is the harm**: 11k chars → 8/10 pass, 299k → 3/10, identical whether the
  added context is relevant or irrelevant; onset non-monotonic, so no single budget number is
  the answer (codegen §3.1).
- **The obligations block**: 38/44 failed runs ended in a false success claim; a detailed
  external checklist restored 10/10 (p = 0.0325) vs 5/10 for generic self-check (codegen §3.1b).
- **Isolate is the missing fourth operation**; "mostly assembly of parts that exist", ranked
  #2 after reading `tools_tokens` (composition §1, §4.2). Child = session with isolated context
  and private tool calls; only the digest returns (codegen §3.4, §6–§7). Explicitly *no*
  sub-agent framework — child sessions give the same isolation with log-auditable provenance.
- **Tools**: ~98% selection accuracy at 10 tools → ~88% at 100; the engine is at the knee
  (~17). On-demand loading ≈85% tool-token reduction and 49%→74% accuracy in the cited eval;
  fixed depth 5 gives 0% on hard queries — under-serving fails silently. Cheapest lever is
  slimming the schemas themselves (`2026-09-08-tool-loading` §1.1–§1.5). Three numbers still
  don't exist: `tools_tokens` on a real session, Bits-over-Random vs depth, a hard-query
  fixture (§5).
- **Block order vs prefix caching**: U-shaped attention, >30% mid-context degradation; the
  current order optimises caching and leaves facts in the trough; never measured; `ns-app
  eval` can settle it offline (composition §3).
- **Requests, not tokens**: a 14k-char `ui_read` was re-sent on every remaining iteration, up
  to 11× (M7 §2); Phase 1 cut the turn-7 trace 14,108 → 1,311 chars and saved 21,225 chars per
  send (M7 §13). Under `serve`, N conversations share one budget and one throttle;
  concurrency overlaps *waiting*, never buys requests (`2026-09-10` §2.9). **A child session
  spends real requests from the same fifty.**
- **Contraindicated**: Temporal server, actor framework, graph memory (trigger unfired),
  consolidated window, a model in the router or fold, a judge on the critical path, RL,
  escalation (M7 §12; `2026-09-10` §6). Vector memory's trigger *has* fired and belongs to M8.

## 3. What the field says, mapped onto this engine

### 3.1 What to hand a child, and what to take back

- Anthropic's stated principle is "the smallest set of high-signal tokens that maximize the
  likelihood of the desired outcome"; their sub-agent architectures return **condensed
  digests of 1,000–2,000 tokens** to the coordinator, and the coordinator keeps lightweight
  identifiers (paths, queries) for just-in-time retrieval rather than pre-loading content.
- Their multi-agent research post found that a brief like "research X" produced duplicated and
  missed work; each subagent needs **an objective, an output format, guidance on tools and
  sources, and explicit task boundaries**, with effort scaled explicitly (a simple fact-find:
  one agent, 3–10 tool calls). Measured: agents use ~4× the tokens of chat, multi-agent
  systems ~15×.
- Cognition's counter-position: parallel sub-agents make conflicting implicit decisions the
  merging agent cannot reconcile; share decisions and key events (a dedicated compression
  step), not individual messages; prefer a single linear agent.
- The 2026 practitioner consensus on handoffs is a **fixed schema** — files modified, tools
  called, decisions, in-progress state, constraints, preferences — over a three-tier store:
  working context → compressed session summary → external cross-session memory retrieved at
  start. The engine's digest + facts + summary already *is* that store; only the handoff
  schema is missing.

For this engine: the child's brief is a fixed record, the child runs sequentially inside the
parent's turn, and what comes back is the digest — the same shape `recall` already reads.

### 3.2 Reasoning stays at the orchestrator

`2026-09-07` finding 11 (reasoning at the orchestrator gives the largest gains, reasoning in
sub-agents limited or negative) and Cognition agree. A child never runs a larger model than
its parent. The cost-optimization reference adds the measured shape: an orchestrator over
cheaper workers only pays when there is bulk to hand off — many independent pieces, ideally
too many for one context; on one dependent chain it "pays for a plan, a handoff, and a merge
that a single model gets for free", and the coordinator's model alone at lower effort came
out ahead in every such case measured.

### 3.3 Cache-aware prefix stability

- Prompt caching is a prefix match; any byte change invalidates everything after it. Render
  order is tools → system → messages. Classify inputs by stability (never / per-session /
  per-turn / per-request) and make the rendered order match. Fork operations must reuse the
  parent's exact prefix or they miss the parent's cache entirely. (`claude-api`
  `shared/prompt-caching.md`.)
- Manus reports KV-cache hit rate as "the single most important metric" for a production
  agent: cached vs uncached input was 10× apart in price at a ~100:1 input:output ratio.
  Rules: byte-stable prefix (no timestamps), append-only, deterministic serialization, **mask
  tool logits rather than remove tool definitions**, recite goals near the end of context to
  beat lost-in-the-middle, keep failed actions in context.
- Measured ceiling: caching cut agent-loop cost 2.5–3.7× at 81–90% hit rates; a triage
  agent's bill fell 83% from caching alone (cost-optimization §2.1).
- Concurrent fan-out with the same prefix writes N cache entries and reads none — send one,
  wait for the first streamed token, then the rest (prompt-caching § Concurrent-request timing).
  Under `serve` with `worker_slots > 1` this is the pattern to avoid if caching ever bills.

The engine's legal-set pruning is the one place it deliberately breaks Manus's rule (12 in the
table). The trade is explicit: pruning buys selection accuracy on a small model
(tool-loading §1.1), and on `:free` the cache miss costs nothing. Keep pruning; measure 08.

### 3.4 Compression and editing

- ACON (arXiv 2510.00615): compression guidelines optimised in natural language from failure
  analysis, no fine-tuning; 26–54% peak-token reduction with >95% accuracy retained when
  distilled into small compressors. Maps onto the summarizer's fixed fields — the M8 lane
  could derive summary guidelines from graded failures instead of hand-writing them.
- Anthropic context editing: 84% token reduction on a 100-turn web-search eval; +39% on
  agentic search with memory + editing, +29% editing alone. The cost-optimization reference
  warns editing is a context-window tool, not a savings lever: every clearing pass rewrites
  the cached prefix. The engine's trace clip is the same operation done once per turn, which
  is the cheap version.
- Mem0: >90% token saving vs full context at +26% judge score on LOCOMO. The engine's ≤10
  facts + `recall_top_k = 5` is that shape already.
- Recursive Language Models (arXiv 2512.24601): treat the prompt as an external variable and
  recurse over chunks; a small model beat a large one on OOLONG at lower cost with flatter
  degradation. This is the isolate operation applied to one oversized tool result — the 14k
  `ui_read` case — and the strongest argument that a child session, when built, should first
  be a *one-call extractor over an artifact*, not a general subtask runner.

### 3.5 Context rot

Chroma's sweep of 18 models at 8 lengths: performance degrades at every increment, a 1M
model still degrades at 50k; distractors and haystack structure matter non-uniformly. This is
the evidence behind keeping every cap in `[memory]` where it is, and behind putting an
obligations block *above* the window in drop priority (codegen §6.3).

### 3.6 OpenRouter specifics that change the arithmetic

- `:free` models: 20 requests/minute; **50/day with under 10 lifetime credits purchased,
  1,000/day once 10+ credits have been bought**. A 429 may come from the upstream provider.
  `GET /api/v1/key` returns `limit_remaining` and daily usage — cheaper than discovering the
  cap by hitting it. Official guidance is exponential backoff, not immediate retry; a
  rejected request still spent an attempt.
- Caching by provider through OpenRouter: automatic for OpenAI, DeepSeek, Groq, Grok, Moonshot;
  **explicit `cache_control` required for Anthropic, Gemini, Qwen**. Read multipliers: 0.1×
  Anthropic/DeepSeek/Qwen, 0.25× Gemini; Gemini implicit caching needs ≥1,024 tokens on Flash.
  OpenRouter routes sticky per provider and accepts a `session_id` to pin multi-turn traffic so
  the cache stays warm.
- No cache discount applies to `:free` variants because they bill nothing. On the free tier
  caching is a latency and routing concern; the moment the key moves to a paid tier it becomes
  the largest lever in this document.

## 4. The child-session handoff, specified against the constraints

Not a plan; the shape a plan would have to take. Everything below reuses parts that exist.

### 4.1 What the child receives (one record, ≤2k tokens, rendered in this order)

1. **Persona** — the parent's, byte-identical, so a paid tier can share the prefix.
2. **Objective** — one sentence, the subtask, from the parent's emitter.
3. **Output format** — the digest fields in 4.2, verbatim, so the child's last action is
   always `finish` with a well-formed digest.
4. **Boundaries** — the legal action set for the child (a subset of the parent's, chosen by
   the parent's router, never the full set), the artifact ids it may read, and a request budget
   (§4.4).
5. **Obligations** — the checklist from codegen §3.1b for this subtask, with drop-priority
   above the window.
6. **Pinned facts** — the parent scope's `user.*` pinned facts (≤5), read-only. Relevant
   facts are *not* copied; the child may `recall` them, which costs it a request.

The child gets **no** parent window, no parent summary, no parent trace. That is the point of
isolation (codegen §3.4) and matches Anthropic's brief-not-transcript rule.

### 4.2 What returns

The existing session digest, extended with three fixed fields: **decisions taken** (Cognition's
"implicit decisions" made explicit), **artifacts produced** (ids, so the parent retrieves
just-in-time), and **obligations unmet**. The parent's emitter sees the digest as one trace
line on its next iteration; the log records the child session id, so provenance is auditable
without a framework.

### 4.3 Scope and store

Under `serve` the child is a session with its own fact scope; it reads the parent scope's
pinned facts through the brief and writes nothing to the parent scope except through the
digest. Under the CLI (global scope) the same rule holds by convention, enforced by the
child's legal set not containing `remember`.

### 4.4 The cost rule and the trigger

A child spends requests from the same daily fifty. It pays for itself only when it removes at
least its own request count from the parent's iterations. The one measured case where that
holds is the oversized tool result (M7 §2: one result re-sent up to 11×). So the trigger is:
**`ns-app budget` on a real desktop session shows a turn whose emitter iterations re-send a
clipped artifact more than the clip saves** — or the M8 lane produces a graded failure class
that a one-call extractor fixes. Until then, [CANDIDATE], not a plan.

## 5. Token and request economy — the levers, ranked for this tier

The Anthropic reference orders levers as caching → input hygiene → loop hygiene → output →
batch → effort → model, with "judge cost per completed task, not per request". On
`openrouter/free` the first reorder is that **requests per turn go first**, because they are
the ceiling, and caching drops to third because it does not bill. Each lever names the number
to read before touching code.

### R1 — Requests per completed turn (binding)

- **Read the floor first**: `ns-app budget cli` after one real desktop turn gives requests per
  turn and by role. M7 §2's ~14 is the only figure on record and predates Phase 1.
- **Confirm the tier**: `GET /api/v1/key` tells whether the key is on the 50/day or 1,000/day
  band and whether `google/gemini-3.8-flash` without `:free` is billing. If it bills, R3 moves
  to first place and the request arithmetic in every doc since M7 needs a footnote.
- **Retries are requests**: `max_emit_retries = 3` means a bad iteration can cost four. Honour
  `Retry-After` when present (none handled today, only backoff at `client.rs:134`), and stop
  retrying a 4xx that is not 429 (already: `client_error_400_fails_immediately_without_retry`).
- **[CANDIDATE] more than one action per emitter request.** The emitter's contract is "propose
  the next action", one tool call per request, twelve per turn. OpenRouter's own guidance and
  every harness in §3 batch tool calls per assistant turn. If the log shows runs of
  consecutive reversible actions with no observation between them (click → type → key), a
  bounded batch (≤3, reversible only, `confirm_irreversible` untouched) is the single largest
  request saver available. Measure the run lengths in the log before designing it; it changes
  the per-action emitter tests (general-harness §2.2b).
- **Summarizer cadence** is one request per four turns; not worth touching.

### R2 — Tokens per call, for quality not cost

- **Read `tools_tokens` on a real session** (06). The composition doc's estimate is 800–1,200
  tokens, 13–20% of a 6,000 budget vs a 2.3% reference share; every tool decision waits on
  this number.
- **Slim the schemas** (tool-loading §1.5) — the cheapest lever for a small emitter, and it
  keeps pruning intact.
- **Build the obligations block** (05): measured 5/10 → 10/10, drop-priority above the window,
  specified in codegen §6.3. Costs tokens, saves false-success turns, which are requests.
- **Run the block-order arm offline** (11): `ns-app eval` with facts-after-summary vs
  facts-first; zero requests spent.

### R3 — Caching, so that it can hit at all

- **Record `cached_tokens`** from `usage.prompt_tokens_details` into `nscore::Usage` and print
  it in `ns-app budget` next to `prompt` (08). One field, one column; every later step is
  unverifiable without it.
- **Move the replier breakpoint** from after the persona to after `<facts>` (stable within a
  session) or after `<summary>` (stable for four turns), so the cached prefix clears the
  1,024-token floor. The tension with block order (11) is real: measure 11 first, then place
  the breakpoint at the last stable block *in the order that wins*.
- **Give the emitter a breakpoint** at the end of the window block: iterations 2..12 of a turn
  share that prefix byte-for-byte. Whether the changing `tools` array defeats it on Gemini via
  OpenRouter is exactly what 08 will show; on Anthropic-shaped caching it does (tools render
  first), which is why 12 stays not-adopted rather than reversed.
- **Send `session_id`** to OpenRouter so sticky routing keeps one provider's cache warm across
  a turn's iterations.
- Expectation if the key ever bills: 2.5–3.7× on the loop (cost-optimization §2.1). On
  `:free`: lower latency per iteration, nothing else — say so in the plan that does it.

### R4 — Effort and model, last

Gemini 3.8 Flash exposes no effort knob the client sends; the model was chosen for the tier.
The reference's rule stands: sweep effort before model, price the tail not the median, and do
not step down a tier without the M8 eval in place to catch it.

## 6. What not to change

Re-affirmed from composition §5 and M7 §12: the bounded window, the fixed-field summary,
≤10 facts, the refusal to accumulate history, legal-set pruning, no model in the router or
fold, no framework for children, no graph memory before its trigger. Nothing in the 2026
literature moves any of these; Chroma and the codegen numbers strengthen them.

## 7. Sources

Repo: `docs/research/2026-09-02-memory-findings.md`, `2026-09-04-entrainment-findings.md`,
`2026-09-07-context-budget-findings.md`, `2026-09-08-context-composition-findings.md`,
`2026-09-08-codegen-decomposition-findings.md`, `2026-09-08-tool-loading-findings.md`,
`2026-09-10-multi-conversation-runtime-findings.md`; plans `2026-09-07-m7-context-budget.md`
§1, §2, §12, §13 and `2026-09-10-multi-conversation-runtime.md` §Results.

Anthropic: Effective context engineering for AI agents
(anthropic.com/engineering/effective-context-engineering-for-ai-agents); How we built our
multi-agent research system (anthropic.com/engineering/multi-agent-research-system); Effective
harnesses for long-running agents (anthropic.com/engineering/effective-harnesses-for-long-running-agents);
Context editing docs (platform.claude.com/docs/en/build-with-claude/context-editing) and the
context-management post (claude.com/blog/context-management); `claude-api` skill references
`shared/prompt-caching.md`, `shared/cost-optimization.md`, `shared/agent-design.md`
(2026-06-24 revision).

Papers and practice: Chroma, Context Rot (research.trychroma.com/context-rot); ACON, arXiv
2510.00615; Mem0, arXiv 2504.19413; Recursive Language Models, arXiv 2512.24601; Manus,
Context Engineering for AI Agents (manus.im/blog); Cognition, Don't Build Multi-Agents
(cognition.com/blog/dont-build-multi-agents); 2026 surveys arXiv 2603.07670, 2606.08151,
2606.30306, 2607.00692; Mem0 state-of-agent-memory 2026 (mem0.ai/blog).

OpenRouter: limits (openrouter.ai/docs/api-reference/limits), prompt caching
(openrouter.ai/docs/features/prompt-caching).

Footnote, Claude Code side (out of scope, recorded once): a Claude Code subagent receives
CLAUDE.md, its delegation prompt (which this box's `graphify_gate.py` hook prefixes with the
graph-first rule), and optionally its own `memory:` directory — never the session's
`MEMORY.md`; only a `fork` inherits the conversation and the parent's prompt cache
(code.claude.com/docs/en/sub-agents, /memory, /prompt-caching).
