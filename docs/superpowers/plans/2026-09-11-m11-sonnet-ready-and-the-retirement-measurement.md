# M11 — Sonnet-ready request shaping, three memory deltas, and the measurement that decides the retirements

Date: 2026-09-11 · Approved 2026-09-11 · Branch `worktree-m11-sonnet-ready` from `worktree-m10-tool-array` @ `20587b9` · Scope fixed by
the user's answers below · Evidence: the repo scaffold audit (every component's recorded
rationale and off-switch), the Sonnet 5 prompting and migration guides, Scaffold Effect
(arXiv 2607.22585), Capacity Not Format (2606.09410), few-shot on reasoning models
(2509.13196), MemStrata (2606.26511), memory-use boundaries (2606.06055), long-context vs
memory (2605.18421), the Czech lexical ceiling (2605.24556), and the Anthropic prompt-audit
keep list. When executed this becomes
`docs/superpowers/plans/2026-09-11-m11-sonnet-ready-and-the-retirement-measurement.md`.

## Decisions taken with the user (2026-09-11)

1. **First strong model: Claude Sonnet 5 via OpenRouter** (`anthropic/claude-sonnet-5`). It
   already works as the replier (`replier.rs` sends no sampling params and is tested against
   that id); it fails as the emitter and summarizer because both send `temperature: 0`, which
   Sonnet 5 rejects with a 400.
2. **Retirements: hard errors only.** Nothing a strong model merely "does not need" is
   switched off in M11. The redundancy table is the record; the retirements become the next
   plan once a live Sonnet session has produced the numbers M11 collects.
3. **The measurement run is approved** once the shaping is green: 40 chat turns on Sonnet 5
   under today's scaffolding, about 110 requests, roughly $1 to $1.60.

4. **Later the same day: the production model is OpenAI GPT-5.6 Luna** (`openai/gpt-5.6-luna`
   on OpenRouter: $0.20 / $1.20 per MTok, $0.40 / $1.80 on long-context requests, 1.05M
   context, 128k max output, tools, `tool_choice` and `json_schema` `response_format`
   supported; served by OpenAI, Azure EU and Bedrock US), chosen for cost — roughly a tenth
   of Sonnet 5 per turn at the measured token counts. What carries over from the Sonnet
   measurement: the grounding flags are the matcher's fault (Czech inflection), not the
   model's, so the retirement decision stands; the per-role shaping fields are model-agnostic.
   What must be re-measured on Luna: whether it rejects sampling parameters (the safety net
   matches Sonnet ids only), tool-calling reliability of a cheaper caller (text fallback,
   `Malformed`), and the summarizer through `response_format` now that the dispatcher fix is
   in. The Luna wave below does exactly that, at a spend ceiling of 70 requests and $0.10.

Standing constraints: desktop line parked; chat and personal memory first; a paid tier is
coming; the chat single-call path is measure only; no model in the guard chain or the fold;
replay and provenance untouched; a knob moves only on a non-regressing arm.

## Context

Every scaffold in the engine was built against a small model, and the repo records why each
exists. Read against Sonnet 5, three of them are hard errors or dead code, most are
structural and stay, and a handful are redundant only on the field's evidence, not yet on
this engine's numbers. The user chose to trust the engine's numbers: M11 makes Sonnet 5 a
legal target for every role, adds the memory work a strong model makes worth doing, and runs
one measured session that reads exactly the four numbers the retirement decision needs.

## What becomes redundant, per model — the record

Common column: all three of Sonnet 5, Gemini 3.8 (Flash and Pro), Qwen3.8-27B. Per-model
column: deltas only. **Nothing in this table is removed by M11 except the hard errors.**

