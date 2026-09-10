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
**[STANDS]** an earlier decision this sweep re-confirms · **[NOTED]** read once, no action ·
**[NOT APPLICABLE]** needs training or weights an API harness cannot touch.

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

## 7. Novel memory architectures, mapped (addendum, same day)

The question: *are there novel memory architectures worth applying?* Method: a web sweep over
2025–2026 memory papers with every arXiv ID opened and title-matched, then a grep of `docs/`
for each name and ID to separate "new to the field" from "new to this repo". Most of the field's
list is already here: the 2026-09-07 catalogue files ReasoningBank, Agent Workflow Memory,
Dynamic Cheatsheet, ACE and Reflexion under finding 09 (procedural memory, **[DONE]** via the
M5 evolution pass); sleep-time compute is **[ADOPTED]** as the pass's driver B; Self-GC
(2607.00692), bi-temporal invalidation (2501.13956, the Zep paper) and ghost-memory state labels
are load-bearing in the M6 spec §14. What follows is what the sweep adds.

The engine already has a CoALA-shaped store: working (window + trace), episodic (fixed-field
summary, digests, typed observations still unbuilt), semantic (versioned scoped facts with four
forgetting mechanisms), procedural (learned rules, notes, ledger, gated). Any "novel
architecture" is judged as a delta against that, under three standing rules: no model-decided
applies (M6 §13), a check becomes a gate only after a measured true-positive rate, and every
model call is a request against fifty a day.

### 7.1 Disposition

