# Memory Redesign — Design & Plan (M6)

Date: 2026-09-02. **Revision 2** (same day), rewritten after the research sweep in
`docs/research/2026-09-02-memory-findings.md`. Changes from revision 1 are marked **[R2]**.
Status: Phases 0–4 implemented on branch `m6-memory` (2026-09-02 evening); Phases 5–7 open.
Implementation notes and deviations are in §15; the open decisions of §12 were settled as
recorded there.
Parent spec: `2026-09-01-neuro-symbolic-harness-design.md` §5 (contexts), §6 (`MemoryStore`,
`Fact`), §7 (persistence), §13 (deferred decisions).
Extends: `2026-09-02-evolution-pass-design.md` (the pass gains an evaluation lane, §8).
Evidence: the `cli` session in `ns.sqlite`, 117 turns over 2026-09-01/02 (§2).
Reference implementation studied: claude-mem 13.23.1 (local plugin). Ideas taken from it are
marked **[claude-mem]**; what we deliberately do differently is in §13.

## 1. Goal

Give the assistant a memory that works at four time scales. Every layer is either a
deterministic projection of the event log or a derivative that carries provenance and trust
and is verified before it changes behavior.

| Layer | Scale | Content | Written by | Read by |
|---|---|---|---|---|
| **Working** | this turn | user text, this turn's trace, rolling window of verbatim turn records | the fold (deterministic) | emitter, replier |
| **Episodic** | this session | rolling summary of turns outside the window; typed observations | cheap model, off the hot path | emitter + replier (summary); `recall` tool (observations) |
| **Semantic** | across sessions | versioned facts with validity windows, scoped, searchable | `remember_fact` / `forget_*`, consolidation | context (pinned + relevant slice), `recall` tool |
| **Procedural** | across sessions | learned rules (M5) | evolution pass | engine |

Plus an **evaluation lane** (§8): symbolic checks first, an evaluator model second, so the
evolution pass can see the failures the engine itself cannot.

Principles carried over: the log is the truth; every context is a bounded projection; the
symbolic engine decides legality; models produce text and signals, never decisions; nothing
derived changes behavior until it is verified.

**[R2] Added principle — verbatim first.** The recorded text of the conversation is the
primary memory. Summaries, facts and observations are indexes over it and aids for
abstention and updates; they never replace it. The controlled ablation in findings §1 puts
extracted artifacts 16–22 points below verbatim text on both standard benchmarks, and the
gap is "lossy distillation, not structure".

## 2. Findings this design answers

From the analysis of the recorded session (2026-09-02):

- **F1 — The replier never sees the user's message.** `turn_trace` keeps only Proposed /
  Rejected / ToolReturned lines and `state_summary` is `turn N, M messages`. On a plain
  `respond_directly` turn the reply model receives facts, a counter, and one line
  `Proposed(respond_directly)`. Every strange reply in the log follows from that.
- **F2 — The emitter never sees facts.** It re-proposed `remember_fact(user.name)` on turns
  63, 64, 65 and 99, and on turn 85 asked for a name the replier already knew.
- **F3 — No memory beyond ~3 exchanges, and none of it carries tool outcomes.** "What time
  was it before you checked" (turn 76) is unanswerable from user/assistant text pairs.
- **F4 — Fact lifecycle is broken.** Rewrites reset `uses`; duplicates are written every
  turn; junk keys with values the user never said; no way to forget; alphabetical
  truncation at 20; "what was my name before" (turns 104–108) is unanswerable because the
  old value was overwritten.
- **F5 — Tool output is not human-readable.** `get_time` returns unix milliseconds.
- **F6 — The pass is blind to bad replies.** Dozens of visibly bad turns, zero signatures.
  No `Corrected` event is ever produced; "that is wrong" / "i dont want that" / "answer
  the question i asked" are invisible.
- **F7 — Replier failures leave no event**, and token usage is discarded.
- **F8 — Provider quota errors are retried blindly**; `Retry-After` is ignored.
- **F9 — Facts are global.** Correct for a single-user CLI, a leak for the Telegram target.

## 3. Trade-offs: what this buys and what it costs **[R2]**

Per layer, with the mitigation built into the design and the evidence behind it.

| Layer / mechanism | Advantage | Disadvantage | Mitigation | Evidence |
|---|---|---|---|---|
| Verbatim turn records in both contexts | Fixes F1–F3 with no model call; deterministic, replay-safe; highest-fidelity memory representation | +≈1k input tokens per call; window carries the bot's own bad replies until fixed | Caps per field; fix reply context in the same phase | findings §1 |
| Facts in the emitter context | Ends re-remembering and asking for known values | Irrelevant facts degrade relevance and constraint awareness; the model may over-trust them | Pinned core + query-relevant slice ≤10; `(unverified)` / `(was …)` markers | findings §1 (2606.25361, 2603.17787) |
| Rolling summary as `Summarized` event | Context beyond the window at ~250 tokens; models' input reconstructible from the log | One extra cheap call every 4 turns; semantic drift over long sessions; a laundering channel for untrusted content; derived prose in the log | Regenerate from verbatim records, periodic full rebuild; summary carries min trust; excluded from replay diff | findings §2 (2603.11768), §5 (2606.24322) |
| Versioned facts (supersede, never delete) | Answers "what was X before"; no ghost-memory mixing; audit trail | More rows; state labels must be rendered correctly or they confuse the small model | Only pinned keys show the previous value; history behind `recall` | findings §3 (2607.01935, 2501.13956) |
| `forget_fact` / `forget_all` / decay to cold | User control; the recorded "reset memory" turn becomes a real action; smaller hot set | Irreversible purge needs the confirmation round-trip; cold facts can be missed | Confirmation gate already exists; cold stays searchable | findings §3 (2604.20300, 2603.14212) |
| `recall` over FTS5 (verbatim first) | Progressive disclosure; unbounded history at bounded context; no new dependency | Lexical only: paraphrases miss; an extra emitter iteration per recall; precision depends on ranking | BM25 threshold + top-k; verbatim ranked above derived; vector search deferred with a measured trigger | findings §1, §4 (2603.09023, 2605.11325) |
| Observations (typed episodic records) | Cross-session episodic memory; promotion path to facts; searchable | Another table and taxonomy to maintain; small-model extraction quality; write amplification if too eager | Notable turns only; dedupe by hash; shared call with the grade; conservative write policy | findings §2, §5 (2606.04329) |
| Evaluation lane (symbolic + judge) | Makes the pass see bad replies at all; reply-scoped notes; calibration data | Judge bias and instability; cost per pass; risk of tuning to the judge | κ-calibrated against symbolic proxies; plain-text rubric; candidates gated as before; reply probe off by default | findings §6 |
| In-turn symbolic grounding interceptor | Catches fabricated outcomes/values live at zero model cost | False positives cost one extra reply call; adds ~1 s on flagged turns | One regeneration max; both replies logged; stoplist + sentence-initial filter | findings §6 (2604.16706) |
| `ModelCall` + context manifest | Token accounting; exact reconstruction; memory influence attribution | 2–6 extra events per turn; log growth | Excluded from replay diff; compact manifest (keys and ids only) | findings §5 (2605.23723) |
| Cooldown on `Retry-After` | No hammering a dead provider; honest replies during outages | A misreported header stalls the bot for its duration | Cap the cooldown at 5 min; still one probe call at expiry | findings §5 |

