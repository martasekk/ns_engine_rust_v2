# Reply Entrainment — the replier stops parroting its own context

**Goal:** the replier answers the user's message in its own words. It never
reproduces a line of the material it was shown, and it never conditions on a
reply that did.

**Research:** `docs/research/2026-09-04-entrainment-findings.md` (9 searches,
6 papers read directly). **Evidence:** live session `cli` in `ns.sqlite`,
turns 137–161.

---

## 1. What actually happened

Turn 137, `recall` returns hits. `turn.rs:1076` renders the fact hits as
`format!("fact {} = {}", f.key, f.value)`, the whole `Vec<String>` is
JSON-serialized into `ToolOutput.summary`, and `turn_trace()` (`turn.rs:1706`)
puts it in the reply prompt verbatim:

```
did:
  ToolReturned(ok: ["t122 bot: …","fact user.previous_name = \"Tomas\"","fact user.name = \"Peter\""])
```

directly above the instruction *"State only outcomes and values that appear
above"*. The model answered `fact user.name = "Peter"`.

Then it stuck — 16 replies over turns 137–158, and **turns 142–152 made no tool
calls at all**. By then the string existed nowhere but in the window's own
`bot:` lines. Each parroted reply was written to the log, folded into the
window, and shown back as the most recent example of what a reply looks like.

Four amplifiers, all ours:

| # | Amplifier | Where |
| --- | --- | --- |
| A1 | Engine-internal syntax (`fact k = v`, JSON arrays) enters model-visible text | `turn.rs:1076`, `turn.rs:1706` |
| A2 | Persona aside, *all* context is one `user` message rendered as a transcript — the task looks like continuation, not reply | `replier.rs:36–83` |
| A3 | The grounding interceptor bounds overlap from **below only**; a copied line is maximally "grounded" and passes | `ground.rs:222` |
| A4 | The reply is committed to the window unfiltered, so the next turn conditions on it | `turn.rs` §4 → `fold` → `state.window()` |

The literature names both halves. A1–A2 produce **contextual entrainment** —
models raise the logits of any token or sentence present in context "regardless
of semantic relevance" (Niu et al., ACL 2025; sentence-level, 26 models,
arXiv 2606.24077). A3–A4 turn it into a **self-reinforcing loop** — the Echo
Trap (RAGEN, arXiv 2504.20073), and *"when LLMs take a wrong turn in a
conversation, they get lost and do not recover"* (ICLR 2026, arXiv 2505.06120).

One finding decides the shape of the fix, and it is ours rather than a
paper's: **the interceptor we already had fired zero times across all sixteen
copied replies.** A check that bounds overlap only from below cannot see this
failure at all, and by rewarding the cheapest way to avoid invention it makes
the failure more likely, not less.

So: **remove the copyable material (A1), mark what remains as data (A2), and
put a hard runtime bound on overlap before the reply is committed (A3/A4).**

Two claims that stood in the first draft of this plan have been **withdrawn**
(2026-09-04, see the corrections in findings §1):

- *"A bigger replier will not fix this"* — rested on filing `fact k = v` under
  the "irrelevant" context category that worsens with scale. By content it is
  **Related** context, the category that *improves* with scale, and the
  measured range was 111M–13B base models. A larger replier may fix our case
  unaided; this is untested.
- *"Better instructions will not hold it either"* — rested on a steering-vector
  α asymmetry, which is an intervention on activations, not on prompts. The
  claim does not transfer.

Neither withdrawal changes the plan. A1–A3 are cheap, the runtime bound is
justified by the zero-firing evidence above, and phase 0 exists precisely so
that these mechanisms are measured rather than argued.

---

## 2. Prior-art pass