| # | Architecture | What it adds over what exists | Calls | Disposition |
|---|---|---|---|---|
| N1 | **Memento** — case-based reasoning: a case bank of past trajectories with outcome labels, k nearest cases retrieved by similarity into the planner's prompt; frozen LLM, optional tiny learned selector (arXiv 2508.16153; 87.9% Pass@3 GAIA, +4.7–9.6 pts OOD) | **Absent from the repo.** The log *is* a case bank; digests carry outcome; bge-m3 is local. Retrieving the 2–3 nearest past episodes' action sequences into the emitter as "how this was done before" is the non-parametric variant. Desktop tasks repeat, which is where Evo-Memory (2511.20857) finds experience memory beats retrieval stores (gain ∝ task similarity, r≈0.72). | 0 per turn (local retrieval); tokens only | **[CANDIDATE]** — the cheap pre-step to compiled flows; same trigger as evolution §10 (10 sessions sharing a tool-call sequence ≥2), measured from the log at zero requests. Entrainment risk (`2026-09-04`): exemplars enter as reference with provenance, never as rules |
| N2 | **ReasoningBank** — strategy items distilled from successes *and* failures, retrieved per task (2509.25140; +34% rel., −16% steps) | The pass mines **failure signatures only** (evolution §3.1) and the symbolic lane is prune/repair-only by construction (ClawTrace). A success lane feeding the *notes* lane, not the symbolic one, is the delta. MaTTS (3–5× rollouts) is out on this tier. | ~1 idle call per session; gate unchanged | **[CANDIDATE]** — small; the GRASP gate already decides whether a note survives |
| N3 | **Dynamic Cheatsheet** — one self-curated persistent strategy sheet in every prompt, rewritten by the model (2504.07952; Game-of-24 10%→99%) | The notes lane in ungated form. "The model rewrites its own sheet and it goes straight into the next prompt" is exactly the claude-mem pattern M6 §13 rejects. The gated equivalent exists. | 1 per session | **[NOT ADOPTED]** as designed; **[DONE]** in gated form |
| N4 | **Agent Workflow Memory** — induce reusable parameterised workflows from trajectories, inject as skills (2409.07429; +24.6% Mind2Web, +51.1% WebArena) | This is compiled flows / TraceCompiler, already **[LATER]** with a trigger (evolution §10). | 1 induction per session | **[STANDS]** — N1 is the cheaper step before it |
| N5 | **Nemori** — episode boundaries by topic shift instead of fixed windows; distil semantic memory only where a next-episode *prediction* failed (2508.03341) | The summary runs every 4 turns and rebuilds every 3rd (M6 §5.1), a fixed cadence chosen against iterative-summarisation drift (2603.11768). A boundary detector need not be a model: cosine distance between consecutive turns on bge-m3 plus the existing `idle_after_secs` gap is a zero-request segmenter. The predict-calibrate half costs a request per episode and is dropped. | 0 (local boundary); summarizer calls become per-episode instead of per-4-turns | **[CANDIDATE]** — measure first, offline: how often does a 4-turn summary straddle a topic shift in the `cli` log? If rarely, nothing to gain |
| N6 | **Decision-aware memory cards / CICL** — rank retrieval candidates by expected effect on the *next action*, not similarity; compress survivors to cards (2606.08151) | Facts selection is precision-aware (2605.11325) but similarity-ranked. The manifest records which facts were in context per call, and M8 grades turns, so a utility-per-fact signal is computable offline. The cross-encoder approximates the scorer. | 0 | **[CANDIDATE]** — lives inside M8 Phase 3's rerank; needs M8 Phase 2's gate first |
| N7 | **Darwinian Memory** — GUI-agent memory items compete; only those with measured downstream usefulness survive, the rest are culled, no training (2601.22528; +18.0% success, +33.9% stability) | The engine's forgetting is decay-, access- and trust-based (M6 §6.2); usefulness-by-outcome is the survey's "learned forgetting" open problem, solvable *symbolically* here: manifest ∩ graded outcome → per-fact, per-note fitness, evaluated in the idle pass. **RMM**'s retrospective reflection (2503.08026; >10% LongMemEval) is the same signal from the other end: boost what the reply actually cited — and `reply_grounding_check` already computes what the reply drew on. | 0 | **[CANDIDATE]** — the one genuinely new *forgetting* mechanism; zero requests; blocked on M8 grading |
| N8 | **Sleep-time compute, the anticipation half** — pre-materialise likely next context while idle (2504.13171; ~5× less test-time compute) | Consolidation half adopted. Anticipation spends idle requests from the same fifty on guesses; the 2026 study of 12 memory systems (2606.24775) finds **localised maintenance beats global reorganisation** on cost — which also argues for the current incremental-then-rebuild summary compromise over either extreme. | idle requests | **[NOT ADOPTED]** on this tier; **[LATER]** if the key moves to a paid band |
| N9 | **Context folding / AgentFold** — branch a subtask, fold its trace to a summary (2510.11967, 2510.24699; ReAct parity at 10× smaller active context, gains from an RL-trained policy) | The scaffold is §4 of this doc (child session, digest back). The policy is trained; not applicable. | per child | covered by §4 |
| N10 | **A-MEM** — Zettelkasten notes with LLM-generated links and rewrites of existing notes on every insert (2502.12110; 6× multi-hop) | One call per stored item plus evolution calls; graph-shaped links next to a discarded graph. | ≥1 per write | **[NOT ADOPTED]** — request cost |
| N11 | **MemOS / MemoryOS / H-MEM** — OS frames, tiered paging, four-layer hierarchy with pointers (2507.03724, 2506.06326, 2507.22925) | Activation and parameter tiers unreachable via OpenRouter; summary depth 1 by decision (M6 §13). | — | **[STANDS]** |
| N12 | **Mem0 / Zep-Graphiti** — extract → conflict-detect → update with scopes; bi-temporal edges (2504.19413, 2501.13956; Zep 63.8% vs Mem0 49.0% LongMemEval) | Versioned facts with validity intervals and scopes already are this. | — | **[DONE]** |
| N13 | **ALMA** — a meta-agent searches memory designs as executable code (2602.07755) | Search is call-expensive; the *discovered* designs are portable. Worth reading the paper's winning schemas once. | — | **[NOTED]** |
| N14 | Mem-α, AdaMEM, DeMem, MEM1, Memory-R1 — RL-trained managers; Titans, Cartridges, SEAL, EM-LLM — model-level | Training or weights. | — | **[NOT APPLICABLE]** |

### 7.2 What the benchmarks say about the direction

- MemoryAgentBench (2507.05257): no method masters retrieval, test-time learning, long-range
  understanding *and* selective forgetting; forgetting is weakest everywhere. N7 targets the
  weakest quadrant with zero requests.
