# The general harness: BAML, symbolic derivation, generated code, and the Tomáš port

**Date:** 2026-09-08. **Status:** design, no code. Feeds a milestone after M8.
**Reads:** M5 `2026-09-01-neuro-symbolic-harness-design.md`, M6
`2026-09-02-memory-redesign-design.md`, M7 `2026-09-07-m7-context-budget.md` (§3 Temporal),
M8 `2026-09-08-m8-local-evaluation-lane.md`.

Written to answer four questions at once, because they turn out to be one question: what
is the load-bearing idea in this engine, and what follows from taking it seriously.

---

## 1. What the engine already is

Stated plainly, because the answers below are all consequences of it:

> **The symbolic layer bounds the neural layer's output space, and no neural output
> becomes an effect without passing a symbolic check.**

It is not an aspiration; it is in the code, in four places:

| Mechanism | Where | What it buys |
|---|---|---|
| The legal set is compiled into the request's `tools` array, strict, `additionalProperties: false` | `llm/src/schema.rs::build_tools` | the emitter **physically cannot** name an illegal action |
| Every argument carries provenance and trust; guards see `ClassifiedProposal`, not text | `core/src/action.rs`, `provenance/` | taint is structural, not a prompt instruction |
| Danger is a type: `SideEffect::{Pure, Reversible, Irreversible}` on the spec, not on the model's opinion | `core/src/action.rs` | the gate reads a field, never a judgement |
| The log is the truth; `fold()` is replay; `verify_patch` replays to settle a candidate | `engine/src/state.rs`, `evolution/` | learning is verifiable rather than asserted |

Everything below either extends this or is rejected for contradicting it.

---

## 2. BAML

### 2.1 Do not adopt BAML the tool

BAML's parts, against what is already here:

| BAML | ns-engine today |
|---|---|
| typed function signature | `ActionSpec.args_schema` + `validate_args` |
| schema compiled into the request | `build_tools`, strict mode |
| Schema-Aligned Parsing (tolerant parse into the type) | `llm/src/salvage.rs` — **fallback branch only** |
| retry policy | `max_emit_retries`, provider retry + throttle |
| swappable client per function | role-based provider config (`[llm.summarizer]`, …) |
| prompt is visible, not hidden templating | templates + `ContextManifest` records what was shown |
| tests beside the function | `ns-app eval` ability set |

Three reasons adoption is the wrong trade *for this repo specifically*:

1. **`ActionSpec` is not a schema.** It carries `side_effect`, `residual_policy` and
   `dedupe_tag` — the fields the guard chain reads. A BAML type cannot carry them, so
   `ActionSpec` stays the source of truth and BAML duplicates the half it can express.
   Two schemas that must agree is precisely the drift this repo keeps refusing elsewhere
   (`trace_for_prompt` is public so `ns-app budget` cannot reimplement it, "which would
   make the saving it reports a fiction").
2. **It is a bet against constrained decoding, and that bet was already made and paid
   for here.** SAP's premise is: let the model emit loosely, parse tolerantly. This engine's
   illegality guarantee comes from constraining generation, and M7 already paid the known
   accuracy cost of constrained decoding with a *measured* mitigation — `_rationale` first
   in the property order, with a load-bearing underscore because `serde_json::Map` sorts
   alphabetically. Switching to SAP-first discards that.
3. **Codegen targets.** BAML generates Python and TypeScript clients. Adding a build step
   and generated Rust to a workspace whose whole ergonomics are `cargo test` is a large
   tax for a capability that mostly exists.

### 2.2 Take three things from it

**(a) Promote SAP from fallback to a measured branch — do not assume, test.**
`salvage.rs` runs only after `serde_json::from_str` has failed, and its own header says the
primary path's guarantee comes from constrained generation. BAML's actual claim is stronger:
that loose-emit-plus-SAP beats strict function calling *on quality*, especially for small
models. That is a claim this repo can settle offline, for free, because `ns-app eval` already
drives the engine with scripted doubles:

> Add an eval arm that replays recorded emitter outputs — including the malformed ones the
> log already holds — through both paths, and report parse-recovery rate and downstream
> action correctness. If SAP-first wins on a 3B emitter, that is a finding worth a
> milestone. If it does not, the fallback stays a fallback and now has a number.

This is exactly the house rule: a check becomes a gate only after a measured true-positive
rate.

**(b) Per-action tests, which BAML puts beside the function and this repo has nowhere.**
The ability set grades the harness end to end; there is no regression that says *given this
legal set and this message, the emitter should propose `pointer_ui_find`*. That is the test
that makes adding an action safe, and its absence is why the legal set is nerve-wracking to
extend. Cheap: a fixture per `ActionSpec`, run by the existing scripted-double machinery.

**(c) The compiled prompt is an artifact — hash it into the manifest.**
`ContextManifest` records what a call was shown, and M7 measures the tools array's token cost
(`Usage::tools_tokens`). What it does not record is *which schema* was compiled. Add a hash of
the compiled tools array to the manifest and "the model saw a different legal set" becomes
visible in a diff instead of invisible in a regression. This also completes M8's T2.3a: a
recorded grade is only reproducible if what was graded is pinned.

---

## 3. The structural gap: the symbolic layer only filters

Today the symbolic layer **vetoes** (guards) and **normalises** (`Patch = NormalizeArg |
AliasAction`). It does not **derive**. Every turn goes to a model, even turns whose answer is
entailed by facts the engine already holds.

On `openrouter/free` — fifty requests a day — a turn a rule could have answered but a model
did answer is not an inefficiency, it is 2% of the day.

**The proposal: let rules complete a turn.** Not a planner, not search. A forward-chaining
step between routing and emitting: if the current facts and the message match a learned
precondition, the engine emits the entailed proposal itself, marks it `Trust::System`, and
runs it through the same guard chain. The model is never asked.

Why this fits rather than strains the design:

- The verification machinery exists. `verify_patch` already replays recorded sessions
  against patched rules; a `Precondition → Action` rule is settled the same way, by replaying
  the sessions that produced it and checking the outcome did not change.
- The gate exists. A derived proposal is still a `ClassifiedProposal` and still faces
  `TaintPolicy`, `SideEffectGate`, `DedupeGate`. Deriving does not widen the legal set.
- It is the honest version of M7's residual T4.1. That residual is written as "make three
  signatures symbolic by growing `Patch` variants", which undersells it: the reason those
  signatures cannot be symbolic is that `Patch` can only rewrite arguments, and the thing
  worth learning is *when an action applies at all*.
- It is measurable in the currency that matters: requests per completed task, already a
  reported column.

**The risk, and the bound:** a wrong rule silently answers instead of asking. The bound is
the one M5 already set — a candidate becomes a rule only with a measured true-positive rate
over replay, and a derived turn is recorded as derived, so the eval can report how many turns
were answered without a model and whether those turns completed.

---

## 4. Asking it to generate code

This is where the architecture is strongest, and it needs no new philosophy — only a refusal
to model it as an action that writes code.

### 4.1 The wrong shape

A `generate_code` action whose argument is a prompt and whose effect is a file, or worse an
execution. That makes the model's output an effect directly, which is the one thing the whole
design exists to prevent.

### 4.2 The right shape, in this engine's own vocabulary

Generated code is **a value with provenance**, not an action:

1. **The model proposes an artifact.** The code is an `ArtifactId` carrying
   `Trust::External` — a model wrote it, and in this design a model is not a trusted source.
   That is the same reasoning that makes `Residual` a category.
2. **Writing it to a workspace is `SideEffect::Reversible`.** A file the engine owns and can
   delete. This needs one new tool and no new core concept.
3. **Checking it is `SideEffect::Pure` — and this is the whole point.** `cargo check`,
   `tsc`, `pytest`, a linter. These are *symbolic verifiers*: deterministic, free of request
   cost, stable across runs, and with a true-positive rate of 1.0 by construction for the
   class of errors they detect. No κ. No judge. No drift.
4. **Executing or installing it is `SideEffect::Irreversible`** and is a *separate* action,
   so `SideEffectGate` stages it and names what will run.

**One emission detail, measured rather than assumed.** A source file must not travel as a JSON
string argument: escaping and newlines make it the likeliest emission to arrive malformed and
burn one of `max_emit_retries`. The 2026 format-tax work also shows the degradation from
structured output originates in the *prompt's* formatting instruction, not the decoder's
constraint — so the right split is to keep the **action choice constrained** (the legality
guarantee is untouched) and let the **code payload come as free text**, recovered by a
schema-aligned parse. That is BAML's SAP applied to the one field where it earns its keep,
which is the narrow version of §2.2(a).

The consequence is worth stating on its own:

> Code generation converts "did the model do a good job", which is unanswerable, needs a
> judge, and is unstable on re-ask, into "does it compile and pass its tests", which is
> answerable, free, and identical every time.

That is the strongest available argument for the neurosymbolic thesis, and it is the one
domain where the engine's insistence on symbolic checks stops being a tax and becomes the
reason it wins.

### 4.3 What already supports it — and what actively does not

**Corrected 2026-09-08 by `docs/research/2026-09-08-codegen-decomposition-findings.md`.**
The first version of this section claimed the clip was "exactly right for compiler output" and
that the budget "already bounds" a fix loop. Both were written without measuring anything, and
measurement says the harness as configured would make code generation *worse*:

| file | ~tokens | × `prompt_budget_tokens` (6000) | × `tool_result_max_chars` (1200) |
|---|---|---|---|
| `crates/engine/src/turn.rs` | 33,995 | **5.7×** | **113×** |
| `crates/engine/src/eval.rs` | 22,008 | 3.7× | 73× |

One file in this workspace is six times the whole context ceiling. Reading it returns 1.1% of
it. `max_iterations = 5` is short for compile-fix. The fold collapses this turn's earlier
edits into a counted line, and in code editing — unlike clicking — what happened three steps
ago *is* the state.

That is not a flaw: M7 sized 6,000 tokens "so … a desktop turn's trace sit inside it with
room". These are desktop numbers working correctly on a workload nothing like a desktop turn.

**What genuinely supports it:**

- **Compiler errors are refusals, and M7's fold never folds refusals** — "refusals are what
  steer the next proposal, and a model that cannot see why it was refused proposes it again".
  The compile-fix loop is the right shape.
- **`inspect_result` is the right primitive**, but addressed wrongly for code — see below.

**What has to change, and it is not "raise the limits":**

> A tool that returns 136 KB is the bug. The clip is a symptom-fixer.

For a desktop control tree the head is representative, so a cap is honest. For a source file no
prefix is representative — what is needed is at line 400, and paging there costs a request per
page. So code tools must return **symbol-scoped slices** — this function, this impl block,
addressed by name rather than character offset. Results then arrive at 1–3 KB, the existing cap
never fires, and the model gets a unit of meaning rather than a truncated prefix. The engine
has the paging primitive; it lacks semantic addressing. A code tier with its own budget is the
safety net, not the fix.

### 4.4 The failure mode that decides the design

The white-box context-rot study (arXiv:2607.17937) reports that under a harmful context,
**38 of 44 failed runs end with a success claim despite unresolved defects**, while requirement
coverage stays at 93–95% and strict success falls to 37.5% — degradation "removes a few
decisive obligations rather than the whole artifact".

The dominant failure of a coding agent is therefore not writing bad code. It is **reporting
done when it is not**. A judge asked "did this go well" is asking the failing component to
grade itself; a compiler is not. M6 §13's "no model in the guard chain" is not conservatism
here — it is the only thing that catches the modal failure.

The same study measures the mitigation: a generic self-check recovers 5/10, while **a detailed
external checklist restating every obligation recovers 10/10** (p = 0.0325). The engine already
has two thirds of that — facts are small, explicit and external; guards are independently
checkable. What is missing is *the obligations of the current task*: a checklist that survives
the fold, the clip and the budget because it is the block that may never be dropped, and that a
turn cannot be reported complete against while any row is unchecked.

### 4.5 What it needs

- A `Workspace` tool: a directory the engine owns, so "delete it" is real and a generated file
  can never land outside it. Path arguments must be validated against the workspace root —
  a `..` in a model-supplied path is the obvious attack and a symbolic check catches it.
- A per-language `check` tool returning structured pass/fail plus output, so the *result* is
  what gates step 4, not the model's claim about the result.
- An iteration cap distinct from `max_iterations`: a compile-fix loop has a different natural
  length from a desktop task, and on fifty requests a day it must be bounded explicitly.

---

## 5. General applicability, and the Tomáš / WhatsApp port

### 5.1 Most of the generality is already built

`HarnessBuilder` has slots for emitter, replier, memory, channel, consolidator, summarizer,
tools and guards. The desktop is *a tool set*. WhatsApp is *a `Channel`*. Nothing in the core
knows about either. That is the port.

### 5.2 Three things that must change

**(a) The confirmation channel is a security boundary, and on WhatsApp it collapses.**

This is the finding that matters most, and it is not hypothetical — it falls straight out of
the current guards:

- `TaintPolicy` escalates an `External` argument to `NeedsConfirmation` — it does **not**
  deny.
- `SideEffectGate` requires a `Confirmed` event this turn for anything `Irreversible`.
- **Both collapse to `Allow` when `ctx.confirmed_this_turn` is set.**

On the CLI that is sound: the confirmer is the person at the keyboard, and the input channel
and the trust channel are the same channel *because there is one user*. On WhatsApp the
sender is a stranger, and unless something stops it, the same stranger supplies both the
tainted argument and the confirmation that clears it. The taint gate becomes a formality.

> **Required for any multi-user channel: confirmation must be authenticated as the owner and
> must not be satisfiable by the message sender.** Either a separate owner channel, or an
> identity check on the `Confirmed` event, with the owner's identity in the event so replay
> can audit it.

Related and equally sharp: `confirm_irreversible = false` — the unattended mode added for
long desktop runs — **must be impossible to set on a multi-user channel**. Unattended plus a
stranger's messages is every irreversible action executing on request. This should be a
startup refusal, not a documentation note.

**(b) Scope stops being theoretical.** M6 §6.6 made the *fact scope*, not the session, the
unit of memory, and recorded that global facts were "correct for a single-user CLI, a leak for
the Telegram target". That decision now pays — but it has never been tested with two users.
The ability set needs a leakage fixture: two scopes, a fact stated in one, and a recall in the
other that must return nothing. Until that fixture exists, the design is right and the code is
unverified.

**(c) Durability — and this is where Temporal's own trigger fires.** M7 §3 declined Temporal
and recorded the condition to revisit: *"a channel where a turn must outlive the process (the
Telegram target …)"*. WhatsApp is that channel. A turn can span a reply that arrives an hour
later, across a restart.