| Scaffold | Anchor | Recorded reason | Common verdict | Per-model delta |
|---|---|---|---|---|
| emitter/replier split | spec 2026-09-01 §2 principle 5, §5 | compliance-by-construction + cost; echo 0.598 → 0.104 came from prompt shape | **keep** | — |
| `tool_choice: required` + `respond_directly` | `emitter.rs:173`, `core/router.rs:72` | legality by generation constraint | **keep** | — |
| `temperature: 0` | `emitter.rs:172`, `summarizer.rs:~104` | pinned determinism for a small emitter | keep on Gemini/Qwen | **Sonnet 5: hard 400** → per-role `sampling = none` |
| manual thinking budget / sampling params | none sent | — | — | Sonnet 5 rejects budgets; `effort` replaces them |
| `max_tokens: 4096` hardcoded | `emitter.rs:43`, `replier.rs:20` | reasoning models spend output on reasoning first | keep as floor, make per-role | Sonnet: emitter 2048 at effort low is enough |
| text-fallback branch (model ignores `tool_choice`) | `emitter.rs:196-208` | free-tier models that ignore forced tools | redundant on a compliant caller | Qwen via a local shim: keep as monitor |
| grounding **flag** + `ReplyCited` | `ground.rs`, `turn.rs` | zero-cost symbolic check; feeds `UngroundedReply` and the fitness join | **keep** | — |
| grounding **regeneration** | `turn.rs` (the `ReplyFlagged` path) | hallucination scaffolding; 1 flag / 21 turns, a cs/en false positive, +1 request | redundant on the field's evidence (Sonnet runs its own verification; self-refine can inject errors) | decided by M11's measurement |
| echo ratio | `engine/src/echo.rs` | entrainment **grows with model size** (2604.13275); demoted to monitor: 21 flags, 0 true positives | **keep as monitor** | matters more on Sonnet |
| `obligation_check` | `turn.rs`, default false | +7.7% replier requests, 0 graded gain (M10 T5.4) | redundant, already off | Qwen: keep the knob |
| `<reference>` fence | `replier.rs:47` | spotlighting (injection hygiene), ~20 tokens | **keep** | — |
| summarizer JSON forced by prompt | `summarizer.rs` | fixed fields = cost projection; prompt-forcing = weak-writer enforcement | keep the fields; mechanism can move to `response_format` | strong models pay ~0 for format (2606.09410) |
| argument examples on two tools (M10 T1.5) | `pointer_tool.rs` | malformed args measured on the recorded log | redundant on reasoning models (few-shot −26 pp worst case) | Qwen 27B: keep |
| per-iteration reminder text | `emitter.rs:15-26` | small-model retention | trim under a strong profile | Gemini 3: trim hardest |
| `learned.toml` notes | `core/learned.rs`, evolution §10 | model-specific by design | archive on model change | the one note today is desktop-only |
| symbolic router, tiers | `core/router.rs` | request budget + replay determinism | **keep** | — |
| context caps, fit, 6,000 budget | `budget.rs`, `config.rs` | cost and precision, not capability | **keep**; a strong profile may raise them if the arms pay | cost decides (2605.18421) |
| `budget_line` | `show_budget_line = false` | — | keep off (countdowns in context are harmful) | — |
| `respond_directly` on cue-less chat | `turn.rs` | "an LLM executor for a deterministic plan" | measure only (M10 decision 1) | — |
| notes lane, Succeeded lane, fitness, κ, residual, versioning, digests, hybrid recall | evolution and memory specs | structural or memory hygiene | **keep** | — |
| replay, fold, hash chain, manifests | M6 §2 principle 2 | structural | **keep** | — |

## What does not change

Everything marked keep above, plus: no Anthropic-direct preset (OpenRouter is the only preset
that forwards `cache_control`, and it forwards it to Anthropic; the emitter prefix with a
stable tool array is ~1,300 tokens on turn 21, above Anthropic's 1,024 floor; the replier's
225 never will be); today's requests byte-for-byte for any model that is not Sonnet 5; the
chat single-call path; every M9 and M10 default.

## Concepts adopted

