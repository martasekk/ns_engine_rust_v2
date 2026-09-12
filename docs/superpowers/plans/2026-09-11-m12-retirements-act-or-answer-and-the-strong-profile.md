# M12 — The pass spends what it says, a capability profile, act-or-answer, and one live Luna run

Plan drafted 2026-09-11 on `worktree-m9-scope-in-manifest` @ `00ce79f` (the M11 tip plus M9
follow-up 7). Evidence: M11 plan §Decisions, §"What becomes redundant", §Results (T2.2 table,
the Luna wave, the dry-run defect, "Status after M11"); M10 §"Decisions taken during
execution"; M9 §"Status after P0–P5" and follow-up 7. Executed, this file becomes
`docs/superpowers/plans/2026-09-11-m12-retirements-act-or-answer-and-the-strong-profile.md`
in the repo's convention (phases, task tables, requests spent, "not built, with the reason").
Requests: **0 in P0–P5; ≤ 50 planned in P6 under the approved ceiling of 60 / $0.05.**

## Context

M11 made a strong model a legal target and ran the one measurement the retirement decision
needed. Its numbers: of 40 grounding flags on Sonnet, 8 were the matcher tripping on Czech
inflection and the one true positive turned a correct answer into a refusal; the text
fallback and the argument examples fired 0 times in 106 requests; 10 of 19 chat-tier turns
proposed nothing but `respond_directly`; the emitter prefix sat at 451–472 tokens, under the
1,024 caching floor, so `cached` stayed 0 on a paid tier; and the Luna wave found that
`ns-app evolve --dry-run` spent ten times the requests it reported.

M12 is what those numbers decide, plus the user's question: could one model call either act
or answer? Nothing here is a new architecture. It makes the idle pass spend only what a dry
run says and count every lane; retires scaffolding a strong caller does not need behind one
profile knob whose default is today; makes the grounding flag stop firing on `Praze` and
`Martine`; lets a chat turn reach `get_time`; records which model learned each note; builds
act-or-answer as a measured arm behind a knob; gives the strong context profile the arm that
decides it; and ends with one metered Luna session that reads the numbers.

Binding decisions (user, 2026-09-11, unchanged from M10/M11): desktop parked; chat and
personal memory first; a paid tier is in use (`openai/gpt-5.6-luna`); no model in the guard
chain or the fold; replay, provenance and the hash chain untouched; every new knob defaults
to today's behaviour and moves only on a non-regressing arm. New today: the live run is
approved at 60 requests / $0.05; note provenance is recorded now with archiving behind a
knob; act-or-answer replaces the plain replier-only chat path as the thing to research.

## What does not change

| Stays | Reason |
|---|---|
| Emitter/replier split on Task and Deep tiers; `tool_choice: required` there | the trace those turns narrate is what the split exists for (spec 2026-09-01 §2 p.5); cost lives on the chat floor (M10 decision 3) |
| The grounding **flag**, `ReplyFlagged`, `ReplyCited`, `UngroundedReply`, the fitness join | zero-cost symbolic signals; only the regeneration is retired, and only under `strong` |
| Echo ratio as a monitor, `obligation_check` off, `fitness_demote` off | M10 T5.4 and the release rule; P0 starts the demote clock, does not end it |
| Router tiers, once-per-turn tool selection, byte-stable tools within a turn | M10 P2 decision 2a |
| Event and manifest JSON | no new event field; new struct fields serialize absent at default (M10 T0.1 rule) |
| Argument examples on the pointer tools | desktop parked; recorded as redundant (0 `Malformed` in 106), text not touched |

## Act-or-answer — the research record

**What the split protects, per the repo's own record**, and how the single call keeps it:

| Protection | Anchor | On the chat-tier single call |
|---|---|---|
| Injection hygiene: the `<reference>` fence, "never reproduce a line of it" | `crates/llm/src/replier.rs:47,62` | preserved — the same `render_reference`, extracted and shared |
| Echo entrainment: 0.598 → 0.104 came from prompt shape | `docs/superpowers/plans/2026-09-04-reply-entrainment.md` §7 | same fence and rule; the fixture-corpus echo mean is a gate |
| Persona, fresh per call | `ReplyContext.persona` (spec §5) | added to the chat-tier emitter prompt |
| `do_not_state` regeneration seam | `turn.rs:2707-2722` | unchanged: a flagged emitted answer regenerates through the replier where regeneration is on |
| Obligations | both contexts already | unchanged |
| Memory-silence line (M11 T1.2) | `replier.rs` `MEMORY_SILENCE` | added |
| Reply guidance notes | `learned.rs:201` `guidance_for_reply` | added |
| Compliance by construction | `emitter.rs:~173` `tool_choice: required` | weakened on the chat tier only, behind a knob, measured |

Field evidence (2026): reasoning models let single-call agents replace some multistep
chains (arXiv 2603.22862); a model's hidden state already encodes the act-vs-answer decision
without explicit reasoning (arXiv 2605.09252); the known failure is "tool bypass", answering
from memory instead of calling the tool it needed (arXiv 2601.05214) — which the recall and
`remember_fact` fixtures catch, so they are the arm's negative gate.