| Source | Decision |
| --- | --- |
| **Contextual entrainment** — Niu et al. ACL 2025 (2505.09338); sentence-level 2606.24077; scaling 2604.13275 | **Adopt the framing, not the scaling claim.** The reply prompt is an allowlist of strings the model may emit; curate it. The scaling result is cited only for "context quality is the lever that compounds" — the model-size extrapolation was withdrawn. Head-masking needs weight access → **later**, self-hosted slot. |
| **Copy-vs-recall asymmetry** — 2601.12075 | **Weakened.** Supports "copying is the cheaper behaviour". Does **not** support "prompt wording is insufficient" — that is a steering-vector result, a different intervention channel. Ship the runtime check because the old check demonstrably missed sixteen copies, not because of this number. |
| **LLMs Get Lost in Multi-Turn** — ICLR 2026 (2505.06120) | **Adopt.** `CONCAT` (consolidated) 95.1% of single-turn vs `SHARDED` 65%; `SNOWBALL` (restate the transcript every turn) 61–65%. Our `render_window` is the SNOWBALL shape. Consolidating the window is the highest-ceiling change — and the riskiest, so it is Phase 4 behind Phase 0's metric. Unreliability +112% vs aptitude −16% also explains why this is intermittent rather than constant. |
| **Spotlighting / delimiting** — Microsoft, CEUR Vol-3920 | **Adopt delimiting** for window and trace blocks: randomized boundary markers plus an explicit rule about them, reported minimal task impact. **Reject datamarking and encoding** — token cost and illegible logs, and our problem is a lazy model, not an adversary. |
| **LoopGuard** (2604.10044) TTR / compression-rate thresholds; **SpecRA** FFT autocorrelation | **Adopt the detector shape, reject the implementations.** Both target periodicity *within* one long generation; ours is one short line repeated *across* turns, and both interventions need local inference. A longest-common-span check over the prompt is the same insight at our granularity, in ~20 lines. |
| **Groundedness practice 2026** — "lexical overlap … cannot tell a paraphrase from a fabrication" | **Overturns M6 §4.5.** A one-sided overlap filter selects *for* copying. `ReplyFlagged` fired zero times across 16 parroted turns. Add the upper bound. |
| **ERGO** entropy-guided reset (2510.14077) | **Adopt the policy, drop the signal.** On detection, rebuild context rather than continue. Entropy needs logprobs, which our presets expose unevenly. |
| **Agent retry matrices / MCP protocol-vs-execution error split** | **Adopt.** Four failure classes, four recoveries. Terminal config errors (402/404) must not be retried at all. |
| **`frequency_penalty` / `presence_penalty`** | **Rejected.** Acts on the completion distribution — wrong failure. Would suppress legitimate repetition, and `CloudReplier` sends no sampling params by design so one request shape works on Sonnet, Mistral and Ollama. |

---

## 3. Phases

### Phase 0 — measure it (do this first, it is the acceptance criterion)

Without a number, every later phase is taste. Add `crates/engine/src/echo.rs`:

```rust
/// Longest contiguous run of words shared by `reply` and `material`,
/// as a fraction of the reply's word count. 1.0 = the reply is a
/// verbatim span of its own prompt.
pub fn echo_ratio(reply: &str, material: &Material) -> f32
```

Word-level (not byte), case-folded, punctuation-stripped. Exclude the user's
own message from the material for this metric — echoing the user back is a
different bug with a different fix.

Then `ns-app echo <session_id>`, alongside `dump`: per-turn ratio, session mean,
count over threshold.

**Baseline, `ns.sqlite` session `cli`, 161 replied turns: mean 0.432, 66 turns
at or over 0.60.** The metric separates cleanly with no tuning:

```
t137   1.00  fact user.name = "Peter"          t153   0.00  Hello! How can I help you today?
t141   0.80  fact user.previous_name = Tomas   t160   0.00  Your previous name is Tomas.
t142   1.00  fact user.previous_name = Tomas   t161   0.38  You do not have a current name recorded…
```

All 16 known parroted turns land in 0.80–1.00; the honest answers at t160/t161
land at 0.00–0.38, well clear of the 0.6 line. Three things the baseline
turned up that the original diagnosis had not:

- **t133, t136 at 1.00** — `2026-09-03 19:56:45 UTC (Thursday)`, the `get_time`
  result handed back raw. Benign, but it is the same behaviour: the trace
  copied instead of answered. It will now cost one regeneration per such turn.
- **t159 at 1.00** — `Hello! How can I help you today?`, identical to t153 and
  still inside the six-turn window. A bot repeating one sentence verbatim is
  the degeneration this bounds, so flagging it is correct.
