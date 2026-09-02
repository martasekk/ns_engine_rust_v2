# Research findings — agent memory structures and optimization (2026 sweep)

Compiled 2026-09-02 for the M6 memory redesign
(`docs/superpowers/specs/2026-09-02-memory-redesign-design.md`, revision 2).
Method: 16 web searches, then the arXiv abstracts of 30 papers read directly; claims below
are taken from those abstracts, not from search snippets or vendor blogs. Numbers are as
reported by the authors and were not reproduced.

Legend: **[ADOPTED]** shapes M6 · **[CHANGES]** overturned a revision-1 decision ·
**[VALIDATES]** confirms an existing choice · **[LATER]** designed-for, built later ·
**[DISCARDED]** considered and rejected.

---

## 1. Representation: verbatim first, structure as augmentation

- **Fidelity Before Structure / "Verbatim chunks beat extracted artifacts"** —
  arxiv.org/abs/2601.00821 — **[CHANGES]**
  Controlled ablation in a fixed retrieve–rerank–reason pipeline, swapping only the stored
  representation: verbatim conversation chunks vs LLM-extracted facts/decisions/events.
  LoCoMo 43.9% vs 28.0%; LongMemEval-S 67.4% vs 45.4%. A semantic graph did not close the
  gap. Diagnosis: "lossy distillation, not structure per se"; accuracy tracked how much
  original text survived. Recommendation: structured memory should *augment* verbatim
  text, never replace it. Caveat: verbatim chunks had worse *abstention*.
  → Revision 1 leaned on facts + summary as the memory; revision 2 makes verbatim turn
  records and FTS over the raw log the primary store, with summary/facts/observations as
  indexes and abstention aids. Recall ranks verbatim hits above derived ones.
- **E-mem: episodic context reconstruction** — arxiv.org/abs/2601.21714 — **[VALIDATES]**
  Preprocessing into embeddings/graphs "severs contextual integrity"; reconstructing the
  episode from uncompressed context at read time reaches >54% F1 on LoCoMo (+7.75 over
  GAM) at >70% lower token cost. → Turn records are a read-time projection of the log.
- **Memory is Reconstructed, Not Retrieved (MRAgent)** — arxiv.org/abs/2606.06036 —
  **[LATER]** Iterative explore-and-prune over a cue–tag–content graph, up to +23% over
  strong baselines with lower cost. The iterative part is the `recall` tool being called
  more than once per turn; the graph is not adopted (see §1 first item, §3 A-TMA).
- **Structured belief state / PrecisionMemBench (Tenure)** — arxiv.org/abs/2605.11325 —
  **[ADOPTED]** Injecting a *typed* belief state as ambient instruction "before the model
  sees the prompt" removes the model's choice of whether to consult memory; dumping the
  whole store achieves perfect recall while masking precision failures (baseline
  precision ≤ 0.22). → Facts stay in the context (not only behind a tool), but a small,
  relevance-selected slice, not 20 by alphabet.
- **Memory Makes the Difference (memory roles)** — arxiv.org/abs/2606.25361 —
  **[VALIDATES]** Clarifying memory improves factual accuracy and constraint awareness;
  *irrelevant* memory reduces topic relevance and constraint awareness. → Precision of the
  facts slice matters more than its size.
- **Governed Memory** — arxiv.org/abs/2603.17787 — **[ADOPTED]** Hybrid of open-set facts
  plus schema-enforced properties; entity isolation with zero cross-entity leakage over
  500 adversarial queries; progressive delivery −50% tokens; *quality saturates at about
  seven governed memories per entity*; 74.8% LoCoMo under governance. → Fact scope column
  now; `facts_in_context` lowered from 20 to 10 (pinned core + query-relevant).

## 2. Hierarchy and consolidation

