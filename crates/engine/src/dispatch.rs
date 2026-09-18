//! One turn at a time per session; `worker_slots` sessions at once.
//!
//! The two properties Temporal gives a workflow execution, provided here
//! in-process (plan `docs/superpowers/plans/2026-09-10-multi-conversation-runtime.md`
//! Phase 2; findings `docs/research/2026-09-10-multi-conversation-runtime-findings.md`
//! §2.2, §2.3, §4):
//!
//! 1. **Per-session serialization** (findings §2.2). Each active session has
//!    a mailbox and one task draining it, so a turn for a session cannot
//!    start before that session's previous turn has finished. Without it two
//!    overlapping turns on one session both number themselves the same turn,
//!    and the store keeps only the first to append — the second vanishes
//!    without an error, after its reply was already sent (`app/tests/e2e.rs`,
//!    `two_overlapping_turns_on_one_session_lose_one_silently`). Through the
//!    mailbox the same two messages become turns 1 and 2.
//! 2. **Worker slots** (findings §2.3, §2.9). A semaphore with
//!    `EngineConfig::worker_slots` permits bounds how many turns run at once
//!    across all sessions. `1` is the CLI's serial behaviour; more overlaps
//!    the *waiting* of several conversations under the one request budget,
//!    never the requests themselves.
//!
//! Timers split as findings §2.5 says: a session quiet for `idle_after` lets
//! its task go — the mailbox is recreated on its next message — and the whole
//! channel quiet for `idle_after` with no turn in flight runs the
//! consolidator. History stays the engine's own log; nothing here is durable,
//! and a crash loses each session's in-flight turn exactly as it did the
//! CLI's (findings §2.7).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nscore::{Channel, ChannelError, Incoming, SessionId};
use tokio::sync::{mpsc, Semaphore, SemaphorePermit};
use tokio::task::{JoinError, JoinSet};

use crate::turn::{Engine, EngineError};

/// How many messages a session's mailbox holds before the dispatcher — and
/// with it the channel — waits for that session to catch up. Backpressure,
/// never a drop: the `WithDesktop` rule.
const MAILBOX_DEPTH: usize = 16;

/// What a dispatcher does with a turn that failed (plan
/// `docs/superpowers/plans/2026-09-13-multi-tenant-runtime.md` Phase 2, T2.1).
///
/// One process used to serve one conversation, so any failure was the
/// process's failure. A shard serving many tenants needs the question asked
/// once, at the one place a session's failure is seen: hazard H2 is that
/// today one tenant's widget disappearing mid-reply ends a process serving
/// every other tenant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnFailure {
    /// The CLI's, and the default: a failed turn ends the run, and a panic
    /// is resumed on the dispatcher's own task. Today's behaviour exactly.
    #[default]
    Fatal,
    /// Serve's: a failed session ends that session, and the dispatcher keeps
    /// serving every other one. What still ends the run is in
    /// [`TurnFailure::verdict`], which is where the whole policy lives.
    Isolate,
}

/// What one failure costs: everything, or one session.
enum Verdict {
    EndTheRun,
    EndTheSession,
}

impl TurnFailure {
    /// The classification, exhaustive over `EngineError` with no wildcard
    /// arm on purpose: a fourth variant added later cannot inherit a policy
    /// by accident, because this will not compile until someone has decided
    /// what it costs.
    fn verdict(self, e: &EngineError) -> Verdict {
        match e {
            // The disk is gone. Fatal for the shard under either policy:
            // every other tenant here writes to a store too.
            EngineError::Store(_) => Verdict::EndTheRun,
            // Not a fault at all - a metered run reaching its ceiling. It
            // ends this tenant's dispatcher and nothing wider, which under
            // `Isolate` is one tenant of the shard rather than the shard.
            EngineError::RequestCap { .. } => Verdict::EndTheRun,
            // H2 itself: one session's connection went away.
            EngineError::Channel(_) => match self {
                TurnFailure::Fatal => Verdict::EndTheRun,
                TurnFailure::Isolate => Verdict::EndTheSession,
            },
        }
    }
}