That still probably does not mean adopting the Temporal server — one process, one operator,
and the Rust SDK is not where the Python one is. It means taking the property Temporal exists
to provide, which M7 already began with T0.4 (persist side effects mid-turn) and which M8
§2.1 sharpened into "a nondeterministic result is recorded, and replay reads the record". The
remaining step is **turn resumption**: a turn's staged proposal and pending confirmation must
be reconstructable from the log after a restart, because `fold()` already replays everything
else. If that proves harder than it looks, *then* the Temporal decision is genuinely open and
should be re-taken against a written comparison rather than assumed.

### 5.3 One thing that must not change

The guard chain and the trust lattice. The CLI never really stressed them, because a
single-user desktop has no adversary. WhatsApp does. Every invariant that has been carried
"because the design says so" becomes the thing standing between a stranger's message and an
irreversible action.

### 5.4 What the port should shrink

The legal action set. There is no desktop on WhatsApp — the tools are messaging, memory and
whatever Tomáš's domain needs. A smaller legal set is a smaller tools array, which is fewer
tokens per request, which is where M7's whole budget effort pays out. The tiered router
(M7 Phase 3) already does most of this; on WhatsApp the `Chat` tier is most traffic.

---

## 6. Order, if this becomes a milestone

1. **The leakage fixture and the confirmation-identity fix** — before any port, because they
   are the two places where the current design is unverified rather than merely unfinished.
2. **Per-action emitter tests** (§2.2b) — makes everything after it safe to add.
3. **Symbolic derivation** (§3) — the largest win in requests per task, and it reuses
   `verify_patch` rather than adding machinery.
4. **The SAP measurement** (§2.2a) — cheap, offline, and settles a question that would
   otherwise be re-argued every time a parse fails.
5. **Code generation** (§4) — after 2 and 3, because it needs per-action tests to be safe and
   benefits from derivation bounding its loops. And it needs, in this order: symbol-scoped code
   tools (§4.3), the obligations block (§4.4), then a code tier's budget as the safety net.
6. **Turn resumption** (§5.2c) — with the port, not before it.

## 7. Not proposed

Adopting BAML (§2.1). Adopting Temporal (§5.2c — the trigger fires, the server still does
not follow). A model in the guard chain, in the router, or judging generated code (M6 §13, and
§4.2 gives the alternative). Graph memory (M6 §13, unchanged). Fine-tuning (M6 §13).