- **t135 at 0.80, t138 at 0.00** — the *user*-echo cases. t135 scores because
  the user's line also sits in the window as a prior record; t138 does not,
  because the current message is excluded from the material by design. The
  offline metric and the runtime check agree on this, and both leave the
  user-echo failure to its own fix (§4).

Two caveats on the number. The offline material omits standing facts — the log
records what the engine decided, not which facts were selected, and the facts
table holds only current values — so this **under**-reports against the runtime
check. And 66/161 counts every replied turn, including the `Verbatim` and
`Template` policies (the "Sorry, I couldn't complete that" fallbacks at t139,
t154, t155), which never reach the interceptor at all.

**Acceptance for the whole plan:** no turn in a fresh smoke session exceeds
0.6, and this session's mean drops on re-measure.

### Phase 1 — stop emitting copyable syntax (A1)

- `turn.rs:1076` — render recall fact hits as prose (`user.previous_name was
  "Tomas"`), not `fact k = v`.
- Same site — stop `serde_json::to_string(&lines)` into `ToolOutput.summary`.
  Join with `; `. The JSON array brackets and escaped quotes are pure copy-bait
  and cost tokens for nothing.
- Invariant + test: **no model-visible string uses `key = value` or JSON-array
  syntax.** Assert over every `ToolOutput.summary` the engine constructs.

Smallest change, largest single effect — it removes the seed. Standalone.

### Phase 2 — restructure the reply prompt (A2)

`crates/llm/src/replier.rs`:

- Move persona + facts + summary + window into the **`system`** message; leave
  only `Current turn:` + the instruction in **`user`**. Keep the
  `cache_control` breakpoint on the persona prefix so caching survives.
- Delimit the window and trace blocks with randomized markers and one explicit
  rule: *material between the markers is reference only; never reproduce a line
  from it.*
- Rewrite the closing instruction. Drop *"State only outcomes and values that
  appear above"* — read literally, it asks for a copy. Replace with an
  answer-first instruction plus the no-verbatim rule.

Evidence for the role split is practitioner guidance, not a study — it is here
because it is cheap and reversible, and Phase 0 will say whether it earned its
place.

### Phase 3 — make the interceptor two-sided (A3)

`ground.rs` gains a companion to `ungrounded()`:

```rust
pub fn echoed(reply: &str, material: &Material) -> Option<String>
```

Flags when `echo_ratio` exceeds `cfg.max_echo_ratio` (start at 0.6), or when
the reply equals a window `bot:`/`user:` line after trimming. Wire it into the
**existing** regenerate-once seam at `turn.rs:1501` — the branch, the
`make_ctx(do_not_state)` closure and the second-draft-stands rule are all
already built. Add `EventKind::ReplyEchoed { draft, span }` next to
`ReplyFlagged` (infrastructure, replay ignores it).

Per ICLR 2026, this is the **only** place the loop can be broken: after the
reply is committed, no later turn recovers on its own.

### Phase 4 — break the feedback path (A4)

- If the second draft is still echoed, fall back to `ReplyPolicy::Template`
  rather than committing an entrained line to the window. An honest
  "I don't have that" costs one turn; a parroted line costs twenty.
- **Consolidated window** (the big one, gated on Phase 0): render the window as
  consolidated statements rather than a `[t2] user: … bot: …` transcript. This
  is `CONCAT` (95.1%) instead of `SNOWBALL` (61–65%). It touches
  `nscore::render_window`, which the emitter, the grounding material and the
  replay corpus all share — so it goes last, measured, and alone.

### Phase 5 — error taxonomy (the *other* "malformed")

Independent of the above; do it whenever.

- Split `RejectReason::Malformed` into `Malformed { detail }` (model produced
  unusable output) and `ProviderUnavailable { status, detail }`. Serde-compat so
  existing events still deserialize — 130 of them are in `ns.sqlite`.
- `turn_trace()` renders provider failures as an endpoint problem, not as the
  emitter's fault. Right now the emitter is told three times a turn that it
  produced malformed output while the endpoint was down.