- **TiMem: temporal-hierarchical consolidation** — arxiv.org/abs/2601.02845 —
  **[VALIDATES]** A temporal memory tree from raw observations up to abstracted
  representations; complexity-aware recall. 75.30% LoCoMo, 76.88% LongMemEval-S, with
  recalled memory length −52.20%. → Window → summary → observations is the same shape;
  recall depth should scale with question complexity (`recall` may be called again).
- **Agentic Context Management** — arxiv.org/abs/2607.21503 — **[VALIDATES]** Five
  primitives: architecting, ingesting, scoping, anticipating, compacting & consolidation.
  Naive accumulation is quadratic in tokens; plain summarization is linear but degrades;
  *validated* compaction is linear with quality kept (92% LongMemEval, 93.2% LoCoMo for
  their implementation). → "Anticipating" is the idle driver; compaction must be checked,
  which our evaluator and replay do.
- **SSGM: Governing evolving memory** — arxiv.org/abs/2603.11768 — **[ADOPTED]** Names
  *semantic drift through iterative summarization* and topology-induced leakage of
  sensitive context into long-term storage; proposes consistency checks before
  consolidation, temporal decay, access control, and decoupling memory evolution from
  execution. → The rolling summary is regenerated from verbatim records (not
  summary-of-summary chains) and carries the minimum trust of its inputs.
- **Self-GC: self-governing context** — arxiv.org/abs/2607.00692 — **[VALIDATES]** Context
  management as lifecycle control over indexed objects with fold/mask/prune, recoverable
  backups and safe commit points; 43.95% of prefix tokens pruned with 84.85% of future
  continuations unaffected (heuristics 54.55–69.70%); production no-impact 91.27–94.58%.
  → Our "backup" is the event log itself; every compaction is a projection.
- **The Missing Memory Hierarchy (demand paging)** — arxiv.org/abs/2603.09023 —
  **[VALIDATES]** Context window as L1 cache; evict stale tool results and definitions,
  page back in on "fault"; −93% context (5,038 KB → 339 KB) over 681 turns at a 0.0254%
  fault rate; 21.8% of production context was structural waste. → Progressive disclosure
  via `recall` is the page-fault path; pinned facts are the pinned working set.
- **Active Context Compression (Focus)** — arxiv.org/abs/2601.07190 — **[VALIDATES]**
  Agent-triggered consolidation into a knowledge block plus pruning of raw history:
  −22.7% tokens at identical accuracy (N = 5, small study). → Supports consolidation
  cadence being state-driven rather than every turn.
- **Mem-π: learning when and what to write** — arxiv.org/abs/2605.21463 — **[LATER]** A
  separate small model decides when to write memory and abstains when it would not help
  (RL-trained; >30% relative gain on web navigation; work in progress). → Our observer's
  notability rule is the hand-written version; a learned write policy is a later lever.
- **LightMem: small-model control plane** — arxiv.org/abs/2604.07798 — **[VALIDATES]**
  STM/MTM/LTM tiers managed by quantized small models (query generation and retrieval
  control; re-ranking and compression; online writing); +≈2.5 F1 over A-MEM on LoCoMo;
  83 ms retrieval, 581 ms end-to-end. → Summarizer and observer on the cheap emitter
  model is the intended regime, not a compromise.
- **Sleep-like consolidation / SCM** — arxiv.org/abs/2605.26099, arxiv.org/abs/2604.20943
  — **[VALIDATES]** Offline consolidation with importance tagging and value-based
  forgetting; SCM reports 90.9% memory-noise reduction. Already the basis of driver B.

## 3. Forgetting, updates and temporal validity

- **Control-plane placement shapes forgetting** — arxiv.org/abs/2606.15903 — **[ADOPTED]**
  "Production failures are predominantly *forgetting* failures rather than recall
  failures." ForgetEval (1,000 templated + 385 adversarial cases, κ = 0.958 on scoring).
  Deterministic primitives: 5% on identifier obfuscation, 0% cross-lingual; inscribe-time
  LLM: 100% canonicalization but 0% on prefix collisions; a *mutation-time* LLM hook
  (supersede / release / purge): 91.7–93.2% overall at $0.17 per 385-case run, 2.3 s per
  mutation. → Deterministic key canonicalization at write time (cheap, catches spelling
  variants), plus LLM-proposed supersedes produced offline and applied only through the
  pass's deterministic acceptance rules. Forgetting gets first-class actions.
