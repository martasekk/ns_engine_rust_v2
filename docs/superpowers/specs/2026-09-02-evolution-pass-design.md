# Evolution Pass — Design Spec (M5, first item)

Date: 2026-09-02. Status: approved in chat section by section; written for review.
Parent spec: `2026-09-01-neuro-symbolic-harness-design.md` §7 (Consolidator slot,
Fact lifecycle), §11 (replay harness), §12 (M5+ "evolution pass"), §13.
Research: `docs/research/2026-09-01-findings.md` §6, plus the 2026 sources in §9 below.

## 1. Goal

The harness learns from its own recorded failures without a human in the loop, and
without ever becoming less safe. A pass reads the event logs, distils candidate
improvements, verifies each one against the recordings, and auto-applies the ones that
pass. Every applied change is auditable as a diff of one file.

Two lanes of improvement, chosen by the user (2026-09-01):

- **Symbolic patches** — deterministic input repairs the engine applies before
  validation. Verified by replay, no model involved.
- **Guidance notes** — short imperative sentences added to the emitter prompt. Written
  by the model, verified by a live probe with a regression budget.

Explicitly deferred to a later milestone: compiled flows / speculative `Pure` prefetch
(TraceCompiler / PreAct / PASTE style) — they need repeated-intent volume that only
real Tomáš traffic will give; tool synthesis stays human-gated (parent spec §6).

### Non-goals

- No change to guards, legal sets, side-effect classes or confirmation flow. The pass
  can only repair model *input* (args, action names) and add prompt text.
- No RL, no fine-tuning, no model selection.
- No new event kinds. The pass never writes to the event log.

## 2. Safety invariant (load-bearing for auto-apply)

**The pass can never relax a guard, widen a legal set, or change a side-effect class.**

Concretely: a learned rule runs *before* the legality check and schema validation, and
its output goes through exactly the same checks as raw model output. A patch can turn
an invalid proposal into a valid one that the guards then judge; it can never bypass a
judgement. Notes only add prompt text. This is why "auto-apply everything" is
acceptable: the worst outcome of a bad rule is a rejected proposal, which is what would
have happened anyway.

## 3. Pipeline

```
sessions ─► mine ─► signatures ─► propose ─► verify (gate per lane) ─► apply ─► ledger
                                   │                                  │
                          symbolic | notes                      learned.toml
```

Crate `crates/evolution` (`ns-evolution`), pure library. Depends on `ns-core` and
`ns-engine` (for `replay_with`). `ns-app` wires it.

### 3.1 Mining: failure signatures

Input: every session the store can enumerate, as event lists. Output: a list of
`Signature { session, turn, event_id, kind, evidence }`.

| Signature | Evidence carried | Lane |
|---|---|---|
| `Rejected{Malformed}` where a normalized arg would validate against the spec | proposal args, action spec | symbolic `normalize_arg` |
| `Rejected{IllegalAction}` whose name is near a tool that was legal at that point | proposed name, legal set | symbolic `alias_action` |
| fallback reply (`Settled Verbatim` with the fallback text) | whole turn trace | note |
| repeated `GuardDenied` with the same reason in one turn | turn trace | note |
| `ToolReturned{Err}` attributable to args | args, error detail | note |
| `Corrected` event | corrected text, prior reply, user text | note; plus a fact if the text parses as `key = value` |

"Near" for `alias_action`: case-insensitive match after stripping non-alphanumerics, or
Damerau-Levenshtein distance ≤ 2 (the `strsim` crate), with a unique best match in the
legal set. Ties produce no candidate.

The legal set at a point is reconstructed by replaying up to that event (`replay_with`
returns per-event context), not guessed from the recording.

### 3.2 Symbolic lane

Exactly two patch types, stored in `learned.toml`:

```toml
version = 1

[[normalize_arg]]
action = "get_weather"
arg = "city"
ops = ["trim", "strip_punct", "lowercase"]     # ordered, from a fixed vocabulary

[[alias_action]]
from = "getTime"
to = "get_time"
```

`ops` vocabulary: `trim`, `strip_punct` (leading/trailing punctuation), `lowercase`,
`strip_prefix:<literal>`. Nothing that can invent content.

Application points in `run_turn`:

1. `alias_action` rewrites `proposal.action` right after the proposal parses, before
   the legality check.
2. `normalize_arg` rewrites the named arg right before schema validation.

The `Proposed` event still records the raw model output; `ToolCalled` records what ran.
The difference is the audit trail. Replay applies the same rules, so recordings made
with rules in force replay faithfully.

### 3.3 Notes lane

A note is one imperative sentence, scoped:

```toml
[[note]]
scope = "global"                  # or "action:<name>"
text = "When the user gives a city, call get_weather before answering."
lift = 0.5                        # measured by the gate, used for eviction
hash = "blake3:9f2c…"             # content hash of scope+text, ledger key
```

Proposal is GRASP-style comparative: the proposing prompt gets the failing turn trace,
the current note set, and asks for one sentence that would have changed the outcome and
does not duplicate an existing note. The library is bounded by `max_notes` (default
20); when full, the lowest-`lift` note is evicted to make room for a newly accepted one.

Delivery: `EmitterContext` gains `guidance: Vec<String>`. The engine fills it with all
global notes plus notes scoped to actions in the current legal set. `CloudEmitter`
renders them as a short "Guidance" block *after* the persona, outside the cached prefix.
`ScriptedEmitter` ignores the field.

## 4. Verification gates

### 4.1 Symbolic gate (deterministic, no model)

1. **Flip check.** Replay each evidence session with the candidate applied, from a clean
   store. Pass iff the normalized line at the signature's position flips from
   `Rejected …` to `ToolCalled <action>`. A newly accepted call has no recorded outcome,
   so verification-mode replay hands it a synthetic `Ok`; the flip is judged only at the
   rejection → valid-call boundary, never on what happens after.
2. **Regression check.** Replay every other recorded session (newest first, capped by
   `regression_replay_cap`) with the candidate applied. Require **zero divergence** from
   its own recording.
3. **Composition.** Candidates are verified against the current learned set plus every
   candidate accepted earlier in this run, in order, so accepted patches never conflict.

Requires a new `replay_with(session, recorded, opts) -> Result<Replayed, ReplayError>`
in `crates/engine/src/replay.rs`, where `opts` carries the learned rules, the extra
guards, and a `synthetic_ok_for_new_calls` flag, and `Replayed` holds the replayed
events plus per-event legal sets. Today's `replay_session` becomes a thin wrapper that
diffs `normalize(replayed)` against `normalize(recorded)`.

### 4.2 Notes gate (live model, probabilistic)

Balanced held-out probe:

- Positives: sessions carrying the signature the note targets.
- Negatives: an equal number of sessions without it, newest first.
- Each probe re-runs the session's user inputs through a live engine using the
  configured production emitter (a note is model-specific), with recorded tool outcomes
  replayed. Run twice: with and without the note.
- Per-turn outcome classified as `ok`, `fallback`, or `rejections: n`.
- Accept iff at least one positive improves **and** regressions on negatives ≤
  `regression_budget` (default 0). GRASP's ablation shows the gate, not the note
  writing, is where the gain comes from, which is why the budget defaults to zero.
- Total cost bounded by `probe_budget_turns` (default 40) per run; when exhausted, the
  remaining candidates are left unverified and reconsidered next run.

`lift` = (positives improved − negatives regressed) / positives probed.

### 4.3 Ledger

`evolution-ledger.json`: map of candidate content-hash → `{ verdict, numbers, evidence:
[(session, turn, event_id)], at }`. The pass never re-proposes a rejected candidate and
never re-verifies an accepted one. Both `learned.toml` and the ledger are written
temp-then-rename in the same directory.