- Evo-Memory (2511.20857): on task streams, procedural memories (Cheatsheet, AWM) beat
  retrieval stores (Mem0, MemOS, A-MEM); gain tracks task similarity. This engine's workload
  is a task stream of repetitive desktop sequences. That favours N1/N2/N4 over any retrieval
  upgrade, and it is the same conclusion the evolution pass was built on.
- LongMemEval (2410.10813): temporal stores plus reflective re-ranking lead; the engine has
  the temporal half, N7's citation loop is the reflective half.
- "Always-On Agents" (2606.30306): the field over-invests in accumulation and under-invests
  in relinquishing state. The engine's forgetting design is ahead of the field here; N7 is
  the next step, not a reversal.

### 7.3 Ranked, for this engine

1. **N7 usefulness-fitness forgetting + citation boost** — zero requests, symbolic, closes the
   benchmark's weakest quadrant; needs M8 Phase 2 grading to exist.
2. **N1 case retrieval into the emitter** — zero requests, uses the vector lane M8 permitted,
   matches the workload's repetitiveness; the trigger is already written in evolution §10.
3. **N6 decision-aware reranking** — zero requests, upgrades M8 Phase 3 from similarity to
   utility; same dependency as N7.
4. **N2 success lane for notes** — one idle call per session; smallest change.
5. **N5 topic-boundary summarisation** — zero-request segmenter; only if the offline count
   shows the fixed cadence straddling topics.

Nothing above changes the disposition table in §0 or the not-to-change list in §6. Every
candidate is gated on the same instrument: the M8 evaluation lane. That lane, not any memory
architecture, remains the critical path.

### 7.4 Second sweep: GUI-agent, cognitive-architecture and dialogue lines

A second pass aimed at three angles the first missed. The GUI-agent line is entirely absent
from `docs/` (no hit for AppAgent, AutoDroid, UFO, Mobile-Agent, Agent S, Synapse, SkillWeaver,
CRADLE, or any UI-transition memory); the dialogue line is present only as catalogue rows in
`2026-09-07` §3. Verification: 23 IDs opened and title-matched, 12 title-matched from listings
only, one ACM paper unverifiable; the sources paragraph marks which.