- **A-TMA: ghost memory** — arxiv.org/abs/2607.01935 — **[CHANGES]** Old, current and
  transition facts coexist in the bank, mix at retrieval and mislead the answer model;
  end-to-end QA accuracy hides where this happens. Explicit state labels (current /
  historical / transition) in the evidence packet: conflict accuracy +0.240 absolute over
  Graphiti; LoCoMo temporal F1 0.0295 → 0.1705. → Revision 1 overwrote facts; revision 2
  versions them (`valid_from`, `valid_to`, state label) and renders "Peter (was Martin
  until t96)". The recorded turn 104 ("what was my name before") is exactly this failure.
- **Zep / Graphiti** — arxiv.org/abs/2501.13956 — **[VALIDATES]** Bi-temporal validity
  windows; facts are invalidated, not deleted; DMR 94.8% vs 93.4% (MemGPT); LongMemEval up
  to +18.5%; latency −90%. → Same invalidate-not-delete rule for facts. The knowledge
  graph itself is not adopted (§1).
- **Temporal Semantic Memory** — arxiv.org/abs/2601.07468 — **[LATER]** Event time is not
  dialogue time; durative memories on a semantic timeline; up to +12.2% absolute. →
  Reserve an `event_time` column on facts/observations now; populate later.
- **FadeMem** — arxiv.org/abs/2601.18642 — **[ADOPTED]** Dual-layer memory with adaptive
  exponential decay modulated by relevance, access frequency and temporal pattern; 45%
  storage reduction with better multi-hop retrieval. → Decay demotes facts to *cold*
  (excluded from pinned selection, still searchable), never deletes.
- **FSFM: selective forgetting taxonomy** — arxiv.org/abs/2604.20300 — **[ADOPTED]** Four
  mechanisms: passive decay, active deletion, safety-triggered, adaptive reinforcement;
  reports +8.49% access efficiency, +29.2% signal-to-noise, elimination of the tested
  security risks. → M6 has all four: decay (cold), `forget_fact`/`forget_all`,
  safety-triggered purge of External-trust facts, restatement bumps (adaptive).
- **MemoryAgentBench** — arxiv.org/abs/2507.05257 — **[VALIDATES]** Four competencies —
  accurate retrieval, test-time learning, long-range understanding, *selective
  forgetting* — and "current methods fall short of mastering all four". → Forgetting is a
  tested requirement in the M6 evaluation suite, not an afterthought.
- **Memory as Asset** — arxiv.org/abs/2603.14212 — **[VALIDATES]** Position paper:
  user ownership and control of memory ("memory in hand"). → `forget_all`, scope,
  `ns-app dump` export.

## 4. Retrieval precision and context budget

- **LongMemEval** — arxiv.org/abs/2410.10813 — **[ADOPTED]** Five abilities: information
  extraction, multi-session reasoning, temporal reasoning, knowledge updates, abstention;
  commercial assistants show a 30% accuracy drop over sustained interaction; session
  decomposition, fact-augmented key expansion and time-aware query expansion help. → The
  five abilities are the categories of the M6 evaluation suite (§11 Phase 7 of the spec).
  Time-aware query expansion for `recall` is **[LATER]**.
- **MemoryArena** — arxiv.org/abs/2602.16313 — **[ADOPTED]** Agents near-saturated on
  LoCoMo "perform poorly" when memory must be *acted on* across interdependent sessions.
  → The evaluation suite is harness replays with tool calls, not QA over transcripts.