- Retry policy by class: 429 → honour `Retry-After`, back off; 5xx → backoff;
  **402/404 → terminal, do not retry.** Turns 154 and 155 each burned all three
  `max_emit_retries` against a 404 that could never succeed.

---

## 4. Not in scope

- `max_iterations = 5` vs the `repeat_gate`: turn 139 spent five `recall` calls
  on one question and fell through to the apology. Real (2607.01641, tool-calling
  loop class), separate, needs its own numbers.
- Attention-head masking (2606.24077, halves entrainment) — needs weight access.
  Same slot as llguidance, self-hosted only.
- The replier echoing the *user's* text (t135 `"hi\nwhat time is it?"`,
  t138 `"Could you tell me a joke?"`). Same mechanism, different material.
  Phase 0's metric deliberately excludes it; fold it in once the fact-line case
  is closed.

---

## 5. Suggested cut

Phases 0–3 are one coherent change: measure, remove the seed, mark the
material, bound the overlap. Phase 5 is small and unrelated. Phase 4 is a
second sitting.

---

## 6. Status

Phases 0–3 built 2026-09-04, then phase 3 demoted to a monitor and phase 5 built the same day after the §7 ablation. Phase 4 closed unbuilt. Workspace: 240 tests passing.

**Live validation**, same `cli` session so the parroted replies were still in
the window (turns 162–165, openrouter / google/gemini-3.8-flash on both roles):

| turn | user | reply | ratio |
| --- | --- | --- | --- |
| 162 | hi | Greetings! What would you like to work on? | 0.00 |
| 163 | whats my name | There is no current name saved for you, though your prior name was Tomas. | 0.00 |
| 164 | what was my previous name | Your earlier name was Tomas. | 0.00 |
| 165 | what time is it? | The time is 07:44:23 UTC on Friday. | 0.44 |

t163 and t164 are the questions that produced `fact user.previous_name =
Tomas` sixteen times. Both interceptors fired once each and both were true
positives:

- t162 `ReplyEchoed`, ratio 0.67 — first draft `"Hello. How can I assist
  you?"` shared the run `hello how can i` with a window `bot:` line.
  Regenerated to a sentence that is not in the window.
- t165 `ReplyFlagged`, span `September` — the draft said "Friday, September 4,
  2026" where `get_time` had returned `2026-09-04`. The *invention* check,
  catching invention, on a draft the copy check passed. The pair works
  independently, on one shared regeneration.

Acceptance met: no new turn at or over 0.6; session mean 0.432 → 0.424 (moved
little because 161 of the 165 turns are history).

| Phase | State | Where |
| --- | --- | --- |
| 0 metric | done | `crates/engine/src/echo.rs`, `ns-app echo <session>` (`app/src/main.rs::render_echo`) |
| 1 no copyable syntax | done | `turn.rs` recall arm + `value_text`; invariant asserted in `turn_loop.rs::recall_searches_turns_beyond_the_window_and_facts` |
| 2 prompt structure | done | `replier.rs::render_reference` (system) / `render_task` (user) |
| 3 copy check | **monitor, not gate** (§8) | `ground.rs::echo_material`, `echo::echoed`, `EventKind::ReplyEchoed`, `EngineConfig::max_echo_ratio` (0.6 = reporting threshold) |
| 4 feedback path | **closed, not built** (§9) | consolidated window contraindicated by arXiv 2601.00821; template fallback moot once echo stopped gating |
| 5 error taxonomy | done | `EmitError::Provider{status,detail}` + `is_retryable()`, `RejectReason::ProviderUnavailable`, class-aware break in the emit loop |

Notes from the build:

- `ScriptedReplier` answers with the turn trace verbatim — that is its job, so
  tests can assert on what the replier was shown. The copy bound correctly
  flags every one of its replies, so `engine_with` sets `max_echo_ratio: 1.1`
  and the bound is exercised on its own double
  (`a_reply_copied_out_of_its_own_prompt_is_flagged_and_regenerated_once`).
  Worth remembering that the test doubles were shaped to satisfy a one-sided
  check, which is its own small piece of evidence for §2's last row.
- Both interceptors now share `ground::reference_parts`, so the two checks and
  the prompt cannot drift apart — the failure mode fixed in 123126e, which
  would otherwise return once there were two consumers.
