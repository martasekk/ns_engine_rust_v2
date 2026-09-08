# Engine structure: what moved, and why turn resumption did not

**Date:** 2026-09-08. **Status:** built, except where marked. **Scope:** the engine
only — `core`, `engine`, `llm`, `memory-sqlite`, `evolution`, `app`. Nothing here is
about the desktop, the pointer, or any other workspace.

**Reads:** M7 `2026-09-07-m7-context-budget.md`, M8 `2026-09-08-m8-local-evaluation-lane.md`,
`2026-09-08-general-harness-design.md` §4.3, §5.2.

---

## 1. The finding this is organised around

The engine's outer architecture is sound and was not changed. Nine ports, all
object-safe; a dependency graph that is acyclic and hub-shaped on `core`; `engine`
independent of `llm`, which is what makes the emitter a trait and the port story
true. The measured problems were all *inside* `crates/engine`, and they had one
root cause:

> **The turn's phases existed only as lexical regions inside one function.**

`run_turn` was ~1,351 lines containing the iteration loop, nine action handlers and
the reply generation. The code's own comments lettered the phases `a.` through `f8.`
— comments, not structure. Most of what follows is a consequence: handlers that
cannot be tested because they are not callable, classification written out seven
times because there is no single dispatch point to put it in, and an expensive phase
with no name.

## 2. What was built

| Change | Effect |
|---|---|
| `ActionSpec::check_arg_names`, enforced in `HarnessBuilder::build` | the rationale ordering is checked, not assumed |
| `ns-testkit` crate: `eval` + `paraphrase` | engine src+tests 12,426 → 9,726 lines; unblocks M8 T4.3 |
| `engine::trace` module | `turn.rs` 3,096 → 2,415 lines |
| `fn classify` | seven inlined copies → one |
| config semantics checked before `build_tools` | a bad config is refused before the process dials anything |
| `b.build()` handled instead of `expect`ed | assembly failures are messages, not backtraces |
| `Engine::generate_reply` | the request-spending reply phase has a name |

Evidence at each step: 479 tests pass (baseline 470 + 9 added), workspace clean
under `clippy --all-targets`, and `ns-app eval` reports 9/9 abilities at
**109 requests → 109 requests** across the refactor. That last number is the one
that matters for the reply extraction: a reply path that had started drafting
differently would move it.

### 2.1 The rationale check, and why it is a check

The think-then-commit mitigation failed silently once already. The field was
`rationale`, `serde_json` serializes `properties` as a `BTreeMap`, and it landed
wherever `r` sorted. M7 T2.5 renamed it `_rationale`.

That fix worked by luck of the alphabet. `_` is 0x5F; uppercase letters (0x41–0x5A)
and digits (0x30–0x39) sort *below* it. An argument named `Volume` or `2fa` returns
straight to the defect, silently, exactly as before — the first occurrence went
unnoticed because the only action exercising the order had one argument that
happened to sort late.

It is checked at assembly rather than per request: a spec whose arguments would
outrank the rationale is a `BuildError`, not a mid-turn panic. The builtin specs
never reach `add_tool`, so they carry their own test; otherwise the half of the
legal set the engine owns is the unchecked half. `core` cannot depend on `llm`, so
the key is spelled twice and a test pins the two spellings together.

## 3. Turn resumption, and why the obvious fix is wrong

M7 §3 declined Temporal and recorded the condition to revisit. The general-harness
design sharpened the remaining step to *turn resumption*: a staged proposal and
pending confirmation should be reconstructable from the log after a restart, since
`fold()` already replays everything else.

Measuring it produced a different answer than expected, and the difference is worth
recording so it is not re-proposed.

**The apparent gap.** `flush()` is called at exactly four sites, all after a
side-effecting `ToolReturned`. Neither staging path — the `NeedsConfirmation` guard
verdict, nor `forget_all`'s own branch — calls it. A `PendingConfirmation` becomes
durable only via the unconditional persist at the end of `run_turn`. A crash in that
window loses the staged proposal; `fold()` then reports `pending_confirmation: None`
and the user's next "yes" attaches to nothing.

**Why flushing there would be a regression.** `run_loop` calls `channel.send` only
after `run_turn` returns `Ok`, and the final persist happens before that return. So
today the invariant is:

> A pending confirmation survives if and only if the turn completed — and the turn
> completing is what causes the user to be shown the prompt.

