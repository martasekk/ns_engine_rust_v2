# M7 — Context budget: a small model under a fixed budget

**Status:** plan, 2026-09-07. Evidence base: `docs/research/2026-09-07-context-budget-findings.md`
(the 2026 brief, with a disposition table against this engine). Builds on M6
(`docs/superpowers/specs/2026-09-02-memory-redesign-design.md`, Phases 0–4 built, 5–7 open)
and on the branch `messages-client` (worktree `suspended-backoff`, four commits ahead of
`main`, unmerged), which this plan assumes merged first.
Recorded decisions it respects: emitter escalation discarded 2026-09-01
(`2026-09-01-findings.md` §6); graph memory and vector search deferred with triggers (M6
§12–§13); the verbatim window is never consolidated (`2026-09-04-reply-entrainment.md` §9);
a check becomes a gate only after a measured true-positive rate (same plan, §8).

---

## 1. What the brief asks for, and how much of it already exists

The brief's thesis: make a small model perform like a large one by controlling what enters
the context — a *token diet as an accuracy strategy* — with memory decomposed into
extraction (α), coarsening (C) and traversal (τ), and the harness treated as the binding
constraint. Its runtime target was Temporal.

Most of the memory half is M6, already built. Read against the eleven findings:

| Brief | ns-engine today | Gap |
|---|---|---|
| 02 last-N spans + running summary | window of 6 verbatim `TurnRecord`s with `did:` lines capped at 120 chars, rolling `SessionSummary` rebuilt from verbatim every third time | across turns: done. **Within a turn: nothing** — see §2 |
| 03 write/read split, consolidation off the hot path | summary runs during the wait for the next message; `Consolidator` on the idle timer and `ns-app evolve` | done |
| 05 cheap construction | lexical FTS5, no graph, one summary level — by decision | done |
| 07 temporal validity, no ghost memory | `Fact{valid_from, valid_to, state}`, "(was X until …)" rendering | done |
| 08 forgetting | cold / forgotten / purge External in `evolution/consolidate.rs` | done |
| 09 procedural memory | M5 learned rules, notes, ledger | done |
| 01/06 bounded, addressable context with recoverable archives | facts, summary and window bounded; **the this-turn trace unbounded on `main`**, clipped without a handle on the branch; `http_tool` archives, `pointer_ui_read` does not; **no token meter anywhere** | §5, §6 |
| 04 route by intent, tiered budgets | none — every turn carries the full legal set and full context; `recall` costs an emitter iteration | §7 |
| 03 coarsening above the session | none — memory across sessions is facts only; M6 Phase 5 "observations" unbuilt | §8 |
| 11 fixed-model regression per harness release | replay harness with scripted doubles; no fixed task set, no metric row per release; M6 Phase 7 suite unbuilt (`crates/engine/tests/` holds `turn_loop.rs` only) | §9 |
| 04/11 validator → escalate to a large model | discarded by user decision | §10, a decision, not a phase |
| Temporal | the engine is already event-sourced and replayable | §3, not adopted |

So M7 is not a memory redesign. It is the **in-turn context lifecycle** the brief calls the
ledger, plus **traversal by intent**, plus **one more level of coarsening**, plus the
**measurement that makes any of it checkable**. In the brief's build order that is steps 1,
3, 4 and 6; step 2 (extraction) exists; step 5 (escalation) is the recorded decision.

---

## 2. The evidence from this machine

`ns-run/ns.sqlite`, session `cli`, 2026-09-07: 10 turns, 98 events, 15 tool calls, one
`Summarized`. Tool results by size:

| action | n | mean chars | max |
|---|---|---|---|
| `pointer_ui_read` | 2 | 12,225 | **14,425** |
| `pointer_screens` | 2 | 113 | 113 |
| everything else | 11 | ≤ 24 | 24 |

Median tool result: 24 characters. Per-turn tool output: median 42, p90 10,025, max 14,425.
One action produces 98% of the tool text, and it is the one the task file already tells the
model to avoid ("ui_read returns the whole control tree and is very expensive").