| Concept | Source | Form here |
|---|---|---|
| Per-role request shape | Sonnet 5 migration guide (sampling and budgets rejected; `effort` replaces them) | four optional fields on `[llm.<role>]`, all unset = today |
| Provider detection as a safety net | the hard 400 | Sonnet ids default `sampling = none` with one logged coercion |
| Structured output where it is free | Capacity Not Format | summarizer `response_format` json_schema where the preset advertises it; fence parser kept |
| Lexical retrieval is the ceiling in Czech | 2605.24556 | hybrid `search_facts` behind `[recall] hybrid` — the gap M10 P3 left |
| Abstain when memory is silent | 2606.06055 | one line in the reply task when every reference block is empty; measured by the abstention arm |
| A stronger offline judge | SCM 2604.20943, M8's blind spot (corrections) | `ClientEvaluator` on the strong model, idle pass only, κ-gated, off by default |
| Already built in different clothes | MemStrata, PGMem, memory-as-tools, proactive memory | a register, not code |
| The harness is the variable | Scaffold Effect (40× tokens, 0–8 pp) | the measurement run reads cost and flags on today's scaffolding before anything is retired |

## Phases

Order: **P0 → P2** is the critical path (the measurement needs the shaping). P1 is independent
and zero-request.

### P0 — Sonnet-ready request shaping (0 requests, ~1 day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T0.1 | `RoleSection` gains `reasoning: Option<String>` (`low\|medium\|high`), `max_tokens: Option<u32>`, `sampling: Option<String>` (`default\|none`), `thinking: Option<bool>`; unset = today | `app/src/config.rs` (`RoleSection`, ~:793) | `config.rs` `role_shaping_fields_default_to_none` | a bare `[llm.emitter]` produces today's request byte-for-byte |
| T0.2 | `RequestShape` in `crates/llm/src/provider.rs`, applied at the three `json!` sites: omit `temperature` when `sampling = none`; `"reasoning": {"effort": …}` when set; `"reasoning": {"enabled": false}` when `thinking = false`; `max_tokens` from the role | `emitter.rs:169-173`, `replier.rs:~155`, `summarizer.rs:~102` | `emitter.rs` `sampling_none_omits_temperature_entirely`; `replier.rs` `effort_rides_as_a_reasoning_block`; `summarizer.rs` `the_summarizer_takes_the_same_shape` | a shaped request has no `temperature` key at all; `request_carries_schema_context_and_forced_tool_choice` still passes for the default shape |
| T0.3 | safety net: a resolved model id matching `anthropic/claude-sonnet-5*` with `sampling` unset defaults to `none`, logged once at startup | `provider.rs` (`sampling_default_for`), `app/src/main.rs` (~:659-676 where targets resolve) | `provider.rs` `sonnet_five_never_receives_a_sampling_param` | the 400 is unreachable from a default config naming Sonnet in any role |
| T0.4 | recommended shapes in `config.example.toml`, not hardcoded: emitter `reasoning = "low"`, `max_tokens = 2048`; replier `reasoning = "medium"`, `max_tokens = 4096`; summarizer `reasoning = "low"`, `max_tokens = 1024`; a commented Qwen block with `thinking = false` on the emitter | `config.example.toml` | the example parses in the existing config doc test | round-trips |
| T0.5 | summarizer structured output: when `Provider.structured_output` is true (OpenRouter: yes), send `response_format: {type: json_schema, json_schema: {name: summary, strict: true, schema: <topic, established[], open[]>}}`; `strip_fence` stays as the fallback parser | `summarizer.rs:~102`, `provider.rs:17` (new bool column) | `summarizer.rs` `structured_output_is_sent_only_where_the_preset_advertises_it`, `a_fenced_reply_still_parses` | identical `SummaryDraft` from both paths |

Note: Anthropic's native `output_config.format` is a Messages-API field; OpenRouter speaks
chat completions, so `response_format` is the portable form and T0.5 is one field, not a
second client.

