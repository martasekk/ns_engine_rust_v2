# Recall cost: three places where a turn pays the encoder for nothing

**Date:** 2026-11-26. **State:** approved 2026-11-27 with D1 to D4 as recommended; RC-1, RC-2 and RC-3 done and committed on `recall-cost`; not pushed, not merged. **Base:** `main` at `cbdc0ae`. **Rules:** `~/.claude/CLAUDE.md`, `~/.claude/rules/rust.md`.
**Step ids:** `RC-<n>`. **Branch for the work:** `recall-cost`, in the worktree
`.claude/worktrees/exciting-williamson-2b97b6` (commit per step after its gates pass; no push, no merge).
**Related:** `2026-11-26-retrieval-split-eval.md` (main checkout, uncommitted) measured these costs and only
measures; this plan changes the search path, so it is a plan of its own.

Hybrid recall (`[recall] hybrid = true`, Task and Deep tiers) reaches a model through `nscore::TextEncoder` in two
ways: it embeds a text and it reranks a list. The retrieval eval of 2026-11-26 took 2.36 s a question on the hybrid
arm and counted 788 embedding requests where 488 were expected, and 2 reranks a question where 1 was expected.

Done means: each fix the owner approves is committed on `recall-cost` with format, clippy and tests green and its
tests named in the Log; the call counts per turn before and after are in section 1.

Out of scope: the retrieval eval and its numbers; a second run of `ns-app eval-recall --remote=voyage` (it needs the
branch `remote-encoder`, which is not merged, and this plan merges nothing); the items of section 5.

**Not verified:** the time saved per question. The counts below are requests counted by a test double; the seconds
are the eval's own estimate from per-call times.

## 1. What was confirmed

Seven probe tests in `crates/engine/tests/recall_cost.rs` (committed with this plan in `aef7324`) run whole turns against
`SqliteStore` with an encoder that records every call. The store holds six facts, all with a vector; five are shown.
All seven pass on `cbdc0ae`, so the numbers in the "today" columns are what the shipped code does.

| Turn | Question embeddings today | Reranks today | Fact embeddings today | After RC-1 to RC-3 |
|---|---|---|---|---|
| Task tier, answers at once | 2 | 2 | 5 | 1, 1, 0 |
| Task tier, one tool call, then answers | 3 | 3 | 5 | 1, 1, 0 |
| Deep tier, with older turns to recall | 3 | 3 | 5 | 1, 2, 0 |
| Chat tier, `[recall] hybrid` on or off | 0 | 0 | 5 | 0, 0, 0 |

The three behaviours, as described in the task, with two corrections:

1. **A write that only counts a use is embedded again: confirmed.** `generate_reply`
   (`crates/engine/src/turn/reply.rs:74-79`) writes every shown fact back with `put_fact`, and
   `SqliteStore::put_fact` (`crates/memory-sqlite/src/lib.rs:1469`) calls `embed_fact` on every write.
   **Correction: this is not a hybrid cost.** It happens on every tier and with `[recall] hybrid` off, whenever the
   store has an encoder. A Chat-tier turn that shows five facts sends five embedding requests. The existing test
   `a_chat_tier_turn_issues_no_embed_call` passes only because its store holds no fact.
2. **The same search runs more than once a turn: confirmed, and it is more than twice.** `compose_prompt`
   (`crates/engine/src/turn/prompt.rs:90`) runs once for every emitter call, and each run searches; the reply
   (`reply.rs:74`) searches once more. A turn with one tool call makes three searches, each with the same question
   and the same facts. On the Deep tier the turn search of `recall_outcome` embeds the same question again.
3. **The budget does not end a slow call: confirmed.** `hybrid_fact_hits` and `hybrid_hits`
   (`lib.rs:654`, `lib.rs:735`) read the clock after `enc.embed` and after `enc.rerank` return. With a 50 ms budget
   and a call of 400 ms the search took 400 ms and then returned the word-match list. After a late embedding no
   rerank is started, so a search waits for one late call, not two.

Found while reading, not one of the three and not fixed here: a live fact that has no vector is missing from a
hybrid result even when word matching ranks it first (`lib.rs:711-713` builds the result from the rows that have a
vector). A fact written while the encoder is down is in that state until the idle pass embeds it. The probe
`probe_a_fact_with_no_vector_is_missing_from_a_hybrid_result` shows it. It matters for RC-1, see there.