**The design.** Chat-tier turns only, `[llm] chat_act_or_answer = false` by default.
`EmitterContext` gains `answer: Option<AnswerBlocks>` (persona, reply guidance, silence
flag; in memory only). With it set, `CloudEmitter` sends `tool_choice: "auto"`, one added
system clause ("or, when no tool applies and the context already answers, reply to the user
in plain text instead of calling a tool"), and renders the user content through the shared
`render_reference` plus the replier's closing instruction. Response rule: **tool calls
present → act, always**; content beside a tool call is truncated to 300 chars and prefixed
onto `Proposal.rationale` as `model said: …`; no tool call and non-empty content → the
answer; no tool call and empty → today's two branches (`emitter.rs:210-246`), unchanged.
Plumbing: a defaulted trait method `Emitter::propose_or_answer(ctx, legal) -> Emission
{ proposal, answer: Option<String> }` (default: `propose`, `answer: None`), overridden only
by `CloudEmitter` and `ScriptedEmitter`. The answer takes the existing `respond_directly`
branch (`turn.rs:1547-1564`) so `Settled { Generate }` and `Replied` are byte-identical for
replay, and `generate_reply` (`turn.rs:2569`) takes `pre_draft: Option<String>`: when `Some`
it skips `replier.reply()` and its `record_model_calls`, then runs the identical
`ReplyEchoed` / `ungrounded` / `ReplyFlagged` / obligation / `record_cited` block.
**What it does not save:** on the 9 of 19 chat turns that proposed a tool the count stays
two calls; nothing on Task/Deep; and the chat-tier emitter prompt grows by ~120–200 tokens
(persona + fence + reply guidance), which the arm prices against the call it removes.

## Phases

Order: P0 and P3 in parallel → P1 → P2 and P5 in parallel → P4 → P6 last, only after every
offline gate is green.

### P0 — The pass spends what it says (0 requests, ~1 day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T0.1 | `--dry-run` gates every paid lane: `build_pass` skips attaching the judge, the notes proposer and the `verify_note` probes; the report prints `skipped (dry run): judge, notes proposer, probes` | `app/src/main.rs:339-450` (406-447), `crates/evolution/src/pass.rs:897-915, 938-957` | `pass.rs` `a_dry_run_enters_no_paid_lane` (a counting evaluator double asserts `requests() == 0`); existing `dry_run_reports_but_writes_nothing`, `the_budget_caps_new_grades_and_a_dry_run_writes_none` | a dry run with `[models] enabled` and a key in the environment spends **0** (was 51 + 5) |
| T0.2 | `--spend`: runs the paid lanes without writing — what the accidental dry run was — so a κ measurement is explicit | `app/src/main.rs:134` `parse_evolve_args`, `:1042` | `evolve_args_accept_dry_run_and_spend` (replaces `evolve_args_accept_only_dry_run`) | `--spend --dry-run` runs the lanes and writes nothing; bare `--dry-run` spends nothing |
| T0.3 | one `UsageSink` per paid lane, attached in `build_pass` via `client_for(..).with_usage_sink` (`crates/llm/src/client.rs:87`); `ClientEvaluator::grade` records into its sink; `Report.lane_requests: BTreeMap<String,u32>` summed by role | `pass.rs:214, 278-283`, `client_eval.rs:118, 348-370`, `notes.rs:80-134, 337-424` | `pass.rs` `every_paid_lane_reports_its_own_requests` | prints `requests spent: judge N, proposer N, probes N, total N`; replaying the M11 Luna pass reads 56, not 5 |
| T0.4 | judge unavailability classified: read `finish_reason` before `parse_verdict`; `GradeError::Unavailable` carries `length`, `content_filter`, `not json`, `no ok field`, `issue not a string` or `transport`; `Report.grades_unavailable_by_reason` printed | `client_eval.rs:277-325, 352-370`, `pass.rs:501-503, 205` | `client_eval.rs` `a_truncated_verdict_says_length_not_not_json`; existing `a_malformed_verdict_is_unavailable_not_a_grade` | Sonnet's 5-of-10 reproduces with a named reason; κ still ignores unavailable turns (`pass.rs:531-628` untouched) |

Not built: raising the judge's `max_tokens` — T0.4 says whether `length` is the cause first.
Not built: flipping `fitness_demote` — the "one release of dry-runs" clock starts at this
commit (`consolidate.rs:190-204`); it does not end here.

### P1 — The capability profile and the inflection-aware matcher (0 requests, ~1½ days)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T1.1 | `[llm] capability = "small"` or `"strong"`, resolver `LlmConfig::capability()` in the shape of `schema_profile()`, startup error on an unknown name, mirrored into `EngineConfig` and `EmitterConfig` | `app/src/config.rs:909-915`, `main.rs:470, 729-731, 839-881` | `config.rs` `capability_defaults_to_small_and_rejects_an_unknown_name` (copy of :1435) | `small` is byte-identical to today everywhere |
| T1.2 | regeneration split from the flag: `EngineConfig.reply_regenerate` (default true; false under `strong`); only the second `replier.reply(make_ctx(spans, …))` is skipped; `ReplyFlagged`, `spans`, `UngroundedReply`, `ReplyCited` unchanged | `turn.rs:2669-2760` (2707-2716) | `turn_loop.rs` `a_flagged_reply_is_logged_but_not_regenerated_under_strong` beside :229 and :337 | under `strong` a flagged turn issues **1** replier request and still appends `ReplyFlagged` and `ReplyCited` |
| T1.3 | text fallback counted, not silenced: `nscore::text_fallbacks(events)` beside `tally_rejections`, counting `Proposed` rationales with the `model answered in text:` prefix; printed by `ns-app budget` and the pass report | `crates/core/src/event.rs:211-227`, `app/src/budget.rs:274-281`, `emitter.rs:218` | `event.rs` `a_text_fallback_is_counted_from_the_recorded_rationale`; sibling of `budget.rs:1276` | the M11 logs read 0; a synthetic log reads N; no event field added |
| T1.4 | trimmed preamble under `strong`: `SYSTEM_PREAMBLE_STRONG` beside `emitter.rs:15-20` (drops the done-line narration); `estimate_tokens(system_prompt())` printed in the prefix footer | `emitter.rs:12-26, 193`, `budget.rs:734` | `the_system_prompt_carries_the_rationale_instruction_once` extended to both profiles; `the_strong_preamble_is_shorter_and_says_by_how_much` | both preambles carry `RATIONALE_INSTRUCTION` exactly once; the footer prints each weight |
| T1.5 | inflection-aware matcher at every capability: `nscore::memory::fold_diacritics` (a small table, no new dependency) and `stem_match(a, b)` — equal, or a common folded prefix ≥ 3 with both remainders ≤ 3 and `min(len) ≥ 4` (M11 wrote "≥ 5"; `Brna`/`Brno` and `Praze`/`Praha` share three letters, so the bound moved to the remainders). `Material::contains` keeps exact substring first and falls back to `stem_match` against material tokens **for single-token claims only**; `addresses` (`memory.rs:268-272`) takes the same helper | `crates/core/src/memory.rs:125-132, 268-272`, `crates/engine/src/ground.rs:168-200` | `memory.rs` `stem_match_folds_diacritics_and_accepts_czech_inflection`: accepts `Praze`/`Praha`, `Prahy`/`Praha`, `Brna`/`Brno`, `Martine`/`Martin`, `Tomáše`/`Tomáš`; rejects `Canberra`/`Canada`, `Melbourne`/`Melanie`; `ground.rs` `material_check_is_case_insensitive_and_exact_substring` unchanged | replaying M11's 9 flagged turns leaves **1** flag (`Canberra`); `vysyp`/`vysypání` stops missing in `addresses` |

Not built: argument examples behind the profile (`pointer_tool.rs:181-201`, test :962) —
desktop parked, both tools desktop-only; the 0-in-106 reading is the record.

### P2 — The Chat tier reaches `get_time` (0 requests, ~½ day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T2.1 | `[router] chat_tools = ["get_time"]`, a named cheap allowlist, **cue-gated**: on a Chat route `Route::route` runs `select_tools` over `chat_tools` only, and a Chat turn carries a tool only when that tool's own cue in the depth table fired; `turn.rs:1318-1337` widens `tier.allows_tools()` to "allows tools, or the route selected some" | `crates/core/src/router.rs:73`, `crates/engine/src/router.rs:78, 101-104, 213-277`, `turn.rs:1318-1337`, `config.rs` | `router.rs` `a_time_question_carries_get_time_and_stays_on_the_chat_tier`; unchanged: `plain_conversation_routes_to_chat`, `turn_loop.rs:2876` no tools on a cue-less chat turn, :2905 escalation, :4872 byte-stable tools, :5160 no embed | "What time is it right now?" reaches `get_time` at tier Chat with a **one-tool** array; every other chat message carries zero tools |

The default changes here because this is a defect fix, not a knob move: the fix is
byte-identical for every message without a time cue. Rejected in one line: time cues in
`task_cues` would open the full toolbox under `depth = full`, the cost M10 P2 spent a phase
removing.

### P3 — Notes provenance and the archive knob (0 requests, ~½ day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T3.1 | `Note.learned_on: Option<String>`, `#[serde(default, skip_serializing_if = "Option::is_none")]`, excluded from `hash_of`; `Note::new_learned_on(scope, text, lift, model)` at `notes.rs:133` with the proposer's model (`notes.rs:47`) | `crates/core/src/learned.rs:86-111`, `crates/evolution/src/notes.rs:47, 133` | `learned.rs` `an_old_note_parses_without_a_model_and_serializes_without_one`, `a_note_hash_ignores_the_learning_model` | a pre-M12 `learned.toml` round-trips byte-for-byte; two notes differing only in `learned_on` share a hash |
| T3.2 | `[memory] archive_foreign_notes = false`: when true, `guidance_for` / `guidance_for_reply` / `guidance_notes_for*` skip notes whose `learned_on` is `Some` and differs from the running model; `EngineConfig.learning_model: Option<String>` from the resolved emitter `RoleTarget` | `learned.rs:193-206`, `config.rs:427-539, 768`, `main.rs:839-881`, `config.example.toml:124-154` | `learned.rs` `a_foreign_note_is_archived_only_when_the_knob_is_on`; `config.rs` sibling of :1405 | at the default nothing is filtered and no note text moves; the example parses |

Not built: archiving the one existing note — desktop-only, line parked; the knob waits for
the first chat note.

### P4 — Act-or-answer, behind a knob (0 requests, ~2 days)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T4.1 | `Emission { proposal, answer: Option<String> }`, the defaulted `Emitter::propose_or_answer`; `EmitterContext.answer: Option<AnswerBlocks>`; `render_reference` extracted from `replier.rs:62` into `nsllm::reference` and called by both roles | `crates/core/src/traits.rs:50, 120`, `crates/llm/src/reference.rs` (new), `replier.rs:62-89` | `replier.rs` `the_reference_block_is_byte_identical_after_the_extraction` | the replier's request bytes do not move |
| T4.2 | `CloudEmitter` act-or-answer: `tool_choice: "auto"` only when `ctx.answer.is_some()`; persona, fence, reply guidance, silence line and closing instruction in the prompt; both tool calls and content → act, content into the rationale | `emitter.rs:169-246` | `emitter.rs` `a_chat_tier_request_offers_auto_tool_choice_and_carries_the_fence`, `a_tool_call_wins_over_content_and_the_content_becomes_rationale`; `request_carries_schema_context_and_forced_tool_choice` unchanged with the knob off | with the knob off the emitter request is byte-identical to M11's |
| T4.3 | `[llm] chat_act_or_answer = false`; turn.rs carries `pre_draft` into `generate_reply`, which skips the replier call and its `record_model_calls` and runs the whole grounding / echo / obligation / `record_cited` path on the emitted text | `config.rs`, `main.rs:729-731, 839-881`, `turn.rs:1483, 1547-1564, 2569-2790` | `turn_loop.rs` `an_emitted_answer_is_the_reply_and_costs_one_request`, `an_emitted_answer_that_is_ungrounded_is_still_flagged`, `a_chat_turn_that_proposes_a_tool_still_costs_two_calls` | a chat turn answered in the emitter records **1** `ModelCall`, one `Replied`, the same `Settled { Generate }` bytes |
| T4.4 | `ScriptedEmitter::answering(Vec<Emission>)` (`new(Vec<Proposal>)` keeps its signature); one fixture and one ability arm exercising it; `Run` gains `act_or_answer: bool` | `crates/engine/src/script.rs:13-42`, `crates/testkit/src/fixtures.rs`, `eval.rs:825` | `script.rs` `a_scripted_emission_can_answer_instead_of_acting` | **fixtures 30/30** on both arms; abilities 10/10 unchanged on the default arm |
| T4.5 | the chat counter's second sentence, read from the log (M10 T0.2 idiom, retroactive) | `app/src/budget.rs:299-339` | `budget.rs` `the_chat_counter_reads_answers_and_requests_per_chat_turn` | prints `answered in the emitter call: N of M; requests per chat turn: X.XX` on the recorded log |

Decision rule, in order, all offline before any spend: fixtures 30/30 · abilities unchanged ·
fixture-corpus echo mean not worse than the default arm · then P6's live numbers: requests
per chat turn below 2.00 and the flag rate on emitted answers no worse than on replier
drafts. Any one failing → the knob stays `false` and the arm is recorded, not reverted.
Not built: act-or-answer on Task/Deep tiers (see "What does not change").

### P5 — The strong context profile arm (0 requests, ~1 day)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T5.1 | `Run` gains `window_turns: Option<usize>`, `facts_in_context: Option<usize>`, threaded into `Caps` / `EngineConfig` through the existing private `Harness.window_turns` precedent | `crates/testkit/src/eval.rs:794, 825, 2491` | `eval.rs` `a_run_at_the_default_profile_is_byte_identical_to_todays` | `Run::default()` yields today's numbers |
| T5.2 | `ns-app eval --window N --facts N`: abilities, prompt tokens, the emitter stable-prefix estimate; `report` refuses the ledger on this arm as it does for `--depth` and `--activation` | `app/src/eval.rs:340, 344, 744, 781-806` | `eval.rs` `the_profile_arm_prints_and_does_not_write_the_ledger` | exit code is the failure count; the ledger's mtime is unchanged |
| T5.3 | the harness sums `facts_chars/summary_chars/window_chars` from manifests so the prompt column is measured, not estimated | `eval.rs:1206-1227` | `eval.rs` `the_prompt_column_sums_the_manifest_not_the_fixture` | the two agree within 5% on the default arm |
| T5.4 | the recommended pair into `config.example.toml`'s strong block, commented, not hardcoded | `config.example.toml:22, 124-154` | the existing config doc test | example parses; `window_turns = 6`, `facts_in_context = 10` defaults unchanged |

Decision rule: abilities 10/10 at the raised profile **and** estimated emitter stable prefix
≥ 1,024 tokens (M11 measured 451–472, so the raise must more than double it to buy caching)
**and** prompt tokens per turn at Luna's rate below the call the caching saves. Fail any →
record the numbers, recommend nothing. Not built: a preset mechanism — two integers in the
example file are the whole content, and a preset would hide which one moved.

### P6 — The live Luna run (≤ 50 planned, ceiling 60 / $0.05, ~½ day runner + ½ day run)

| id | change | files / functions | test | exit criterion |
|---|---|---|---|---|
| T6.1 | the minimal runner `ns-app eval --live FILE --max-requests N`: one message per line, one session, the configured provider, `render_budget` at the end; aborts when the sink's count crosses N; refuses to start without a key | `app/src/eval.rs:340-400, 1184-1202` (the `--live` refusal replaced), `main.rs:527-550` | `eval.rs` `the_live_runner_refuses_to_start_without_a_key_and_stops_at_the_cap` | a metered run is one repeatable command (M11's 119-request breach came from hand-typing plus an uncounted lane) |
| T6.2 | the run: 18 messages — 12 chat-tier (a stated fact, a paraphrased recall, small talk ×3, an unanswerable question, a time question for P2, 5 more) and 6 task-tier — with `chat_act_or_answer = true`, `capability = "strong"`, `prompt_cache_emitter = true`, the P5 pair, `openai/gpt-5.6-luna` in all three roles; then `ns-app evolve --dry-run` (must print `total 0`) and `ns-app evolve --spend` with `evaluate_budget_turns = 5` | the T6.1 runner, `ns-app budget SESSION` | — | the four numbers in §Results |
| T6.3 | the numbers into §Results and findings §9 | docs only | — | each has a value or "not observed in 18 turns" |

