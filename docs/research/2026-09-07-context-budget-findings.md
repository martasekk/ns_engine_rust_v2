# Research findings — agent memory, context budgets, harness design (2026 sweep)

Compiled 2026-09-05 as a standalone brief; filed here 2026-09-07 as the evidence base for
`docs/superpowers/plans/2026-09-07-m7-context-budget.md`. Every arXiv ID was resolved
against the arXiv API when the brief was compiled; numbers are the papers' own, not
reproduced. Sections 1–6 are the brief as compiled. The disposition table is new and maps
each finding onto what `ns-engine` already does, following the convention of
`2026-09-02-memory-findings.md`.

Legend: **[DONE]** already in the engine · **[PARTIAL]** shape exists, a piece is missing ·
**[ADOPTED]** shapes M7 · **[LATER]** designed-for, built later · **[DISCARDED]** considered
and rejected, with the recorded reason.

## Disposition against ns-engine (2026-09-07)

| # | Finding | Where the engine stands (main @ a478d46, branch `messages-client`) | Disposition |
|---|---|---|---|
| 01 | Context growth degrades reasoning before the window is full | Window (6 turns), facts (≤10), summary (≤800 chars) are all capped. The this-turn **trace** is not, on `main`: `turn_trace` is re-sent on every emitter iteration. The `messages-client` branch clips each line at 1200 chars. | **[PARTIAL]** → M7 Phase 1 |
| 02 | Less, better-chosen context beats full history | The cross-turn shape of 2606.10209 (last-N verbatim + running summary) is M6 §4–§5 as built. Within a turn there is no last-N; nothing measures tokens. | **[PARTIAL]** → M7 Phases 0, 1 |
| 03 | Split write from read; consolidation off the critical path | Summary runs during the wait for the next message; consolidation on the idle timer / `ns-app evolve`. | **[DONE]**; extended by Phase 4 (session digests) |
| 04 | Route by intent first, tier-specific budgets | None. Every turn carries the full legal set, the full context, and `recall` costs an emitter iteration. | **[ADOPTED]** → M7 Phase 3 (symbolic router) |
| 05 | Cheap construction + smart utilization beats expensive graphs | Lexical FTS5, no graph, summary depth 1 — by decision (M6 §12.8, §13). | **[DONE]** |
| 06 | Governed context objects with recoverable archives | `http_tool` archives oversized bodies as artifacts; `pointer_ui_read` does not; the branch's clip drops the tail with no handle; no budget meter. | **[PARTIAL]** → M7 Phases 1, 2 |
| 07 | Multi-view graphs and temporal validity | Facts carry validity intervals and state labels (M6 §6.1). Graphs discarded 2026-09-02 (fidelity gap, 2601.00821). | **[DONE]** for facts; graphs **[LATER]**, trigger unchanged |
| 08 | Forgetting is a feature | cold / forgotten / purge-External in `evolution/consolidate.rs`. | **[DONE]** |
| 09 | Procedural memory improves the harness | M5 learned rules, notes, ledger. | **[DONE]** |
| 10 | Train the memory / compaction policy | Out of scope (parent spec §13). | **[DISCARDED]** |
| 11 | The harness is the binding constraint; regression-test per release | Replay harness with scripted doubles; M5 gates. No fixed-model task set, no per-release metric row. | **[PARTIAL]** → M7 Phase 5 |
| §4 | Temporal as the runtime | The engine is already an event-sourced, hash-chained, replayable loop; every non-deterministic step sits behind a trait. | **[NOT ADOPTED]** — mapping and trigger in the plan, §3 |
| §2 04/11 | Validator → escalate to a large model | Discarded 2026-09-01 by user decision (`2026-09-01-findings.md` §6). | **[DECISION]** re-raised in the plan, §9, off by default |

---

## 0. Context for a fresh session

**Goal this was gathered for:** build an agent harness where a *small* model performs like a
big one, by controlling what enters the context window rather than by scaling the model.
Runtime target is Temporal (durable workflows + stateless workers).

**Three claims worth carrying:**
1. Context growth degrades reasoning before the window is full, so a token diet is an
   accuracy strategy, not only a cost strategy.
