# Many conversations at once: what Temporal provides, what the engine already has, and what is actually missing

Date: 2026-09-10 · Measured against `main` @ `206ea0d` (every branch merged) ·
Sources: the code, M7 §3, general-harness §5.2c, engine-structure §3/§6,
context-composition §2, local-retrieval §2, and the Temporal docs cited in §8.

The question was: build a Temporal-like process so one server can hold several
conversations at once. The answer, in one paragraph:

> The engine is already Temporal-shaped in the one way that matters — it keeps no
> per-conversation state in memory, and every turn is a replay of the log
> (`fold`). What stops it from serving many conversations is not durability, which
> the docs have decided three times, but **two constructs Temporal provides that the
> engine does not: one workflow task at a time per execution, and N executions in
> flight across worker slots.** Both are in-process constructs — a mailbox per
> session and a semaphore — and the codebase already contains the mailbox pattern
> (`WithDesktop`). The Temporal *server* is still not the right buy, for reasons
> restated below with one leg of the old reasoning withdrawn: the Rust SDK shipped
> 1.0.0 on 2026-09-04.

---

## 1. "Temporal-like", taken apart

Temporal is nine properties wearing one name. A request for "Temporal-like so the
server handles many conversations" needs (b) and (c); the rest are separate
decisions, most already taken.

| # | Temporal property | Needed for many conversations? |
|---|---|---|
| a | Event history is the state; progress is replayed to recover | no — already have it |
| b | **One Workflow Task at a time per Workflow Execution** (Signals buffer in history, delivered in order) | **yes — this is the invariant** |
| c | **Many Executions in flight, bounded by worker slots** (`max_concurrent_workflow_task_executions`) | **yes** |
| d | Signals: external messages become history events | yes, and `Incoming{session,text}` is already the shape |
| e | Timers | yes, per session rather than per process |
| f | Activities behind retry policies; non-deterministic results recorded once | no — already have it (traits, `ModelCall`, M8 §2.1) |
| g | Durable waits across a process restart | no — decided, engine-structure §3 |
| h | Continue-As-New | no — decided "not yet, nearly built", context-composition §2.1 |
| i | Task queues, a worker fleet, a Web UI | no — one process |

---

## 2. Where the engine stands on each, with evidence

### 2.1 (a) History: have, and it is the product's own

`Engine` is four fields — `parts`, `cfg`, `clock`, `builtin_guards`
(`crates/engine/src/turn.rs:162`) — and none of them is a session. The single
assignment to any field in the file is the channel swap in `run`
(`turn.rs:2206`). Every turn loads the whole session (`turn.rs:835`), folds it
(`:839`), and takes one rules snapshot (`:842`). Pending confirmation, staged
proposals, the window, the summary: all are `fold()` output (`state.rs:8`). The log
is hash-chained (`event.rs:170`), append is idempotent by id
(`memory-sqlite/src/lib.rs:321`), and SQLite runs in WAL (`lib.rs:104`).

So `run_turn` is already what Temporal calls a stateless worker: hand it any
session's `Incoming` and it reconstructs the conversation from the store. That is
the whole reason this is a small change and not a redesign.

### 2.2 (b) Per-session serialization: implicit today, and violating it is silent

`run_loop` (`turn.rs:2210`) is one `recv → run_turn → send` at a time, so the
invariant holds trivially. Nothing *enforces* it. If two turns for the same session
ever overlap:

- both compute `turn = fold(...).turn + 1` and both assign ids from the same
  `next_id` floor (`event.rs:173`);
- `append` keeps only `e.id > MAX(id)` (`lib.rs:330-331`), so the second turn's
  events are **dropped without an error**. Its reply was already sent to the user.
  The chain of the survivor verifies. Nothing reports the loss.

That is exactly the failure Temporal's per-execution task serialization exists to
make impossible, and it is the one property this work must add, before any
parallelism. The existing "concurrent" test (`tests/turn_loop.rs:2619-2650`) is
about the summary overlapping `recv`, not about two sessions; there is no test of
two sessions today.

### 2.3 (c) Worker slots: none; `&mut self` is inherited, not needed