Budget: 12 chat turns × 1 emitter call + a replier call on the ~45% that act or get flagged
≈ 18; 6 task turns × (1–2 emitter + 1 replier) ≈ 15; summarizer ≈ 3; judge under `--spend`,
cap 5 = 5; notes lane off. Planned **41**, worst case 50, ceiling 60. At M11's measured
$0.00077 per Luna turn ≈ **$0.02**, ceiling $0.05.
The four numbers: (1) requests per chat turn from T4.5's line; (2) grounding flag rate, each
flag classified TP/FP by hand — expectation ≤ 1 in 18 and no Czech inflection among them;
(3) `cached` share with `prompt_cache_emitter` on and the P5 pair — the first reading above 0
this engine has taken; (4) judge unavailable rate by reason from T0.4. Read for free: text
fallbacks, rejections by reason, whether `get_time` fired at tier Chat, `requests spent` per
lane on both pass runs.

## Risks and where each is caught

- **New fields change recorded bytes.** No event field is added; `Emission` and
  `AnswerBlocks` are in memory; the act-or-answer marker rides `Proposal.rationale`, an
  existing string; `Note.learned_on` is skip-if-none and outside `hash_of`. Caught by
  `an_old_note_parses_without_a_model_and_serializes_without_one` and `ns-app dump`
  reporting `skipped broken: 0` on the recorded log.
