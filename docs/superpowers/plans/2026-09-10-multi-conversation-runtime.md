# Many conversations at once — implementation plan

Date: 2026-09-10 · From `docs/research/2026-09-10-multi-conversation-runtime-findings.md` ·
Branch `worktree-research-multi-conversation`, base `main` @ `206ea0d`.

The findings doc decided the shape: Temporal's per-execution serialization and worker
slots, in-process, with the engine's own log as the only history. This plan sequences
it so that each phase is green on its own and the CLI behaves identically after every
one. Two hazards get a test before any task is spawned (findings §2.2, §2.6).

## 0. Rules for every phase

- Target is `x86_64-pc-windows-gnu`; `cargo` lives in `~/.cargo/bin` and is not on the
  Git Bash PATH. `cargo test --workspace` stays green at the end of every phase; write
  the full output to a file and read the counts from it — never pipe through `tail`.
- `rustfmt` only on the files actually changed (it follows `mod` trees).
- Implementers do not commit; the reviewer does, one commit per phase.
- Every behavioural claim in this plan has a named test. A phase whose tests do not
  exist is not done.
- Nothing here changes what a turn *does*. Replay (`replay.rs`, `verify_patch`) must
  produce the same events for the same recording before and after.

---

## Phase 0 — pin and measure

**T0.1 `&self`.** `Engine::run_turn(&self)`, `Engine::maybe_summarize(&self)`, and any
private helper they drag along. `run` / `run_loop` stay `&mut self` until Phase 2.
Callers need no change (a `&mut` binding still calls `&self`). If it does not compile
as `&self`, stop and report what holds a mutable borrow — the findings said nothing
does (§2.3), and this is where that is checked.

**T0.2 Two sessions, one channel.**
`two_sessions_interleaved_through_one_channel_keep_separate_intact_logs` in
`crates/engine/tests/turn_loop.rs`: a scripted channel yields messages for sessions
`"a"`, `"b"`, `"a"`, `"b"`, then closes; after `run()` each session's log has exactly
two turns, `verify_chain` passes on both, and every `UserSaid` text is in the log of
the session it was sent to. Passes today, serially. It is the regression guard for
Phase 2.

**T0.3 The hazard, reproduced.**
`two_overlapping_turns_on_one_session_lose_the_second_silently`: with `run_turn(&self)`,
`tokio::join!(e.run_turn(a1), e.run_turn(a2))` for one session, with a scripted emitter
(or replier) that `tokio::task::yield_now().await`s before answering so both turns load
the log before either appends. Assert: both calls return `Ok` with a reply; the store
holds exactly one turn's events; `verify_chain` passes; the second message's `UserSaid`
is nowhere in the store. The doc comment cites findings §2.2 and says Phase 2 inverts
this test. This is the reason the dispatcher exists, and the test must fail loudly the
day someone "fixes" `append`.

**T0.4 Measure `load` + `fold`.** A scratch cargo project *outside the repo*
(`$CLAUDE_JOB_DIR/tmp/measure-fold`) depending on the worktree's `ns-engine` and
`ns-memory-sqlite` by path. Copy `C:\Users\Acemagic\ns-run\ns.sqlite` to the scratch
dir — never open the live file — and time, for session `cli`, 200 iterations each of
`MemoryStore::load` and `fold(&events)`. Report median and p95 in microseconds and the
event count. Nothing from this lands in the repo except the numbers in §Results.

**Exit.** Workspace green; T0.2 and T0.3 pass; §Results has the T0.4 numbers.

---

## Phase 1 — attribution travels with the call

**Why.** One process-wide `UsageSink` (`app/src/main.rs:532`) drained after each of the
engine's calls (`turn.rs:807-814`) assumes calls never overlap. Under concurrency
session A's tokens land on session B's `ModelCall` (findings §2.6).