- **Memory for Autonomous LLM Agents (survey)** — arxiv.org/abs/2603.07670 —
  **[VALIDATES]** Memory as a write–manage–read loop; five mechanism families
  (context-resident compression, retrieval stores, reflective self-improvement,
  hierarchical virtual context, policy-learned management); evaluation has shifted to
  multi-session agentic tests; frontiers: continual consolidation, causally grounded
  retrieval, trustworthy reflection, learned forgetting. M6 covers the first four
  families; the fifth is [LATER].
- **A Survey of Agent Memory in the Second Half** — arxiv.org/abs/2602.06052;
  **Memory in the Age of AI Agents** — arxiv.org/abs/2512.13564 — **[VALIDATES]**
  Taxonomies (substrate / cognitive mechanism / subject; forms / functions / dynamics;
  factual vs experiential vs working memory). M6's four layers map onto
  working / episodic / semantic / procedural directly.
- **Externalization in LLM Agents** — arxiv.org/abs/2604.08224 — **[VALIDATES]** Memory,
  skills, protocols and harness engineering as externalized capability under "governed
  execution"; the harness is the coordination layer. This is the thesis of the parent spec.

## 5. Security of long-term memory

- **TMA-NM: non-malleable, origin-bound memory authority** — arxiv.org/abs/2606.24322 —
  **[VALIDATES + ADOPTED]** Three laundering channels turn untrusted content into trusted
  memory: *agent summarization*, *trusted-tool echoes*, *manufactured corroboration*.
  Proof sketch: no content- or lineage-based defense is sound under laundering; write-time
  origin binding is necessary. Existing defenses fail at up to 68% attack success; TMA-NM
  reaches 0% at full utility. → Facts already bind provenance at write time; revision 2
  extends min-trust propagation to summaries and observations (the summarizer is a
  laundering channel) and applies `TaintPolicy` to `recall` output.
- **From Untrusted Input to Trusted Memory (MPBench)** — arxiv.org/abs/2606.04329 —
  **[ADOPTED]** Four write channels, nine structural vulnerabilities, six attack classes;
  "agents designed to write and retrieve memory more aggressively are more exploitable";
  prompt-injection defenses do not cover memory poisoning. → Conservative write policy:
  observations only for notable turns, Residual values flagged, no auto-promotion of
  External-trust content.
- **Sleeper memory poisoning** — arxiv.org/abs/2605.15338 — **[VALIDATES]** Poisoned
  memories written in up to 99.8% of attempts; on retrieval they cause attacker-intended
  actions in 60–89% of evaluations. → Recall results are `External` when their source
  was; side-effectful actions on them need confirmation (existing guard).
- **MemAudit** — arxiv.org/abs/2605.23723 — **[ADOPTED (enabler)]** Post-hoc causal
  attribution (counterfactual memory influence) plus consistency-graph anomaly detection
  cut two attacks from 70% and 83.3% success to 0%. Attribution needs to know which
  memories were shown when. → `ModelCall` events carry a context manifest (fact keys,
  summary id, window range) so influence can be computed from the log.
- **Long-term memory security survey** — arxiv.org/abs/2604.16548 — **[NOTED]** Lifecycle
  view of attacks, defenses and governance; used as a checklist only.

## 6. Evaluation: benchmarks and LLM judges

- **Reliability without Validity** — arxiv.org/abs/2606.19544 — **[ADOPTED]** 21 judges,
  ≈541k judgments: exact-match agreement overstates reliability (κ deflation 33–41 pp on
  MT-Bench); judges with test–retest reliability > 0.95 still show position bias > 0.10;
  rankings move up to 14 places across benchmarks. Proposes a minimum viable validation
  protocol. → The evaluator's calibration uses Cohen's κ against symbolic proxies, not raw
  agreement, and reports test–retest on a re-graded sample.