/// A ceiling on the turns a whole *process* runs at once, shared by every
/// dispatcher in it (plan `.claude/plans/continue-polymorphic-pearl.md`
/// Part B, B1).
///
/// One dispatcher per tenant means one `worker_slots` pool per tenant, so N
/// tenants bound N turns each and the process nothing at all. This is the
/// missing outer bound: clone it into every dispatcher on the shard and the
/// turns in flight across all of them cannot exceed it.
///
/// # The acquisition order is the safety property
///
/// A turn takes its tenant's permit **first** and the shard's **second**,
/// never the other way round. That order is total, so the wait-for graph has
/// no cycle: a task may hold a tenant permit while waiting for a shard
/// permit, but nothing holding a shard permit ever waits for a tenant one.
/// Reverse it anywhere and two tenants deadlock, each holding what the other
/// waits for.
///
/// The order is the *type's* to enforce, not this paragraph's:
/// [`ShardSlots::acquire`] takes the tenant permit by reference, so a shard
/// permit is unobtainable without a tenant permit in hand, and the borrow
/// keeps that permit held for at least the whole wait.
#[derive(Clone)]
pub struct ShardSlots(Arc<Semaphore>);

impl ShardSlots {
    /// A ceiling of `slots` turns at once, floored at 1 for the reason the
    /// worker slots are: zero permits would park every turn forever.
    pub fn new(slots: usize) -> Self {
        Self(Arc::new(Semaphore::new(slots.max(1))))
    }

    /// A shard permit, given the tenant permit already held. `held` is never
    /// read — it is the proof of order documented on the type.
    async fn acquire<'a>(&'a self, held: &SemaphorePermit<'_>) -> SemaphorePermit<'a> {
        let _ = held;
        self.0
            .acquire()
            .await
            .expect("the shard semaphore is never closed")
    }
}

/// Reads one channel and runs its sessions, each on its own task.
pub struct Dispatcher {
    shared: Arc<Shared>,
    n_slots: usize,
    /// The live sessions, by id.
    sessions: HashMap<SessionId, Mailbox>,
    tasks: JoinSet<Stopped>,
    /// Numbers each incarnation of a session's task, so a stop report from
    /// an old one can never evict a newer mailbox.
    generations: u64,
    /// Messages handed back by a stopping task, delivered again before the
    /// channel is read.
    pending: VecDeque<Incoming>,
}

/// What every session task shares with the dispatcher.
struct Shared {
    engine: Arc<Engine>,
    channel: Arc<dyn Channel>,
    /// One permit per worker slot.
    slots: Semaphore,
    /// The shard-wide ceiling, when this dispatcher is one of several in one
    /// process. `None` for every caller that has not asked for one, and that
    /// is today's behaviour exactly: `slots` is then the only bound.
    shard: Option<ShardSlots>,
    /// Turns completed since the consolidator last ran: the session tasks
    /// count, the dispatcher resets.
    turns_since_pass: AtomicU32,
    idle_after: Option<Duration>,
    policy: TurnFailure,
}

struct Mailbox {
    tx: mpsc::Sender<Incoming>,
    generation: u64,
}

/// What a session task hands back when it stops.
struct Stopped {
    session: SessionId,
    generation: u64,
    /// Messages that landed in the mailbox after the task had decided to
    /// stop on idle: it closed the mailbox, drained what was already in it,
    /// and left them to the dispatcher rather than run them itself beside a
    /// fresh task for the same session.
    leftovers: Vec<Incoming>,
    /// Set when the session stopped because its turn went wrong, rather
    /// than because its mailbox closed or it went quiet.
    failure: Option<Failure>,
}

/// Why a session stopped, when it stopped badly.
enum Failure {
    /// The turn returned an error, to be classified by [`TurnFailure`].
    Turn(EngineError),
    /// The turn panicked and the panic was contained in the session task
    /// (T2.2), carrying what it was raised with — see [`panic_message`].
    /// Only ever produced under [`TurnFailure::Isolate`]; under `Fatal` the
    /// panic unwinds the task and is resumed in `reap`.
    Panic(String),
}

/// What a panic was raised with, recovered from the payload `catch_unwind`
/// hands back, so the one line the operator gets about a contained panic says
/// more than that there was one.
///
/// `panic!`, `assert!`, `unwrap` and `expect` all produce a `String` or a
/// `&'static str` — the text a test harness prints — so those two downcasts
/// cover every panic a turn is likely to raise. A payload of any other type
/// (`panic_any`) cannot be read without knowing the type, so it is described
/// instead of dropped.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "a panic payload of neither String nor &str".to_string()
    }
}