- `state.rs` drops `ReplyEchoed` from the fold exactly as it drops
  `ReplyFlagged`: a copied draft must never become a window record. That is
  the half of phase 3 that stops the loop compounding, rather than merely
  catching one turn of it.
- `EventKind::ReplyFlagged` and `ReplyEchoed` fire independently but share the
  single regeneration, so the interceptor still costs at most one extra reply
  call per turn.
- Not addressed, and visible in the baseline: `state.rs::call_label` renders
  `remember_fact user.age=17` into window `did:` lines — engine `k = v` syntax
  in model-visible text, the same class as phase 1. It reads as a machine log
  rather than as content, and nothing has copied it, so it is left alone for
  now; the phase 1 invariant covers `ToolOutput.summary` only.

---

## 7. Ablation, 2026-09-04 — what the phases actually do

Four cumulative arms, 12 turns each, fresh store per arm, same script, same
model (openrouter / google/gemini-3.8-flash on both roles). Arms A and B built
from reverted source; C and D share a binary and differ only in
`max_echo_ratio` (1.1 = bound off, 0.6 = on).

| arm | variant | mean ratio | max | ≥0.6 | ReplyEchoed |
| --- | --- | --- | --- | --- | --- |
| A | baseline — no P1/P2/P3 | 0.598 | 1.00 | 7 | — |
| B | +P1 prose recall | 0.654 | 1.00 | 8 | — |
| C | +P1+P2 prompt shape | 0.104 | 0.50 | 0 | — |
| D | +P1+P2+P3 interceptor | 0.030 | 0.36 | 0 | **0** |

Then arms E and F, 11 turns, a script written to force `recall` (information
given once, pushed out of the six-turn window, then asked for repeatedly):
E baseline 0.284, F +P1 0.310.

**Phase 2 does all of the measurable work.** 0.598 → 0.104 is an order of
magnitude. It is not variance: C and D are the *same condition* (D's bound
never fired, so its config never mattered), which gives a same-condition
run-to-run spread of 0.074 — and the A→C gap is seven times that. In arms A/B
the model answers three repetitions of "whats my name" with the identical
sentence each time; in C/D it varies — "Your name is Peter" / "You are Peter"
/ "It's Peter". This was the phase with the least literature behind it (§2
records no primary study for the role split, and the delimiter evidence comes
from a different outcome measure). It is the one that worked.

**Phase 1 is untested, twice over.** A→B is +0.056, inside the 0.074 noise
floor, and worse, `recall` was never called: 46 turns across four arms
produced `remember_fact` and `get_time` only. `remember_fact` plus fact
selection covers the case that recall exists for, so the emitter never reaches
for it. The phase-1 rendering is therefore unexercised in live traffic here.
It costs nothing and the invariant test still holds the string shape; it has
simply not been shown to matter.

**Phase 3 has never once caught the failure it was built for.** It fired zero
times in arm D. Scored across the four arms where the bound was off, it *would*
have fired 21 times — and all 21 are correct answers:

```
A t5  1.00  Your name is Peter.            E t10 1.00  The budget is 2000 crowns.
A t9  1.00  Hello Peter, how can I help…   F t9  1.00  Your project budget is 2000 crowns.
A t12 0.67  It is 08:15:40 UTC.            B t3  0.62  I have remembered that your age is 17.
```

A user who asks the same question twice gets the same answer, and a time
question must contain the time. **21 flags, 0 true positives.** The single live
catch recorded in §6 (t162, "Hello. How can I assist you?") belongs to this
same class on re-reading — a repeated greeting, not the fact-line bug.

And no threshold rescues it. The sixteen historical parroted turns scored
0.80–1.00; these false positives score 0.60–1.00. The distributions **overlap
completely at 1.00**, so no cut on `echo_ratio` separates "copied an engine
artifact" from "gave the same short correct answer twice". The metric measures
verbatim overlap, and those two things have identical overlap.