- **Judging the Judges (bias mitigation)** — arxiv.org/abs/2604.23178 — **[ADOPTED]** Nine
  debiasing strategies on five judges: style bias (0.10–0.76, markdown over plain text)
  dominates; position bias ≤ 0.04; a cheap judge with a combined budget strategy reached
  71.0% agreement (κ 0.549) at ≈$0.001 per evaluation, ≈15× cheaper than the best
  frontier setup (69.5%). → Plain-text replies and rubric, absolute per-turn scoring (no
  pairwise), cheap model is fine.
- **AgentProp-Bench** — arxiv.org/abs/2604.16706 — **[ADOPTED]** A single small judge
  (κ 0.567 vs human) beat a three-model ensemble (κ 0.432); human–human κ 0.835; a
  parameter error propagates to a wrong answer with p ≈ 0.62; a *runtime interceptor*
  against fabricated tool executions cut that hallucination by up to 24 pp. → Single
  cheap evaluator; and an in-turn **symbolic** reply-grounding interceptor (no model) that
  triggers one regeneration when a reply claims outcomes or values absent from its context.

## 7. Experience and procedural memory (ties to M5)

- **Experiential Reflective Learning** — arxiv.org/abs/2603.24639 — **[VALIDATES]**
  Heuristics distilled from single attempts, retrieved selectively, beat few-shot
  trajectory prompting; +7.8% success on Gaia2; "selective retrieval is essential". → M5
  notes are heuristics; action-scoped delivery is the selective retrieval.
- **Procedural Memory Distillation** — arxiv.org/abs/2607.01480 — **[LATER]** Three
  abstraction levels (trajectories, lessons, patterns) distilled into weights via RLVR;
  +3.8–13.6 pp on two benchmarks. Requires fine-tuning a small model the user has
  available (decision 2026-09-02: revisit once the M6 log carries graded turns, which
  are the training signal it needs); the harness-level loops stay primary.
- **MemPro: memory system as an evolvable program** — arxiv.org/abs/2606.00619 —
  **[LATER]** A version tree of runnable memory pipelines, evolved by failure-mode-guided
  edits, verified on benchmarks. → Once the M6 evaluation suite exists, memory knobs
  (window size, cadence, ranking) become a candidate lane of the evolution pass.

---

## Design decisions changed by this sweep (revision 1 → 2)

| # | Revision 1 | Revision 2 | Source |
|---|---|---|---|
| 1 | Summary + facts are the memory; verbatim only in the window | Verbatim log is primary; summary/facts/observations are indexes; `recall` ranks verbatim hits first | 2601.00821, 2601.21714 |
| 2 | 20 facts by recency | ≤10: pinned core + query-relevant via FTS; superseded value shown for pinned keys | 2603.17787, 2605.11325, 2606.25361 |
| 3 | Overwrite facts | Versioned facts with validity windows and state labels; invalidate, never delete | 2607.01935, 2501.13956, 2606.15903 |
| 4 | Incremental summary-of-summary | Regenerate from verbatim records; periodic full rebuild; summary carries min trust | 2603.11768, 2606.24322 |
| 5 | Evaluator calibration = raw agreement ≥ 0.6 | Cohen's κ ≥ 0.4 plus test–retest; plain-text rubric; single cheap judge | 2606.19544, 2604.23178, 2604.16706 |
| 6 | In-turn reply check deferred entirely | Symbolic grounding interceptor in-turn (one regeneration); *model* check still deferred | 2604.16706 |
| 7 | Context not logged | `ModelCall` carries a context manifest for attribution and exact reconstruction | 2605.23723 |
| 8 | Decay lowers confidence | Decay demotes to cold; four forgetting mechanisms explicit | 2601.18642, 2604.20300 |
| 9 | Exit criteria = live smoke | Evaluation suite over LongMemEval's five abilities as harness replays | 2410.10813, 2602.16313 |
| 10 | Graph memory not discussed | Graph memory **[DISCARDED]** for now: did not close the fidelity gap; needs state labels anyway | 2601.00821, 2607.01935 |