System-level costs, honestly:

- **Model calls.** Today 2 per generated turn. After M6: 2 on the critical path (unchanged),
  plus a summarizer call every 4 turns, plus a bounded number of evaluator/observer calls
  per idle pass. Flagged turns pay one extra reply call.
- **Tokens.** Reply context grows from ≈150 to ≈1.5–2k input tokens; emitter context by
  ≈1k. Prefix caching keeps persona/facts/summary cheap where the provider supports it.
- **Complexity.** 3 new event kinds, 2 tables, 3 FTS indexes, 3 synthetic actions, 2 new
  traits, ≈10 config knobs. Replay and the pass must learn the new events.
- **Attack surface.** A memory that persists is a memory that can be poisoned. Write-time
  origin binding, trust propagation and conservative writes are the answer the literature
  gives; they are in scope, not optional.
- **Small-model ceiling.** Summaries and grades come from the same small model that
  produced the weird replies. The design assumes it can summarize and grade *given the
  right input*, which is what the working-memory fix provides; the evaluation suite (§11
  Phase 7) is how we find out.

## 4. Working memory

### 4.1 Turn records and the window

The fold builds one record per completed turn, replacing `history: Vec<(String, String)>`
as the source both contexts read:

```rust
// crates/core/src/memory.rs (new; crosses the trait boundary)
pub struct TurnRecord {
    pub turn: u32,
    pub user: String,          // verbatim, capped
    /// One line per action of the turn, in order:
    ///   "get_time -> ok: 2026-09-02 10:41:28 UTC"
    ///   "remember_fact user.age=17 -> ok"
    ///   "wipe -> denied (taint_policy: external source)"
    ///   "asked: Could you clarify …"          (ask_clarification)
    ///   "staged: 'wipe' awaits confirmation"  (PendingConfirmation)
    ///   "reply failed: HTTP 402 …"            (ReplyFailed)
    pub did: Vec<String>,
    pub reply: String,         // verbatim, capped
    pub trust: Trust,          // min trust over the turn's tool outputs (User if none)
}
// crates/engine/src/state.rs
pub struct SessionState { …, pub records: Vec<TurnRecord>, pub summary: Option<SessionSummary>, … }
pub fn window(records: &[TurnRecord], k: usize, caps: &Caps) -> String;
```

Rendering (shared by both contexts, deterministic, capped per field):

```
[t74] user: tell me the time
      did:  get_time -> ok: 2026-09-02 10:41:28 UTC | get_time -> denied (repeat_gate)
      bot:  It is 10:41 UTC.
```

Sizes (config `[memory]`, §10): `window_turns = 6`, `record_max_chars = 300`, tool-output
line cap 120 chars; the window stays under ≈1k tokens.

### 4.2 One projection, two renderings

```rust
// crates/core/src/traits.rs
pub struct EmitterContext {
    pub facts: Vec<FactView>,             // NEW (F2): pinned + relevant, with state markers
    pub summary: Option<SessionSummary>,  // NEW (§5.1)
    pub window: Vec<TurnRecord>,          // replaces recent_turns (F3)
    pub trace_so_far: Vec<String>,
    pub rejections_this_turn: Vec<String>,
    pub guidance: Vec<String>,
}
pub struct ReplyContext {
    pub persona: String,
    pub facts: Vec<FactView>,
    pub summary: Option<SessionSummary>,
    pub window: Vec<TurnRecord>,          // NEW (F3)
    pub user_text: String,                // NEW (F1)
    pub turn_trace: String,
    pub guidance: Vec<String>,            // NEW: reply-scoped notes (§8.5)
    pub do_not_state: Vec<String>,        // NEW [R2]: spans flagged by the interceptor (§4.5)
}
```

Block order in the reply prompt stays stable-first for prefix caching: persona (system) →
facts → summary → window → current turn (user text, trace) → guidance → instruction.

Token budget for the reply context (Mistral small, defaults):

| Block | Budget |
|---|---|
| persona | as configured |
| facts (≤10, §6.5) | ≤ 300 tokens |
| summary (`summary_max_chars = 800`) | ≤ 250 tokens |
| window (6 records) | ≤ 1000 tokens |
| current turn | user text + trace, uncapped (trace lines capped at 200 chars) |

### 4.3 The reply prompt

```
Standing facts:
- user.name: "Peter"  (was "Martin" until t96)
- user.age: "17"

Conversation so far: <summary.topic>. Established: <…>. Open: <…>.

Recent turns:
[t105] user: what was my name before …

Current turn:
user: what was my name before i changed it to peter
did:  (nothing)

Reply to the user's current message. Use the recent turns and standing facts for context.
State only outcomes and values that appear above; if something failed or was refused, say
so plainly. Do not invent tool results, names, or numbers.
```