**Design.** `EmitterContext`, `ReplyContext` and `SummaryInput` each gain
`usage: Option<Arc<UsageSink>>` — *the sink this call records into*. `run_turn` creates
one `UsageSink` per turn, puts it in every context it builds, and `record_model_calls`
drains that sink and nothing else. `maybe_summarize` does the same for its own call.
The `crates/llm` client records into the context's sink when one is present and
otherwise into the sink it was constructed with (`with_usage_sink`, `client.rs:87`) —
that fallback keeps the evolution pass's probes and any caller outside a turn working
unchanged. `EngineConfig.usage` becomes unnecessary for the chat path; remove it if
nothing else reads it, and say so. `core` gets no new dependency (a task-local would
need tokio in `core`, which is why this is explicit).

**P1.1 Test.** `usage_from_two_overlapping_turns_lands_on_their_own_model_calls`: two
sessions, `tokio::join!` on two `run_turn`s, a scripted emitter and replier that record
a `Usage` into the context's sink with a distinguishable `model` string per session and
`yield_now` between; each session's `ModelCall` events carry only its own records.

**P1.2** Existing `ns-app budget` tests and `crates/llm` sink tests unchanged and green.

**Exit.** Green; no `drain()` of a process-wide sink inside `run_turn`.

---

## Phase 2 — the dispatcher

**D2.1 `Channel` by shared reference.** `recv(&self)` and `send(&self)`; implementors
use interior mutability (`tokio::sync::Mutex` around the reader and the writer; an
atomic for `WithDesktop::last_from_desktop`). `HarnessParts.channel` becomes
`Arc<dyn Channel>`; `HarnessBuilder::set_channel` accepts what it accepts today and
wraps. Fourteen implementors, all listed by
`grep -rn "impl.* Channel for" crates app`; most are test doubles.

**D2.2 `crates/engine/src/dispatch.rs`.**
`Dispatcher::new(engine: Arc<Engine>, channel: Arc<dyn Channel>, slots: usize)` and
`async fn run(self) -> Result<(), EngineError>`.

- *Dispatcher loop:* `timeout(idle_after, channel.recv())`.
  `Message` → the session's mailbox (`mpsc::channel(16)`, created on demand),
  `send(..).await` — backpressure, never drop (the `WithDesktop` rule).
  `Closed` → drop every mailbox sender, await every session task, return `Ok`.
  `Idle` → if no turn is in flight (`slots.available_permits() == slots`) and
  `turns_since_pass > 0`, run `consolidator.run(&*memory)` and reset — the same gate
  as today's `run_loop`, minus the chance of running beside a live turn.
  A `run_turn` `Err` is fatal for the process, as today.
- *Session task:* `loop { timeout(idle_after, rx.recv()) }`.
  `Some(incoming)` → acquire a slot permit → `run_turn` → drop the permit →
  `channel.send(&sid, &text)` → `turns_since_pass += 1` → then the *same* biased
  `select!` as today's `run_loop` between the next `rx.recv()` and
  `maybe_summarize(&sid)`, so an abandoned summary is recomputed at the next boundary
  exactly as now. `None` → exit. Timeout → exit and remove its own mailbox. A message
  for a just-evicted session finds a closed sender; the dispatcher creates a fresh
  mailbox and re-sends — no message is lost to the race.