`run_turn(&mut self)` (`turn.rs:832`) and `maybe_summarize(&mut self)` (`:474`)
are `&mut` because `run_loop` is. `run_turn` writes no field and calls no `&mut`
method (grep over `:832-2200`). `HarnessParts` (`core/src/plugin.rs:53`) holds the
roles as `Box<dyn …>` whose methods take `&self`, memory as `Arc`, tools as
`Vec<Arc>`. The only slot that genuinely needs `&mut` is `channel`. **Expected:
`run_turn(&self)` compiles once the channel leaves `HarnessParts`.** This is the
first thing the plan verifies, not assumes.

### 2.4 (d) Signals: the shape exists, one channel already buffers them

`Channel::recv` yields `Incoming { session, text }` (`core/src/action.rs:260`),
so a channel may multiplex sessions. None does yet: `CliChannel` pins `"cli"`
(`channel-cli/src/lib.rs:21`), and `WithDesktop` deliberately joins the compose
box to the *same* session (`components-std/src/desktop_channel.rs:11-17`). But
`WithDesktop::spawn` (`:83`) is the mailbox pattern in miniature: a poller task,
`mpsc::channel(64)`, blocking rather than dropping when the engine is behind, and
`select!` in `recv` (`:131`). The dispatcher below is that, per session.

`send(&mut self, session, text)` (`core/src/traits.rs:205`) on one owned `Box` is
the blocker on the way out: N session tasks need a shared outbound handle.

### 2.5 (e) Timers: process-wide, and the consolidator runs inline

`idle_after` (300 s in `ns-run/config.toml`) is a timeout on the one `recv`
(`turn.rs:2277`); on expiry the evolution pass runs *inside the loop*
(`:2246`). With N sessions, "idle" splits into per-session idle (this
conversation went quiet: summarize, digest, evict its mailbox) and process idle
(no mailbox active: consolidate). The rolling summary is already keyed per session
(`summary_due`, `:2213`, `:2262`) and already tolerates being abandoned and
recomputed at the next boundary, so it moves to a per-session task after `send`
without changing its contract.

### 2.6 (f) Activities: have; the accounting assumes no overlap

Retries and spacing live in the client (`llm/src/client.rs:7` `Throttle`, one per
provider via `main.rs:125`). Fine, and correctly global: the rate limit is per key.

`UsageSink` is not fine. One process-wide sink (`main.rs:532`), drained after each
of the engine's own calls (`turn.rs:807-814`), on the stated assumption that
"those never overlap" (`main.rs:530`). Two sessions calling the replier at once
would land session A's tokens on session B's `ModelCall`. `ns-app budget` — the
instrument the fifty-request constraint is managed with — would then be wrong in
a way no test catches. **Per-turn attribution is the one non-mechanical change in
this work** (§5, Phase 1).

### 2.7 (g) Durable waits: decided, and concurrency does not reopen it

engine-structure §3 measured turn resumption and found the obvious flush to be a
safety regression: a `PendingConfirmation` durable before it was shown lets the
next message confirm something nobody asked. Serving many conversations changes
none of that. A crash still loses the in-flight turn of every session, each
independently, and each recovers the same way the CLI does today.

### 2.8 (h) Continue-As-New: unchanged, same trigger, now closer

context-composition §2.1 already corrected M7 §3 here: `load` + `fold` is O(n)
per turn, three or four times a turn. With N sessions that cost is paid under the
one `Mutex<Connection>` (`lib.rs:11`), so it becomes the contention point rather
than an invisible constant. engine-structure §5.6 says measure `fold()` first;
this work is the reason to.

### 2.9 What the free tier does to the promise

On `openrouter/free` (~50 requests/day) N conversations share one budget and one
`Throttle`. Concurrency buys **overlap of waiting** — the user typing, tool
latency, a desktop action in flight — not more requests. Model calls stay
globally throttled as they are. Fairness between sessions is a product decision;
until measured, FIFO by arrival and no per-session quota.

---

## 3. Correction to M7 §3, in writing

M7 §3 (2026-09-07) declined Temporal on three legs and general-harness §5.2c
(2026-09-08) repeated them: a server, a second event history, and "a Rust SDK that
is not where the Python one is".