/// What one wait — on the channel, or on a mailbox — produced.
enum Next {
    Message(Incoming),
    /// `idle_after` elapsed with nothing arriving.
    Idle,
    Closed,
}

type TaskResult = Result<Stopped, JoinError>;

/// Which of the dispatcher's two waits woke it.
enum Woke {
    Task(Option<TaskResult>),
    Channel(Result<Next, EngineError>),
}

impl Dispatcher {
    /// A dispatcher under the default policy, [`TurnFailure::Fatal`]: the
    /// CLI's, and every caller that has not said otherwise.
    pub fn new(engine: Arc<Engine>, channel: Arc<dyn Channel>, slots: usize) -> Self {
        Self::with_failure_policy(engine, channel, slots, TurnFailure::Fatal)
    }

    /// A dispatcher that answers a failed turn the way `policy` says.
    pub fn with_failure_policy(
        engine: Arc<Engine>,
        channel: Arc<dyn Channel>,
        slots: usize,
        policy: TurnFailure,
    ) -> Self {
        // Zero permits would park every turn forever. The config rejects 0;
        // this is the same floor for a caller that did not go through it.
        let n_slots = slots.max(1);
        let idle_after = engine.config().idle_after;
        Self {
            shared: Arc::new(Shared {
                engine,
                channel,
                slots: Semaphore::new(n_slots),
                shard: None,
                turns_since_pass: AtomicU32::new(0),
                idle_after,
                policy,
            }),
            n_slots,
            sessions: HashMap::new(),
            tasks: JoinSet::new(),
            generations: 0,
            pending: VecDeque::new(),
        }
    }

    /// Also bounded by a ceiling it shares with the other dispatchers in the
    /// process ([`ShardSlots`]). Without this the dispatcher keeps only its
    /// own `slots`, which is what every existing caller gets.
    pub fn with_shard_slots(mut self, shard: ShardSlots) -> Self {
        // Unique here by construction: `Shared` is cloned into a session
        // task only once `run` starts, and `run` takes `self` by value.
        Arc::get_mut(&mut self.shared)
            .expect("no session task exists before the dispatcher runs")
            .shard = Some(shard);
        self
    }

    /// Reads the channel until it closes, then lets every session finish.
    /// What a turn that fails costs is the dispatcher's [`TurnFailure`]
    /// policy: under `Fatal` the process, as it was when the loop ran the
    /// turn itself; under `Isolate` usually just that session.
    pub async fn run(mut self) -> Result<(), EngineError> {
        let channel = self.shared.channel.clone();
        // One `recv` at a time, and never dropped half-way: `CliChannel`'s is
        // a `read_line`, and a timeout or a task stopping first must not lose
        // the bytes it has already taken. The future is created once, polled
        // through `&mut` by whatever waits on it, and replaced only after it
        // has completed.
        let mut recv = channel.recv();
        loop {
            self.deliver_pending().await?;
            let woke = tokio::select! {
                biased;
                stopped = self.tasks.join_next(), if !self.tasks.is_empty() => Woke::Task(stopped),
                next = wait(&mut recv, self.shared.idle_after) => Woke::Channel(next),
            };
            match woke {
                Woke::Task(None) => {}
                Woke::Task(Some(result)) => {
                    let stopped = self.reap(result)?;
                    self.pending.extend(stopped.leftovers);
                }
                Woke::Channel(next) => match next? {
                    Next::Message(incoming) => {
                        recv = channel.recv();
                        self.deliver(incoming).await?;
                    }
                    Next::Idle => self.consolidate_if_quiet().await,
                    Next::Closed => return self.close().await,
                },
            }
        }
    }

    async fn deliver_pending(&mut self) -> Result<(), EngineError> {
        while let Some(m) = self.pending.pop_front() {
            self.deliver(m).await?;
        }
        Ok(())
    }