The old instruction "narrate ONLY what the trace supports" widens to "only what appears
above", the same guarantee over a larger projection of the log. Plain text, no markdown:
the judge literature finds style bias the dominant bias, and plain replies keep the
evaluator honest (findings §6).

### 4.4 Emitter context additions

The emitter's user message gains `Facts:` before the window; the window replaces `Recent
turns:`; `This turn so far` stays. Persona stays out of the emitter.

### 4.5 In-turn symbolic grounding interceptor **[R2]**

After a `Generate` reply is drafted and before `Replied`:

1. Collect *grounding material*: user texts in the window and the current turn, tool
   output summaries, rendered facts, summary, persona, guidance.
2. Extract *claims* from the draft: numbers (≥2 digits), quoted strings, capitalized tokens
   not sentence-initial and not in a small stoplist, and tool names.
3. Any claim absent from the material → append `ReplyFlagged { spans }`, regenerate once
   with `do_not_state = spans`, and use the second draft whatever it says. Both drafts are
   in the log; `ReplyFlagged` is a mining signature (§8.4).

No model is involved; the check is the provenance index applied to output. The runtime
interceptor in findings §6 cut fabricated tool executions by up to 24 points with the same
idea. Config `reply_grounding_check = true`; at most one regeneration per turn.

### 4.6 Worked examples against the recorded turns

| Turn | Then | After M6 |
|---|---|---|
| 76 "what time was it before you checked?" | `get_time` again, repeat gate, clarification | window carries `[t74] … 10:41:28` and `[t75] … 10:41:44`; emitter picks `respond_directly`; replier answers from the records. |
| 85 "whats my name" | asked the user for their name | emitter sees `user.name` in facts → `respond_directly`; replier: "Martin". |
| 93 "why not" | "the session has reached 185 messages…" | replier sees the user text and `[t92] … bot: I can't assist with that request.`; the interceptor would have flagged "185 messages" had it appeared. |
| 104 "what was my name before" | "Peter, your message was not delivered." | pinned fact renders `user.name: "Peter" (was "Martin" until t96)`; answer is in context. |
| 114 "can you reset the memory" | stored `memory_reset_requested = true` | `forget_all` is legal and Irreversible → staged → "This will forget 3 facts in scope global. Confirm?" → "yes" executes. |

## 5. Episodic memory

### 5.1 Rolling summary **[claude-mem]** (their fixed-field session summary)

Covers the turns that have fallen out of the window, so window and summary never overlap.

```rust
// ns-core
pub struct SessionSummary {
    pub through_turn: u32,
    pub topic: String,            // what the user is trying to do, one sentence
    pub established: Vec<String>, // decisions / answers given, not already facts
    pub open: Vec<String>,        // pending questions, unconfirmed requests
    pub trust: Trust,             // [R2] min trust over the summarized records
    pub rebuilt_from: u32,        // [R2] first turn of the verbatim range used
}
#[async_trait] pub trait Summarizer: Send + Sync {
    async fn summarize(&self, previous: Option<&SessionSummary>, records: &[TurnRecord],
                       facts: &[FactView]) -> Result<SessionSummary, SummarizeError>;
}
EventKind::Summarized { summary: SessionSummary }
```