## 2. Decisions for the owner

- **D1. Which fixes, in which order.** Recommended: all three, RC-1, then RC-2, then RC-3. RC-1 is the largest
  saving (five requests a turn on every tier) and the smallest change. RC-3 changes no average but bounds the worst
  turn, and it is better after RC-2 because a late answer then has somewhere to go.
- **D2. How a turn stops repeating the search (RC-2).** Recommended: the store keeps the encoder's last answers
  (the vector of a question, the order of a reranked list) and reuses one when the same input comes again. All of it
  is in `memory-sqlite`, no signature changes, and it covers every repeat in the table above. The other way, the one
  the task names first, is for the engine to hand the reply the facts the prompt showed. That removes one search of
  the two or three, changes three engine files, and changes what the reply counts as used in one case (an answer
  given beside an action that wrote a fact). It is left out; section 5 says when it would be worth doing.
- **D3. What happens to a call that runs past the budget (RC-3).** Recommended: the search stops waiting and the
  call finishes on its own. The other way is to cancel the call. Cancelling breaks a rule of the local client: it
  takes itself out after two calls in a row that got no answer (`crates/evolution/src/local.rs`,
  `give_up_after`), and a cancelled call never reports. With the service off, today's cost is 8 s once; with
  cancelling it would be 800 ms for every search of every turn, for as long as the process runs.
- **D4. Two gates are already red on `cbdc0ae`, in code these steps do not touch.** RR-0 and RR-0b of the rules
  rebuild own both.
  - Format: `cargo fmt -p ns-memory-sqlite -- --check` reports 14 places and `-p ns-engine -- --check` reports 6.
  - Clippy with `-D warnings`: `large_enum_variant` on `EventKind` (`crates/core/src/event.rs`), which stops the run
    before it reaches the two crates; and, with `--no-deps`, `single_element_loop` at
    `crates/engine/tests/turn_loop.rs:3866`. Nothing else in the two crates is reported.

  Recommended: leave all of it. A step's gate is then: the lines it adds or changes are formatted and the two counts
  stay 14 and 6; `cargo clippy -p ns-memory-sqlite -p ns-engine --all-targets --no-deps --keep-going -- -D warnings`
  reports that one line of `turn_loop.rs` and nothing else. The other choice is to fix the two clippy lines first, as
  a step of their own; the one in `turn_loop.rs` is an edit to an existing test, so it needs the owner's word.

## 3. Steps

Every step: test first, then the change; the format and clippy gates as D4 settles them;
`cargo test -p ns-memory-sqlite -p ns-engine`; one commit. After RC-3, once: `cargo test -p ns-testkit -p
ns-evolution -p ns-app`, the three crates that open a `SqliteStore` with an encoder. No new dependency, no public
item, no change to `MemoryStore` or `TextEncoder`. The engine is not changed at all: its part is the test file.

### RC-1. `put_fact` embeds only a text that is new

File: `crates/memory-sqlite/src/lib.rs`.

Signatures (all private):
- new `enum FactText { Changed, Unchanged }`: what a write did to the text a vector is made from;
- `write_fact(&self, fact: &Fact) -> Result<(), StoreError>` becomes `-> Result<FactText, StoreError>`. It is
  `Changed` for a new version and for a write in place with another value, `Unchanged` for a write in place with the
  same value;
- `embed_fact(&self, fact: &Fact)` becomes `embed_fact(&self, fact: &Fact, text: FactText)`. It returns without
  calling the encoder when the text is unchanged and the row has a vector for the encoder's model.

Behaviour that changes: a write that moves only `uses`, `last_used`, `confidence`, `last_validated`, the state or
another counter sends no request. Behaviour that stays: a new version is embedded; a changed value is embedded and
replaces the old vector; a fact with no vector for this model gets one on its next write; the old vector stays in
place until the new one arrives, so a fact is never left without one (see the finding at the end of section 1).

Tests, in the store's own test module with a recording encoder:
- `a_write_that_only_counts_a_use_is_not_embedded_again`;
- `a_changed_value_is_embedded_again` (in place and as a new version);
- `a_fact_with_no_vector_for_this_model_gets_one_on_its_next_write` (written with the encoder down, and after a
  change of model).