    /// Puts a message in its session's mailbox, opening the session if it
    /// has none. Waits on a full mailbox rather than dropping. A mailbox
    /// whose task has stopped meanwhile refuses the message: that task is
    /// reaped first — anything it hands back goes ahead, in order — and a
    /// fresh one takes over. No message is lost to that race.
    async fn deliver(&mut self, incoming: Incoming) -> Result<(), EngineError> {
        let session = incoming.session.clone();
        let mut queue = VecDeque::from([incoming]);
        while let Some(m) = queue.pop_front() {
            let tx = self.mailbox(&session);
            if let Err(mpsc::error::SendError(back)) = tx.send(m).await {
                let leftovers = self.reap_session(&session).await?;
                queue.push_front(back);
                for l in leftovers.into_iter().rev() {
                    queue.push_front(l);
                }
            }
        }
        Ok(())
    }

    /// The sender for a session, opening a mailbox and a task if it has none.
    fn mailbox(&mut self, session: &SessionId) -> mpsc::Sender<Incoming> {
        if let Some(m) = self.sessions.get(session) {
            return m.tx.clone();
        }
        let (tx, rx) = mpsc::channel(MAILBOX_DEPTH);
        self.generations += 1;
        let generation = self.generations;
        self.tasks.spawn(session_task(
            self.shared.clone(),
            session.clone(),
            generation,
            rx,
        ));
        self.sessions.insert(
            session.clone(),
            Mailbox {
                tx: tx.clone(),
                generation,
            },
        );
        tx
    }

    /// The mailbox of `session` refused a message, so its task is stopping:
    /// waits until that task has been reaped and returns what it handed
    /// back. Other tasks that stop meanwhile are handled as the loop would
    /// handle them, their leftovers queued for delivery before the channel
    /// is read again.
    async fn reap_session(&mut self, session: &SessionId) -> Result<Vec<Incoming>, EngineError> {
        while let Some(result) = self.tasks.join_next().await {
            let stopped = self.reap(result)?;
            if stopped.session == *session {
                return Ok(stopped.leftovers);
            }
            self.pending.extend(stopped.leftovers);
        }
        // Not reached in practice: a mailbox only closes when its task
        // stops, and a task stays in the set until it is reaped. Should it
        // happen, the stale mailbox goes so the retry opens a fresh one.
        self.sessions.remove(session);
        Ok(Vec::new())
    }

    /// One task's result, classified by the dispatcher's policy. A failure
    /// [`TurnFailure::verdict`] calls `EndTheRun` leaves here as `Err`, as
    /// every failure did when the loop ran the turn itself; one it calls
    /// `EndTheSession` is a stop like any other, so the mailbox goes and
    /// whatever was queued behind it is handed back for a fresh task.
    fn reap(&mut self, result: TaskResult) -> Result<Stopped, EngineError> {
        let mut stopped = match result {
            Ok(stopped) => stopped,
            Err(join) => {
                // Under `Isolate` a panic is contained in the session task
                // and arrives as `Failure::Panic`, so this is `Fatal`'s
                // path - and a cancellation's, under either policy.
                if join.is_panic() && self.shared.policy == TurnFailure::Fatal {
                    std::panic::resume_unwind(join.into_panic());
                }
                // Never aborted from here; a cancellation is the runtime
                // shutting down under the dispatcher.
                return Err(EngineError::Channel(format!("session task: {join}")));
            }
        };
        match stopped.failure.take() {
            None => {}
            Some(Failure::Turn(e)) => match self.shared.policy.verdict(&e) {
                Verdict::EndTheRun => return Err(e),
                Verdict::EndTheSession => eprintln!("session {}: {e}", stopped.session.0),
            },
            // Contained, so it can only ever have cost one session (T2.2).
            Some(Failure::Panic(message)) => {
                eprintln!(
                    "session {}: the turn panicked: {message}",
                    stopped.session.0
                )
            }
        }
        let current = self
            .sessions
            .get(&stopped.session)
            .is_some_and(|m| m.generation == stopped.generation);
        if current {
            self.sessions.remove(&stopped.session);
        }
        Ok(stopped)
    }