## 5. Runtime integration

- **Rules handle.** The engine holds `Arc<ArcSwap<LearnedRules>>` (`arc-swap` crate).
  A turn loads one snapshot at its start and uses it throughout. A driver swaps a new
  set in without restart. `EngineConfig` gains `learned: Arc<ArcSwap<LearnedRules>>`
  (default: empty).
- **The pass is a `Consolidator`.** `ns_evolution::EvolutionPass` implements the
  existing trait. Since `Consolidator::run` only receives the store, the pass takes the
  rest at construction: the rules handle, an emitter factory for probes (`None` = notes
  lane off), the two file paths, and the knobs.
- **Store enumeration.** `MemoryStore` gains `async fn sessions(&self) ->
  Result<Vec<SessionId>, StoreError>`, implemented for `InMemoryStore` and
  `SqliteStore` (`SELECT DISTINCT session_id FROM events ORDER BY MAX(at) DESC`).
- **Driver A — `ns-app evolve [--dry-run]`.** Loads config and store, runs the pass
  once, prints a report: signatures by kind, candidates, verdict and numbers per
  candidate, probe turns spent. Writes `learned.toml` and the ledger unless `--dry-run`.
  Needs no API key for the symbolic lane; the notes lane is skipped with a warning when
  the configured key env var is unset. Exit 0 whenever the pass completes, non-zero on
  config or store errors.
- **Driver B — idle timer.** `Engine::run` wraps `channel.recv()` in
  `tokio::time::timeout(idle_after)`. On timeout, if any event was appended since the
  last pass, it runs the consolidator once, swaps the rules in, and resumes. Runs on the
  engine task, so a pass never interleaves with a turn. `idle_after_secs = 0` disables
  it. This is the sleep-time-compute pattern from findings §6, now adopted.
- **Startup.** `ns-app` loads `learned.toml` if present; a parse error is fatal with a
  message naming the file (a bad rule set must not be silently ignored).

## 6. Configuration

```toml
[evolution]
enabled = true
learned_path = "learned.toml"
ledger_path = "evolution-ledger.json"
idle_after_secs = 300        # 0 = driver A only
regression_budget = 0        # notes gate
probe_budget_turns = 40
max_notes = 20
regression_replay_cap = 200  # newest sessions replayed by the symbolic gate
```

All fields have the defaults shown; the section may be omitted entirely.

## 7. Error handling

- A pass that fails mid-way (store error, provider error during probes) writes nothing
  and logs the error; the next run starts from the ledger as it was.
- A probe that hits a provider error counts as "unverified", not as a regression, and
  consumes budget.
- Replay chain-broken sessions are skipped and reported, never used as evidence.
- `learned.toml` unparsable at startup: fatal. Unparsable during a pass: the pass
  aborts before proposing.

## 8. Testing

- **Unit (`ns-evolution`).** Mining hand-built event lists yields the expected
  signatures, including the "near name" rules and tie → no candidate. Patch application
  is a pure function with table-driven cases. Ledger idempotence (second run proposes
  nothing new). `learned.toml` roundtrip; write is temp-then-rename.
- **Replay tier (`ns-engine`).** `replay_with` reproduces `replay_session` on all
  existing fixtures. The symbolic gate accepts a patch that flips a recorded Malformed
  rejection and rejects one that makes any other recorded session diverge. Composition:
  two candidates where the second only flips with the first applied.
- **Notes gate.** A scripted emitter that succeeds only when `guidance` contains a
  marker proves acceptance and the `lift` number. A note that breaks a negative probe is
  rejected under budget 0 and accepted under budget 1.
- **Driver B.** Engine test with a channel double whose `recv` delays past
  `idle_after`: exactly one pass runs, and the next turn observes the swapped rules.
  With no new events, no second pass runs.
- **Driver A.** `ns-app evolve --dry-run` on a fixture DB prints the report and leaves
  both files untouched.