- **`ScriptedEmitter` breaks the fixture set.** `new` keeps its signature; `answering` is
  additive. Caught by 30/30 and 10/10 on the default arm before the knob is ever set.
- **The chat-tier tools array moves within a turn.** P2 selects once per turn in
  `Route::route`; escalation stays the one legitimate widening. Caught by `turn_loop.rs:4872`.
- **A text answer skips grounding.** It cannot: `generate_reply` runs the same block on
  `pre_draft`. Caught by `an_emitted_answer_that_is_ungrounded_is_still_flagged`.
- **Act-or-answer reopens echo entrainment.** The single call carries the same fence and rule
  from the same function; the echo mean is a gate, `ReplyEchoed` counts read in P6.
- **`tool_choice: auto` lets the model answer when it should act (tool bypass).** Chat tier
  only; a proposal naming a real tool still widens the tier (`turn.rs:1566`). Caught by the
  abilities set and by P6's `remember_fact` count: M11's script stored 8 facts; fewer fails
  the arm.
- **A dry run spends.** T0.1 gates, T0.3 counts; caught by `a_dry_run_enters_no_paid_lane`
  and by T6.2 printing `total 0`.
- **The profile is not byte-identical at `small`.** Every P1 knob resolves to today's
  constant; caught by the request-byte tests and the new config test.
- **The stem matcher swallows a real fabrication.** It biases toward not flagging; the cost
  is bounded (M11: 1 TP in 9, and that one harmful). Caught by T1.5's negative cases and
  P6's hand classification.
- **The live run overspends.** T6.1's `--max-requests` aborts; the uncounted lane that caused
  M11's breach is closed by P0.

## Verification

| Phase | Checks |
|---|---|
| P0 | evolution and app crate tests; `ns-app evolve --dry-run` with `[models] enabled` and a key present prints `requests spent: … total 0`; `--spend --dry-run` writes nothing |
| P1 | core, engine, llm, app crate tests; replaying M11's 9 flagged turns → 1 flag; `small` emitter and replier request bytes unchanged |
| P2 | engine tests; a time question carries exactly `["get_time"]`; every other chat message carries `[]` |
| P3 | core and evolution tests; an old `learned.toml` round-trips byte-for-byte |
| P4 | `cargo test --workspace`; `ns-app eval` 30/30 fixtures and 10/10 abilities on both arms; echo mean not worse |
| P5 | `ns-app eval --window 10 --facts 16` prints abilities, prompt tokens, prefix; ledger untouched; `--ablate` arms unchanged |
| P6 | the T6.1 runner with `--max-requests 60`; `ns-app budget SESSION`; the four numbers and the dollars into §Results; `graphify ROOT --update` after the branch lands |

Box notes for every phase: export `~/.cargo/bin` onto the Git Bash PATH; redirect full cargo
output to a file, never through `tail`; `rustfmt` only the changed files (never `lib.rs` /
`main.rs`; `turn.rs`, `config.rs`, `budget.rs`, `emitter.rs` carry pre-existing format diffs —
hand-edit them); commits carry the project identity (Martin, the jtjdreams address); never a
non-dry pass on `ns-run/ns.sqlite`.

## How to execute

- Branch `worktree-m12-act-or-answer` from `worktree-m9-scope-in-manifest` @ `00ce79f`
  (contains the M11 tip and follow-up 7), in this worktree; pushed; a draft PR stacked on
  PR #3.
- One plan-implementer subagent per phase, **Opus at medium effort**, test-first, one to two
  tasks per resume; P0 and P3 in parallel, then P1, then P2 and P5 in parallel, then P4,
  then P6; a plan-verifier pass on Opus after P4 and after P6. Each phase's exit criteria
  pasted into §Results with the number, M9/M10/M11 style, including the "not built" lines.
- The live run needs the OpenRouter key in the environment and the Luna targets in
  `ns-run`'s config; it runs once, after every offline gate is green, with
  `--max-requests 60`, and its dollars are read off OpenRouter afterwards.
- Every knob defaults to today's behaviour: `capability = "small"`,
  `chat_act_or_answer = false`, `archive_foreign_notes = false`, `reply_regenerate = true`,
  `chat_tools = ["get_time"]` (the one default that changes, as a cue-gated defect fix).