    /// The channel has been quiet for `idle_after`. The consolidator runs
    /// once if a turn has completed since its last pass and no turn is in
    /// flight — checked by taking every slot, which also keeps it true for
    /// the length of the pass: a turn arriving meanwhile waits for its slot,
    /// as it would behind another turn. The same gate as the serial loop
    /// had, minus the chance of running beside a live turn.
    async fn consolidate_if_quiet(&self) {
        if self.shared.turns_since_pass.load(Ordering::SeqCst) == 0 {
            return;
        }
        let Ok(_all_slots) = self.shared.slots.try_acquire_many(self.n_slots as u32) else {
            return;
        };
        let parts = self.shared.engine.parts();
        if let Err(e) = parts.consolidator.run(&*parts.memory).await {
            eprintln!("evolution pass failed: {e}");
        }
        self.shared.turns_since_pass.store(0, Ordering::SeqCst);
    }

    /// The channel closed. Every mailbox closes with it; each task finishes
    /// what it holds and stops. A task that was stopping on idle at that
    /// moment may hand a message back, and that one still gets its turn.
    async fn close(mut self) -> Result<(), EngineError> {
        loop {
            self.deliver_pending().await?;
            self.sessions.clear();
            while let Some(result) = self.tasks.join_next().await {
                let stopped = self.reap(result)?;
                self.pending.extend(stopped.leftovers);
            }
            if self.pending.is_empty() {
                return Ok(());
            }
        }
    }
}

/// One wait on the channel, with the idle timeout folded in. Takes the recv
/// by `&mut` so that a timeout drops only the wait, never the recv.
async fn wait<F>(recv: &mut F, idle_after: Option<Duration>) -> Result<Next, EngineError>
where
    F: Future<Output = Result<Incoming, ChannelError>> + Unpin,
{
    let received = match idle_after {
        Some(d) => tokio::time::timeout(d, recv).await,
        None => Ok(recv.await),
    };
    match received {
        Ok(Ok(i)) => Ok(Next::Message(i)),
        Ok(Err(ChannelError::Closed)) => Ok(Next::Closed),
        Ok(Err(e)) => Err(EngineError::Channel(e.to_string())),
        Err(_elapsed) => Ok(Next::Idle),
    }
}

/// One wait on a mailbox, with the session's idle timeout folded in. A
/// plain mpsc `recv` is cancel-safe, so the timeout may simply wrap it.
async fn next_in_mailbox(rx: &mut mpsc::Receiver<Incoming>, idle_after: Option<Duration>) -> Next {
    let received = match idle_after {
        Some(d) => tokio::time::timeout(d, rx.recv()).await,
        None => Ok(rx.recv().await),
    };
    match received {
        Ok(Some(i)) => Next::Message(i),
        Ok(None) => Next::Closed,
        Err(_elapsed) => Next::Idle,
    }
}

/// One session's task: drains its mailbox one turn at a time, holding a
/// worker slot for each turn, and stops when the mailbox closes, when the
/// session has been quiet for `idle_after`, or when its turn goes wrong.
///
/// However it stops, it stops the same way: the mailbox closes here and
/// whatever was already in it is handed back, so the message that arrived
/// one instant before a failure is re-delivered to a fresh task rather than
/// dropped with this one. No message is lost to a failure, exactly as none
/// is lost to an idle eviction.
async fn session_task(
    shared: Arc<Shared>,
    session: SessionId,
    generation: u64,
    mut rx: mpsc::Receiver<Incoming>,
) -> Stopped {
    let failure = match shared.policy {
        // T2.2: the panic is contained here, so the task reports a failed
        // session instead of unwinding and taking the dispatcher with it.
        // Safe for a reason worth recording: the store's lock is a
        // `tokio::sync::Mutex`, which does not poison the way `std`'s does,
        // and `run_turn` takes `&self`, so a panic leaves no unusable lock
        // behind and little in-memory state to corrupt.
        TurnFailure::Isolate => match catch_unwind(session_turns(&shared, &session, &mut rx)).await
        {
            Ok(outcome) => outcome,
            Err(payload) => Some(Failure::Panic(panic_message(payload.as_ref()))),
        },
        // Under `Fatal` a panic is not caught at all: it unwinds the task
        // and `reap` resumes it, which is what the CLI did and still does.
        TurnFailure::Fatal => session_turns(&shared, &session, &mut rx).await,
    };
    // Closing first makes the dispatcher's next `send` on this mailbox fail
    // rather than land; whatever landed before the close is handed back, to
    // run on a fresh task once this one is gone - not here, beside it.
    rx.close();
    let mut leftovers = Vec::new();
    while let Ok(m) = rx.try_recv() {
        leftovers.push(m);
    }
    Stopped {
        session,
        generation,
        leftovers,
        failure,
    }
}