What that costs on `main`: `run_turn` rebuilds `trace_so_far` from `turn_trace()` on
**every emitter iteration** (`crates/engine/src/turn.rs`, step b), uncapped, and hands the
same trace to the replier. A 14k-char result at iteration *k* is sent again on each of the
remaining `max_iterations − k` iterations — with `max_iterations = 12` in `ns-run/config.toml`,
up to eleven more times, at roughly 3.6k tokens each. The branch commit `eef270c` ("Clip
tool results where the trace becomes a prompt") caps each line at 1,200 chars and says
`[N more characters]`. That fixes the cost. It leaves two things: the dropped text is
unreachable from the prompt (the model can only re-run the tool), and nothing measures
whether 1,200 is the right number for any action.

The second resource, which the brief does not weight and this deployment must: **requests.**
`openrouter/free` allows 50 requests a day (`crates/llm/src/provider.rs`). A desktop turn
can spend up to 12 emitter requests, 1 replier, 1 grounding regeneration and a summarizer
call every fourth turn — ~14 requests, three or four such turns a day. On this tier a
mechanism that saves an *iteration* is worth more than one that saves tokens. Both are
counted below.

Nothing records either number. `EventKind::ModelCall` (M6 §9, Phase 6) is unbuilt; the
only usage data is the raw `NS_TRACE` wire log, opt-in and unparsed.

---

## 3. Temporal: not adopted, and what is taken from the mapping

The brief maps α/C/τ onto Temporal Workflows and Activities. The engine already has the
properties that mapping exists to provide, and got them in Rust with no server:

| Temporal construct (brief §4) | ns-engine construct | Note |
|---|---|---|
| Workflow, deterministic, event-sourced | `Engine::run_turn` over `EventLog`; `fold()` is the replay | hash-chained, `verify_chain` on load |
| Activity behind a retry policy | `Emitter` / `Replier` / `Summarizer` / `Tool` trait calls; `OpenRouterClient` retry + throttle | every non-deterministic step is behind a trait — the brief's invariant 1 |
| Idempotent writes by turn id | `MemoryStore::append` skips ids ≤ the stored max; `put_fact` keyed by `(scope, key, valid_from)` | already replay-safe |
| Signals, timers | `Channel::recv` with the idle timeout in `next_message` | |
| Consolidator Workflow | `Consolidator` (`EvolutionPass`), idle driver + `ns-app evolve` | |
| Context ledger as workflow state | the log; every context is a projection of it | the brief's invariant 2 becomes: *the budget is computed from the log and recorded in it* |
| `continue-as-new` with the folded ledger | not needed — the window and summary already bound what any turn reads; the log is unbounded and stays so | |
| Human in the loop that survives a process restart | **missing**: `run_turn` persists only after `Replied` | see T0.4 |

Adopting Temporal would add a server, a second event history duplicating the engine's
own (the brief's §6 names this cost itself), and a dependency on a Rust SDK that is not
where the Python one is; the Temporal Agent Harness is Python and pre-1.0. It would buy
durable waits across process restarts and a worker fleet. Neither is a need this
deployment has: one process, one desktop, a CLI.

**Trigger to revisit:** a channel where a turn must outlive the process (the Telegram
target, or the desktop compose-box channel from `14e9ad7` once a task runs for hours), or a
second worker. Until then the one durability gap worth closing is cheap and is T0.4.

---

## 4. Phase 0 — Merge, measure, persist (½–1 day)

Nothing else in this plan can be evaluated without this phase.

**T0.0 Merge `messages-client`.** It carries the trace clip (`eef270c`), unattended mode
(`8837c33`, `confirm_irreversible`), the override backoff and the desktop channel. The rest
of M7 edits the same lines in `turn.rs`; merging after would be a conflict for nothing.

**T0.1 `ModelCall` events.** M6 §9 as specified, with the manifest extended for M7:

```rust
EventKind::ModelCall {
    role: String,                 // emitter | replier | summarizer
    model: String,
    prompt_tokens: u32, completion_tokens: u32,
    estimated: bool,              // true when the provider sent no `usage`; chars/4
    latency_ms: u32,
    manifest: ContextManifest,    // fact_keys, summary_through, window: (from, to),
                                  // trace_lines, trace_chars, clipped_chars, tools: n,
                                  // tier (Phase 3), budget: BudgetReport (Phase 2)
}
```

Plumbing: `OpenRouterClient::chat` already returns the whole body, which carries `usage`
on every OpenAI-style endpoint including Ollama. Rather than change the three trait
signatures and every scripted double, add `nscore::UsageSink` (`Arc<Mutex<Vec<Usage>>>`),
installed on each role client by `with_usage_sink(role)`; the engine drains it after each
`propose` / `reply` / `summarize` and appends one `ModelCall` per entry, next to the event
it belongs to. Excluded from `replay::normalize`; ignored by `fold`. Files:
`core/src/event.rs`, `core/src/traits.rs`, `llm/src/client.rs`, `engine/src/turn.rs`,
`engine/src/replay.rs`, `app/src/main.rs`.

**T0.2 `ns-app budget <session>`.** Per turn: requests by role, prompt and completion
tokens, peak prompt, trace chars sent (summed over iterations), clipped chars, tool count;
totals; requests per turn as its own column. For sessions recorded *before* T0.1 the
command reconstructs the emitter context per iteration from the log the way `render_echo`
does, and reports estimated chars with the facts block as a known floor. That gives the
baseline for the `cli` session today, without spending a request.

**T0.3 Baseline row.** Run T0.2 on `cli` and record it in the plan's §12 status. The
number to beat is the summed trace chars of turn 7.

**T0.4 Persist after side effects.** `run_turn` appends to the store once, after
`Replied`. A crash between a `pointer_click` that happened and the end of the turn loses
the `ToolCalled`/`ToolReturned` pair — the one record that a click *did* happen on a real
desktop. `append` is already idempotent by id, so: append after every `ToolReturned` whose
spec is `Reversible` or `Irreversible`, and at the end as now. One line at step i, one test
(a tool that panics the engine after returning leaves its outcome in the store). This is
the whole of what the brief's "Activity result survives a crash" buys here.

Exit: `cargo test --workspace` green; `ns-app budget cli` prints a table; `ModelCall` rows
appear in a fresh live turn with real `usage`.

---

## 5. Phase 1 — Tool results: clip with a handle, recover on demand (1 day)

The log already holds the full result inside `ToolReturned` — it *is* the archive the brief
wants (Self-GC's "recoverable sidecar"). What is missing is a handle in the prompt and a
way back.

**T1.1 Clip at render, name the remainder.** The branch's `TRACE_LINE_MAX_CHARS` becomes
`[memory] tool_result_max_chars = 1200` on `EngineConfig`, and the clipped line carries the
call's event id:

```
ToolReturned(ok: MODAL (handle this before anything behind it): | button "OneDrive…
  … [r42: 14425 chars, 1200 shown — inspect_result to see more])
```

Head-clipping is the right cut for `ui_read` specifically: the tool puts `MODAL` first, so
the part a model must not miss survives every cap. Verify that with a test on the recorded
14k tree, not by assumption.

**T1.2 `inspect_result { id, query? }`** — a synthetic action, `Pure`, legal only while a
clipped result exists in this turn's events (this-turn state only, so replay reproduces
it; M6 §15 rule). With `query`: the lines of result `r<id>` containing the query tokens,
capped at `tool_result_max_chars`. Without: the next page. Output trust = the original
`ToolOutput.trust`. Provenance: `ValueIndex::from_events` already indexes the full stored
summary, so a coordinate copied from an inspected line classifies as `CopiedOutput` — one
test to pin that. `pointer_ui_find` stays the better path for the desktop and its
description says so; `inspect_result` is the generic one, for `http_tool` bodies and every
tool that comes later.

**T1.3 Fold this turn's older spans.** Within a turn, keep the last `trace_verbatim_lines`
(default 5) trace lines verbatim and collapse everything older into one deterministic
line: `earlier this turn: pointer_move ×3 ok · pointer_click ok · pointer_ui_read ok (r42,
clipped)`. Rejections stay verbatim — they are what steers the next proposal. This is the
2606.10209 arm (last-N tool spans + summary, 71% → 92% completion), and it is *not* the
consolidated window that `2026-09-04-reply-entrainment.md` §9 rules out: the cross-turn
verbatim window is untouched, and the fold is a count, not a model's paraphrase.

Tests: handle format; `MODAL` survives the cap on the fixture tree; `inspect_result`
returns matching lines, pages, and is illegal with nothing clipped; the fold keeps the last
K and every rejection; the existing replay fixtures are byte-identical (no turn in them has
a result over 1,200 chars — check, do not assume).

Exit: on the `cli` reconstruction, the trace block at turn 7 from iteration 2 on is ≤ 2k
chars instead of ≥ 14k, and the dropped tree is reachable through `inspect_result`.

---

## 6. Phase 2 — The budget as engine state (1–2 days)

**T2.1 `ContextBudget`.** `[memory] prompt_budget_tokens = 6000` and a deterministic
`fit(ctx) -> (ctx, BudgetReport)` applied to `EmitterContext` and `ReplyContext` before
rendering. Estimator: chars/4, fixed — a calibration ratio per model is *reported* from
`ModelCall` usage (Czech text runs nearer chars/3) but never changes the decision, so
replay stays deterministic. Priority when over budget, first to go: oldest window records;
non-pinned relevant facts; the folded trace line; then `SessionSummary::clamp`. Never the
user text, the rejections, the pending-confirmation line, or the last `trace_verbatim_lines`.
Same shape as `clamp`, which is why it belongs in `core/src/memory.rs` beside it.

**T2.2 Monitor before gate.** `budget_mode = "report" | "enforce"`, default `report`:
`fit` computes what it *would* drop, records it in the `ModelCall` manifest, and drops
nothing. `enforce` is switched on when `ns-app budget` over real sessions shows a
**no-impact rate** — the share of would-be drops not followed by a fallback, re-ask or
`Corrected` in the same or next turn — that is acceptable. Self-GC reports 85% on that
metric for its prunes; below that the priority order is wrong, not the idea.

**T2.3 Budget line for the model — an experiment, off by default.** VISTA lifted a
Flash-class model from 22.7% to 50.7% by showing it its own block sizes and remaining
budget. One line, `show_budget_line = true`:
`Context: ~2.1k of 6k tokens · 1 result clipped (r42) — narrow with pointer_ui_find or
inspect_result.` Whether a small emitter acts on it is unknown; Phase 5's desktop task set
measures it as an arm.

**T2.4 Tool schemas.** `build_tools` sends every legal tool's description and JSON schema
on every emitter request: with the desktop wired in that is 10 pointer tools, `get_time`
and up to six synthetic actions, each with a rationale property. **Measured as of T0.1**:
`Usage::tools_tokens` records the serialized `tools` array per call, so
`tools_tokens / prompt_tokens` is the fraction, per turn and per legal set. T0.2 reports it
as a column.

What that fraction decides is *not* whether to restructure the schema. Under a quarter,
shortening descriptions is housekeeping, and Phase 3's tiering already takes the pointer
schemas off `Chat` turns. Only if pointer-heavy states push it toward 60% is anything
structural warranted — and the usual candidate, collapsing N flat strict tools into one
union-typed tool, would be paid for where this workspace can least afford it. N flat strict
tools is the most portable construct available; strict-mode *unions* are the worst-supported
thing on weak OpenAI shims, which is to say on `ollama`, `llamacpp` and `lmstudio` — three of
the seven shipped presets, and the endpoints whose failures `CloudEmitter`'s text-only and
empty-message fallbacks already document. Portability here is an asset, not an accident. The
union rewrite is a measurement question, and the measurement now exists.

**T2.5 The rationale field was not first, and that was load-bearing** *(found and fixed
during T0.1)*. Findings 2026-09-01 adopt "free-text `rationale` field FIRST in the proposal
schema" as the mitigation for the constraint tax — constrained decoding degrading task
accuracy — and the parent spec §172 repeats it: "FIRST field in the emitted schema
(think-then-commit)". `build_tools` injected it as `obj["properties"]["rationale"]`, and
`serde_json::Map` is a `BTreeMap` unless the `preserve_order` feature is on, which it is not
here. Properties therefore serialize alphabetically, and under strict mode it is `properties`
order that shapes generation. The field landed wherever `r` sorts: after `button` for
`pointer_click`, after `key` and `modifiers` for `pointer_type`, after `address`, `id`,
`path` or `query` elsewhere. The one action it was right for is `echo`, whose sole argument
is `text` — which is why the existing test passed. That test asserted the order of
`required`, a `Vec`, which was correct all along.

Fixed by renaming the schema key to `_rationale` (0x5F sorts before every lowercase letter),
**not** by enabling `preserve_order`. The audit that settled it: `Engine::call_key` builds the
repeat gate's identity as `action + args` serialized, resting in a comment on "equal objects
serialize identically", so a global ordering change would silently stop the gate catching the
repeated-call failure it exists for; and `event_hash` re-serializes whole events, args
included, for a chain `verify_chain` recomputes. `ArtifactId::for_content` (raw bytes) and
`Note::hash_of` (strings) are unaffected; `Patch::hash` carries no `Value` today, though its
comment claims canonical JSON. The emitter still strips a bare `rationale` too, for shims
that ignore `strict` and generate from the description instead.

BAML-style tolerant parsing of a malformed tool call belongs in `CloudEmitter`'s existing
fallback branch, not on the primary path: the primary path's illegality guarantee comes from
the provider constraining generation to the legal set, and that is worth more than the
parses it would recover.

Exit: `enforce` on the fixture set never exceeds the cap; replay fixtures unchanged;
`ns-app budget` shows the per-turn would-drop list on `cli`.

---

## 7. Phase 3 — Intent router and tiered budgets (1–2 days)

MemFlow's shape, without a model in the router: `Router` is a trait in `nscore`, the
default is `KeywordRouter`, and it is pure over the user text and the folded state — which
keeps replay deterministic and makes the cue list a candidate the evolution pass can
propose and gate later.

```rust
pub enum Tier { Chat, Task, Deep }
pub trait Router: Send + Sync {
    fn route(&self, user_text: &str, state: &SessionState, legal: &[ActionSpec]) -> (Tier, Vec<String>); // tier, cues hit
}
```

Rules, in order: an active pending confirmation → `Task`; a recall cue → `Deep` (`before`,
`earlier`, `last time`, `what did i`, `previously`, and the Czech ones this machine
actually uses: `předtím`, `dřív`, `minule`, `co jsem`); a cue for a registered tool family
(`click`, `type`, `open`, `screen`, `window`, `klikni`, `napiš`, `otevři`) or a tool call in
the previous turn → `Task`; otherwise `Chat`.

| Tier | legal set | facts | extra material | budget |
|---|---|---|---|---|
| Chat | synthetic actions only | pinned | — | 3k |
| Task | everything | pinned + relevant | — | `prompt_budget_tokens` |
| Deep | everything | pinned + relevant | **pre-emptive recall**: the engine runs `search_turns` + `search_facts` on the user text and injects a `Recalled:` block, same rendering and trust rules as the `recall` action's output | `prompt_budget_tokens` |

Two consequences worth the phase on their own. `Chat` turns carry no pointer schemas, which
is T2.4's cost gone for the turns that never needed it. `Deep` turns get their evidence
without spending an emitter iteration on `recall` — one request saved per such turn on a
50-a-day budget.

**Escalate instead of refuse.** A misroute is the risk. If the emitter proposes a tool
that exists but was tiered out, the engine escalates the tier for the next iteration
instead of recording `IllegalAction` and narrowing — one iteration of cost, never a wrong
refusal. This is MemFlow's "validator retries with a heavier tier", symbolic. The
escalation goes into the manifest as `tier: Chat→Task` and becomes a mining signature
(`Misrouted`) so the cue list can be corrected offline.

Tests: each rule; the `Recalled` block appears in `Deep` only and carries min trust; a
tiered-out tool proposal escalates and executes on the next iteration; a session replayed
through the router yields the same tiers.

Exit: on the memory task set, "what did I ask you first today?" is answered in one
emitter request; on the desktop set, `Chat` turns show `tools: 7` in the manifest, not 17.

---

## 8. Phase 4 — Session digests: one level of coarsening above the session (1–2 days)

M6's episodic memory stops at the session: the rolling summary is per session, and across
sessions only facts survive. The brief's MemForest/TiMem answer is a temporal tree with
dirty-path refresh. One level of it, derived from what already exists:

- **Session digest** = the last `SessionSummary` of a closed session, written once
  (idempotent by session id) into a `session_digests` table with FTS5, by the consolidator
  and at channel close. No model call: the summary was already paid for.
- **Scope digest** = ρ over the last `digest_sessions` (8) digests of a scope, one
  summarizer call, regenerated **only when a new session digest exists** since the last
  one — the dirty-path rule, at depth one. Rendered as one block, `Earlier sessions: …`,
  ≤ `digest_max_chars` (400), in the `Deep` tier only. Trust = min over its inputs.
- **Recall across sessions.** `recall` and the `Deep` tier's pre-emptive recall search the
  current session first, then the last `recall_sessions` (3) of the scope, verbatim turns
  still ranked above digests. Needs `MemoryStore::search_turns_in(&[SessionId], …)`, a
  small generalization of the existing FTS query.

This takes the slot M6 §7 reserved for "observations" and replaces them, for now, with
something that adds zero hot-path model calls and stays inside the verbatim-first rule:
digests are summaries of summaries of verbatim records, with the chain bounded by the
existing rebuild cadence, and they are indexes for `recall`, not replacements for it. The
typed, model-extracted observations of M6 Phase 5 remain `[LATER]`; the ablation that put
extracted artifacts 16–22 points below verbatim text (`2601.00821`) has not been answered.

Tests: a session closed twice writes one digest; the scope digest is regenerated only when
dirty; `Deep` renders it, `Chat` and `Task` do not; a two-session fixture — a fact stated
in session 1 without `remember_fact`, asked in session 2 — is answered from cross-session
recall.

Files: `memory-sqlite/src/lib.rs` (table, FTS, `search_turns_in`), `engine/src/store.rs`
(in-memory twin), `evolution/src/consolidate.rs` (digest step), `core/src/traits.rs`.

---

## 9. Phase 5 — Fixed-model regression set and the offline loop (1–2 days; can start after Phase 0)

"Don't Blame the LLM" (`2607.03691`): fix the model, vary the harness, measure. The replay
harness verifies determinism; it does not measure the model. This phase adds the set.

**T5.1 Task set** under `crates/engine/tests/harness_eval/`:
- the six M6 Phase 7 memory abilities (extraction, multi-session, temporal, updates,
  abstention, forgetting), scripted;
- three desktop tasks against `NullPlatform` with the recorded 14k control tree as the
  `ui_read` fixture: open-and-search, find-and-click, a task that needs the clipped tail.

Each run reports: completion, requests per task, prompt tokens per task (estimated
offline, real when live), peak prompt, clipped chars, `inspect_result` calls, fit drops,
tier per turn, recall hits. Scripted doubles by default; `--live` runs the real model
under a request cap (`eval_request_cap = 20`, because the day has 50).

**T5.2 `ns-app eval [--live]`** prints the table and appends a row to the ledger keyed by
harness git hash and model id. The previous row is diffed and regressions named. Every
harness release runs it once; that is the whole procedure.

**T5.3 Mining signatures for the pass** (`evolution/src/mine.rs`): `BudgetDropped` (a
would-drop or drop preceded a fallback / re-ask in the next turn), `ResultClippedThenInspected`
(the cap was too low for that action), `Misrouted` (tier escalated). Symbolic-lane
candidates from them: a per-action `tool_result_max_chars`, a cue added to the router. Both
are deterministic config patches that the existing symbolic gate can replay-verify. This is
ACON's "refine the compression guidelines from failures", with the evolution pass as the
loop and no model in it.

Exit: `ns-app eval` runs offline in under a minute and leaves a baseline row; one `--live`
run recorded with the model id.

---

## 10. Escalation — a decision, not a phase

The brief's step 5 is a validator on the small model's answer with a larger model behind
it. The parent spec discarded the emitter cascade on 2026-09-01. The brief re-raises it with
stronger evidence than was available then (MemFlow's validator-then-heavier-tier; FrugalGPT;
`2601.11327` putting the reasoning budget in the orchestrator).

Recommendation: **keep the decision until Phase 5 says otherwise.** Phases 1–4 are the
cheaper harness fixes the brief itself ranks first, and Phase 3's tier escalation is the
validator-retries shape without a second model. If the eval rows then show a failure class
concentrated where a stronger emitter would plausibly help — malformed proposals exhausting
`max_emit_retries`, or `ReplyFlagged` regenerations that still fail grounding — the build
is small: an optional `[llm.emitter_strong]` role, used only on those two triggers, one
request, logged in the manifest as `escalated: true`. On the free tier there is no second
model to escalate to, so this is moot until a paid role exists.

---

## 11. Configuration, files, order, metrics

```toml
[memory]
tool_result_max_chars = 1200   # Phase 1; per-action overrides may come from the pass
trace_verbatim_lines = 5       # Phase 1
prompt_budget_tokens = 6000    # Phase 2
budget_mode = "report"         # "enforce" once the no-impact rate is known
show_budget_line = false       # Phase 2 experiment
recall_sessions = 3            # Phase 4
digest_sessions = 8
digest_max_chars = 400

[router]                        # Phase 3
enabled = true
recall_cues = ["before", "earlier", "last time", "what did i", "předtím", "dřív", "minule"]

[evolution]
eval_request_cap = 20          # Phase 5 --live
```

| Crate | Files touched | Phases |
|---|---|---|
| core | `event.rs` (`ModelCall`), `traits.rs` (`UsageSink`, `Router`, digest and `search_turns_in` on `MemoryStore`), `memory.rs` (`ContextBudget`, `fit`, manifest types) | 0, 2, 3, 4 |
| engine | `turn.rs` (clip handle, `inspect_result`, fold, `fit`, router hook, incremental persist, `Recalled` block), `state.rs` (clipped-result index), `replay.rs` (normalize), new `budget.rs`, new `router.rs` | 0–4 |
| llm | `client.rs` (usage), `emitter.rs` / `replier.rs` (budget line) | 0, 2 |
| memory-sqlite | digests table + FTS, `search_turns_in` | 4 |
| evolution | `consolidate.rs` (digests), `mine.rs` (signatures), `pass.rs` / `ledger.rs` (eval rows) | 4, 5 |
| app | `config.rs`, `main.rs` (`budget`, `eval` subcommands) | 0, 5 |

**Order:** 0 → 1 → T5.1 (the set, so every later phase is measured on it) → 2 → 3 → 4 →
rest of 5 → §10 decision. Seven to ten days. Phase 1 is the one that pays on the first
desktop run; Phase 3 is the one that pays in requests.

**Metrics, in the order they matter here:**
1. requests per turn and per completed task (the free-tier constraint)
2. prompt tokens per turn, peak prompt, and trace chars summed over iterations
3. clipped chars and `inspect_result` rate per action (is the cap right?)
4. fit would-drop / drop count and the no-impact rate (Self-GC's number)
5. tier distribution, misroute rate, iterations saved by pre-emptive recall
6. digest freshness: session close → digest searchable
7. task-set completion per harness release, fixed model

---

## 12. Risks, and what is deliberately not built

- **The small model ignores the budget line.** Then T2.3 stays off; it costs one line to
  find out and the arm is in the task set.
- **Misroutes.** Escalate-on-illegal bounds the damage to one iteration; `Misrouted` in the
  pass corrects the cues. A model that never proposes the tiered-out tool because it
  cannot see it is the residual risk, and the reason `Task` is the default whenever the
  previous turn used a tool.
- **`inspect_result` spends iterations** the free tier does not have. Its description
  points at `pointer_ui_find` first; the eval counts how often it is used and whether the
  turn then completes.
- **chars/4 is wrong for Czech.** The estimate is reported next to real usage per model;
  the cap is set with headroom; the estimator constant does not move at runtime.
- **Two more synthetic actions in the schema** (`inspect_result`, and the router's effect on
  the legal set) are two more things for a small model to misuse. Legality rules keep both
  out of the schema when they cannot apply.

Not built, with the recorded reason: Temporal (§3); typed observations (§8); graph or
vector memory (M6 §12.8, trigger unchanged); a model in the router or in the fold (both
would put a judge on the critical path — M6 §8.7's argument); a consolidated window (plan
2026-09-04 §9); any RL or fine-tuning (parent spec §13); escalation (§10, a decision for
after Phase 5).

---

## 13. Status

- 2026-09-07: plan written against `main` @ a478d46 and branch `messages-client` @ eef270c.
  Baseline numbers in §2 from `ns-run/ns.sqlite` session `cli` via `ns-app dump`. Nothing
  built yet; T0.0 (merge) is the first step.
- 2026-09-08: Phase 0 and Phase 1 built on branch `m7-context-budget`, which is based on
  `messages-client` rather than merging it — T0.0's purpose was to have that code underneath,
  and basing achieves it without touching anyone's branches.

  | Task | State | Commit |
  |---|---|---|
  | T0.0 base on `messages-client` | done | branch point |
  | T0.4 persist side effects mid-turn | done | `096636c` |
  | T0.1 `ModelCall` + `UsageSink` + manifest | done | `93bf508` |
  | T2.4 tools-array measurement (`Usage::tools_tokens`) | done, pulled forward | `93bf508` |
  | T2.5 `_rationale` ordering fix | done | `93bf508` |
  | T1.1 clip with a handle · T1.2 `inspect_result` | done | `37c6e37` |
  | T1.3 fold this turn's older steps | done | `ca3c7d8` |
  | T0.2 `ns-app budget` | done | `76663d9` |
  | T0.3 baseline row | done | `76663d9` |
  | `[memory] trace_verbatim_lines` config key | done | `76663d9` |

  **Phase 1 exit criterion, met.** `ns-app budget cli` on the recorded session
  (`ns-run/ns.sqlite`, copied so the live store was untouched) reports the raw trace beside
  the same trace as the engine would send it now — the second number computed by calling
  `trace_for_prompt`, not by reimplementing it:

  ```
  turn      window  summary    trace     sent    chars  ~tokens  tools
  t6          1026        0     9742     1314    10768     2692      1
  t7          1327        0    14108     1311    15435     3858      1
  total       9049        0    25986     4761    35035     8758     15
  ```

  Turn 7: 14,108 → 1,311 characters, against the criterion of "≤ 2k instead of ≥ 14k".
  Across the session, 21,225 characters saved **per send** — and the emitter re-sends the
  trace on every remaining iteration, so the turn-level saving is that figure multiplied by
  however many iterations followed. `inspect_result` reaches the dropped tail, which is the
  other half of the criterion.

  The reconstruction is still a floor: it counts neither standing facts, nor the system
  prompt, nor the tool schemas, and counts one send per turn rather than one per iteration.
  The measured path (any session recorded from now on) has all of those.

  Two things learned while building, both recorded above: the rationale field was not
  actually first in the emitted schema (§T2.5), and the fold's verbatim budget has to count
  *outcomes* rather than lines — counting lines let proposal/rejection churn fill the window
  and folded away the turn's only result, after which the reply narrated something its own
  prompt no longer contained.

  Deferred to the config pass that follows T0.2, to avoid two writers in `app/`:
  `[memory] tool_result_max_chars` and `trace_verbatim_lines` exist as `EngineConfig` fields
  with the plan's defaults (1200, 5) but are not yet readable from `config.toml`.