**The third leg is withdrawn.** `temporalio-sdk` entered public preview in May
2026 and published **1.0.0 on 2026-09-04** (0.5.0 on 06-29, 0.6.0 on 08-04,
0.7.0 on 08-17, 0.8.0 on 09-02). docs.rs for 1.0.0 lists workflows, activities,
signals, queries, updates, timers, child workflows, continue-as-new, workers and a
`workflow_replayer`; plugins are marked experimental, nothing is listed as
unsupported. The SDK is no longer a reason.

**The other two legs stand, and a third replaces the withdrawn one:**

1. *A second history.* The engine's log is the source of truth for provenance,
   replay, `verify_patch`, the evolution pass and `ns-app budget`. Under Temporal,
   Temporal's history is what gets replayed and the engine's becomes a copy. Two
   authorities for one conversation.
2. *A server and its database* on a one-desktop, one-operator deployment.
3. *Determinism rules invert the store.* Temporal workflow code may not do I/O;
   `run_turn` reads the store directly (`memory.load`, `turn.rs:835`) and folds.
   Under Temporal every load becomes an Activity — three or four history events per
   turn on top of the model calls, which is the history-bloat trap
   context-composition §2.1 named — or the log moves into workflow state and is
   then bounded by Continue-As-New. Either way the engine's central design move
   (*the log is unbounded; every context is a projection of it*) is the thing
   Temporal forces you to give up.

What Temporal would buy that §5 does not: the Web UI, durable timers across
restarts, exactly-once across processes. None is asked for.

**Trigger to revisit, updated:** a second process or worker; an operator who needs
the UI; or the engine-structure §3 safety trade being re-decided in favour of
durability. Not "a channel where a turn outlives the process" alone — that
condition is now met by any server channel and §2.7 shows it changes nothing.

---

## 4. Temporal's shape, in-process

```
                    ┌────────────────────── ns-app serve ──────────────────────┐
  conn A ──┐        │  Inbound.recv() → Incoming { session, text }             │
  conn B ──┼─ tcp ──┤        │                                                 │
  stdin  ──┘        │        ▼                                                 │
                    │  Dispatcher { HashMap<SessionId, Mailbox> }              │
                    │        │  one task per active session, serial            │  ← (b)
                    │        ▼                                                 │
                    │  Semaphore(slots) ─▶ Arc<Engine>::run_turn(&self, in)    │  ← (c)
                    │        │             (Throttle per provider, unchanged)  │
                    │        ▼                                                 │
                    │  Outbound.send(&self, session, text)                     │
                    │  then: summary task for that session                     │
                    │  mailbox idle → digest, evict     no mailbox → consolidate│  ← (e)
                    └──────────────────────────────────────────────────────────┘
```

| Temporal | here | note |
|---|---|---|
| Workflow Execution, id = conversation | a mailbox task; history = that session's log | evicted on idle, recreated on the next message |
| Workflow Task, one at a time | the mailbox processes one `Incoming` at a time | the §2.2 hazard becomes impossible by construction |
| Signal | `Incoming` pushed to the mailbox; buffered while a turn is in flight | `WithDesktop` already does this for one session |
| Worker slots | `tokio::sync::Semaphore(N)` around `run_turn` | N small; the throttle is the real limit |
| Activity + retry policy | trait calls, `OpenRouterClient` backoff, `Throttle` | unchanged |
| Timer | per-mailbox `timeout` on the receive; process idle when no mailbox is live | consolidator moves off the turn path |
| Continue-As-New | session rollover, digest as checkpoint | trigger unchanged (context-composition §2.1) |
| Task queue, second worker | not built | trigger unchanged |

The CLI is the N = 1 degenerate case: one mailbox, one slot, identical behaviour.
`run_loop` does not survive as a second code path.

---

## 5. Phases, sized, with what each must prove

**Phase 0 — pin and measure (½ day).**
- `run_turn(&self)`, `maybe_summarize(&self)`; the channel leaves `HarnessParts`.
  Compiles or the plan is wrong.
- Test: two sessions interleaved through a scripted channel produce two intact
  logs. Passes today, serially; it is the guard for everything after.
- Test: the §2.2 hazard, reproduced — two `run_turn`s on one session overlapped
  with a yielding scripted emitter; assert the second turn's events are missing.
  Kept as the reason the dispatcher exists, then inverted in Phase 2.