- `run_loop` and `DetachedChannel` are deleted. `Engine::run` becomes a thin
  constructor of a `Dispatcher` so `main.rs` and the `run()` tests keep their call
  shape. `[engine] worker_slots` (default `1`, the CLI's behaviour) sets `slots`.

**D2.3 Tests.**
- T0.3 inverted: `two_messages_for_one_session_through_the_dispatcher_become_two_turns`
  — the scripted channel delivers `a1`, `a2` back to back; the log has turns 1 and 2,
  both `UserSaid`s present, chain verifies. Keep T0.3 as the direct-`run_turn` test; it
  documents what the dispatcher prevents.
- `two_sessions_run_concurrently_under_two_slots` — `slots = 2`; the scripted emitter
  parks session `a`'s turn on a `Notify` that only session `b`'s completed turn
  releases; with a test timeout it passes only if the turns overlap.
- `one_slot_serializes_across_sessions` — same rig with `slots = 1`, assert `b` is not
  started until `a` finishes (an `AtomicUsize` high-water mark of in-flight turns is 1).
- The existing `run()` tests (`tests/turn_loop.rs:2619` summary-during-recv;
  the consolidator cadence tests) pass unchanged.

**Exit.** Green; T0.2 still passes; the CLI with `worker_slots = 1` behaves
identically.

---

## Phase 3 — a channel with more than one session: `ns-app serve`

**S3.1 `crates/channel-tcp`** (`ns-channel-tcp`, lib `nschannel_tcp`).
`TcpChannel::bind(listen: &str, token: String, max_connections: usize, allow_remote: bool)
-> Result<Arc<TcpChannel>, io::Error>`, implementing `Channel`. Wire: one JSON object per
line, both ways. First line from a client: `{"token":"…","session":"…"}`; a bad token
or an empty configured token closes the connection; a non-loopback bind is refused
unless `allow_remote` (both lessons from `serve_tcp`, `docs/windows-handoff.md` §1).
Then `{"text":"…"}` in, `{"session":"…","text":"…"}` out. Every connection is its own
task (precedent `crates/pointer/src/agent.rs:688`); the connection cap is machine-wide,
not per socket. A session's replies go to the connection that most recently claimed
that session id; a reply for a session with no live connection is logged and dropped
(the log already has it — that is what the log is for).

**S3.2 `ns-app serve`.** `[serve] listen = "127.0.0.1:7375"`, `token_env =
"NS_SERVE_TOKEN"`, `max_connections = 8`, `allow_remote = false`. In serve mode
`scope_for` maps a session to its own scope (the session id); the CLI keeps `global`.
`worker_slots` from `[engine]`.

**S3.3 Tests.**
- Engine-level leakage fixture (general-harness §5.2b), in `tests/turn_loop.rs`:
  `scope_for = |sid| sid`; a fact remembered in session `a`; a recall in session `b`
  returns nothing and the reply does not contain the value.
- `crates/channel-tcp/tests`: two clients on distinct sessions interleave and each
  receives only its own replies; a wrong token is closed before any message; the
  `max_connections + 1`th connection is refused; a reply to a departed session does
  not panic.
- `app/tests/e2e.rs`: `serve` boots with a scripted engine and answers one client.

**Exit.** Green; two clients driven by hand (`Test-NetConnection` is not enough — a
PowerShell `TcpClient` script or `nc`) get independent conversations.

---

## Phase 4 — gated, not in this plan

Session rollover (Continue-As-New) at an event-count threshold, digest as checkpoint.
Trigger: the T0.4 number, or a Phase 3 session that lives for weeks.

---

## Results

| item | value | phase |
|---|---|---|
| session `cli` | 259 events, 20 turns; db 200 KB + WAL 1.5 MB | 0 |
| `load` for session `cli` (release, 200×) | median 583 µs, p95 650 µs, max 756 µs | 0 |
| `fold` for session `cli` (release, 200×) | median 43 µs, p95 47 µs, max 83 µs | 0 |
| `verify_chain` for session `cli` (release, 20×) | median 742 µs, p95 831 µs | 0 |
| T0.1 | `run_turn(&self)` and `maybe_summarize(&self)` compile with no other change | 0 |
| T0.3 observed | one of the two overlapping turns is lost, whichever appends second; `tokio::join!` rotates polling, so in the first run it was the *first* message; `InMemoryStore::append` applies the same `id > max` filter as SQLite | 0 |

**Reading of the T0.4 number.** A turn's three or four folds cost well under a
millisecond at twenty turns; `load` dominates and is itself sub-millisecond. Under
N sessions the store mutex is held for ~0.6 ms per load, so contention would need
hundreds of turns a second to matter, which no free tier allows. Continue-As-New
(Phase 4) stays gated; the single WAL connection stays.
