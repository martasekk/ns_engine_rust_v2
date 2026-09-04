# Research findings — reply parroting, context entrainment, agent loops (2026 sweep)

Compiled 2026-09-04 after diagnosing 16 parroted replies in the live `cli` session
(`ns.sqlite`, turns 137–158). Plan: `docs/superpowers/plans/2026-09-04-reply-entrainment.md`.

Method: 9 web searches, then the abstract or body of the primary papers read directly
via fetch. Sources are marked **[read]** where the paper itself was retrieved and
**[snippet]** where the claim comes only from a search summary — treat the latter as a
lead, not a citation. Numbers are as reported by the authors and were not reproduced.

Legend: **[ADOPTED]** shapes the fix · **[VALIDATES]** confirms an existing choice ·
**[CHANGES]** overturns one · **[LATER]** designed-for, built later ·
**[DISCARDED]** considered and rejected.

---

## 0. The observation this sweep explains

Turns 137–158: the replier answered 16 turns with `fact user.previous_name = Tomas`
instead of prose. Origin at t137 was a `recall` result rendered into the reply prompt as
`ToolReturned(ok: ["…","fact user.previous_name = \"Tomas\""])`. Turns 142–152 made **no
tool calls at all** and still emitted the line — by then the only place it existed was the
window's own `bot:` lines. The literature below names both halves of that: *contextual
entrainment* (the copy) and a *self-reinforcing loop* (the persistence).

---

## 1. Why the model copies: contextual entrainment

- **Llama See, Llama Do: A Mechanistic Perspective on Contextual Entrainment and
  Distraction in LLMs** — Niu et al., ACL 2025 — arxiv.org/abs/2505.09338 — **[snippet]**
  **[ADOPTED]**
  Defines *contextual entrainment*: LMs assign higher logits to **any** token that
  appeared in context, "regardless of semantic relevance" — including randomly sampled
  tokens. Reported boost of 10–100× in probability for distractor tokens. Mechanistically
  localized to a small set of attention heads.
  → Names our failure exactly. Anything we put in the reply prompt is a candidate for
  literal reproduction; "it is only there as reference material" is not a property the
  model has. **Design consequence: the reply prompt is an allowlist of strings the model
  may emit, not a pile of context.**

- **Sentence-Level Contextual Entrainment in Large Language Models** —
  arxiv.org/abs/2606.24077 — **[read]** — **[ADOPTED]**
  Extends entrainment from tokens to whole sentences: measured over 26 models from 7
  families, sentences present in the prompt gain substantial probability at inference
  time "even if they are counterfactual statements". Only 2–4% of attention heads carry
  the behaviour; masking the shared heads roughly halves entrainment with no capability
  loss.
  → Confirms the unit of copying is the **line**, not the token — which is what we see
  (`fact user.previous_name = Tomas` reproduced whole, punctuation included). Head masking
  needs logit/weight access; **[LATER]**, self-hosted only, same slot as llguidance.

- **Better and Worse with Scale: How Contextual Entrainment Diverges with Model Size** —
  arxiv.org/abs/2604.13275 — **[read]** — **[ADOPTED]**
  Opposite scaling signs by context type. Counterfactual entrainment falls with scale
  (Cerebras-GPT b = −0.330; 9.69 → 2.30 over 111M–13B), but **random/irrelevant
  entrainment rises** (b = +0.217; 0.82 → 1.97). Gold-vs-distractor gap widens 3.0× for
  random context. Replicated on Pythia 410M–12B. No technical mitigation proposed; the
  authors' conclusion is that "context quality becomes a sharper lever as models grow,
  making retrieval curation compound rather than diminish in importance."
  → **Corrected 2026-09-04, after a challenge to this reading.** The first draft of this
  entry claimed a bigger replier would not help, on the grounds that `fact k = v` is
  "irrelevant filler" and so falls in the category that worsens with scale. That is a
  category error: in answer to *"what's my name"*, `fact user.previous_name = Tomas` is
  **Related** context by content, which is the category where entrainment *falls* with
  scale (b = −0.135). A larger replier may well fix our case unaided. The measured range
  is also 111M–13B base models (Cerebras-GPT, Pythia), one to two orders of magnitude and
  a generation of instruction-tuning away from the flash-class replier in use — the
  extrapolation was never available. **What survives** is the authors' own conclusion,
  which needs no extrapolation: context quality is the lever that compounds, and curating
  what enters the prompt is the durable fix. That is the direction the M6 design already
  leans, and it is the only thing this paper is cited for.