### P1 — Memory deltas (0 requests, ~1½ days, independent)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T1.1 | hybrid `search_facts`: embed `key + " " + value` at write time into the existing `embeddings` table (`kind = "fact"`, owner = scope, model in the PK), fuse bm25 and cosine candidates with `rrf_fuse` (core, k = 5), rerank; behind `[recall] hybrid`; service down = `lexical_rank` byte-for-byte | `crates/memory-sqlite/src/lib.rs` (`search_facts`), `crates/evolution/src/pass.rs` (backfill batch), `crates/core/src/memory.rs` untouched | `memory-sqlite` `hybrid_search_facts_equals_lexical_with_the_service_down`; a `--paraphrase --facts` arm in `crates/testkit/src/paraphrase.rs` | the `cs/name` miss (M10 T3.4's one miss) closes with the verbatim arm not regressed, or revert and record |
| T1.2 | memory-silence line: when `ctx.facts` is empty, no summary exists and no recall fired this turn, `render_task` adds one sentence, "Nothing in memory bears on this; say so rather than guessing." | `crates/llm/src/replier.rs` (`render_task`, ~:79) | `replier.rs` `the_silence_line_appears_only_when_every_reference_block_is_empty` | M10's abstention arm stays 30/30 and the answerable arm stays 30/30 |
| T1.3 | paid judge: `ClientEvaluator` implementing `Evaluator` over `OpenRouterClient` on `[models] judge_model` (default unset = not constructed), wired by `with_evaluator` in the idle pass only, κ-printed and `evaluator_min_kappa`-gated like `LocalEvaluator`, its requests counted in the pass report; never in a turn, never in the guard chain | new `crates/evolution/src/client_eval.rs` beside `local.rs`, `app/src/main.rs` (`build_pass`), `pass.rs` | `client_eval.rs` `a_paid_evaluator_below_kappa_yields_observations_only`, `it_is_never_constructed_without_a_judge_model` | κ(client, symbolic) prints on the recorded copy when configured; zero requests when not |
| T1.4 | the register, in §Results: **[DONE]** MemStrata bi-temporal supersession = keyed facts with validity intervals (`consolidate.rs:103`, `memory-sqlite`); PGMem provenance = `Fact.prov` (`action.rs:206`); memory-as-tools = `remember_fact` / `recall` / `forget_*`; proactive memory = the idle pass. **[LATER]** time-aware recall, trigger: the tie corpus separating on time rather than credits | — | — | recorded |

### P2 — The measurement run on Sonnet 5 (≈110 requests, ≈ $1.00–1.60, after P0 is green)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T2.1 | a scripted set of 20 chat messages (cs and en, drawn from the fixtures' seeds: a stated fact, a paraphrased recall, a knowledge update, an unanswerable question, small talk) run live twice on `openrouter:anthropic/claude-sonnet-5` in all three roles under today's scaffolding: arm A with `reasoning` unset (provider default), arm B with the T0.4 shapes; `ns-app budget` after each | the REPL on stdin suffices; a `--live <file>` runner in `app/src/eval.rs` is optional | — | four numbers per arm in §Results: requests per turn, grounding-flag rate with each flag classified true/false positive by hand, `cached_tokens` share, completion tokens (and dollars actually spent) |
| T2.2 | the retirement decision, written into §Results and carried into the next plan: for each candidate row of the table (regeneration, text fallback, reminder text, argument examples, note archiving, strong context profile) the number that decides it | plan doc only | — | each row has a number or "not observed in 40 turns" |

Cost basis: the measured 5,696 prompt and 1,441 completion tokens per turn at $2 / $10 per
MTok is $0.0258 per turn, $1.03 for 40; reasoning on the replier can triple its completion,
so the ceiling is about $1.60. Requests 2–3 per turn, at most 110. Nothing else in M11
spends a request.

## Deferred to the next plan, with the evidence that will decide them

The `capability = strong` knob and its retirements (grounding regeneration off, text fallback
off, trimmed preamble, examples off), `learned.toml` notes archived per model (`Note.learned_on`,
absent at default), the strong context profile (`window_turns` 6 → 10, `facts_in_context`
10 → 16, cost-decided), and the chat single-call path (needs the M10 P4 counter). Each waits
for a P2 number.

## Minimal cut (one day)

T0.1–T0.3 (Sonnet becomes a legal emitter and summarizer) and T1.2 (the silence line). Cut
T0.4, T0.5, T1.1, T1.3 and the measurement. That removes the hard error and nothing else.

## Risks and where each is caught