- **Live smoke (manual).** One Mistral session with a deliberately misspelled tool
  name, `ns-app evolve`, alias appears in `learned.toml`, next session's identical
  misspelling runs the tool.

## 9. Prior art consulted for this design

- **GRASP** — arxiv.org/abs/2605.29668 — **[ADOPTED]** acceptance gate with hard
  regression budget on a balanced held-out probe; ablation shows the gate carries the
  gain. → notes gate (§4.2).
- **PreAct** — arxiv.org/abs/2606.17929 — **[ADOPTED (store-time verification)]**
  verify from a clean state before caching. → flip + regression from a clean store
  (§4.1). Compiled state-machine replay **[LATER]**.
- **HarnessFix** — arxiv.org/abs/2606.06324 — **[ADAPTED]** trace → IR → attribution →
  narrowly targeted repair with regression-aware validation. → narrow patch vocabulary
  (§3.2), safety invariant (§2).
- **MetaSkill-Evolve** — arxiv.org/abs/2607.05297 — **[LATER]** two-timescale skill
  loop; relevant once compiled flows exist.
- **ClawTrace** — arxiv.org/abs/2604.23853 — **[NOTED]** prune patches matter more than
  preserve patches; our symbolic lane is prune/repair-only by construction.
- **TraceCompiler** — arxiv.org/abs/2608.02680 — already **[ADOPTED]** in findings §3;
  compiled flows deferred to the next evolution milestone.
- **AgentErrorTaxonomy** — arxiv.org/abs/2509.25370 — **[ADAPTED]** the signature table
  covers its Action and System-level categories; Planning/Reflection map to notes.
- **Sleep-time compute** — letta.com/blog/sleep-time-compute, arxiv.org/abs/2504.13171
  — **[ADOPTED]** driver B.
- Crates: `arc-swap` (hot-swap), `tempfile` (atomic write), `strsim`
  (Damerau-Levenshtein).

## 10. Out of scope, with triggers

- Compiled flows and speculative prefetch — when real traffic shows at least 10
  sessions sharing one tool-call sequence of length ≥ 2.
- Note proposals from a different (cheaper) model than the production emitter — never,
  a note is model-specific by design.
- Cross-replica ledger merge — when a second replica exists.

## 11. Amendments (2026-09-02, plan time)

Settled while writing the implementation plan
(`docs/superpowers/plans/2026-09-02-m5-evolution-pass.md`, Global Constraints):

1. **Tool-arg validation is new.** The engine had no schema check on tool args, so
   `Rejected{Malformed}` never fired for them. `ns-core::validate_args` (required /
   type / enum subset) now runs right before classification; a failure does not
   narrow the legal set, so the emitter can retry with repaired args. This is what
   makes the `normalize_arg` lane observable. `remember_fact`'s dotted-key rule stays
   engine-custom.
2. **Hashes are sha256, not blake3.** Note hash = `sha256:<hex>` of
   `scope + "\n" + text`; patch hash = `sha256:<hex>` of the patch's canonical JSON.
   `sha2` was already a workspace dependency.
3. **`alias_action` mining uses the production specs.** The legal set is
   `known_specs` (the app's tool specs) plus the synthetic actions, not a per-event
   replay reconstruction. Replay doubles carry the real `ActionSpec` when known, so
   legality and validation match production.
4. **Baseline-divergent sessions are excluded from the regression check.** A session
   that already diverges from its recording under the *current* rules cannot witness
   a regression; it is counted as `skipped_baseline`, and only baseline-clean sessions
   that diverge under the candidate count as regressions.

Also: "attributable to args" for `ToolReturned{Err}` means `kind == "bad_args"`;
a `Corrected` text of the form `key = value` becomes a `Fact` with
`Provenance::Residual` and confidence 1.0; a note probe costs 2× the session's turns
(one run without the note, one with).
