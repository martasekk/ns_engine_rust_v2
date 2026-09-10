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
use tokio::sync::{mpsc, Semaphore};
use tokio::task::{JoinError, JoinSet};

use crate::turn::{Engine, EngineError};

/// How many messages a session's mailbox holds before the dispatcher — and
/// with it the channel — waits for that session to catch up. Backpressure,
/// never a drop: the `WithDesktop` rule.
const MAILBOX_DEPTH: usize = 16;

/// Reads one channel and runs its sessions, each on its own task.
pub struct Dispatcher {
    shared: Arc<Shared>,
    n_slots: usize,
    /// The live sessions, by id.
    sessions: HashMap<SessionId, Mailbox>,
    tasks: JoinSet<Result<Stopped, EngineError>>,
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
    /// Turns completed since the consolidator last ran: the session tasks
    /// count, the dispatcher resets.
    turns_since_pass: AtomicU32,
    idle_after: Option<Duration>,
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
}

/// What one wait — on the channel, or on a mailbox — produced.
enum Next {
    Message(Incoming),
    /// `idle_after` elapsed with nothing arriving.
    Idle,
    Closed,
}

type TaskResult = Result<Result<Stopped, EngineError>, JoinError>;

/// Which of the dispatcher's two waits woke it.
enum Woke {
    Task(Option<TaskResult>),
    Channel(Result<Next, EngineError>),
}

impl Dispatcher {
    pub fn new(engine: Arc<Engine>, channel: Arc<dyn Channel>, slots: usize) -> Self {
        // Zero permits would park every turn forever. The config rejects 0;
        // this is the same floor for a caller that did not go through it.
        let n_slots = slots.max(1);
        let idle_after = engine.config().idle_after;
        Self {
            shared: Arc::new(Shared {
                engine,
                channel,
                slots: Semaphore::new(n_slots),
                turns_since_pass: AtomicU32::new(0),
                idle_after,
            }),
            n_slots,
            sessions: HashMap::new(),
            tasks: JoinSet::new(),
            generations: 0,
            pending: VecDeque::new(),
        }
    }

    /// Reads the channel until it closes, then lets every session finish.
    /// A turn that fails is fatal for the process, as it was when the loop
    /// ran the turn itself.
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

    /// One task's result. `Err` from a turn is fatal for the process, as it
    /// was when the loop ran the turn itself; a panic is resumed for the
    /// same reason.
    fn reap(&mut self, result: TaskResult) -> Result<Stopped, EngineError> {
        let stopped = match result {
            Ok(Ok(stopped)) => stopped,
            Ok(Err(e)) => return Err(e),
            Err(join) => {
                if join.is_panic() {
                    std::panic::resume_unwind(join.into_panic());
                }
                // Never aborted from here; a cancellation is the runtime
                // shutting down under the dispatcher.
                return Err(EngineError::Channel(format!("session task: {join}")));
            }
        };
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
/// worker slot for each turn, and stops when the mailbox closes or the
/// session has been quiet for `idle_after`.
async fn session_task(
    shared: Arc<Shared>,
    session: SessionId,
    generation: u64,
    mut rx: mpsc::Receiver<Incoming>,
) -> Result<Stopped, EngineError> {
    let engine = &shared.engine;
    // Whether the turn that just ended may owe a rolling summary.
    let mut summary_due = false;
    loop {
        let next = {
            let mut pending = std::pin::pin!(next_in_mailbox(&mut rx, shared.idle_after));
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
                // the reply and types. If the user gets there first the
                // summary is dropped mid-flight — it is recomputed from the
                // store at the next boundary, and its input range is capped
                // by summary_input_max_chars, so an abandoned summary cannot
                // make the next one unbounded.
                tokio::select! {
                    biased;
                    next = &mut pending => next,
                    summarized = engine.maybe_summarize(&session) => {
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
                let text = engine.run_turn(incoming).await?;
                // Counted while the slot is still held, so the dispatcher
                // can never see every slot free and this turn uncounted.
                shared.turns_since_pass.fetch_add(1, Ordering::SeqCst);
                drop(permit);
                shared
                    .channel
                    .send(&session, &text)
                    .await
                    .map_err(|e| EngineError::Channel(e.to_string()))?;
                summary_due = true;
            }
            Next::Closed => {
                return Ok(Stopped {
                    session,
                    generation,
                    leftovers: Vec::new(),
                });
            }
            Next::Idle => {
                // Quiet for `idle_after`: let the mailbox go. Closing it first
                // makes the dispatcher's next `send` fail rather than land;
                // whatever landed before the close is handed back, to run on
                // a fresh task once this one is gone — not here, beside it.
                rx.close();
                let mut leftovers = Vec::new();
                while let Ok(m) = rx.try_recv() {
                    leftovers.push(m);
                }
                return Ok(Stopped {
                    session,
                    generation,
                    leftovers,
                });
            }
        }
    }
}