- Measure `load`+`fold` per turn on the 20-turn `cli` session (engine-structure
  §5.6). The number decides whether the store mutex needs anything.

**Phase 1 — attribution (½–1 day).** `UsageSink` per turn, not per process. The
sink is baked into each client at construction (`client.rs:87`), so attribution
must travel with the call: either a `(session, turn)` tag threaded through
`propose` / `reply` / `summarize` contexts, or a task-local set by `run_turn`.
The explicit one fits this repo (nothing hidden that replay cannot see). Test: two
sessions' `ModelCall`s land on their own turns.

**Phase 2 — dispatcher (1 day).** `Inbound` + `Outbound` split of `Channel`,
mailboxes, the slot semaphore, per-session idle → summary/digest/evict, process
idle → consolidator. The Phase 0 hazard test now asserts the opposite. Exit:
`cargo test --workspace` green; the CLI behaves identically.

**Phase 3 — a channel with more than one session (1–2 days).** `ns-app serve`:
loopback TCP, one JSON line per message, a token, every connection its own task —
the precedent is `serve_tcp` (`pointer/src/agent.rs:688`) and its lesson that the
caps are machine-wide, not per socket (`docs/windows-handoff.md`). Connection →
`SessionId` by handshake. `scope_for` (`main.rs:627`, `global` today) maps a
session to its scope, and the exit test is the leakage fixture general-harness
§5.2b asked for: a fact stated on connection A, a recall on connection B that
returns nothing.

**Phase 4 — gated.** Session rollover when a session's event count crosses a
threshold; the digest as checkpoint. Trigger: the Phase 0 `fold` measurement, or a
session the Phase 3 channel keeps alive for weeks.

---

## 6. Not adopted, with the reason

- **The Temporal server** — §3. The SDK objection is gone; the history-duplication
  and determinism objections are structural.
- **An actor framework** (actix, ractor) — the whole need is `mpsc` + one task per
  session, and `WithDesktop` already is that pattern in the tree.
- **A connection pool or per-session SQLite connections** — the single WAL
  connection behind one mutex is correct; §2.8 says measure first.
- **Durable waits across restarts** — engine-structure §3, unchanged by any of
  this.
- **Heartbeats** — local-retrieval §2.4, unchanged.
- **Multi-user auth** — a v1 non-goal (harness design §1); the server binds
  loopback with a token like the pointer, and identity is the connection.

---

## 7. What this corrects in the reading it started from

1. "Many conversations needs Temporal's durability." It needs Temporal's
   *serialization* and *slots*. Durability is a separate axis, decided three times.
2. "The Rust SDK is not ready." It is 1.0.0 as of 2026-09-04 (§3).
3. The engine's stateless shape was assumed to make concurrency "just spawn". Two
   things break silently under a naive spawn: the same-session id collision (§2.2)
   and usage attribution (§2.6). Both get a test before any task is spawned.

---

## 8. Sources

- Temporal, *Workflow Execution* (one Workflow Task at a time, Signals in history)
  — https://docs.temporal.io/workflow-execution
- Temporal, *Workflow definition: deterministic constraints* —
  https://docs.temporal.io/workflow-definition#deterministic-constraints
- Temporal, *Worker performance* (`max_concurrent_workflow_task_executions`) —
  https://docs.temporal.io/develop/worker-performance
- Temporal, *Rust SDK public preview* — https://temporal.io/changelog/rust-sdk-public-preview
- `temporalio-sdk` on crates.io (1.0.0, 2026-09-04) —
  https://crates.io/crates/temporalio-sdk · docs —
  https://docs.rs/temporalio-sdk/latest/temporalio_sdk/ · repo —
  https://github.com/temporalio/sdk-rust
- This repo: `docs/superpowers/plans/2026-09-07-m7-context-budget.md` §3;
  `docs/superpowers/specs/2026-09-08-general-harness-design.md` §5.2;
  `docs/superpowers/specs/2026-09-08-engine-structure.md` §3, §5.6, §6;
  `docs/research/2026-09-08-context-composition-findings.md` §2;
  `docs/research/2026-09-08-local-retrieval-and-lane-findings.md` §2;
  `docs/windows-handoff.md` §1.