2. The harness (routing, memory, context lifecycle), not the model, is the binding
   constraint — and harness gains are *largest* for the weakest models.
3. Memory decomposes into three operators: extraction, coarsening, traversal. Those map
   cleanly onto Temporal Activities and Workflows.

---

## 1. Corrections to three previously-cited papers

These three were the starting point. Two had errors.

### A. Toward a Theory of Hierarchical Memory for Language Agents — VERIFIED
`arXiv:2603.21564` · 23 Mar 2026

Defines three operators:
- **Extraction (α)** — raw data to atomic information units.
- **Coarsening (C = (π, ρ))** — π partitions units into groups, ρ assigns a representative
  (summary) to each group.
- **Traversal (τ)** — selects which units enter the context given a query and a token budget.

Two details often missed: the paper defines a **self-sufficiency spectrum** for the
representative function ρ and shows it constrains which retrieval strategies are viable
(a coarsening–traversal coupling); and it instantiates the decomposition on **eleven existing
systems**, so it doubles as a comparison chart.

### B. SimpleMem — ID CORRECT, TITLE AND NUMBERS WERE WRONG
`arXiv:2601.02553` · 5 Jan 2026 · Semantic Scholar: 117 citations

Real title: **"SimpleMem: Efficient Lifelong Memory for LLM Agents"** (not "An Efficient
Memory Framework for Lifelong LLM Interaction").

Three stages: Semantic Structured Compression; **Online Semantic Synthesis** (intra-session
and immediate — *not* an asynchronous background job); Intent-Aware Retrieval Planning.

Real results: **+26.4% average F1 on LoCoMo**, inference-time tokens cut **up to 30x**.
The "80%+ token reduction" figure does not appear in this paper — that number belongs to
AMA (`2601.20352`). Code: https://github.com/aiming-lab/SimpleMem

### C. G-Memory — WRONG ID, AND THE CAUSAL-EDGE DESCRIPTION IS NOT IN IT
Real ID: `arXiv:2506.07398` · 9 Jun 2025 · Semantic Scholar: 87 citations

`2510.12345` resolves to a paper on Carleman estimates for backward anisotropic stochastic
parabolic equations — unrelated.

The real G-Memory is a memory system for **multi-agent** systems grounded in organizational
memory theory. It uses a **three-tier graph hierarchy** — insight graph, query graph,
interaction graph — with bi-directional traversal on each new query. It does **not** define
`CausedBy` / `Supersedes` / `DependsOn` edges.

If causal-dependency edges are what you want, the verified papers closest to that idea are:
- **MAGMA** `2601.03236` — separate semantic, temporal, causal and entity graphs; retrieval
  as policy-guided traversal.
- **Zep** `2501.13956` — temporal knowledge graph with fact validity intervals.
- **HiGram** `2608.05095` — path-level localization and coordinated rewrite of inter-unit
  dependencies.
- **MemIR** `2605.25869` — typed memory that keeps provenance roles distinct.

---

## 2. Eleven findings, with the numbers behind them

### 01 — Context growth degrades reasoning non-uniformly, before the window is full
- **Lost in the Middle** `2307.03172` — U-shaped position sensitivity.
- **Same Task, More Tokens** `2402.14848` — isolates *length itself* as the cause.
- **RULER** `2404.06654` and **NoLiMa** `2502.05167` — effective context is far below the
  advertised number once literal lexical matching is removed.
- **Context Rot** (Chroma, https://www.trychroma.com/research/context-rot) — 18 models
  tested; a *coherent* haystack hurts more than a shuffled one; low needle–question
  similarity degrades faster with length.
- **ACON** `2510.00615` — names the agent-specific mechanism, "context distraction" by
  irrelevant history, and reports it hits small models hardest.

### 02 — Less, better-chosen context beats full history on accuracy AND cost
| Result | Source |
|---|---|
| Completion 71% → 92% (full history vs. last-5 tool calls + summary) | Less Context, Better Agents `2606.10209` |
| Tokens 1,480,996 → 553,374 in that same study | `2606.10209` |
| 26–54% peak token reduction *while improving* success | ACON `2510.00615` |
| MEM1-7B beats Qwen2.5-14B by 3.5x with 3.7x less memory | MEM1 `2506.15841` |
| ~80% token reduction vs. full context | AMA `2601.20352` |
| −52% recalled memory length at state-of-the-art accuracy | TiMem `2601.02845` |

This is the core argument for the small-model plan.

### 03 — Split the write path from the read path; make consolidation parallel and off the critical path
- **MemForest** `2605.23986` — reframes memory as a write-efficient temporal data-management
  problem: parallel extraction, hierarchical temporal tree, **localized dirty-path refresh**
  instead of global rewrites. 6–9.5x higher build rate than EverMemOS at equal accuracy.
- **GAM** `2604.12285` — decouples encoding from consolidation; merges the live event graph
  into the long-term topic network only on a semantic shift.
- **LightMem (SLM)** `2604.07798` — online retrieval under a fixed budget (83 ms median)
  separated from offline consolidation.
- **Sleep-time Compute** `2504.13171` — pre-compute over context while idle.

### 04 — Route by intent first, then compile evidence under a tier-specific budget
- **MemFlow** `2605.03312` — Router classifies the query; Memory agent runs one of three
  tiers (Profile Lookup, Targeted Retrieval, Deep Reasoning) under a tier-aware token
  budget; Answer agent writes from the compact pack; Validator retries with a heavier tier
  only when the answer is unsupported. On a frozen **Qwen3-1.7B** this nearly doubles
  accuracy over full-context prompting.
- Same idea at other granularities: SimpleMem's intent-aware retrieval planning, TiMem's
  complexity-aware recall, **Self-Route** `2407.16833` (RAG vs. long-context per query).

### 05 — Cheap construction + smart utilization beats expensive graph building
- **Chain-of-Memory** `2601.14287` — complex construction gives marginal gains at high cost.
  Lightweight storage plus organizing retrieved fragments into inference paths with adaptive
  truncation: **+7.5–10.4% accuracy at ~2.7% of the tokens and 6% of the latency**.
- **LightMem** `2510.18866` — same efficiency case.

### 06 — Treat in-run context as governed, addressable objects with recoverable archives
- **Self-GC** `2607.00692` — indexes turns, tool spans and skill state as objects; a
  side-channel planner proposes fold/mask/prune; the harness enforces recoverable sidecars,
  safe commit boundaries, cache-aware commits. **44% of prefix tokens pruned with 85% of
  future continuations unaffected**; 10–15% lower daytime input tokens in production.
- **VISTA** `2606.30005` — a "proprioception" dashboard of block sizes, recency and remaining
  budget. Lifts Gemini-3-Flash from **22.7% to 50.7%** on LOCA-Bench, training-free.
- **Context-Folding** `2510.11967`, **AgentFold** `2510.24699`, **MemAct** `2510.12635`,
  **Focus** `2601.07190` (22.7% fewer tokens at equal accuracy, Claude Haiku 4.5) — give the
  agent explicit fold/consolidate actions.
- **Everything is Context** `2512.05470` — file-system abstraction for the same objects.

### 07 — Multi-view graphs and temporal validity answer "why"; flat vector stores do not
MAGMA `2601.03236`, Zep `2501.13956`, HiGram `2608.05095`, MemIR `2605.25869`.
MemIR's failure mode is worth naming: flat text storage causes **provenance-role collapse**,
where the agent forgets who said what.

### 08 — Forgetting is a feature: decay, tombstone, reconcile
- **FadeMem** `2601.18642` — differential exponential decay by relevance, access frequency and
  recency across a dual-layer hierarchy, with LLM-guided conflict resolution. **−45% storage**
  with improved multi-hop retrieval.
- **FSFM** `2604.20300`, **Nemori** `2508.03341` (learned "what deserves memory").
- **AMA** `2601.20352` — a Judge detects conflicts, a Refresher updates or removes stale
  entries.

### 09 — Procedural/experiential memory improves the harness without touching weights
Agent Workflow Memory `2409.07429` · ReasoningBank `2509.25140` · Memp `2508.06433` ·
ACE `2510.04618` (evolves a playbook while avoiding "context collapse" from
over-summarization) · Dynamic Cheatsheet `2504.07952` · Reflexion `2303.11366`.

### 10 — If you can fine-tune, train the memory or compaction policy itself
MEM1 `2506.15841` · Memory-R1 `2508.19828` · MemPO `2603.00680` (+7.1 F1 over prior SOTA,
67–73% fewer tokens) · MMPO `2605.30159` (penalizes summaries that raise "belief entropy";
holds 97.1% performance at 1.75M-token contexts) · CompactionRL `2607.05378` (+5.5 to +7
points on SWE-bench Verified for 30B–106B open models) · ContextBudget/BACM-RL `2604.01664`
(>1.6x gains at high complexity).

Training-free routes (findings 04 and 06) get most of the way there; these show the ceiling.

### 11 — The harness is the binding constraint; orchestrator capacity dominates
- **Can Small Agents Collaborate to Beat a Single LLM?** `2601.11327` — a minimal
  orchestrator plus specialized small sub-agents with restricted communication beats much
  larger single models *even when those have tools*. **Reasoning at the orchestrator gives
  the largest gains; reasoning in sub-agents gives limited or negative benefit.**
- **HarnessX** `2606.14249` — evolving the harness from traces: **+14.5% average, up to +44%,
  with gains largest where baselines are lowest.**
- **Don't Blame the LLM** `2607.03691` — holding the model fixed across 35 harness releases
  produces measurable quality swings that practitioners misattribute to the model.
- **HarnessBridge** `2606.12882`, **EvoHarness-RL** `2608.05446` — learn the
  observation/action projection and the harness-use policy.
- Cost routing complements: RouteLLM `2406.18665` · FrugalGPT `2305.05176` ·
  Talker-Reasoner `2410.08328` · Scaling test-time compute `2408.03314`.

---

## 3. Paper catalogue by use

### Theory and surveys
| Paper | arXiv | Date | Use for |
|---|---|---|---|
| Toward a Theory of Hierarchical Memory for Language Agents | 2603.21564 | Mar 2026 | The α / C / τ vocabulary; eleven-system comparison |
| Memory for Autonomous LLM Agents: Mechanisms, Evaluation, Frontiers | 2603.07670 | Mar 2026 | Survey; names continual consolidation, causally grounded retrieval, learned forgetting as open |
| Cognitive Architectures for Language Agents (CoALA) | 2309.02427 | Sep 2023 | Working/episodic/semantic/procedural split |
| Position: Episodic Memory is the Missing Piece | 2502.06975 | Feb 2025 | Case for instance-specific single-shot memory |
| Governing Evolving Memory in LLM Agents (SSGM) | 2603.11768 | Mar 2026 | Semantic drift, poisoning, governance |
| What makes a harness a harness | 2606.10106 | Jun 2026 | Definitions; product vs. scaffold vs. eval harness |
| Agent Design Pattern Catalogue | 2405.10467 | May 2024 | Pattern names for design docs |

### Hierarchical and tiered memory systems
| Paper | arXiv | Date | Steal this |
|---|---|---|---|
| SimpleMem | 2601.02553 | Jan 2026 | Structured compression at ingest; intent-aware retrieval scope |
| MemForest | 2605.23986 | May 2026 | Parallel extraction; dirty-path refresh; freshness latency as a metric |
| TiMem | 2601.02845 | Jan 2026 | Temporal Memory Tree; complexity-aware recall |
| GAM | 2604.12285 | Apr 2026 | Encode/consolidate decoupling; merge on semantic shift |
| H-MEM (EACL 2026) | 2507.22925 | Jul 2025 | Positional index per layer; confidence-weighted retrieval |
| Lightweight LLM Agent Memory with SLMs | 2604.07798 | Apr 2026 | STM/MTM/LTM; online–offline split; two-stage coarse-then-rerank |
| MemFlow | 2605.03312 | May 2026 | Router → three tiers → Answer → Validator |
| HMARS | 2606.28349 | Jun 2026 | Sub-agents own bounded memory regions |
| Multi-Layered Memory Architectures | 2603.29194 | Mar 2026 | Working/episodic/semantic with retrieval gating; false-memory rate |
| AMA | 2601.20352 | Jan 2026 | Constructor/Retriever/Judge/Refresher roles |
| Chain-of-Memory | 2601.14287 | Jan 2026 | Cheap construction, path-organized utilization |
| LightMem | 2510.18866 | Oct 2025 | Low-overhead sensory/short/long pipeline |
| MemGPT | 2310.08560 | Oct 2023 | Virtual context management; paging |
| Mem0 | 2504.19413 | Apr 2025 | Production extract/consolidate/retrieve loop |
| A-MEM | 2502.12110 | Feb 2025 | Zettelkasten-style linking |
| MemOS | 2507.03724 | Jul 2025 | Memory as a schedulable OS resource |
| MemoryOS | 2506.06326 | May 2025 | Short/mid/long with heat-based promotion |
| MIRIX | 2507.07957 | Jul 2025 | Typed memory managed by specialist agents |
| HiAgent | 2408.09559 | Aug 2024 | Subgoal-chunked working memory |
| MemTree | 2410.14052 | Oct 2024 | Dynamic tree schemas |
| MemInsight | 2503.21760 | Mar 2025 | Autonomous attribute augmentation |
| MemoryBank | 2305.10250 | May 2023 | Ebbinghaus forgetting, first version |
| Think-in-Memory | 2311.08719 | Nov 2023 | Store post-thinking conclusions, not raw history |
| Generative Agents | 2304.03442 | Apr 2023 | Recency x importance x relevance; reflection trees |

### Graph, temporal and causal memory
| Paper | arXiv | Date | Steal this |
|---|---|---|---|
| MAGMA | 2601.03236 | Jan 2026 | Semantic/temporal/causal/entity graphs; policy-guided traversal |
| G-Memory | 2506.07398 | Jun 2025 | Insight/query/interaction tiers for multi-agent |
| Zep | 2501.13956 | Jan 2025 | Fact validity intervals; invalidate, don't delete |
| HiGram | 2608.05095 | Aug 2026 | Coordinated rewrite of a unit and its dependencies |
| MemIR | 2605.25869 | May 2026 | Typed records keeping source and role separate |
| HippoRAG / HippoRAG 2 | 2405.14831 / 2502.14802 | 2024–25 | Personalized PageRank over a KG |
| RAPTOR | 2401.18059 | Jan 2024 | Recursive cluster-and-summarize tree (canonical coarsening) |
| MemoRAG | 2409.05591 | Sep 2024 | Cheap global-memory model drafts clues, expensive model answers |

### In-run context lifecycle and compression
| Paper | arXiv | Date | Steal this |
|---|---|---|---|
| Self-GC | 2607.00692 | Jul 2026 | Fold/mask/prune with recoverable sidecars, cache-aware commits |
| VISTA | 2606.30005 | Jun 2026 | Typed addressable blocks + token dashboard the model sees |
| Less Context, Better Agents | 2606.10209 | Jun 2026 | Last-N tool spans + running summary |
| ACON | 2510.00615 | Oct 2025 | Refine compression guidelines from failure analysis; distill compressor into a small model |
| Context-Folding | 2510.11967 | Oct 2025 | Branch into sub-trajectory, fold on return |
| AgentFold | 2510.24699 | Oct 2025 | Proactive multi-scale folding |
| MemAct | 2510.12635 | Oct 2025 | Context curation as policy actions |
| Active Context Compression (Focus) | 2601.07190 | Jan 2026 | Agent-triggered consolidation into a Knowledge block |
| ReSum | 2509.13313 | Sep 2025 | Periodic summarization without architectural change |
| Everything is Context | 2512.05470 | Dec 2025 | Mount context artefacts as files |
| Recursive Language Models | 2512.24601 | Dec 2025 | Long prompt as an environment the model recurses over |
| ReadAgent | 2402.09727 | Feb 2024 | Gist memory + on-demand page lookup, 20x effective context |
| Chain of Agents | 2406.02818 | Jun 2024 | Workers pass a running summary along a chain |
| Sleep-time Compute | 2504.13171 | Apr 2025 | Pre-compute over context while idle |

### Learned memory / compaction policies (need training)
MEM1 `2506.15841` · MemPO `2603.00680` · MMPO `2605.30159` · CompactionRL `2607.05378` ·
ContextBudget (BACM-RL) `2604.01664` · Memory-R1 `2508.19828` · MemAgent `2507.02259` ·
Nemori `2508.03341`

### Forgetting, procedural memory, playbooks
FadeMem `2601.18642` · FSFM `2604.20300` · Agent Workflow Memory `2409.07429` ·
ReasoningBank `2509.25140` · Memp `2508.06433` · ACE `2510.04618` ·
Dynamic Cheatsheet `2504.07952` · Learn-by-interact `2501.10893` · Reflexion `2303.11366`

### Token/KV-level compression (inference side)
LLMLingua `2310.05736` / LongLLMLingua `2310.06839` / LLMLingua-2 `2403.12968` ·
RECOMP `2310.04408` · Gist tokens `2304.08467` / ICAE `2307.06945` /
AutoCompressors `2305.14788` · StreamingLLM `2309.17453` / H2O `2306.14048` /
SnapKV `2404.14469` · EM-LLM `2407.09450` · Memory Layers at Scale `2412.09764` / M+ `2502.00592`

### Long-context limits
Lost in the Middle `2307.03172` · Same Task More Tokens `2402.14848` · RULER `2404.06654` ·
NoLiMa `2502.05167` · RAG-or-Long-Context (Self-Route) `2407.16833` ·
Context Rot (Chroma technical report)

### Small models, routing, harness engineering
| Paper | arXiv | Date | Steal this |
|---|---|---|---|
| Can Small Agents Collaborate to Beat a Single LLM? | 2601.11327 | Jan 2026 | Reasoning budget in the orchestrator; sub-agents cheap and narrow |
| HarnessX | 2606.14249 | Jun 2026 | Typed harness primitives; trace-driven evolution (AEGIS) |
| Don't Blame the LLM | 2607.03691 | Jul 2026 | Fix the model, vary the harness; regression-test releases |
| Agentic Harness Engineering | 2604.25850 | Apr 2026 | Observability-driven automatic harness edits |
| HarnessBridge | 2606.12882 | Jun 2026 | Learned observation/action projections |
| EvoHarness-RL | 2608.05446 | Aug 2026 | Belief/Progress/Experience as harness state; "harness annealing" |
| Harness Updating Is Not Harness Benefit | 2605.30621 | May 2026 | Base capability doesn't predict self-evolution benefit |
| Harness Handbook | 2607.13285 | Jul 2026 | Keeping a fast-changing harness navigable |
| Small Language Models are the Future of Agentic AI | 2506.02153 | Jun 2025 | LLM-to-SLM conversion procedure in the appendix |
| RouteLLM / FrugalGPT | 2406.18665 / 2305.05176 | 2024/2023 | Preference-trained routers; escalation cascades |
| Talker-Reasoner | 2410.08328 | Oct 2024 | Fast conversational model + slow planner sharing belief state |
| Scaling Test-Time Compute Optimally | 2408.03314 | Aug 2024 | Compute-optimal allocation per prompt difficulty |
| Mixture-of-Agents / Archon | 2406.04692 / 2409.15254 | 2024 | Layered aggregation; inference-architecture search |
| DSPy / AFlow | 2310.03714 / 2410.10762 | 2023/2024 | Compile and search pipelines instead of hand-tuning prompts |

### Orchestration, durable execution, benchmarks
- **Temporal Agent Harness** (experimental, Python) —
  https://github.com/temporal-community/temporal-agent-harness ·
  https://temporal.io/blog/temporal-agent-harness-durable-agent-infrastructure
  Every agent is a Workflow. Three tool flavors: `@agent.activity_tool_defn` (durable,
  retried Activity), `@agent.tool_defn` (inline in the workflow),
  `@agent.callback_tool_defn` (runs on an attached client; the workflow pauses durably).
  Standardized AgentEvent stream. Plugins for Google Gemini, OpenAI Agents SDK, Pydantic AI.
- Open Agent Specification `2510.04173` · Production-Grade Agentic Workflows guide
  `2512.08769` · AIOS `2403.16971`
- Agent Harness survey — DOI 10.20944/preprints202604.0428.v3 (preprints.org, *not* arXiv);
  paper list at https://github.com/Gloriaameng/Awesome-Agent-Harness
- Benchmarks: LoCoMo `2402.17753` · LongMemEval `2410.10813` · MemoryAgentBench `2507.05257`

---

## 4. Architecture the brief proposed: operators mapped onto Temporal

Kept as compiled; the plan's §3 gives the ns-engine equivalent and the reasons Temporal is
not adopted now.

```
                    +--------------------------------+
                    |      Agent event stream        |
                    |  traces -> offline harness loop|
                    +--------------------------------+
                        ^                        |
                        | emits                  | tunes prompts,
                        |                        v compressor, routing
  +--------------------------------------------------------------+
  | AGENT WORKFLOW (one per task, deterministic, event-sourced)   |
  |                                                               |
  |  Context ledger                    Turn loop                  |
  |  - typed blocks                    1. route intent -> tier    |
  |  - system/task/plan                2. tau retrieve -> pack    |
  |  - working set (last-N spans)      3. model call -> tools     |
  |  - evidence pack, knowledge        4. validate; escalate      |
  |  - archive refs                    5. emit events; fold;      |
  |  - token meter (8.4K/12K)             continue-as-new         |
  +--------------------------------------------------------------+
            |            |            |             |
            v            v            v             v
  +----------+  +-------------+  +-------------+  +------------+
  | llm-small|  | llm-large   |  | memory-read |  | memory-    |   + tools
  | many     |  | few, rate-  |  | (tau)       |  | write (α)  |
  | replicas |  | limited,    |  | coarse ->   |  | atomic     |
  | orches-  |  | only after  |  | rerank ->   |  | units,     |
  | trator + |  | validator   |  | graph by    |  | fan-out,   |
  | answerer |  | fails       |  | tier; hard  |  | idempotent |
  |          |  |             |  | token cap   |  | by turn id |
  +----------+  +-------------+  +-------------+  +------------+
                                                        | signal
                                                        v
  +--------------------------------+   +---------------------------------+
  | CONSOLIDATOR WORKFLOW          |-->| STORES                          |
  | (long-lived, per user/thread)  |   | atomic units <s,p,o,t_valid,    |
  | triggers: semantic shift,      |   |   t_invalid, source>            |
  |   N new units, idle timer      |   | summary tree (representatives)  |
  | pi: cluster -> dirty-path      |   | edges: semantic/temporal/       |
  |     refresh of parents only    |   |   causal/entity                 |
  | rho: regenerate reps;          |   | archive blobs (full-fidelity,   |
  |     reconcile -> tombstone     |   |   recoverable tool outputs)     |
  | decay: relevance x access x    |   | playbook: procedures,           |
  |     recency; prune             |   |   strategies, guidelines        |
  | distill procedures from done   |   +---------------------------------+
  +--------------------------------+
```

| Concern | Temporal construct | Design rule (source) |
|---|---|---|
| Agent loop | Workflow, one per task | Deterministic code only. Holds context ledger + token meter as workflow state. |
| Model call | Activity on `llm-small` / `llm-large` | Retries and rate-limit backoff in the Activity policy. Escalate only when a validator says the small answer is unsupported (MemFlow, FrugalGPT). Put the reasoning budget in the orchestrator (2601.11327). |
| Extraction α | Activities on `memory-write`, fanned out per turn | Idempotent by turn id so replay never double-writes. Runs *after* the reply is sent (MemForest, SimpleMem). |
| Coarsening C | Separate long-lived Consolidator Workflow, Signal- and timer-driven | Dirty-path refresh only (MemForest, TiMem). Merge on semantic shift, not every turn (GAM). Reconcile by invalidating with a validity interval, not deleting (Zep, SimpleMem). Decay and prune (FadeMem). Expensive passes during idle (Sleep-time Compute). |
| Traversal τ | Activity on `memory-read` with an explicit token cap argument | Classify intent first, then tier and budget (MemFlow, SimpleMem). Coarse vector → rerank → graph traversal only for the deep tier (LightMem-SLM, MAGMA). Return a compact evidence pack with source ids (Chain-of-Memory). |
| Tool output bloat | Activity tools returning a truncated view + archive ref | Keep last N spans verbatim + running summary — 71% → 92% completion at a third of the tokens (2606.10209). Full output to an archive blob fetchable by id (Self-GC, VISTA). |
| Fold / prune decisions | Side-channel planner Activity on `llm-small`, committed at safe boundaries | Show the model its own budget (VISTA). Fold completed sub-trajectories (Context-Folding). Commit at cache-aware boundaries so prefix caching survives (Self-GC). |
| Bounding Temporal's own history | `continue-as-new` carrying the compacted ledger | Event history grows with every Activity; roll over with the folded ledger as input. |
| Human in the loop | Callback tools + Signals | Workflow pauses durably for minutes or days without holding a worker. |
| Multi-agent handoff | Child Workflows via typed operations | Sub-agents get bounded memory regions and their own budgets (HMARS). Restrict communication to typed I/O; don't let sub-agents reason freely (2601.11327). |
| Observability → improvement | AgentEvent stream consumed by an offline job | Feed failures into compressor guidelines (ACON), routing thresholds, playbook (ACE, ReasoningBank). Regression-test each harness release against a fixed model and task set (2607.03691). |

**Two invariants that make the budget hold by construction:**
1. *Determinism* — the Workflow never calls a model or store directly. Every
   non-deterministic step is an Activity with an idempotency key, so replay after a crash
   reproduces the same ledger.
2. *Budget* — the token meter is workflow state. Folding, archiving and continue-as-new are
   decided from it, so context size is bounded regardless of task length.

**What Temporal does NOT give you:** neither the Agent Harness README nor the blog post
describes context compaction, summarization, or history-size handling. That layer is yours
to build, and it is exactly the α / C / τ machinery above. The harness is marked
experimental and pre-1.0, with APIs that change between releases.

---

## 5. Build order and metrics (as proposed in the brief)

1. **Ledger and truncation first.** Typed context blocks, token meter, last-N tool spans +
   running summary, archive refs. No memory store yet. This alone captures the biggest gain
   in the literature (finding 02).
2. **Extraction α as fan-out Activities.** Atomic units with validity times and source.
   Idempotent writes.
3. **Traversal τ with an intent router and per-tier caps.** Start with three tiers (MemFlow).
   Measure evidence-pack size, not just accuracy.
4. **Consolidator Workflow.** Dirty-path summaries, contradiction invalidation, decay.
   Trigger on semantic shift and idle timers.
5. **Escalation cascade.** Validator on the small model's answer; large queue only on
   failure. Log the escalation rate.
6. **Offline loop.** Mine the AgentEvent stream for failures; update compressor guidelines
   and playbook. Re-run a fixed task set before each harness release.

**Metrics the papers agree on**
- Tokens per completed task, and peak context size (ACON, Less Context Better Agents)
- Escalation rate to the large model, and accuracy with/without it (FrugalGPT, MemFlow)
- Memory freshness latency: fact appears → fact is retrievable (MemForest)
- No-impact rate of pruning: share of future steps unaffected by removals (Self-GC)
- Recalled memory length at fixed accuracy (TiMem); false-memory rate (2603.29194)
- Benchmarks: LoCoMo and LongMemEval-S for the memory layer; MemoryAgentBench for
  incremental multi-turn; your own fixed task set for harness regressions

---

## 6. Caveats — read the evidence with these in mind

- Most 2026 entries are **preprints with single-digit citation counts**.
- Several benchmark on **LoCoMo / LongMemEval-S only**, where a fair share of gains come from
  better question routing on conversational QA rather than tool-heavy agent runs.
- **SLMs for Efficient Agentic Tool Calling** `2512.15943` reports 77.55% on ToolBench from a
  one-epoch fine-tune of OPT-350M against weak baselines. Treat as a lead, not evidence.
- The Agent Harness survey is on **preprints.org, not arXiv**.
- Semantic Scholar citation lookups succeeded for only about a third of these papers; absence
  of a count is not evidence of low impact.
- One candidate ID (`2509.25911`) failed to resolve after retries and was dropped rather
  than guessed.
- **The architecture in section 4 is not proven optimal.** It is the shape the evidence
  supports for a small model under a fixed budget. Real trade-offs: an Activity per model
  call buys per-step retries but bloats Temporal's event history (alternative: one Activity
  per whole turn with heartbeats); a separate consolidator costs freshness lag and a second
  operational surface; a task queue per model size is premature with a single endpoint; the
  multi-view graph store is the most speculative piece, and Chain-of-Memory suggests starting
  with flat units plus a summary tree.