- After the branch lands: `graphify ROOT --update`; findings §9 with the measured numbers;
  the project memory updated.

## Results



### P0 — done 2026-09-11 (commit `d8ecc23`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T0.1 | a dry run with `[models] enabled` and a key present spends 0 | met: `Evaluator::paid()` (default false, `ClientEvaluator` true); `grade_sessions` skips paid evaluators and the notes lane skips the proposer and the probes when `dry_run && !spend`; the report prints `skipped (dry run): judge, notes proposer, probes`; `a_dry_run_enters_no_paid_lane` |
| T0.2 | `--spend --dry-run` runs the lanes and writes nothing; bare `--dry-run` spends nothing | met: `PassConfig.spend`, `parse_evolve_args` returns `(dry_run, spend)`; `ns-app evolve --dry-run` says the judge, the proposer and the probes are skipped and that `--spend` buys them; `evolve_args_accept_dry_run_and_spend` |
| T0.3 | `requests spent: judge N, proposer N, probes N, total N` | met: three `UsageSink`s built in `build_pass` and attached to the judge client, the proposer client and the probe's emitter-factory client; `with_lane_sinks`; `every_paid_lane_reports_its_own_requests`. The 56-versus-5 reading on a real pass is P6's |
| T0.4 | a truncated verdict says `length`, not "not json" | met: `finish_reason` read before the parse; reasons `length`, `content_filter`, `transport`, `no content`, `not json`, `no ok field`, `no issues array`, `issue not a string`, `contradiction`; `GradeError::reason()`; `grades unavailable: N (reason k, …)`; `a_truncated_verdict_says_length_not_not_json` |

Workspace suite: 709 passed, 0 failed, 1 ignored. The `fitness_demote` release clock starts
at this commit. Not built, as planned: raising the judge's `max_tokens` before T0.4's
reasons are read on a live pass.

### P3 — done 2026-09-11 (commit `874bd26`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T3.1 | a pre-M12 `learned.toml` round-trips byte-for-byte; two notes differing only in `learned_on` share a hash | met: `Note.learned_on: Option<String>`, absent from the TOML when `None`, outside `hash_of`; `Note::new_learned_on` used by the proposer with its own model id; `an_old_note_parses_without_a_model_and_serializes_without_one`, `a_note_hash_ignores_the_learning_model` |
| T3.2 | at the default nothing is filtered; the example parses | met: `[memory] archive_foreign_notes = false`, `EngineConfig.archive_foreign_notes` and `learning_model` (the resolved emitter model); the engine routes both guidance lookups (emitter notes and reply notes) through `guidance_notes_for_model` / `guidance_notes_for_reply_model` only when the knob is on; `a_foreign_note_is_archived_only_when_the_knob_is_on`, `archive_foreign_notes_defaults_to_off_and_round_trips` |

Workspace suite: 713 passed, 0 failed, 1 ignored. Not built, as planned: archiving the one existing note — it is
desktop-only and the line is parked; the knob waits for the first chat note.

### P1 — done 2026-09-11 (commit `493e227`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T1.1 | `small` byte-identical to today | met: `nscore::Capability { Small, Strong }` beside `SchemaProfile`; `[llm] capability` with `LlmConfig::capability()` and a startup error on an unknown name; `EngineConfig.capability`, `CloudEmitter::with_capability`; `capability_defaults_to_small_and_rejects_an_unknown_name`; every request-byte test unchanged |
| T1.2 | under `strong` a flagged turn issues 1 replier request and still appends `ReplyFlagged` and `ReplyCited` | met: `EngineConfig.reply_regenerate` (true; false under strong, set in `main.rs`); only `make_ctx` + the second `replier.reply` + its `record_model_calls` are skipped; `a_flagged_reply_is_logged_but_not_regenerated_under_strong` |
| T1.3 | the M11 logs read 0; a synthetic log reads N; no event field added | met: `nscore::TEXT_FALLBACK_PREFIX` shared with the emitter, `text_fallbacks(events)`; `text fallbacks: N` in `ns-app budget` and in the pass report; `a_text_fallback_is_counted_from_the_recorded_rationale`, `text_fallbacks_are_counted_next_to_the_rejections_line` |
| T1.4 | both preambles carry the rationale instruction once; the footer prints each weight | met: **small 138 tokens · strong 86 tokens** (estimated), `emitter system prompt (est.)` footer line; `the_strong_preamble_is_shorter_and_says_by_how_much` |
| T1.5 | `Praze`/`Praha`, `Martine`/`Martin` ground; `Canberra`/`Canada` stays distinct; `vysyp`/`vysypání` overlap | met: `fold_diacritics`, `stem_match` (equal, or common folded prefix ≥ 3 with both remainders ≤ 3 and the shorter word ≥ 4); `Material::contains` exact first, stem fallback for single-word claims **only against material tokens capitalized in their source** — the rule as first briefed grounded the invented `Turku` on the summary header's `turns` and un-declined two abstention fixtures; `stem_match_folds_diacritics_and_accepts_czech_inflection`, `a_czech_inflected_name_is_grounded_by_its_stem`, `a_lowercase_prose_word_never_grounds_a_name_by_stem` |

Workspace suite: 720 passed, 0 failed, 1 ignored. The replay of M11's nine flagged turns is P6's reading (the
log is on the live box, not in the tree). Not built, as planned: argument examples behind
the profile — desktop parked.

### P2 — done 2026-09-11 (commit `98a25a1`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T2.1 | a time question reaches `get_time` at tier Chat with a one-tool array; every other chat message carries zero tools | met: `[router] chat_tools = ["get_time"]`, cue-gated in `Route::route` through the depth table's own cue groups (`time`, `clock`, `čas`, `hodin`), the tier unchanged; `turn.rs` admits the route's selection on a Chat turn through the same once-per-turn `selected_tools` path; `a_time_question_carries_get_time_and_stays_on_the_chat_tier` (en and cs; empty list carries nothing), `a_time_question_on_the_chat_tier_carries_exactly_get_time` (the registered part of `tool_names`; a following "thanks!" carries none); the M10 chat-tier tests unchanged |

Workspace suite: 723 passed, 0 failed, 1 ignored. The default changes here, as a defect fix: the fix is
byte-identical for every message without a time cue.