- **To Copy or Not to Copy: Copying Is Easier to Induce Than Recall** —
  arxiv.org/abs/2601.12075 — **[read]** — **[VALIDATES]**
  Steering-vector study of the copy-vs-recall decision. Strong asymmetry: inducing copying
  needs α = −3.0 with negligible fluency cost, while forcing parametric recall needs
  α = +30.0 with large perplexity spikes. Copy→recall restoration 66–74% EM, recall→copy
  induction 34–46% (Gemma2-2B, T5Gemma-2B). "Smaller models show substantially weaker
  effects" for the corrective direction.
  → Copying is the cheap default and prose synthesis is the expensive one; a flash-class
  replier will take the cheap path whenever a copyable line is present.
  **Caveat added 2026-09-04:** this result is about *steering-vector* interventions on
  activations, and was initially used here to argue that *prompt wording* cannot hold the
  behaviour back. Those are different intervention channels and the α asymmetry does not
  transfer between them. The paper supports "copying is the cheaper behaviour"; it does
  not establish that instructions are insufficient. The runtime check is justified by the
  live evidence in §4 — an interceptor that fired zero times on sixteen copied replies —
  not by this number.

---

## 2. Why it persisted for 22 turns: self-reinforcing loops

- **LLMs Get Lost In Multi-Turn Conversation** — arxiv.org/abs/2505.06120, ICLR 2026 —
  **[read]** — **[ADOPTED]**
  15 models, 6 tasks, 200k+ simulated conversations. Sharded multi-turn loses 39% vs
  fully-specified single-turn. The decomposition is the important part: **aptitude falls
  only 16%, unreliability rises 112%** — the models are not less able, they are less
  consistent. Conditions: `CONCAT` (all information restated as one bullet list) recovers
  **95.1%** of single-turn performance; `RECAP` (final restatement turn) ~70–76%;
  `SNOWBALL` (restate everything every turn) ~61–65%. "When LLMs take a wrong turn in a
  conversation, they get lost and do not recover."
  → Two things land here. (1) A **consolidated** context beats a transcript by ~30 points;
  restating a transcript (SNOWBALL) barely helps and can hurt. Our `render_window`
  produces a transcript, and it is the SNOWBALL shape. (2) *Do not recover* is the
  operative phrase: once t137's fact-line was written into the window, no later turn was
  going to talk itself out of it. **The only place to break this is before the reply is
  committed.**

- **RAGEN: Understanding Self-Evolution in LLM Agents via Multi-Turn RL** —
  arxiv.org/abs/2504.20073 — **[snippet]** — **[VALIDATES]**
  Names the **"Echo Trap"**: when a model is trained on its own trajectories it reuses
  memorized paths, collapsing diversity. Their setting is RL training, ours is a context
  window, but the loop shape is identical — our `Replied` event becomes next turn's `bot:`
  line, so the model is conditioned on its own output with no filter in between.

- **Circular Reasoning: Understanding Self-Reinforcing Loops in Large Reasoning Models** —
  arxiv.org/abs/2601.05693 — **[snippet]** — **[VALIDATES]**
  Looping arises from risk aversion toward harder correct actions and from self-reinforcing
  attention; once in a low-entropy repetitive regime the trajectory is hard to escape.
  → "Risk aversion toward the harder correct action" describes t142–t152 precisely:
  emitting the fact-line is cheaper than composing an answer.

- **LoopGuard: Breaking Self-Reinforcing Attention Loops via Dynamic KV Cache
  Intervention** — arxiv.org/abs/2604.10044 — **[snippet]** — **[LATER]**
  Detects loops with fixed thresholds on **type–token ratio** and **compression rate**
  (flagging e.g. <20% unique tokens), then intervenes in the KV cache.
  → Detector shape is adoptable and cheap; the KV intervention needs local inference.

- **SpecRA: Monitor Degenerative Repetition in LLM Agents using Randomized FFT** —
  openreview.net/forum?id=xVO4BqmzVD — **[snippet]** — **[DISCARDED for now]**
  Random projection of the vocabulary onto a complex sequence, FFT autocorrelation, peaks
  reveal periodicity with tolerance to minor variation. Built from 813 repetitive samples
  mined from 1.13M anonymized agent records.
  → Right idea, wrong scale: it detects periodicity *within* one long generation. Our
  repeats are one short line per turn across turns. A longest-common-span check over the
  prompt is the same insight at our granularity, and is ~20 lines of Rust.

- **When Agents Do Not Stop: Uncovering Infinite Agentic Loops in LLM Agents** —
  arxiv.org/abs/2607.01641 — **[read]** — **[VALIDATES]**
  Taxonomy: tool-calling loops / reasoning loops / state-based loops / response-based
  loops. Detection signals: repeated invocations with identical arguments, cyclic state
  transitions, stalled progress, token spend over threshold.
  → We already hold the tool-calling case (`repeat_gate` fired 25× in the log). We have
  **no** detector for the response-based case, which is the one that bit us.