- **A shaped request changes behaviour on Gemini.** Every shaping field is unset by default;
  the existing emitter and replier request tests pin today's bytes.
- **OpenRouter drops `reasoning` or `response_format` for a model.** The fence parser stays for
  the summarizer; `ns-app budget`'s completion column shows whether effort took.
- **Hybrid facts over-retrieve on short keys.** The `--paraphrase --facts` arm gates it;
  service down is byte-identical to today.
- **The silence line makes the replier decline when it should answer.** The answerable arm
  (30/30) is the guard; the line renders only when every reference block is empty.
- **The paid judge spends the budget.** Not constructed without `judge_model`; idle pass
  only; κ-gated; its own counter in the report.
- **The measurement run writes to the live log.** It is meant to: 40 turns on the `cli`
  session, exactly as the M9 smoke turn did; copies for every pass afterwards.
- **A new struct field changes serialized bytes.** `RoleSection` is config, not an event;
  no manifest or event field is added in M11.

## Verification

| Phase | Checks |
|---|---|
| P0 | `cargo test -p ns-llm -p ns-app -p ns-core`; a Sonnet-named role produces a request without `temperature`; a bare config produces today's bytes; the example config parses |
| P1 | `cargo test -p ns-memory-sqlite -p ns-evolution -p ns-llm -p ns-testkit`; `ns-app eval --paraphrase --facts` with nsmodels up and down; `--ablate` arms unchanged; `ns-app evolve --dry-run` with and without `judge_model` |
| P2 | `ns-app budget cli` after each arm; the four numbers and the dollars in §Results |
| all | `cargo test --workspace`; box notes as in M10: cargo on the Git Bash PATH, test output to a file, `rustfmt` only changed files, `turn.rs`/`config.rs`/`budget.rs` hand-edited, never a non-dry pass on `ns-run/ns.sqlite`, restart nsmodels before P1 (`cd ~/models && ./.venv/Scripts/python.exe -m nsmodels serve --model quality --rerank`) |

## How to execute