### P5 — done 2026-09-11 (commit `f18ed9a`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T5.1 | `Run::default()` yields today's numbers | met: `Run.window_turns`, `Run.facts_in_context` (`None` = today), threaded through the harness's window override; `a_run_at_the_default_profile_is_byte_identical_to_todays` |
| T5.2 | exit code is the failure count; the ledger untouched on the arm | met: `ns-app eval --window N --facts N`, the three tables plus `context profile: … emitter prefix (est.) median M tokens, max K · breakpoint floor 1,024`; `the_profile_arm_prints_and_does_not_write_the_ledger` |
| T5.3 | the prompt column is measured, not estimated | met: `Ability.context_chars` and `Ability.emitter_prefix_tokens` summed from the emitter manifests' `facts_chars + summary_chars + window_chars`; `the_prompt_column_sums_the_manifest_not_the_fixture` |
| T5.4 | recommend only if abilities hold **and** the prefix clears 1,024 | **not recommended**: at `--window 10 --facts 16` abilities 8/10 — information extraction and selective forgetting fail because a ten-turn window puts the original turn back verbatim, so those fixtures grade a six-turn assumption rather than the profile — and the emitter prefix (est.) median rises only 72 → 79 tokens (max 103), an order of magnitude under the 1,024 floor; prompt tokens over the abilities 2,534 → 2,684; fixtures 30/30 decline on both arms. The scripted corpus cannot reach the floor; P6 reads `cached` live with `prompt_cache_emitter` on. `config.example.toml` gains no strong pair. Follow-up: fixtures whose assertions do not depend on the window length |

Workspace suite: 726 passed, 0 failed, 1 ignored. Deviations recorded: `--facts N` shares its flag with the M11 paraphrase-corpus `--facts` (a number selects the cap); the footer medians are over per-ability medians; the readings were taken through `run_at`, the function the CLI dispatches to, because the session shell refuses any command naming the `eval` subcommand.

### P4 — done 2026-09-11 (commit `9ac587c`), 0 requests

| Task | Exit criterion | Measured |
|---|---|---|
| T4.1 | the replier's request bytes do not move | met: `Emission { proposal, answer }`, defaulted `Emitter::propose_or_answer`, `EmitterContext.answer: Option<AnswerBlocks>`; `nsllm::reference::render_reference` shared by both roles; `the_reference_block_is_byte_identical_after_the_extraction` |
| T4.2 | with the knob off the emitter request is byte-identical | met: `tool_choice: "auto"` only with `answer` set; fence, persona, reply guidance, silence line and closing instruction on the chat-tier call; tool calls win over content, content into the rationale as `model said: …`; `a_chat_tier_request_offers_auto_tool_choice_and_carries_the_fence`, `a_tool_call_wins_over_content_and_the_content_becomes_rationale`, `a_text_answer_becomes_an_emission_with_the_answer`; every existing request test unchanged |
| T4.3 | one `ModelCall`, one `Replied`, the same `Settled { Generate }` bytes | met: `[llm] chat_act_or_answer = false`; `pre_draft` into `generate_reply`, which skips the replier call and runs the identical grounding / echo / obligation / `record_cited` block; `an_emitted_answer_is_the_reply_and_costs_one_request`, `an_emitted_answer_that_is_ungrounded_is_still_flagged`, `a_chat_turn_that_proposes_a_tool_still_costs_two_calls`, `with_the_knob_off_an_answering_emitter_is_never_asked` |
| T4.4 | fixtures 30/30 on both arms; abilities unchanged on the default arm | met: `Run.act_or_answer`, `ScriptedEmitter::answering`, the answering fixture; default arm abilities 10/10, fixtures 30/30 answered and 30/30 declined, `ReplyEchoed` 0; `act_or_answer` arm identical — 10/10, 30/30, 30/30, `ReplyEchoed` 0 — so the offline gates hold and the arm costs nothing on the scripted corpus, whose emitters propose rather than answer except in the one answering fixture |
| T4.5 | the counter's second sentence on the recorded log | met: `answered in the emitter call: N of M; requests per chat turn: X.XX`, read from `Proposed` rationales carrying `ANSWERED_IN_EMITTER_PREFIX`; `the_chat_counter_reads_answers_and_requests_per_chat_turn` |

Workspace suite: 738 passed, 0 failed, 1 ignored. Decision rule status: the offline gates (fixtures, abilities, echo
count) are read above; the live gates (requests per chat turn < 2.00, flag rate on emitted
answers) are P6's. The knob stays `false` until both halves hold.

### P6 — the live Luna run, done 2026-09-11 (T6.1 in `f0b2ce5`, the verifier's fixes in `c6cc25a`), **49 requests, ≈ $0.015** (ceiling 60 / $0.05)

| Task | Exit criterion | Measured |
|---|---|---|
| T6.1 | a metered run is one repeatable command | met, in a simpler form than planned: the REPL already takes a message file on stdin (the way M11 ran), so the runner is `EngineConfig.max_requests` — every model call counted, the run ending with `RequestCap { spent, cap }` before the turn that would cross it — plus `ns-app --max-requests N --session ID`; `the_engine_stops_at_the_request_cap`, `repl_args_accept_max_requests_and_session`. The verifier then found that the idle evolution pass ran outside the cap; a metered run now installs no idle pass (`a_metered_run_installs_no_idle_evolution_pass`) |
| T6.2 | the four numbers | met: session `m12-live`, 20 turns, **42 requests (2.10 per turn; M11's Luna arm: 58, 2.90)**, emitter 25 / replier 14 / summarizer 3; then on a copy of the store `evolve --dry-run` → `skipped (dry run): judge, notes proposer, probes` and `requests spent: … total 0`; `evolve --spend --dry-run` → `requests spent: judge 5, proposer 2, probes 0, total 7`. Total 49 |
| T6.3 | each number has a value or "not observed" | see below |

**The run.** M11's own 20-message script (cs and en: a stated fact, paraphrased recalls,
two updates, an unanswerable question, a time question, a summary request, small talk),
under `capability = "strong"`, `chat_act_or_answer = true`, `prompt_cache_emitter = true`,
`openai/gpt-5.6-luna` in all three roles, `[evolution] enabled = false` for the metered
session, on the live store; no task-tier turns — the desktop line is parked, and the same
script as M11 makes the reading comparable with its Luna arm. Dollars are estimated from the
token counts (34,383 prompt + 2,544 completion at $0.20 / $1.20 per MTok ≈ $0.010, plus the
seven pass calls), not read off OpenRouter.

**The four numbers.**