| # | Architecture | What it adds | Calls | Disposition |
|---|---|---|---|---|
| S1 | **UI-transition memory** — the app as a state machine: nodes are UI states, edges are actions with their observed next state. AutoDroid's UTG (2308.15272; 71.3% completion), EAM's state graph with action-group mining and search-not-generation (2605.12294; +19.6% AndroidWorld, 6× fewer tokens), GraphPilot's validate-before-execute against stored transitions (2601.17418; "almost one LLM query" per task), UI-KOBE's node-neighbourhood as the candidate action set for a small on-device model (2605.29534), ActionEngine's program synthesis over the graph (2602.20502; **95% success at ~1 call per task vs 66%, 11.8× cheaper**) | Nothing like it exists. The verbatim log already records action → resulting `UiView`, so the graph can be **mined offline at zero requests** instead of crawled. Three symbolic uses, in order of safety: (a) the current node's known transitions *narrow* the emitter's legal set (never widen — M5 §2 holds); (b) a proposed action is checked against stored transitions, a check that becomes a gate only after its true-positive rate is measured; (c) a task matching a stored routine end-to-end replays as a compiled flow with one confirming call. (c) *is* TraceCompiler / compiled flows (evolution §10), given its missing substrate. **Prerequisite**: `UiView` carries no window or process identity (`crates/pointer/src/ui.rs:272-283`); a state key needs (process, window class, hash of the reduced control set). | 0 to mine; ~1 per task on replay | **[CANDIDATE]** — the largest requests-per-turn lever in either sweep; trigger unchanged from evolution §10 (10 sessions sharing a sequence ≥2), countable from the log today. The LLM-crawler variant is **[NOT ADOPTED]**: it spends requests and clicks a live desktop |
| S2 | **Subgoal-chunked working memory** — HiAgent (2408.09559): once a subgoal closes, its observations are replaced by one summary line; **2× success, 3.8 fewer steps** on five long-horizon tasks | M7 Phase 1 clips trace lines; M7 §1 named "one more coarsening level" and never built it. Collapsing the this-turn trace per completed sub-sequence is that level. Zero requests if the boundary rides on the action call (a `done_with` field) or on a symbolic signal (a modal closed, a window changed). Fewer steps are fewer requests. | 0 | **[CANDIDATE]** — measure trace lines per turn after Phase 1 first; if the median is already ≤5, nothing to gain |
| S3 | **Activation-scored retrieval, zero-call** — ACT-R base-level activation (recency × frequency decay) and spreading activation as ranking. SuperLocalMemory V3.3 (2604.04514): **70.4% LoCoMo with zero LLM calls on CPU**, Ebbinghaus forgetting tied to embedding compression. Hindsight (2512.12818): recall is retrieval-only (RRF + a local MiniLM cross-encoder), reflect is one opt-in call; 83.6% LongMemEval with a 20B model | `recall` ranks by BM25, M8 Phase 3 adds vector → cross-encoder. A recency-frequency prior on facts and turns is one column and one formula. Counter-evidence: vstash (2604.15484) found frequency+decay rescoring *and* cross-encoder reranking failed to beat adaptive RRF on BEIR — but BEIR is document retrieval, and M8 measured the cross-encoder helping here (paraphrase miss 33% → 25%). Hindsight's zero-call recall / one-call reflect split is exactly the engine's recall / gated-notes split. | 0 | **[MEASURE]** in the M8 suite, offline; mixed prior |
| S4 | **Amortised relevance judgements** — EARM (2608.22767): past query × memory LLM scores fill a matrix, matrix completion predicts the rest; **+6.62% accuracy with only 17.5% of candidates ever scored by the model** | The engine's reranker is local and free, so no saving there. It applies to one thing: the paid `ClientEvaluator` M8 Phase 2 needs for *corrections* (entailment, not similarity). Score 17.5%, complete the rest. | −82% evaluator calls | **[CANDIDATE]** inside M8 T2.8's budget, if and when the evaluator ships |
| S5 | **Versioned, append-only skill libraries** — Skill-Evo4GUI (2609.04869): +5.7 to +18.6 pp on OSWorld, and an honest instability finding: in-place skill edits broke their originating tasks. SkillWeaver (2504.07079): skills as code transfer +54.3% to weaker agents. Mobile-Agent-E (2501.11733): Tips + Shortcuts, +33.9 pp | The ledger, content-hash notes, and the GRASP gate with a regression budget are the defence Skill-Evo4GUI arrives at. The live `learned.toml` holds exactly one Tip. SkillWeaver's transfer result argues that compiled flows, being code, are model-agnostic even though notes are model-specific by design (evolution §10). | — | **[STANDS]** — validates the design; one note for the compiled-flows plan |
| S6 | **Pre-storage reasoning** — PREMem (2509.10852): typed fragments (factual / experiential / subjective) with cross-session relations written at store time, so small models match large ones under a token budget. TiM (2311.08719): store conclusions, not history | M6 §5.2 typed observations, Phase 5, unbuilt. Same shape. | idle | **[STANDS]** → M6 Phase 5 |
| S7 | **Decision-conflict forgetting** — DeMem (2605.10870): refine or split memory only where merging two states would change a *decision*; exact forgetting boundary, near-minimax regret | The principle behind N7: forget what never changed an action. Deciding "would change a decision" symbolically needs the manifest ∩ outcome join N7 proposes. | 0 | **[NOTED]** as N7's justification |
| S8 | **Memory-cost-aware use** — ATMem (2606.31612) trains a reward from memory-on vs memory-off rollouts, learning *when memory is worth paying for* | RL, not applicable. The ablation itself is: `ns-app eval` with and without each memory block, offline, is the zero-request version and the number M8 should print per block. | 0 | **[NOTED]** → an M8 ablation arm |
| S9 | Agent KB's disagreement gate on retrieved knowledge (2507.06229; +16 pp GAIA); Hindsight's four typed networks; SimpleMem's intent-planned retrieval scope (2601.02553; 30× fewer tokens) | Provenance + `TaintPolicy` + grounding check; facts / digests / observations / residuals; the M7 intent router. | — | **[DONE]** in different clothes |
| S10 | SYNAPSE 2026 spreading-activation graph (2601.02744); ActMem causal graph (2603.00026); Global Workspace blackboard agents (2604.08206) | Graph memory, and a shared blackboard against the per-session scope decision (M6 §12.2, zero measured leakage in scoped designs). | — | **[NOT ADOPTED]** |
| S11 | MementoGUI learned controller (2605.18652), Mem-W latent memory tokens (2605.09317; +30 pts), UFO2 knowledge substrate (2504.14603) | Trained controllers or vision-backbone tokens; UFO2's substrate is a per-app vector store of docs, demos and traces — the Windows-native precedent for S1's storage, now permitted by the vector trigger. | — | **[NOT APPLICABLE]** / UFO2 **[NOTED]** |
| S12 | Soar chunking, classical TMS belief revision | No 2024–2026 agent paper adopts either with numbers. Versioned facts already are a TMS in the small. | — | nothing to take |