- **Cadence.** After the reply of turn T is sent (in `Engine::run`, off the user's path),
  let `S = T − window_turns`. If `S − last_summarized ≥ summary_every_turns` (default 4),
  summarize, append `Summarized`, persist. A failure appends nothing; the next boundary
  retries with the larger range. `summary_every_turns = 0` disables the layer.
- **[R2] Drift control.** The model always receives the *verbatim records* of the new
  range plus the previous summary for continuity; every `summary_rebuild_every` (default
  3) summaries it receives *all* records `1..=S` (oldest truncated first to
  `summary_input_max_chars`) and no previous summary, so summary-of-summary chains never
  exceed three links. Findings §2 names iterative summarization as the drift mechanism.
- **[R2] Trust.** `trust` = minimum over the records' trust. A summary built from External
  tool output is External; `TaintPolicy` treats side-effectful args grounded only in it
  like any External value. This closes the "agent summarization" laundering channel from
  findings §5.
- **Model.** `[llm.summarizer]`, default = the emitter model (small-model control planes
  are the norm in the 2026 systems, findings §2). JSON output, temperature 0, `max_tokens`
  400; output clamped deterministically to `summary_max_chars`.
- **Persistence as an event.** The summary is what the models were shown, so it belongs in
  the log; excluded from `normalize()` in the replay diff; `doubles_from` collects recorded
  summaries into a `QueueSummarizer`. Alternative (a table) in §12.

### 5.2 Observations **[claude-mem]** (their typed observation records)

Typed episodic records for *notable* turns only, written by the observer in the evolution
pass (§8), never on the hot path.

```sql
CREATE TABLE observations (
  id INTEGER PRIMARY KEY, scope TEXT NOT NULL, session_id TEXT NOT NULL, turn INTEGER NOT NULL,
  type TEXT NOT NULL, title TEXT NOT NULL, facts_json TEXT NOT NULL, narrative TEXT NOT NULL,
  concepts_json TEXT NOT NULL, trust TEXT NOT NULL, origin_json TEXT NOT NULL,  -- [R2] event ids
  event_time INTEGER,                                                          -- [R2] reserved
  content_hash TEXT NOT NULL UNIQUE, relevance_count INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL);
CREATE VIRTUAL TABLE observations_fts USING fts5(title, narrative, facts_json, concepts_json,
  content='observations', content_rowid='id');
```

- **Taxonomy from config** **[claude-mem]**: `[memory.observations]` lists `types` and
  `concepts`. Default chat set: `preference`, `event`, `decision`, `correction`, `task`.
- **Dedupe** by `content_hash = sha256(type + title + facts_json)` **[claude-mem]**.
- **relevance_count** increments when `recall` returns the row **[claude-mem]**.
- **[R2] Origin binding.** `origin_json` lists the event ids the record was written from;
  `trust` is the minimum over them. Records derived from External content are never
  promoted to facts without a user restatement.
- **Promotion.** Consolidation promotes an observation restated in ≥3 sessions of the same
  scope into a fact with confidence 0.8 and `Provenance::Transform{func:"promoted"}`.
- **Notable** = the turn called a tool, changed a fact, was corrected or flagged, or the
  evaluator marked it. Greetings and repeats are skipped **[claude-mem]**. Conservative
  writing is also the security recommendation (findings §5: aggressive writers are more
  exploitable).

## 6. Semantic memory: facts

### 6.1 Versioned facts **[R2]** (supersede, never overwrite)

```sql
ALTER TABLE facts ADD COLUMN scope TEXT NOT NULL DEFAULT 'global';
ALTER TABLE facts ADD COLUMN trust TEXT NOT NULL DEFAULT 'System';
ALTER TABLE facts ADD COLUMN valid_from INTEGER;      -- write time
ALTER TABLE facts ADD COLUMN valid_to INTEGER;        -- NULL = current
ALTER TABLE facts ADD COLUMN state TEXT NOT NULL DEFAULT 'current';  -- current|superseded|cold|forgotten
ALTER TABLE facts ADD COLUMN event_time INTEGER;      -- reserved (findings §3, temporal semantic memory)
-- primary key becomes (scope, key, valid_from); `facts(scope, prefix)` returns state='current'
```

Write policy (engine-side; the store stays dumb):

| Case | Result |
|---|---|
| same key, same value | keep `uses`, `last_validated = now`, `confidence = min(1.0, c + 0.1)` |
| same key, new value | old row → `state = superseded`, `valid_to = now`; new row `current`, `uses` inherited, `confidence 1.0` |
| new key | new `current` row |
| key differs only by `squash()` (case/punctuation) from an existing key | treated as the same key (deterministic canonicalization at write time) |

Ghost memory (old and current values mixed at retrieval) is the failure findings §3
measures; state labels are the fix. Rendering: pinned keys show `(was "X" until tNN)` when
a superseded row exists in the same session; the full chain is behind `recall`.

### 6.2 Forgetting (four mechanisms, findings §3)

| Mechanism | M6 |
|---|---|
| passive decay | unused for `fact_stale_days` (90) → `state = cold`: excluded from pinning, still searchable, revived on restatement |
| active deletion | `forget_fact { key }` — `Reversible`, sets `state = forgotten`, `valid_to = now`; unknown key → `Rejected{Malformed}`. `forget_all {}` — `Irreversible`, purges rows in scope; goes through `SideEffectGate` + `stage()` ("This will forget N facts in scope X.") and executes on `confirm_pending`. |
| safety-triggered | consolidation purges facts with `trust = External` that were never restated by the user; reported in the pass |
| adaptive | restatement bumps confidence (§6.1); contradiction supersedes |

`MemoryStore` gains `supersede_fact`, `forget_fact`, `purge_facts(scope)`, `fact_history`.

### 6.3 Residual values

`remember_fact` gets a `residual_policy` on `value`; config `[memory] remember_residual =
"flag" | "never"`:

- `flag` (default): a value with no grounding in the user's words or a tool output is
  stored with `confidence = 0.5` and rendered `(unverified)`; promoted to 1.0 on
  restatement. Keeps "remember I like coffee" working when the model paraphrases.
- `never`: `ResidualPolicy` denies it (forced clarification follows). For deployments where
  facts drive side effects (Tomáš orders).

### 6.4 Consolidation (in the pass)

- **[R2] Mutation-time proposals, deterministic acceptance.** The observer may propose
  `supersedes(K1, K2)` for keys `squash()` does not catch (`memory_reset_requested` vs
  `memory.reset.requested`). The pass applies a merge only when both current values are
  equal or the user restated one of them; otherwise it reports. Findings §3: LLM
  mutation-time hooks reach 91.7–93.2% where deterministic canonicalization fails, but our
  safety rule keeps the model as proposer.
- Decay to cold; safety purge; observation promotion (§5.2); duplicate-key report.

### 6.5 Selection and budget **[R2]**

`facts_for_context(scope, user_text)`:

1. **Pinned core**: current facts whose key starts with a `pinned_prefixes` entry
   (default `["user."]`), newest-validated first, at most `pinned_max` (5).
2. **Query-relevant**: `facts_fts` MATCH on the current user text (and the previous user
   text), top `relevant_max` (5) above `recall_min_score`, excluding pinned and cold.
3. Total ≤ `facts_in_context` (10). Governed Memory reports quality saturating near seven
   memories per entity; PrecisionMemBench shows dumping the store masks precision failures.

### 6.6 Scope (F9)

`Incoming` gains `scope: Option<String>` (CLI: `None` → `global`; Telegram later: the chat
id). `remember_fact` writes into the session's scope; `global` is written only by config or
consolidation. Zero cross-entity leakage is a measured property of scoped designs
(findings §1); decision in §12.

## 7. Recall tool and full-text search **[claude-mem]** (their index + fetch-by-id)

```
recall { query: string }   -> Pure, dedupe_tag: None
```

Three FTS5 sources (FTS5 is compiled into the bundled SQLite via `libsqlite3-sys`
`-DSQLITE_ENABLE_FTS5`; no new dependency):

| Rank | Source | Table | Returned as |
|---|---|---|---|
| 1 **[R2]** | verbatim turns of this scope beyond the window | `events_fts(kind_json)` external-content over `events` (UserSaid / Replied rows) | `t74 user: tell me the time / bot: It is 10:41 UTC.` |
| 2 | current facts in scope, then history | `facts_fts(key, value_json)` | `fact user.name = "Peter" (since t96; was "Martin" t22–t96)` |
| 3 | observations in scope | `observations_fts` | `obs #12 preference: prefers times in 24h format` |

- **[R2] Verbatim first**: for equal BM25 rank, event hits outrank derived rows.
- **[R2] Precision**: only hits above `recall_min_score`, at most `recall_top_k` (5). "No
  results" is a legitimate output and is what makes abstention possible.
- Output `trust` = the lowest trust among returned items; recalled values become
  `CopiedOutput` leaves of the recall call, so an arg copied from a recalled fact
  classifies as grounded and `TaintPolicy` sees External where it applies.
- **[LATER]** time-aware query expansion ("before", "last week") and vector search, with
  the trigger in §12.

## 8. Evaluation lane: the evaluator model beside the pattern matcher

The M5 mining lane (`crates/evolution/src/mine.rs`) is a symbolic pattern matcher over event
kinds. It cannot see F6. The evaluation lane adds a second signature source. **The
evaluator produces signals, never decisions**: it may add signatures and observations; it
may not write rules, write facts above confidence 0.5, relax anything, or run inside
`run_turn`.

### 8.1 Where it runs

In the pass, after mining, over turns the ledger has not graded, within
`evaluate_budget_turns` (60) per pass. Symbolic checks run on every turn for free; the
model runs only on turns that ended in `Generate` and were flagged or sampled.

### 8.2 Symbolic checks (no model, always on)

| Check | Rule | Signature |
|---|---|---|
| **Re-ask** | normalized user text equals one of the previous 3 user texts | `UserReask { times }` |
| **Ungrounded reply** | the interceptor's claim extraction (§4.5) over the logged reply and its manifest | `UngroundedReply { spans }` (also produced live as `ReplyFlagged`) |
| **Question ignored** | user text is a question and the reply shares no content token with it | `IgnoredQuestion` (weak; feeds the model check) |
| **Fallback / repeated denial / bad_args / Corrected / ReplyFailed** | M5, plus F7 | as today |

### 8.3 Model check

```rust
#[async_trait] pub trait Evaluator: Send + Sync {
    async fn grade(&self, view: &TurnView) -> Result<TurnGrade, String>;
}
pub struct TurnView { window_before: Vec<TurnRecord>, facts: Vec<FactView>, summary: Option<SessionSummary>,
                      user: String, did: Vec<String>, reply: String, next_user: Option<String> }
pub struct TurnGrade {
    pub answers_user: u8,             // 0 no, 1 partly, 2 yes
    pub grounded: bool,
    pub user_signal: UserSignal,      // None | Reask | Correction | Frustration (from next_user)
    pub issue: Issue,                 // None | IgnoredQuestion | InventedOutcome | WrongFormat | RepeatedQuestion | Other
    pub observation: Option<Observation>,
    pub note: String,                 // ≤200 chars, what would have fixed it
}
```

`ClientEvaluator`: JSON only, temperature 0, `[llm.evaluator]` model (default = emitter
model). **[R2]** The rubric and the view are plain text with fixed field order; scoring is
absolute per turn, never pairwise; `TurnView` is rebuilt from the `ModelCall` manifest
(§9) so "grounded" is judged against exactly what the replier saw. One call returns the
grade and, for notable turns, the observation **[claude-mem]**.

### 8.4 New signatures and their lanes

| Signature | Source | Lane |
|---|---|---|
| `UserReask` | symbolic | note (emitter or reply scope, proposer chooses) |
| `UngroundedReply` / `ReplyFlagged` | symbolic | reply-note |
| `BadReply { issue, answers_user }` | model | note / reply-note |
| `ImplicitCorrection { of_turn }` | model | note + `correction` observation; **no fact** |

### 8.5 Gate changes

- **Reply-scoped notes.** `Note.scope` gains `"reply"`; `guidance_for_reply()` feeds
  `ReplyContext.guidance`, rendered after the current-turn block.
- **Probing reply quality** needs a live replier and a scorer: `TurnOutcome::Graded(i64)`,
  score deterministic from the grade. Off by default (`reply_probe = false`); when off,
  reply-scoped candidates are recorded **Unverified** and listed for a human to apply by
  editing `learned.toml`.
- **[R2] Calibration.** Over turns where a symbolic proxy exists (re-ask, fallback, explicit
  `Corrected`, `ReplyFlagged`), compute Cohen's κ between "evaluator says issue" and "proxy
  present", plus test–retest agreement on a 5% re-graded sample. Both go in the report and
  ledger. Below `evaluator_min_kappa` (0.4) the evaluator's signatures still yield
  observations but **no candidates**. Raw agreement is not used: it overstates judge
  reliability by 33–41 points in the 2026 audit (findings §6).
- Everything else (GRASP balanced probe, zero regression budget, composition, ledger) is
  unchanged.

### 8.6 What the evaluator may not do

Write `learned.toml`; write a fact above confidence 0.5; touch guards, legal sets or
side-effect classes; run inside `run_turn`. Its verdicts are inputs to the same gates as
every other candidate.

### 8.7 In-turn *model* reply check (still deferred, §12)

The symbolic interceptor (§4.5) is in scope. A model grade before `Replied` is not: it puts
a judge with measured position and style bias on the critical path. Revisit after Phase 7
shows the residual bad-reply rate.

## 9. Observability and resilience

- `EventKind::ReplyFailed { detail }` (F7): appended when the replier errors, before the
  fallback `Replied`. Mining treats it like a fallback.
- `EventKind::ModelCall { role, model, prompt_tokens, completion_tokens, latency_ms,
  manifest }` (F7) **[R2]**: one per provider call; `manifest = { fact_keys, summary_through,
  window_turns: (from, to), guidance_hashes }`. Excluded from replay normalization. Enables
  (a) exact reconstruction of any model input from the log, (b) memory-influence
  attribution — which fact keys were present in turns the evaluator graded badly — the
  post-hoc audit findings §5 shows driving two attacks to 0%. The pass reports "facts most
  present in bad turns".
- **Quota cooldown** (F8) **[claude-mem]**: the client stops retrying on 402; on 429/402
  with `Retry-After` it returns `ApiError::Cooldown { until }` (capped at 5 min). The engine
  answers turns before `until` with a `Verbatim` "the model provider asked us to wait N
  seconds" and makes no model call.

## 10. Configuration

```toml
[memory]
window_turns = 6
record_max_chars = 300
summary_every_turns = 4          # 0 = no rolling summary
summary_rebuild_every = 3        # [R2] full rebuild from verbatim every N summaries
summary_max_chars = 800
summary_input_max_chars = 6000   # [R2]
facts_in_context = 10            # [R2] was 20
pinned_prefixes = ["user."]      # [R2]
pinned_max = 5                   # [R2]
relevant_max = 5                 # [R2]
recall_top_k = 5
recall_min_score = 0.0           # [R2] BM25 threshold; 0 = any match
remember_residual = "flag"       # "never" for side-effect-driving deployments
fact_stale_days = 90
reply_grounding_check = true     # [R2] §4.5
observations = true

[memory.observations]
types = ["preference", "event", "decision", "correction", "task"]
concepts = ["how-it-works", "why-it-exists", "gotcha", "pattern"]

[llm.summarizer]
model = "mistral-small-latest"   # default: emitter model
[llm.evaluator]
model = "mistral-small-latest"   # default: emitter model

[evolution]
evaluate_budget_turns = 60
evaluator_min_kappa = 0.4        # [R2] was evaluator_min_agreement = 0.6
reply_probe = false
```

All fields have the defaults shown; every section may be omitted.

## 11. Phased plan

Each phase is independently shippable, TDD per task, one commit per task, on a feature
branch `m6-memory` in a worktree (repo rules: per-file `rustfmt --edition 2021`, clippy
`-D warnings`, full `cargo test --workspace`; delete the worktree's `target/` before
merging). Phases 0–2 fix the observed failures; 3–6 add the layers; 7 measures.

### Phase 0 — Small fixes that stand alone (½ day)

| Task | Files | Test |
|---|---|---|
| `get_time` returns `2026-09-02 10:41:28 UTC (Wednesday)`; unix ms kept in a JSON summary so provenance leaves still match | `components-std/src/time_tool.rs` | output format; `ValueIndex` still yields the ms leaf |
| `ReplyFailed` event | `core/src/event.rs`, `engine/src/turn.rs`, `engine/src/replay.rs` | a failing `ScriptedReplier` leaves `ReplyFailed` before the fallback `Replied` |
| Fact restatement keeps `uses` (§6.1 row 1; full versioning is Phase 2) | `engine/src/turn.rs` | re-remember keeps `uses`, bumps `last_validated` |

Exit: existing 158 tests green plus 3 new; replay fixtures unchanged.

### Phase 1 — Working memory (F1–F3) (1–2 days)

| Task | Files | Test |
|---|---|---|
| `TurnRecord` + `SessionState.records`; `window()` renderer with caps; record `trust` | `core/src/memory.rs` (new), `engine/src/state.rs` | fold of the recorded fixture yields records with `did` lines for ok / denied / asked / staged; caps truncate with `…` |
| `EmitterContext` gains facts, summary, window, trace_so_far (drop `recent_turns`, `state_summary`) | `core/src/traits.rs`, `engine/src/turn.rs`, `llm/src/emitter.rs`, `engine/src/script.rs` | emitter request contains `Facts:` and `[tNN]` records; guidance test still passes |
| `ReplyContext` gains user_text, window, summary, do_not_state; new prompt (§4.3) | `core/src/traits.rs`, `llm/src/replier.rs`, `engine/src/turn.rs` | request contains the current user text after the window; block order asserted |
| **[R2]** Grounding interceptor (§4.5): claim extraction, `ReplyFlagged` event, one regeneration | `engine/src/ground.rs` (new), `engine/src/turn.rs`, `replay.rs` | a scripted replier that invents "42 orders" is flagged and regenerated once; a grounded reply passes; the second draft is used even if still flagged |
| `context_at(events, turn)` reconstructs what the models saw | `engine/src/state.rs` | equals the live context for every turn of a recorded fixture |

Exit: live smoke re-typing turns 73, 76, 85, 93, 104 of the recorded session gives
on-topic answers; a new fixture is committed under `crates/engine/tests/fixtures/`.

### Phase 2 — Fact lifecycle, forgetting, scope (F4, F9) (2 days)

| Task | Files | Test |
|---|---|---|
| **[R2]** Versioned facts: new columns, `(scope, key, valid_from)` key, `facts()` = current only, `fact_history`, `supersede_fact`, `forget_fact`, `purge_facts`; `Incoming.scope` | `core/src/traits.rs`, `core/src/action.rs`, `memory-sqlite`, `engine/src/store.rs`, `channel-cli` | migration on the existing DB; old rows read as `global`/`System`/`current`; supersede leaves the old row with `valid_to` |
| Write policy §6.1 incl. `squash()` canonicalization | `engine/src/turn.rs` | `memory_reset_requested` then `Memory-Reset-Requested` is one key |
| `forget_fact`, `forget_all` with `stage()` text | `engine/src/turn.rs` | `forget_all` is staged and asks; `confirm_pending` executes; unknown key is Malformed |
| `remember_fact` residual policy `flag` / `never` | `engine/src/turn.rs`, `app/src/config.rs` | Residual value at 0.5 rendered `(unverified)`; under `never` denied and clarification forced |
| **[R2]** Selection §6.5: pinned + relevant via `facts_fts`; `(was … until tNN)` rendering | `engine/src/turn.rs`, `memory-sqlite` | 30 facts → ≤10 with pinned `user.*` first; superseded value shown for a pinned key |
| Consolidation: decay to cold, safety purge, duplicate-key report, observer-proposed merges with deterministic acceptance | `evolution/src/consolidate.rs` (new), `pass.rs` | a stale fact becomes cold and revives on restatement; unequal-value merge proposal is reported, not applied |

Exit: "reset the memory" in live smoke stages `forget_all` and executes on "yes"; "what
was my name before" is answered from the pinned rendering.

### Phase 3 — Rolling summary (§5.1) (1–2 days)

| Task | Files | Test |
|---|---|---|
| `SessionSummary` (+trust, rebuilt_from), `Summarizer` trait, `Summarized` event, fold picks latest | `core/src/memory.rs`, `core/src/event.rs`, `core/src/traits.rs`, `engine/src/state.rs` | fold returns the last summary; serde round trip |
| `CloudSummarizer` (JSON, clamp) + `ScriptedSummarizer` + `NoopSummarizer` | `llm/src/summarizer.rs` (new), `engine/src/script.rs` | over-long output is clamped deterministically |
| Cadence + **[R2]** rebuild rule + trust propagation in `Engine::run`; builder slot `set_summarizer` (default Noop) | `engine/src/turn.rs`, `core/src/plugin.rs` | window 2 / every 2 / rebuild 2: turn 4 → `Summarized{through:2}`, turn 8 → rebuilt from 1..=6 with no previous summary; a record with External trust makes the summary External |
| Replay: `QueueSummarizer`; `Summarized` excluded from `normalize` | `engine/src/replay.rs` | fixture with summaries replays clean; probes without recorded summaries run |
| Config `[memory]`, `[llm.summarizer]` | `app/src/config.rs`, `app/src/main.rs`, `config.example.toml` | defaults parse; 0 disables |

Exit: a 12-turn live session shows a summary in the reply prompt at turn 10 and the replier
answers a question about turn 2 correctly.

### Phase 4 — Recall and FTS5 (§7) (1 day)

| Task | Files | Test |
|---|---|---|
| FTS5 tables: `facts_fts`, `events_fts` (external content), triggers | `memory-sqlite/src/lib.rs` | search finds a fact by value and a turn by a word in the user text |
| `MemoryStore::search(scope, session, query, k, min_score) -> Vec<Hit>` with **[R2]** verbatim-first ordering | `core/src/traits.rs`, both stores | equal-rank event hit precedes an observation hit; below-threshold hits are dropped; `InMemoryStore` does substring search |
| `recall` synthetic action; results as `CopiedOutput` leaves; trust = min | `engine/src/turn.rs`, `provenance/src/index.rs` | arg copied from a recalled fact is `CopiedOutput`; recalled External value keeps `External` |

Exit: "what did I ask you first today?" on a 20-turn session is answered via `recall`.

### Phase 5 — Evaluation lane and observations (§5.2, §8) (3–4 days)

| Task | Files | Test |
|---|---|---|
| Symbolic checks: re-ask, ungrounded reply (shared with §4.5), question-ignored | `evolution/src/evaluate.rs` (new) | the recorded session yields `UserReask` for turns 52–60, 67, 90, 100, 105 and `UngroundedReply` for turn 69 |
| `Evaluator` trait, `ClientEvaluator` (plain-text rubric, JSON out), `ScriptedEvaluator` | `evolution/src/evaluate.rs` | fence-tolerant parse; invalid JSON → `Err`, counted unverified |
| `observations` table + FTS5; observer writes from grades; dedupe; origin + trust; `relevance_count` on recall | `memory-sqlite`, `core/src/traits.rs`, `evolution/src/pass.rs` | same turn observed twice writes once; External origin yields External trust |
| New signatures, reply-scope notes, `guidance_for_reply`, `ReplyContext.guidance` | `evolution/src/mine.rs`, `core/src/learned.rs`, `llm/src/replier.rs` | a reply note renders after the current-turn block |
| Gate: `TurnOutcome::Graded`, live-replier probe behind `reply_probe`, Unverified path when off | `evolution/src/notes.rs`, `pass.rs` | with `reply_probe=false` a reply-scope candidate is Unverified and listed |
| **[R2]** Calibration: Cohen's κ vs proxies, 5% test–retest; `evaluator_min_kappa` | `pass.rs`, `ledger.rs` | κ computed from a table-driven contingency; below threshold → observations only |
| Budget `evaluate_budget_turns`; graded turns remembered | `pass.rs` | second run grades nothing new |

Exit: `ns-app evolve --dry-run` on the current `ns.sqlite` reports ≥ 10 `UserReask` /
`BadReply` signatures where today it reports zero, with κ printed.

### Phase 6 — Observability and resilience (§9) (1 day)

| Task | Files | Test |
|---|---|---|
| `ModelCall` events with **[R2]** context manifest | `llm/src/client.rs`, `engine/src/turn.rs`, `replay.rs` | usage parsed; manifest lists the fact keys shown; excluded from normalize; report sums tokens per turn and lists "facts most present in bad turns" |
| `Retry-After` / cooldown (capped 5 min) | `llm/src/transport.rs`, `llm/src/client.rs`, `engine/src/turn.rs` | 429 with `Retry-After: 120` → one attempt, `Cooldown`; next turn within the window replies Verbatim without a model call |

### Phase 7 — Memory evaluation suite **[R2]** (1–2 days, can start after Phase 1)

Modeled on the five LongMemEval abilities, run as *harness replays with tools* (MemoryArena's
lesson: QA over transcripts overstates agentic memory), scripted where possible and live
against Mistral on demand.

| Ability | Fixture (scripted sessions in `crates/engine/tests/memory_eval/`) | Pass condition |
|---|---|---|
| information extraction | user states name, age, preference across 8 turns; asked at turn 9 | reply contains the values; no `recall` needed (pinned) |
| multi-session reasoning | two sessions, same scope; fact from session 1 needed in session 2 | answered from facts or `recall`, not clarification |
| temporal reasoning | name changed at turn 6; "what was it before" at turn 12 | superseded value rendered; answer names both |
| knowledge updates | age restated with a new value | one `current` row, old row `superseded`; no ghost mixing in the context |
| abstention | question about a fact never stated | reply says it does not know; `recall` returns no hits; interceptor finds no invented value |
| selective forgetting (MemoryAgentBench) | `forget_fact` then asked | reply does not state the forgotten value |

Reported per run: pass rate per ability, tokens per turn, `recall` hit/miss, interceptor
flags. These numbers are the exit criteria for Phases 1–5 and the trigger for §12 item 8.

## 12. Open decisions

1. **Summary as an event vs a `summaries` table.** Recommended: event, now also because the
   manifest and the evaluator need to reconstruct model inputs from the log alone.
2. **Fact scoping now or with Telegram.** Recommended: now (Phase 2); measured zero
   cross-entity leakage in scoped designs, and the migration is one `ADD COLUMN`.
3. **`remember_residual` default.** `flag` for the CLI, `never` in the Tomáš config.
4. **Evaluator model.** A single cheap judge is supported by the evidence; a model
   different from the replier is preferable when `reply_probe` is on (self-preference).
5. **In-turn checks.** **[R2]** Split: symbolic interceptor in Phase 1 (yes); model check
   deferred until Phase 7 numbers exist.
6. **Observations now (Phase 5) or after Telegram traffic.** Recommended: Phase 5; the
   marginal cost is small once the evaluator exists.
7. **[R2] Versioned facts vs overwrite.** Recommended: versioned (§6.1); it is the only way
   to answer the recorded "what was my name before" and the fix for ghost memory.
8. **[R2] Vector search trigger.** Stay lexical; add embeddings when the Phase 7 suite shows
   `recall` miss rate above 20% on paraphrased queries, or when Tomáš traffic needs
   semantic matching of product names.

## 13. Non-goals and what we do differently from claude-mem

- No raw *accumulated* history in any prompt; the window is bounded, the summary
  fixed-field and rebuilt from verbatim records.
- No prose memory without provenance. claude-mem injects narrative text into the system
  prompt; here every fact, summary and observation carries provenance and trust, recall
  results enter the provenance index, and `TaintPolicy` still applies.
- No model in the guard chain, and no model-decided applies. claude-mem's observer output
  goes straight into the next context; here it goes through the M5 gates.
- **[R2]** No graph memory for now: it did not close the fidelity gap against verbatim text
  in the controlled ablation and needs state labels to avoid ghost memory anyway; revisit
  with Tomáš data. No parametric/RL memory (fine-tuning is out of scope). No vector search
  until the §12 trigger. No cross-replica sync.
- No new lane may relax a guard, widen a legal set, or change a side-effect class (M5 §2
  invariant, unchanged).

## 15. Implementation notes (Phases 0–4, 2026-09-02)

Decisions taken while building, where the code differs from the text above:

- **Decisions §12.** 1: summary is an event. 2: scope column now; the CLI maps every
  session to `global` through `EngineConfig::scope_for` rather than an `Incoming.scope`
  field (no churn across channel and test code; a channel supplies its own mapping).
  3: `remember_residual = "flag"`. 4: the summarizer runs on its own `[llm.summarizer]`
  role with optional `base_url` / `api_key_env`, defaulting to the emitter's model and
  provider (user decision: same Mistral model now, swappable later). 5: symbolic
  interceptor built; model check not built. 6: observations are Phase 5 (not built).
  7: versioned facts built. 8: lexical only.
- **Legality never depends on store state.** A first version made `forget_all` legal
  only when facts existed and put the fact count in the confirmation prompt; both made
  replay from a fresh store diverge. Forgetting is now narrowed by this turn's own events
  only: illegal after a `remember_fact` this turn (seen live: "my name is now Peter"
  ended in `forget_fact` of the new key plus a staged `forget_all`) and after any forget
  already ran; a second missing-key miss drops `forget_fact` from the schema.
- **`QueueSummarizer` is unnecessary.** Replay drives `run_turn`, which never
  summarizes; summarization happens in `Engine::run` after the reply is sent
  (`maybe_summarize`). Recorded `Summarized` events are excluded from `normalize()`.
- **`last_used` on facts.** Decay uses the later of `last_validated` and `last_used`
  (set on every recall into a context), so a name that is shown daily but never restated
  does not go cold.
- **`facts()` returns current and cold rows**; superseded and forgotten versions are
  reachable only through `fact_history`. Pinning excludes cold; search includes it.
- **Retry backoff** raised to 1 s / 2 s / 4 s (four attempts): a turn with a tool call
  makes three or four provider requests back to back and Mistral's free tier answered
  429 to every 0.5 s retry during the smoke. The `Retry-After` cooldown stays Phase 6.
- **Recall sources:** verbatim turns beyond the window (FTS5 over `events.kind_json`,
  bm25-ranked, index built once for existing logs) and live facts (lexical rank shared
  by both stores). Observations join in Phase 5.
- **Live smoke findings.** The recorded failures reproduce fixed: "whats my name" answers
  from the pinned fact, "what was my name before" from the `(was …)` marker, "reset the
  memory" stages `forget_all` and purges on "yes". The small model still answers some
  open questions with the nearest fact ("tell me something nice" → the user's name), which
  is the reply-quality signal Phase 5's evaluator is for.

## 14. Prior art

Full list with tags in `docs/research/2026-09-02-memory-findings.md`. Load-bearing items:

- Verbatim over extracted artifacts (2601.00821) and episodic reconstruction (2601.21714)
  → §1 verbatim-first, §7 ranking.
- Ghost memory and state labels (2607.01935), bi-temporal invalidation (2501.13956),
  control-plane placement (2606.15903) → §6.1–6.4.
- Precision-aware retrieval (2605.11325), memory roles (2606.25361), governed memory
  saturation at ~7 (2603.17787) → §6.5.
- Iterative-summarization drift (2603.11768), laundering channels and write-time origin
  binding (2606.24322), memory poisoning (2606.04329, 2605.15338), post-hoc attribution
  (2605.23723) → §5.1 drift/trust, §5.2 origin, §9 manifest.
- Judge validity (2606.19544), judge bias mitigation (2604.23178), single-judge and
  runtime interceptor (2604.16706) → §4.5, §8.5.
- Hierarchy and consolidation (2601.02845, 2607.21503, 2607.00692, 2603.09023,
  2604.07798) → §4–§5 shape, small-model control plane.
- Forgetting (2601.18642, 2604.20300, 2507.05257, 2603.14212) → §6.2.
- Evaluation (2410.10813, 2602.16313) → §11 Phase 7.
- claude-mem 13.23.1 (local): fixed-field summaries, typed observations with config
  taxonomies, content-hash dedupe, relevance counts, token accounting, quota cooldown.