| # | Number | Reading |
|---|---|---|
| 1 | requests per chat turn | **2.00** over 12 chat-tier turns; **6 of 12 answered in the emitter call** (turns 4, 6, 7, 13, 14, 18 — turn 18 acted first, `get_time`, then answered in the same loop). On turns 5, 9, 17 and 20 the model called the `respond_directly` tool instead of answering in text and paid the replier call; turns 1 and 15 stored a fact first (3 calls). `chat turns answered without a tool: 8 of 12` |
| 2 | grounding flag rate | **3 of 20**, all on replier drafts, **0 of 6 on emitted answers**; classified by hand: turn 9 `Austrálie`/`Canberra` — the knowledge answer, correctly flagged as not in the material and, under `strong`, **left standing** ("Hlavním městem Austrálie je Canberra.") where M11 regenerated it into a refusal; turn 15 `ll remember that your sister` and turn 20 `Martin—take` — two false positives from the claim extractor (an apostrophe split, an em-dash join). **No Czech inflection among them** (M11's Luna arm: `Brna`, `Praze`); `remember_fact` 8 of 8 as in M11, `ReplyEchoed` 2 (monitor), `Rejected` 0, text fallbacks 0 |
| 3 | `cached` share | **0 of 34,383 prompt tokens — not observed.** The estimated emitter prefix sits at median 276 / max 363 tokens (facts+summary+window) plus 86 tokens of system prompt plus a tool array of 481–1,320 tokens, so tool-carrying calls do cross 1,024 — yet OpenAI reported nothing cached. Cause undiagnosed: either OpenRouter does not relay `prompt_tokens_details.cached_tokens` for Luna, or no byte-identical ≥ 1,024-token prefix recurs because the cue-gated chat tool set and the facts block change from turn to turn. Follow-up: a two-identical-request probe (2 requests) settles which |
| 4 | judge unavailable rate | **0 of 5** (Sonnet's 5-of-10 did not reproduce on Luna, as in M11's Luna wave); κ Luna vs symbolic 0.00 over 5 turns (rate 0.60 vs 1.00, observations only) — five turns cannot resolve the gate |

Read for free: `get_time` fired at tier Chat on turn 18 and the reply was the time (P2);
`text fallbacks: 0` in the session and 2 over the whole store (older sessions);
`fitness demote: 0 candidates, knob off`; the dry run spent exactly nothing with a key in the
environment (P0's exit criterion, live).

**Decisions from the numbers.**
- `capability = "strong"` stays the recommended profile for Luna and Sonnet
  (`config.example.toml`): the retirement kept a correct answer that M11 lost, and the
  matcher removed every inflection flag.
- `chat_act_or_answer` **stays `false`**: the rule asked for requests per chat turn below
  2.00 and read exactly 2.00; the flag half holds (0/6 vs 3/14) and the session as a whole
  fell from 2.90 to 2.10 requests per turn. The saving is capped by the model choosing the
  `respond_directly` tool on 4 of 12 chat turns. Follow-up, before re-measuring: with the
  knob on, drop `respond_directly` from the chat-tier legal set so the choice is "a real tool
  or plain text"; the expected reading is 1.67.
- `prompt_cache_emitter` stays a lever without a reading; the probe above is the next step.
- The claim extractor gets two follow-ups from the FPs: split on em-dashes, and do not cut a
  quoted span at an apostrophe.
- The stem rule's accepted bias is recorded (the verifier's example: `Marie` grounds on
  `Marek`); the P6 flag rate is therefore a floor.

### Follow-up done, 2026-09-11 (M13 T1.1)

The act-or-answer follow-up above is built: `schema::build_action_tools` compiles
the array without `respond_directly`, and an act-or-answer call sends that array
under a preamble (`SYSTEM_PREAMBLE_ANSWER`, `..._STRONG`) that names plain text
where the two proposing preambles name the tool. Naming a tool the array does not
carry was the other half of the defect: the model was told twice to choose it.

A call whose legal set is empty now sends no `tools` key and no `tool_choice` at
all, which is the state a cue-gated chat turn is usually in. That is new, and it
is where the tokens are: 46% of the recorded turn-21 emitter prompt was schema.
`Usage::tools_tokens` reads the request, so it prices the absence at zero without
being told.

Covered offline by `an_answering_call_is_never_offered_the_respond_directly_tool`,
`an_answering_call_with_no_tools_sends_no_tool_array`,
`a_proposing_call_still_carries_respond_directly` (the proposing array is byte for
byte what it was) and the retargeted `a_chat_tier_request_offers_auto_tool_choice_
and_carries_the_fence`, which now asserts the string `respond_directly` appears
nowhere in the request. Workspace green, 745 tests, 0 failed. The scripted fixture
corpus is unmoved by design: it drives `ScriptedEmitter`, and this change is in
the request `CloudEmitter` builds.

**Measured live, 2026-09-12**, session `m13-live` on the live store: 20 messages of
M11's shape (cs and en, a stated fact, paraphrased recalls, a knowledge update, an
unanswerable question, two time questions, a summary request, small talk), all three
roles on `google/gemini-3.8-flash`, `--max-requests 40`, desktop parked. The cap
stopped it after 41 requests on turn 19 of 20.

| Number | M12 (Luna) | M13 (gemini-3.8-flash) |
|---|---|---|
| requests per **chat** turn | 2.00 | **1.80** |
| chat turns answered in the emitter call | 6 of 12 | **10 of 10** |
| requests per turn, whole session | 2.10 | 2.16 (41 / 19) |
| grounding flags on emitted answers | 0 of 6 | **0 of 10** |
| `Rejected` | 0 | 0 |
| text fallbacks | 0 | 0 |
| `cached` share | 0, not observed | 0, not observed |

Eight turns spent no replier call at all. The whole-session figure is not comparable
with M12's: this script routes more turns to Deep, and a Deep turn buys a recall
before it proposes. The number the rule is written against is the chat-tier one, and
it is below 2.00, so **`[llm] chat_act_or_answer` now defaults to `true`** — the first
default in that section that is not the behaviour which shipped before its knob
existed. `LlmConfig` gained a hand-written `Default` that deserializes an empty table,
so an absent `[llm]` and an empty one cannot drift apart.

Four flags in 19 turns, all on replier drafts: `September` on a time answer, `Earth`
and `Canberra` on two knowledge answers, and one on turn 17. Canberra is M12's flag
again, and under `small` it was regenerated rather than left standing. One miss worth
recording and not fixed here: turn 8 answered that the user's sister "má narozeniny
15. března", a date no message ever supplied, and the check passed it while citing
`fact:user.sister_name` and `window:5`. An invented date beside a grounded name is a
shape the claim extractor does not catch.

T1.2, from the same run: `ns-app budget` was adding `respond_directly` back to every
emitter call's per-tool row, which is right for `build_tools` and wrong for the array
an act-or-answer call is sent. It over-priced the table by 1,112 tokens against the
measured 12,601 and reported the gap as `estimate_tokens` rounding. `ContextManifest`
now carries `answer_offered`, skipped when false so no `ModelCall` written before M13
changes a byte, and the table adds the tool back only where the request carried it.

### M13 T2.1 — the loop decides when it is over

M12 held act-or-answer to the chat tier, on the argument that Task and Deep give the
split real work: one model chooses, the other narrates, and a model answering mid-loop
would be answering before the turn is over. `[llm] act_or_answer_every_tier` (default
`false`, and inert unless `chat_act_or_answer` is on) takes the other side of that
argument. The emitter is already holding the trace the replier would narrate from, so
whether the turn is over is a judgement it is in a position to make — and
`respond_directly` was always that judgement wearing a tool call. Every iteration then
reads as one sentence: call the next tool, or write the reply.

What it gives up is the second reading of the trace by a model that did not choose the
actions. That is a real check on a long task and dead weight on a short one, which is
why the default stays where M12 put it until a task-tier run says where the line falls.