- **ERGO: Entropy-guided Resetting for Generation Optimization in Multi-turn LMs** —
  arxiv.org/abs/2510.14077 — **[read]** — **[LATER]**
  Uses an entropy spike as the signal that a multi-turn trajectory has degraded, then
  resets and reconstructs context from a checkpoint rather than continuing.
  → Needs logprobs, which OpenRouter exposes unevenly across our presets. The *policy* —
  on detection, rebuild context rather than continue — is adopted in Phase 4 without the
  entropy signal.

---

## 3. Prompt structure: instructions vs. material

- **Defending Against Indirect Prompt Injection Attacks With Spotlighting** — Microsoft;
  ceur-ws.org/Vol-3920/paper03.pdf — **[snippet]** — **[ADOPTED]**
  Three variants for making external content syntactically distinct from instructions:
  **delimiting** (randomized boundary markers plus a rule about them), **datamarking**
  (interleave a special character between every word), **encoding** (base64/ROT13).
  Reported as having "minimal detrimental impact on underlying NLP tasks".
  → Adopt **delimiting** for the window and turn-trace blocks. The security framing is not
  ours, but the mechanism is exactly the one we need: make the transcript read as *data*
  rather than as a document to continue. Datamarking and encoding are rejected — both cost
  tokens and legibility, and our threat is a lazy model, not an adversary.

- **Capacity, Not Format: Rethinking Structured Reasoning Failures** —
  arxiv.org/abs/2606.09410, building on Tam et al. 2024 — **[snippet]** — **[VALIDATES]**
  Structured-output degradation is not inherent to machine-readable formats but comes from
  **premature serialization** — forcing schema-compliant tokens before reasoning finishes.
  Performance recovers whenever unconstrained reasoning precedes structured submission.
  → Validates the existing choice of a free-text `rationale` field first in the proposal
  schema (2026-09-01 findings §2). No change.

- **Role separation, system vs. user message** — no primary study found for the specific
  comparison "flattened history in one user message vs. native role messages"; the
  available material is practitioner guidance (all three major providers recommend
  delimited sections; system prompts specify behaviour, user messages carry the transient
  task). — **[snippet]** — **[ADOPTED, low confidence]**
  → Adopted as a cheap, reversible change with a clear mechanism, **not** as an
  evidence-backed one. Phase 0's metric is what will decide whether it earned its place.

---

## 4. Verification: why our interceptor did not catch this

- **Groundedness / faithfulness evaluation practice, 2026** — Openlayer and Langfuse
  guides; span-level detection arxiv.org/abs/2607.00895 — **[snippet]** — **[CHANGES]**
  "Deterministic checks like citation presence or lexical overlap are useful pre-filters,
  but they cannot tell a paraphrase from a fabrication." Compact encoder detectors (Luna,
  LettuceDetect) localize unsupported spans below LLM-judge cost.
  → **This overturns an assumption in M6 §4.5.** `crates/engine/src/ground.rs` checks one
  direction only: it flags claims *absent* from the material. A reply copied verbatim from
  the material is maximally grounded by that test — it scored perfectly on all 16 parroted
  turns, which is why `ReplyFlagged` appears zero times in a log full of parroting. A
  lexical-overlap check does not merely fail to catch entrainment; **as a one-sided filter
  it selects for it.** The interceptor needs an upper bound on overlap as well as a lower
  one.

- **Sampling penalties (`frequency_penalty` / `presence_penalty`)** — vendor docs and
  practitioner comparisons — **[snippet]** — **[DISCARDED]**
  Penalties apply to the sampling distribution over the completion; they discourage a
  model from repeating *itself within one response*, which is not our failure. They would
  also suppress legitimate repetition (a name stated twice), their scope over prompt
  tokens differs by provider, and `CloudReplier` deliberately sends no sampling params so
  the same request works on Sonnet, Mistral and Ollama. Rejected.

---

## 5. The other "malformed": error taxonomy

- **Agent failure taxonomy and retry matrices, 2026** — practitioner sources (aident.ai
  retry matrix, agent-works.ai recovery patterns); MCP spec separates protocol errors from
  tool execution errors — **[snippet]** — **[ADOPTED]**
  Four failure classes, each with a different recovery: transient transport (429/5xx —
  retry with backoff, honour `Retry-After`), model-output failure (malformed JSON, bad
  schema — re-prompt with the diagnostic), tool errors, and terminal configuration errors
  (404 unknown model, 402 out of credits — **do not retry**). "Most AI agent tool failures
  should not be retried."
  → Our `RejectReason::Malformed` is the catch-all for all four. In the live log 96 of 130
  rejections are HTTP 402/404/429 recorded as if the model misbehaved, then replayed to
  the emitter through `turn_trace` as `Rejected(malformed: …)` — teaching it, three times
  per turn, that it produced bad output when the endpoint was simply down. Turns 154–155
  each burned all three `max_emit_retries` on a 404 that could never succeed.