Flushing at the staging point breaks the "only if". A crash between the flush and
delivery leaves a live `PendingConfirmation` in the log that the user was never
shown. It stays active for exactly the next turn (the expiry rule), during which the
emitter — which can see the pending in its trace — may propose `confirm_pending`
against a message the user never intended as confirmation. That trades a safety
property for the durability of one emitter request. It is the wrong trade, and it is
the wrong trade specifically in the direction this engine exists to refuse.

**So the current behaviour is correct and stays.** Not "unfinished": load-bearing.
A staged proposal is a question, and a question nobody heard has no answer.

**What is genuinely still open.** The honest residual is narrower than "resumption":
a crashed turn also loses its `ModelCall` events, so `ns-app budget` under-reports
spend that was really incurred — which matters on a fifty-request day. The safe
version records what was *paid for* without creating anything actionable, and its
natural point is right after `record_model_calls` in the emit phase, before any
staging event exists this turn.

It is not built here, because it is not free: it makes an orphaned turn visible in
`fold()`'s records as a turn with an empty reply, and what the conversation window
shows the user is a product decision, not a refactor. **Open, with a stated design;
needs a decision on window rendering before it is built.**

## 4. Corrections to the reading this work started from

Recorded because both were asserted before being measured, and this repo's house
rule is that the correction goes in writing.

1. **`script.rs`, `store::InMemoryStore` and `NoopConsolidator` are not test-only.**
   They read like test doubles and are production infrastructure: `replay::replay_with`
   constructs all three on its ordinary path, and that function is the hard dependency
   of `ns-evolution`'s `verify_patch` gate. The first plan moved them to a test crate,
   which would have put production gating behind test code. Only `eval` and
   `paraphrase` moved — 2,645 lines, not the ~2,700 claimed for a larger set.
2. **The `confirm_irreversible = false` refusal is scoped, not general.** The
   general-harness design asks for a startup refusal; it asks for it *on a multi-user
   channel*, where the sender of a tainted argument could also supply the confirmation
   that clears it. On a single-user CLI the confirmer is the person at the keyboard,
   and unattended operation is a legitimate posture with its own brakes. The refusal
   belongs with the channel that makes it unsafe. Not built, deliberately.

## 5. What remains, in order

1. **Reify the turn (`TurnCtx`).** The remaining blocker to extracting the nine
   action handlers is twelve ad-hoc `let mut` locals threaded through one lexical
   scope: `rejections_this_turn`, `denied_this_turn`, `calls_this_turn`,
   `never_residual_this_turn`, `forget_misses`, `emit_failures`, `last_emit_error`,
   `proposed_this_turn`, `settled`, `tier`, plus `log` and `proposal`. Bundling them
   is mechanical; the payoff only arrives with the extraction, and the extraction is
   where the borrow checker has opinions (a handler wants `&mut ctx`, `&mut log` and
   `&self` at once). Worth doing as one deliberate arc, not as a rename.
2. **One dispatch pipeline.** Nine builtin handlers each reimplement a slice of
   validate → classify → guard → stage/perform. Collapsing the classification was
   the first step; the rest needs (1).
3. **Per-action emitter tests.** Needs (2) to be meaningful.
4. **`core` should shed policy.** `core/src/budget.rs` holds a context-fitting
   algorithm with a hardcoded drop order and a 4-chars-per-token heuristic;
   `core/src/router.rs` holds tier policy citing VISTA and MemFlow. That is one
   harness's tuning inside the crate that defines the shared vocabulary. Not in any
   milestone; should be.
5. **A narrow `engine::sim` surface.** `ns-evolution` reaches into `engine`'s
   `InMemoryStore`/`ScriptedEmitter`/`NoopConsolidator` wholesale to gate learned
   notes. The dependency is structurally right — a regression guarantee must replay
   real turn semantics — but it has no defined surface, so a refactor of `turn.rs`
   can break the gate silently.
6. **Measure `fold()`.** `run_turn` folds at the top of every loop iteration, and
   `route_turn`/`maybe_summarize` fold too. The snapshots residual still has no
   trigger beyond "when fold time is measurable". M7's Temporal dismissal was already
   corrected once for conflating *reading* with *replaying*; this is the same axis and
   still unmeasured. Measure before designing.

## 6. Not proposed

Splitting `engine` into more crates — `EngineConfig`'s 30 fields and `HarnessParts`'
8 trait objects would become a public contract for no gain. A model in the guard
chain, the router, or judging. Adopting Temporal: the trigger fires on a channel
where a turn outlives the process, and no such channel exists here yet; §3 is what
the property costs when it is taken seriously rather than adopted wholesale.