### 7.5 Ranked additions from the second sweep

1. **S1 UI-transition memory mined from the log**, used first to narrow the legal set, then
   as a check, then as compiled-flow replay. It is the only mechanism in either sweep whose
   measured effect is on *requests per task* rather than tokens or accuracy. Two prerequisites,
   both cheap: window identity in `UiView`, and the sequence-sharing count from evolution §10.
2. **S2 subgoal-collapsed trace** — the coarsening level M7 left open; zero requests; measure
   trace length first.
3. **S3 activation prior on recall** — one column, one formula, decided in the M8 suite.
4. **S4 EARM for the paid evaluator** — only once the evaluator exists.

Combined with §7.3, the whole candidate set still hangs on two instruments: the M8 lane for
anything touching retrieval or forgetting, and one `ns-app budget` read of a real desktop
session for anything touching requests. Neither has been run since the log was reset.

### 7.6 Ranked for efficiency, intelligence and lowest context cost, desktop set aside

The user's stated priority on 2026-09-11, after both sweeps: not the desktop line for now;
the most efficiency and intelligence at the lowest context cost. Re-ranking both sweeps under
that single criterion, with the measured number that earns each place and what it costs in
model calls. Every item is generic; S1 (UI-transition memory) is parked.

| Rank | Mechanism | Measured | Calls | Context cost |
|---|---|---|---|---|
| 1 | **Structured zero-call recall done right** — typed stores, local rerank, an activation prior (Hindsight 2512.12818, SuperLocalMemory 2604.04514, S3) | 83.6% LongMemEval with a 20B model vs 39% full-context; 70.4% LoCoMo with zero LLM calls | 0 | Falls: the prompt carries five ranked hits instead of history. Engine has the shape; M8 Phase 3 + one activation column completes it |
| 2 | **Strategy memory from successes and failures, plus case exemplars** (ReasoningBank N2, Memento N1) | +34.2% relative effectiveness and −16% steps; +4.7–9.6 pts OOD | ~1 idle per session; 0 per turn | Small, bounded: ≤3 strategy lines and ≤2 exemplars per prompt, replacing guesswork iterations, which are the expensive kind of context |
| 3 | **Usefulness-fitness forgetting with a citation boost** (N7: Darwinian 2601.22528, RMM 2503.08026, DeMem's principle S7) | +18.0% success, +33.9% stability; >10% LongMemEval | 0 | The only lever whose context cost *falls over time*: memory that never changed an outcome stops being loaded |
| 4 | **Subgoal-collapsed working memory** (HiAgent S2) | 2× success, 3.8 fewer steps | 0 | In-turn trace shrinks to one line per closed subgoal; fewer steps are fewer requests |
| 5 | **Failure-derived compression guidelines for the summarizer** (ACON 2510.00615, §3.4) | 26–54% peak-token reduction, >95% accuracy retained, up to +46% for small-LM agents | idle only, inside M8 | Summary block smaller and better-targeted; no new mechanism, a better prompt for one that exists |
| 6 | **Engine-level context hygiene** (§5 R2–R3: read `tools_tokens`, slim schemas, obligations block, a breakpoint that can hit) | tool-token share unmeasured, estimated 13–20% of budget; obligations 5/10 → 10/10 | 0 | The floor everything else sits on; cheapest to do, and already specified |

Parked under this criterion: S1 UI-transition memory (desktop-specific, requests-first);
N8 idle anticipation (spends requests on guesses); N3 ungated cheatsheet (gains real, rule
against model-decided prose in the prompt stands; the gated form is rank 2); S4 EARM (only
matters once a paid evaluator exists). Decision aids: SimpleMem's 30× token figure (S9) comes
from intent-planned retrieval scope, which is M7's router — already banked, not a new lever.

The dependency is unchanged and worth saying once more: ranks 1, 3 and 5 are decided inside
the M8 evaluation lane and cannot be measured without it. Rank 6 needs nothing. Rank 2 needs
only the idle pass. Building M8 Phase 2 is therefore the efficiency lever, before any of the
architectures above.

## 8. Sources

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

Memory architectures (§7), every ID opened and title-matched on 2026-09-11: Memento
2508.16153 · ReasoningBank 2509.25140 · Dynamic Cheatsheet 2504.07952 · Agent Workflow Memory
2409.07429 · Nemori 2508.03341 · decision-aware memory cards / CICL 2606.08151 · Darwinian
Memory 2601.22528 · Reflective Memory Management 2503.08026 · sleep-time compute 2504.13171 ·
Context-Folding 2510.11967 · AgentFold 2510.24699 · A-MEM 2502.12110 · MemOS 2507.03724 ·
MemoryOS 2506.06326 · H-MEM 2507.22925 · Mem0 2504.19413 · Zep/Graphiti 2501.13956 · ALMA
2602.07755 · Self-GC 2607.00692 · surveys 2603.07670, 2606.30306 · "Are We Ready For An
Agent-Native Memory System?" 2606.24775 · benchmarks LoCoMo 2402.17753, LongMemEval
2410.10813, MemoryAgentBench 2507.05257, Evo-Memory 2511.20857 · RL-trained (not applicable)
Mem-α 2509.25911, AdaMEM 2606.05684, DeMem 2605.10870 · model-level (not applicable) Titans
2501.00663, Cartridges 2506.06266, SEAL 2506.10943, EM-LLM 2407.09450. MIRAS and MemoryLLM/M+
were not cited because their IDs could not be verified in the sweep.

Second sweep (§7.4), opened and title-matched: EAM 2605.12294 · ActionEngine 2602.20502 ·
UI-KOBE 2605.29534 · GraphPilot 2601.17418 · SYNAPSE 2601.02744 · ActMem 2603.00026 · Hindsight
2512.12818 · DeMem 2605.10870 · Mem-W 2605.09317 · ATMem 2606.31612 · Global Workspace Agents
2604.08206 · SimpleMem 2601.02553 · MementoGUI 2605.18652 · Skill-Evo4GUI 2609.04869 ·
SuperLocalMemory V3.3 2604.04514 · EARM 2608.22767 · vstash 2604.15484 · Human-Inspired Memory
2605.08538 · AutoDroid 2308.15272 · MemoryBank 2305.10250 · TiM 2311.08719 · ExpeL 2308.10144.
Title-matched from listings only: Mobile-Agent-E 2501.11733 · Synapse 2306.07863 · UFO2
2504.14603 · SkillWeaver 2504.07079 · Agent KB 2507.06229 · CRADLE 2403.03186 · Theanine
2406.10996 · MemInsight 2503.21760 · HiAgent 2408.09559 · SeCom 2502.05589 · PREMem 2509.10852 ·
O-Mem 2511.13593. Unverified: an ACT-R-inspired dialogue architecture (HAI 2025, ACM, 403).

Footnote, Claude Code side (out of scope, recorded once): a Claude Code subagent receives
CLAUDE.md, its delegation prompt (which this box's `graphify_gate.py` hook prefixes with the
graph-first rule), and optionally its own `memory:` directory — never the session's
`MEMORY.md`; only a `fork` inherits the conversation and the parent's prompt cache
(code.claude.com/docs/en/sub-agents, /memory, /prompt-caching).