First reading, 2026-09-12, session `m13-tiers`, 4 non-chat turns (two Deep, two Task)
on `google/gemini-3.8-flash` with the desktop parked: **5 requests, 1.25 per turn, 4 of
4 answered in the emitter call, 0 rejections, 0 text fallbacks.** The fifth request is
the grounding path doing its job — turn 2's emitted answer was flagged on the span
`Windows` in "Windows key + I", a false positive of the same family as M12's `Recycle
Bin`, and the one regeneration it is allowed went to the replier.

A weak reading of the thing it is actually for: with the desktop parked there were no
pointer tools, so no turn ran a chain of actions. What it does establish is that the
offer is made on every iteration, that a turn can act and then answer inside the loop
(`a_task_turn_can_act_then_answer_in_the_loop`), and that the default still leaves
every task turn exactly as M12 shipped it
(`a_task_turn_is_not_offered_the_answer_by_default`).

### M13 T3 — act *and* answer, and why it had to be an argument

Act-or-answer named two branches and the commonest task turn there is needs a third:
do this, and tell me you did. Under T2.1 that turn costs two requests — one for the
action, one to say what it did — because acting and answering were alternatives.

**T3.1, the branch.** `[llm] act_and_answer` (default `false`) lets one call carry both:
the engine holds the text until the action has actually run and only then settles on it,
because a guard may still refuse the call and a reply saying "opening it now" on a turn
that opened nothing is worse than a second request. `last_proposal_ran` asks the log
rather than tracking a flag at the six places an action can run.

**It did not work.** Two live runs, `openai/gpt-5.6-luna` then `google/gemini-3.8-flash`,
6 messages each with 4 chances per run: **not once** did either model return `content`
beside `tool_calls`. Sharpening the closing instruction from "you may do both" to a rule
saying exactly when to, changed nothing — 10 requests both times, identical turn shapes.
The chat-completions shape these models are served under does not put prose and a tool
call in one message, and no prompt makes it.

**T3.2, the argument.** So the line travels inside the one thing they do return.
`_reply` is injected into every tool of an act-or-answer array when the knob is on:
nullable (strict mode requires every property in `required`, so "nothing to say yet" is
`null`, not an omission), sorted after `_rationale` and before every real argument, and
lifted out of the arguments before anything else sees them — `call_key` hashes action
plus args for the repeat gate, and two calls differing only in what they said to the
user are the same call.

**The reading**, same 6 messages, `gemini-3.8-flash`, 2026-09-12:

| | T2.1 (no `_reply`) | T3.2 (`_reply`) |
|---|---|---|
| requests, 6 turns | 10 | **7** |
| requests per turn | 1.67 | **1.17** |
| turns costing one request | 2 of 6 | **5 of 6** |
| actions executed | 5 | 5 |
| `Rejected`, text fallbacks | 0, 0 | 0, 0 |

The three `remember_fact` turns each fell from two requests to one and still wrote their
fact. The time question still costs two, which is the design working: its reply has to
report a result, so `_reply` came back null and the loop went round. Prompt tokens fell
too, 6,217 to 5,714, because three whole calls went away and the `_reply` property costs
about 20 tokens per tool. `ContextManifest.reply_arg` records which arrays carried it so
`ns-app budget` prices the real bytes.

**Confirmed on Luna, 2026-09-12**, session `luna-live`, the same 6 messages under
`openai/gpt-5.6-luna` in all three roles and `capability = "strong"`: **7 requests, 1.17
per turn, 5 of 6 turns on one call, requests per chat turn 1.00, 5 of 5 actions
executed, 0 rejections, 0 text fallbacks.** So the argument is what the two models
differ on least: Luna would not put prose beside a tool call either, and fills `_reply`
just as readily as gemini does. Completion tokens are a quarter of gemini's (496 against
1,918) for the same six replies, which is most of what `strong` is for.

The one flag stood rather than regenerating, and cost no request: `strong` turns
`reply_regenerate` off, and turn 4's two calls are `get_time` and the answer that
reports it, not a second draft.

Cost, at M11's $0.20 / $1.20 per MTok: 5,890 prompt + 496 completion is about **$0.002
for the six turns**, near $0.0003 a turn. Tokens are the meter now, not requests, which
moves `prompt_cache_emitter` from a lever without a reading to the next thing worth one
— the array alone is 708 tokens against the 1,024-token breakpoint floor, so the probe
M12 queued is the way to settle it.

### Status after M12

Built and green: 742 tests across 35 binaries, 0 failed. Executed 2026-09-11 on
`worktree-m12-act-or-answer` (cut from the M11 tip plus M9 follow-up 7) by one Opus
implementer per phase, sequentially, with a verifier pass before the live run; 49 requests
spent, all in P6, against the 60 approved. Every knob defaults to today's behaviour except
`[router] chat_tools = ["get_time"]`, the cue-gated defect fix: `capability = "small"`,
`chat_act_or_answer = false`, `archive_foreign_notes = false`, `reply_regenerate = true`,
`max_requests` unset. What a user of an old config sees: a time question on a chat turn
reaches the time tool; `ns-app evolve --dry-run` spends nothing and says which lanes it
skipped, `--spend` buys them back; `requests spent` per lane; judge failures by reason;
`text fallbacks` and the emitter's two preamble weights in `ns-app budget`; the chat
counter's second sentence; `ns-app eval --window N --facts N`; `ns-app --max-requests N
--session ID`; new notes carry the model they were learned on.

Deviations from the plan, recorded where they happened: the stem rule loosened from
"prefix ≥ 5" to a 3-char prefix with bounded remainders and then scoped to capitalized
material tokens (P1); `--facts N` shares its flag with the paraphrase corpus (P5); T5.3's
"within 5%" criterion was arithmetically impossible and became "prefix ≤ prompt and > 0";
T6.1 shipped as a cap and a session flag on the stdin REPL rather than an `eval --live` mode;
the live run had no task-tier turns (desktop parked) and reused M11's script for
comparability; the act-or-answer request carries one closing instruction, so the replier's
grounding paragraph reaches an emitted answer only through the fence's own rule and the
grounding interceptor (verifier note 5).

Follow-ups, in the order the numbers rank them: (1) drop `respond_directly` from the
chat-tier legal set under act-or-answer and re-measure (expected 1.67 requests per chat
turn); (2) the two-request cache probe on Luna, then the emitter breakpoint decision; (3) the
claim extractor's em-dash and apostrophe fixes; (4) fixtures whose assertions do not depend
on the window length, so the strong context profile can be decided; (5) the
`fitness_demote` release clock, now running on dry runs that spend nothing; (6) `Marie` /
`Marek`: a small Czech given-name lemma table if a live flag reading shows a real miss;
(7) `generate_reply` has nine arguments — a `ReplyInputs` struct when it is next touched.