In `crates/engine/tests/recall_cost.rs`: `a_turn_embeds_no_fact_whose_text_did_not_change`, on the Task and the Chat
tier (the probes of today's count, turned to 0).

### RC-2. One embedding and one rerank for one question

File: `crates/memory-sqlite/src/lib.rs`.

Signatures:
- new private `struct MemoEncoder` that holds the encoder it was given and implements `nscore::TextEncoder`. It
  keeps the last 32 question vectors and the last 32 rerank answers. A rerank answer is found again only for the
  same question, the same documents in the same order and the same `k`. Passage embeddings and failed calls are
  never kept;
- `SqliteStore::with_encoder` (public, signature unchanged) wraps what it is given.

Behaviour that changes: the second and later identical calls are answered from memory, also across turns, until 32
newer ones have pushed them out or the process restarts. A fact written between two searches changes the documents,
so the rerank is asked again. Nothing is kept on disk, and the kept rerank entries hold a hash of the documents, not
their text. Behaviour that stays: what every search returns.

Tests, in the store's test module:
- `the_same_question_is_embedded_once`;
- `the_same_list_is_reranked_once_and_a_changed_fact_is_reranked_again`;
- `a_failed_call_is_not_kept`.
In `recall_cost.rs`: `a_task_turn_embeds_the_question_once_and_reranks_once` (answering at once, and with one tool
call) and `a_deep_turn_embeds_the_question_once`.

### RC-3. A search stops waiting when its budget is spent

File: `crates/memory-sqlite/src/lib.rs`.

Signatures (all private):
- `hybrid_fact_hits` and `hybrid_hits` take `enc: &Arc<dyn TextEncoder>` instead of `&dyn TextEncoder`;
- new `async fn within<T>(deadline: tokio::time::Instant, what: &'static str, call: impl Future<Output =
  Result<T, StoreError>> + Send + 'static) -> Result<T, StoreError>`: it starts the call on its own task and waits
  for it until the deadline;
- the two `started.elapsed() > budget` checks after each call go; the deadline replaces them.

Behaviour that changes: a search returns the word-match list at the budget (800 ms) and no later. The late call is
not cancelled: it ends by the client's own timeout, the client's two-failures rule still sees it, and after RC-2 its
answer is kept for the next search of the same question. An encoder that panics now costs the search its hybrid
result and no longer takes the turn down. Behaviour that stays: every result; a search that finishes inside the
budget is untouched.

Tests, in the store's test module:
- `a_slow_embedding_does_not_hold_a_search_past_its_budget` and the same for a slow rerank, for facts and for
  turns (a 2 s call, a 50 ms budget, the search back in under 1 s);
- `a_late_answer_is_kept_for_the_next_search`.
The existing `a_recall_over_the_rerank_budget_falls_back_to_lexical` stays as it is.

## 4. Expected effect on the eval's hybrid arm

Estimates, not measured. Embedding requests for 60 questions: 788 to 488. Rerank requests: 121 to 61. Per question:
five single-fact embeddings and one of the two searches go, which the eval's own estimate puts at about 1.3 s and
about 0.5 s of the 2.36 s. Checking it needs `ns-app eval-recall --remote=voyage` on `remote-encoder` rebased onto
these commits.

## 5. Left out

- **The reply reusing the prompt's facts** (the other way of D2). Worth doing if the reply's bookkeeping should
  describe exactly what the emitter saw: today, and after this plan, the reply searches again and can get another
  list than the prompt did when one of the two searches ran past the budget. It then counts a use on facts the
  emitter never saw. A change to `prompt.rs`, `run.rs` and `reply.rs`.
- **The fact with no vector that a hybrid search drops** (end of section 1). A bug of its own in
  `hybrid_fact_hits`.
- **`nearest_digests` has no budget at all** (`lib.rs:1322`). Exemplars are off in every deployment
  (`exemplars_max = 0`).
- **One store call to count a use on several facts.** After RC-1 the five writes are local SQL only.

## 6. Risks

- RC-2 and RC-3 touch the functions that RR-8 (split of `memory-sqlite/src/lib.rs`), RR-9 and RR-10 of the rules
  rebuild will move. Nothing there has started; whichever lands second rebases.
- RC-3's tests measure time. They use a 2 s call against a 50 ms budget and pass under 1 s, so a loaded machine has
  a wide margin, but they are the tests here most likely to fail for a reason that is not the code.
- RC-3 runs encoder calls on spawned tasks, so the store's hybrid path needs a tokio runtime with timers. Every
  caller has one today (`#[tokio::main]`, `#[tokio::test]`; no hand-built runtime in the workspace).

## Log

- 2026-11-26: read the code at `cbdc0ae`; wrote the seven probes; all pass (section 1). Baseline on `cbdc0ae` for
  the two crates, target folder `C:/Users/Acemagic/cargo-targets/recall-cost`: `cargo test -p ns-memory-sqlite -p
  ns-engine` green (29 in the store; 74, 10, 4 and 96 in the engine, plus the 7 probes); format and clippy red in
  older code as D4 lists. Not run: the tests of `ns-testkit`, `ns-evolution` and `ns-app`.
- 2026-11-27: the owner approved D1 to D4 as recommended. **RC-1 done.** `write_fact` returns `FactText`
  (in-place writes compare the stored and the new value as JSON values); `embed_fact` skips an unchanged text when
  the row has a vector for the encoder's model. New store tests: `a_write_that_only_counts_a_use_is_not_embedded_again`,
  `a_changed_value_is_embedded_again`, `a_fact_with_no_vector_for_this_model_gets_one_on_its_next_write`. In
  `recall_cost.rs`: the Chat probe became `a_turn_embeds_no_fact_whose_text_did_not_change` (Task and Chat tiers),
  `probe_put_fact_embeds_a_write_that_only_moves_a_counter` became its inverse
  `a_write_that_only_moves_a_counter_sends_nothing`, and the Task and Deep probes' fact count went from 5 to 0 (their
  question and rerank counts are unchanged until RC-2). Known limit, written on `embed_fact`: a write in place with
  another value whose embedding fails keeps the old vector, and a later unchanged write does not replace it (a new
  version is a new row and is not affected). Gates: `cargo test -p ns-memory-sqlite -p ns-engine` green (32 in the
  store; 74, 10, 4, 7 and 96 in the engine); format still 14 and 6 places, none in new code; clippy reports only
  `turn_loop.rs:3866`.