**The largest caveat: the bug did not reproduce.** Zero of six arms — including
both baselines, on unmodified code — produced a reply containing engine syntax.
The original failure needed a contaminated window (turns 142–152 made no tool
calls at all and copied only their own prior replies), and a fresh store cannot
supply one. So this ablation measures **verbatim self-repetition**, which is a
real quality defect that phase 2 largely fixes; it does not measure the
parroting bug, and cannot show that any phase fixes it.

### What follows

- **Keep phase 2.** It is the only phase with a measured effect, and the effect
  is large.
- **Keep phase 1.** Free, invariant-tested, unexercised. No reason to revert.
- **Phase 3 needs re-tuning or scoping before it is trustworthy.** As shipped
  it costs one extra reply call per false positive, and on this evidence nearly
  every firing is a false positive. Options, none yet tested: raise the
  threshold to 1.0 with a longer minimum span; or restrict the echo material to
  the turn trace and facts, excluding prior `bot:` lines from the window —
  which would catch the *seed* (t137 copied the recall trace) while ignoring
  the correct-repetition class, though it would also miss the propagation
  (t142–152 copied only window lines). The seed is the half worth catching,
  since without it there is nothing to propagate.
- **A real test of the fix needs a seeded window**, not a fresh store: replay
  the contaminated `cli` history into a scratch store, then run the same
  questions per arm. That is the experiment that would settle it.

---

## 8. The ablation against the research

| Research claim | What §7 measured | Verdict |
| --- | --- | --- |
| Sentence-level entrainment reproduces whole lines (2606.24077) | Baseline arms reproduced `"Your name is Peter."` and `"Hello Peter, how can I help you today?"` intact | **Confirmed.** The unit of copying is the line, and a word-run detector finds it |
| Consolidated ≫ transcript: CONCAT 95.1%, SHARDED 65%, SNOWBALL 61–65% (ICLR 2026) | P2 — fencing the transcript, moving it out of the user turn — was the only phase with a measured effect | **Best prediction in the set** |
| Unreliability +112% against aptitude −16% (ICLR 2026) | The bug failed to reproduce in either baseline arm | **Predicted, and treated as a surprise.** A variance phenomenon should not be expected to recur on demand |
| Echo Trap: conditioned on its own output, a model compounds and does not recover (2504.20073, 2505.06120) | Fresh stores never entered the loop; the original needed t142–152 copying only their own prior replies | **Confirmed — and it predicted the experimental design error.** The mechanism needs a seeded window; §7 removed exactly that condition |
| "Lexical overlap cannot tell a paraphrase from a fabrication" | 21 flags, 0 true positives; parrots 0.80–1.00 and correct answers 0.60–1.00 overlap completely | **Predicted phase 3's failure outright** |
| Copying is far cheaper to induce than recall (2601.12075) | A prompt-structure change flipped the behaviour by an order of magnitude | **Contradicted** on the strong reading. Already withdrawn in §1; the data would have forced it |
| "Context quality is the lever that compounds" (2604.13275, the surviving half) | Same model, same facts, different context structure → 6× difference | **Confirmed.** The one claim kept from that paper is the one the data supports |
| Prose tool output vs `fact k = v` | — | **Silent.** No source addresses it; phase 1 remains untested |

Three faults, all mine, all avoidable by reading the sources as method rather
than as motivation.

**The phenomenon was imported without its operationalization.** Every
entrainment paper measures entrainment *relative to a gold answer* — Niu et al.
compare the logit shift on a distractor against the correct token; 2604.13275
reports a "gold–distractor gap". Entrainment is a failure only when the
entrained content is wrong. `echo_ratio` is reference-free, so it cannot
separate entrainment on a wrong line (the bug) from entrainment on a right one
(`"Your name is Peter."`, which arguably *should* be stable across askings).
That one substitution produced all 21 false positives.

**Monitors were turned into a gate, at a threshold far outside any source.**
SpecRA is titled *Monitor* degenerative repetition. LoopGuard intervenes only
on *persistent* loops, at thresholds like "<20% unique tokens" — near-total
collapse. §2 rejected both as "wrong scale", built a lexical analogue, and
wired it at 0.6 as a blocking check costing one reply call per firing. The
sources' own thresholds are an order of magnitude more conservative, which is
exactly consistent with the measured false-positive rate.