Branch `worktree-m11-sonnet-ready` from `worktree-m10-tool-array`. One Opus agent per phase
at medium effort, test first, one commit per phase, each exit criterion pasted into §Results
with its number. P0 first, P1 in parallel with it (disjoint files: P0 touches `llm`,
`config.rs`, `main.rs`; P1 touches `memory-sqlite`, `evolution`, `replier.rs`'s `render_task`;
hand `replier.rs` to P0 and let P1's silence line land after P0 commits). P2 last, with the
OpenRouter key from the run script and the live `ns-run` directory, spending only the
approved budget. After the branch lands: `graphify <root> --update`; the redundancy table and
the P2 numbers go into the findings doc as §9.

## Results

### P0 + T1.2 — done (commit `96e19c2`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T0.1–T0.3 | a Sonnet-named role sends no `temperature`; a bare config is byte-identical | met: `RoleShaping` / `RequestShape::apply` is the one place `max_tokens`, `temperature` and the reasoning block are written; the safety net coerces `anthropic/claude-sonnet-5*` to `sampling = none` and prints it once. A Sonnet emitter request carries `model, tools, tool_choice, messages, max_tokens, reasoning` and nothing Sonnet rejects |
| T0.4 | example parses | met; Sonnet shapes and a Qwen `thinking = false` block, commented |
| T0.5 | identical `SummaryDraft` from both paths | met: `Provider.structured_output` (openrouter, openai); strict `json_schema` `response_format`; fence parser kept |
| T1.2 | abstention and answerable arms unmoved | met: 30/30 and 30/30; the line renders only when facts, summary and recall are all empty |

### T1.1 — done (commit `7387736`), 0 requests

Facts embedded at write time and by the idle backfill (kind `fact`, owner scope, rowid, model
in the PK); `search_facts_hybrid` = lexical ∪ cosine by RRF rank, reranked, equal to
`lexical_rank` whenever the encoder is absent or refuses (byte-for-byte, tested). **12-fact
cs/en corpus: paraphrase miss 100% lexical → 0% hybrid, verbatim 0% on both**, NOT MEASURED
with the service down. Two follow-up lines for wave B: `select_facts` in `turn.rs` gated on
`[recall] hybrid` **and** a non-Chat tier; retire the `FACTS_ARM` static via `main.rs`.

### P2 — the measurement (T2.1), done: **106 requests, $0.475**, under the approved 110 / $1.60

Arm A = turns 22–41 of the live `cli` session (reasoning unset), arm B = turns 42–61 (the
T0.4 shapes). Same 20 messages, cs and en: stated facts, paraphrased recalls, a knowledge
update, an unanswerable question, small talk.

| | requests | per turn | emitter / replier / summarizer | prompt tok | completion tok | cached | $ |
|---|---|---|---|---|---|---|---|
| A unset | 53 | 2.65 | 29 / 24 / 0 | 103,333 | 4,378 | 0 | 0.250 |
| B shaped | 53 | 2.65 | 28 / 25 / 0 | 96,039 | 3,238 | 0 | 0.225 |

B is strictly cheaper at equal request count and equal correctness: completion **−26%**,
prompt −7%, $0.0112 per turn against the plan's $0.0258 estimate. Cached 0% in both arms:
the emitter prefix is 451–472 tokens and the replier's 267–294, under the 1,024 floor.
Tool schemas are **23.5%** of emitter prompt tokens, down from M9's 45.9%: M10's work paid.

**Memory behaved.** Eight `remember_fact` calls per arm, all persisted: name, city (Brno →
Prague), language, sister's birthday (14 → 15 March), colleague, response-style preference.
"Kdy má sestra narozeniny, přesně?" → "15. března" in both arms. The unanswerable question
was declined correctly in both arms. Zero `Rejected` events in the 40 turns.

**Grounding flags: 9 of 40 turns, 8 false positives, the 1 true positive harmful.**

| turn | spans | verdict |
|---|---|---|
| A t22 / B t42 | `Martine` | FP — vocative of the name stated that turn |
| A t27, A t40, B t47, B t60 | `Praze`, `Prahy` | FP — locative/genitive of the stored Prague fact |
| B t54 | `Tomáše` | FP — accusative of the colleague stated that turn |
| A t30 / B t50 | `Canberra`, `Sydney`, `Melbourne` | TP by the letter (world knowledge, not in memory) — **the regeneration replaced a correct "Canberra" with "I don't know for sure, look it up", in both arms** |

Nine extra replier requests, 17% of all replier calls, for a net loss of correctness.

**Two defects the run exposed, neither a Sonnet finding:**
- **The summarizer never ran in 40 turns.** `dispatch.rs:363` runs `maybe_summarize` in a
  biased `select!` against the next inbound message; with input already queued the next turn
  wins every time and the summary is dropped mid-flight. Any fast user under `serve` hits the
  same starvation. Fixed in wave B (T1.5).
- **"What time is it right now?" never reached `get_time`.** The router put the turn in the
  Chat tier, which withholds every registered tool, so the model answered that it has no clock
  although the tool was registered. A router cue for time is the fix; recorded, not built
  (tool line).

Also seen: `recall` fired once in 40 turns; `ReplyEchoed` 0 in A, 2 in B, both on correct
one-line answers (monitor false positives, as the entrainment plan already recorded).

### T2.2 — the retirement decision, from this engine's numbers

| Candidate | Number | Decision for the next plan |
|---|---|---|
| grounding regeneration | 9/40 flags, 8 FP on Czech inflection, 1 TP harmful; 9 requests | **retire the regeneration; keep the flag**; make the matcher inflection-aware (lemma or prefix match ≥ 5 chars) so the flag stops firing on `Praze`/`Martine` |
| text fallback | 0 in 106 requests | redundant on Sonnet; keep only as a monitor for local shims |
| argument examples | 0 `Malformed` in 106 | redundant on Sonnet; keep for Qwen |
| reminder text | not isolated; 10 of 19 chat-tier turns proposed only `respond_directly` | trim under a strong profile; the chat single-call counter now has its first reading |
| note archiving | not observed (the one note is desktop-only) | build when a chat note exists |
| strong context profile | prompt 4,800–5,200 tok/turn at $0.011/turn, prefix 451–472 tok, cached 0 | affordable; raising the window also pushes the prefix toward the caching floor; decide on the ablation arms |
| T0.4 shapes | −26% completion at equal correctness | recommend as the Sonnet default in `config.example.toml` (done); not hardcoded |

### Wave B — done (commit `dee9c47`), 0 requests planned, 10 spent on the judge check

`ClientEvaluator` on `[models] judge_model` (never constructed without one), idle pass only,
κ-gated, its requests printed; `select_facts` hybrid only when `[recall] hybrid` and the tier
is not Chat; the `FACTS_ARM` static retired; **T1.5**: the dispatcher awaits a due summary
before the next queued turn (awaited, not spawned, to keep one turn at a time per session);
the test reads 0 → 1 `Summarized` with the fix, and turns 22–61 of the live log had zero
summaries before it. Judge check on a copy with Sonnet, cap 10: 10 requests, **5 verdicts
unavailable**, κ 0.12 over 5 — diagnosed in the Luna wave below.

### Luna wave — done, 119 requests, **$0.032** (ceiling was 70 / $0.10: dollars held, requests breached — see the defect)

**Sampling probe (5 requests, on a copy):** Luna **accepts `temperature`** and honours
`reasoning: {effort}`; no safety-net change, `SAMPLING_REJECTED_BY` stays Sonnet-only.
`config.example.toml` gains the Luna block (emitter low/2048, replier low/4096, summarizer
low/1024, no `sampling`), and still parses.

**Measurement, one shaped arm, turns 62–81 of the live `cli` session (58 requests):**

| | requests | per turn | e / r / s | prompt tok | completion tok | cached | $ |
|---|---|---|---|---|---|---|---|
| Luna shaped | 58 | 2.90 | 29 / 24 / 5 | 55,958 | 3,443 | 0 | **0.0153** |

$0.00077 per turn: **14.7× cheaper than Sonnet arm B** at the same script. The summarizer
**ran 5 times** (the wave B fix holds) and every summary parsed through `response_format`
without the fence fallback. `Rejected` 0, `Malformed` 0, text-fallback 0: the cheaper caller
never dropped the forced tool call. Eight `remember_fact` calls, all `Ok`, every fact and
both updates stored; "přesně 15. března"; the car question declined; the name recalled.

Grounding flags **4 of 20**: `Brna` (genitive, FP), `Praze` (locative, FP), an
apostrophe-split span of the user's own sentence (FP), and `Canberra` — the same harmful
true positive as both Sonnet arms, a correct answer regenerated into a refusal. The
retirement decision is model-independent and stands.

**Judge on Luna** (cap 5, `NS_TRACE`): 5 graded, **0 unavailable**, κ 0.55 [−0.25, 1.34]
over 5 turns, observations only; every verdict `finish_reason stop`, strict `json_schema`
honoured, none fenced. Sonnet's 5-of-10 unavailable did not reproduce; no `max_tokens`
change was needed, and the cause on Sonnet remains undiagnosed (recorded).

**Defect found:** `ns-app evolve --dry-run` with `[models] enabled` and a key in the
environment fires the **notes lane's proposer and probes** — 51 requests at `max_tokens 300`
— and the report's `requests spent:` line counts only the judge's 5. A dry run spent ten
times what it reported. Two fixes for the next plan: `--dry-run` must not run live probes
(or must say it will), and `requests spent` must count every lane's requests from the
per-call `UsageSink`, which already records them.

### Status after M11

Built and green (workspace suite below). Every knob defaults to today's behaviour; the
recommended Luna shapes live in `config.example.toml`. Retirement decisions, now backed by
Sonnet and Luna numbers: retire the grounding **regeneration** and make the flag's matcher
inflection-aware; drop the text fallback and argument examples to monitors under a strong
profile; the reminder text, note archiving and the strong context profile wait for a chat
note and the ablation arms. Open defects: the pass's unreported paid requests; the Chat
tier withholding `get_time`; Sonnet's unavailable judge verdicts.