/// `catch_unwind` for a future: polls it inside [`std::panic::catch_unwind`],
/// so a panic becomes a value the caller can report. Boxed rather than
/// pin-projected so this needs no `unsafe`.
async fn catch_unwind<F: Future>(fut: F) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        let polled =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx)));
        match polled {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await
}

/// The turns themselves: `None` when the session stopped because there was
/// nothing more to do, `Some` when a turn failed.
async fn session_turns(
    shared: &Shared,
    session: &SessionId,
    rx: &mut mpsc::Receiver<Incoming>,
) -> Option<Failure> {
    let engine = &shared.engine;
    // Whether the turn that just ended may owe a rolling summary.
    let mut summary_due = false;
    loop {
        let next = {
            let mut pending = std::pin::pin!(next_in_mailbox(rx, shared.idle_after));
            if !summary_due {
                pending.await
            } else {
                // The rolling summary (M6 §5.1) runs *concurrently with the
                // wait for the next message*, not before it: it is
                // sleep-time work, and a local summarizer can take tens of
                // seconds — long enough to hold up the prompt if it sits on
                // the critical path.
                //
                // The mailbox is polled first (biased), as `recv` was when
                // the serial loop did this, so the prompt appears before the
                // summary starts; the summary then runs while the user reads
                // the reply and types.
                //
                // M11 T1.5. What the *other* branch does is the fix. The
                // original dropped the summary mid-flight whenever the user
                // got there first, on the reasoning that the next boundary
                // would recompute it — but `biased` means a message already
                // in the mailbox wins before the summary future is polled
                // even once, and after that turn the summary is due again
                // and loses again. The M11 measurement is the evidence: 40
                // turns under `serve` at `summary_every_turns = 4`, zero
                // `Summarized` events. Anyone typing faster than the
                // summarizer never gets a summary at all, which is
                // starvation, not deferral.
                //
                // So a due summary is now *finished* rather than abandoned:
                // the message is held and `maybe_summarize` is awaited to
                // completion before the turn it belongs to runs. Awaited
                // here rather than spawned deliberately — a spawned summary
                // would read and append to the same session's log while
                // `run_turn` is folding it, and one-turn-at-a-time per
                // session is the property the whole session task exists to
                // hold. The cost is the summarizer's latency in front of a
                // fast user's next reply, which is exactly the case that
                // previously bought a summary that never happened.
                tokio::select! {
                    biased;
                    next = &mut pending => {
                        if let Err(e) = engine.maybe_summarize(session).await {
                            eprintln!("summary: {e}");
                        }
                        next
                    }
                    summarized = engine.maybe_summarize(session) => {
                        if let Err(e) = summarized {
                            eprintln!("summary: {e}");
                        }
                        pending.await
                    }
                }
            }
        };
        match next {
            Next::Message(incoming) => {
                let permit = shared
                    .slots
                    .acquire()
                    .await
                    .expect("the slot semaphore is never closed");
                // Second, and only ever second: the order the whole
                // deadlock-freedom argument on `ShardSlots` rests on, which
                // is why this takes the tenant permit it already holds.
                let shard_permit = match &shared.shard {
                    Some(shard) => Some(shard.acquire(&permit).await),
                    None => None,
                };
                let text = match engine.run_turn(incoming).await {
                    Ok(text) => text,
                    Err(e) => return Some(Failure::Turn(e)),
                };
                // Counted while the slot is still held, so the dispatcher
                // can never see every slot free and this turn uncounted.
                shared.turns_since_pass.fetch_add(1, Ordering::SeqCst);
                // Released in the reverse of the order they were taken, and
                // both before the reply goes out: a send that blocks holds
                // up neither this tenant's slots nor the shard's.
                drop(shard_permit);
                drop(permit);
                if let Err(e) = shared.channel.send(session, &text).await {
                    return Some(Failure::Turn(EngineError::Channel(e.to_string())));
                }
                summary_due = true;
            }
            // The mailbox closed, or the session has been quiet for
            // `idle_after`: either way this task is done, and `session_task`
            // hands back whatever is left in the mailbox.
            Next::Closed | Next::Idle => return None,
        }
    }
}