**The sources recommended against the mechanism and named alternatives.** The
groundedness material calls deterministic lexical checks *useful pre-filters*
and points at compact learned detectors (Luna, LettuceDetect) for the decision
itself. §4 of the findings quotes that line to justify adding an upper bound,
and then builds the upper bound out of the mechanism the quote warns about.

### Consequence: phase 3 is demoted from gate to monitor

`ReplyEchoed` is still computed and logged; nothing is regenerated on it. This
keeps the observability that surfaced every finding above, costs no extra reply
call, and matches how both loop papers actually use their detectors.
`max_echo_ratio` becomes the *reporting* threshold. `ReplyContext.do_not_repeat`
stays as the seam for a future reference-aware check, unpopulated for now.

A copy bound should not gate replies again until it has a measured
true-positive rate on a seeded window.

---

## 9. Phase 4 reassessed — the consolidated window is contraindicated

§8 ranks the ICLR 2026 result as the best-supported prior in the set, and §3
lists "render the window as consolidated statements rather than a transcript"
as its phase 4 payoff. On a closer read that inference is the same species of
transfer that §8 just faulted, and this repo has a directly contradicting prior
of its own.

`CONCAT` in ICLR 2026 concatenates **the user's instruction** — requirements
dribbled out over turns, restated as one bullet list. It is a claim about how a
*task is specified*, not about how *conversation memory is represented*. The
paper the M6 design already rests on measures the second thing directly:

> **Fidelity Before Structure** (arXiv 2601.00821, findings 2026-09-02 §1) — a
> controlled ablation swapping only the stored representation: verbatim
> conversation chunks vs LLM-extracted facts/decisions/events. LoCoMo 43.9% vs
> 28.0%; LongMemEval-S 67.4% vs 45.4%. "Structured memory should augment
> verbatim text, never replace it."

Consolidating the window is the losing arm of that ablation, by ~15 points, in
the setting that actually matches ours. **Not built.** If it is ever revisited
it must be as an addition alongside the verbatim window, never a replacement,
and measured.

What survives of phase 4: the template-fallback-on-second-echo idea is moot,
since echo no longer gates. Phase 4 is closed.

---

## 10. Phase 5 as built

`EmitError` gains `Provider { status: u16, detail: String }`, produced by
`emitter.rs` instead of flattening an HTTP status into a `Transport(String)`.
The engine decides recovery from the class, not by parsing a status back out
of a formatted message:

- `EmitError::is_retryable()` — 429, 408 and 5xx are transient; every other
  4xx is a statement about the request or the account. `Malformed` stays
  retryable: the model may do better if asked again.
- The emit loop breaks on a non-retryable failure instead of spending
  `max_emit_retries`. Session `cli` t154/t155 each burned three attempts on a
  404; the equivalent turn now makes one.
- `RejectReason::ProviderUnavailable { status, detail }` records it as an
  endpoint failure. `turn_trace` renders it as `(provider unavailable: HTTP
  404: …)` and `state.rs` as `model endpoint unavailable (HTTP 404)`, so the
  emitter is no longer told three times a turn that it produced malformed
  output while the endpoint was down. `mine.rs` skips these entirely — a
  provider outage teaches the evolution pass nothing, since no proposal was
  ever made.
- Additive enum variant, so the 105 historical `Malformed` rejections in
  `ns.sqlite` still deserialize; all 964 events load.

One regression found by the live test and fixed: breaking out after a single
terminal failure made the fallback say *"ran out of steps after 5 actions"*.
The branch now tests whether the emitter ever produced a proposal, not whether
the retry budget was spent.

**Live, against a real 404** (`model = "google/gemini-does-not-exist"`):
one `ProviderUnavailable` event rather than three, and

```
Sorry, I couldn't complete that. Reason: the model provider answered HTTP 400:
google/gemini-does-not-exist is not a valid model ID.
```

**Live, working config, turns 166–169:** the same question asked twice
(t167, t168) drew two differently worded correct answers, both scoring 0.00,
with no `ReplyEchoed` and no regeneration — the false-positive class from §7
no longer costs anything. `ReplyFlagged` still gated t169, dropping an
invented "September 4, 2026" the tool never returned.