- 2026-11-27: **RC-2 done.** `with_encoder` wraps the encoder in the private `MemoEncoder`: the last 32 query
  vectors (a single-text `embed(.., "query")`) and the last 32 rerank answers (keyed by question, a hash and the count
  of the documents in order, and `k`), in memory, oldest dropped first; passages and failed calls go straight through.
  New store tests: `the_same_question_is_embedded_once`,
  `the_same_list_is_reranked_once_and_a_changed_fact_is_reranked_again`, `a_failed_call_is_not_kept`,
  `the_memo_keeps_the_last_answers_only`; the test encoder gained a rerank counter, a fail-next count and separate
  embed and rerank delays (new fields; no existing assertion changed). In `recall_cost.rs` the two Task probes became
  `a_task_turn_embeds_the_question_once_and_reranks_once` (at once and with one tool call: 1, 1, 0) and the Deep probe
  became `a_deep_turn_embeds_the_question_once` (1, 2, 0). Gates: tests green (36 in the store; 74, 10, 4, 6 and 96 in
  the engine); format 14 and 6; clippy only `turn_loop.rs:3866`.
- 2026-11-27: **RC-3 done.** `hybrid_fact_hits` and `hybrid_hits` take `&Arc<dyn TextEncoder>` and set a
  `tokio::time::Instant` deadline at their start; each embed and rerank runs through the new private `within`, which
  spawns the call and waits until the deadline. The two `started.elapsed() > budget` checks are gone. New store tests:
  `a_slow_embedding_does_not_hold_a_search_past_its_budget`, `a_slow_rerank_does_not_hold_a_search_past_its_budget`
  (facts and turns each, a 2 s call on a 50 ms budget, back under 1 s), `a_late_answer_is_kept_for_the_next_search`.
  `a_recall_over_the_rerank_budget_falls_back_to_lexical` is unchanged and passes. In `recall_cost.rs`
  `probe_a_slow_call_holds_the_search_past_its_budget` became its inverse
  `a_slow_call_does_not_hold_the_search_past_its_budget` (2 s call, back under 1 s). The only probe left as it was is
  `probe_a_fact_with_no_vector_is_missing_from_a_hybrid_result` (section 5). Gates: tests green (39 in the store; 74,
  10, 4, 6 and 96 in the engine); format 14 and 6; clippy only `turn_loop.rs:3866`.
