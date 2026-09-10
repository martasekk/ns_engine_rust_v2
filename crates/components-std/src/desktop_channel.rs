//! The desktop's compose box as a second mouth on an existing channel.
//!
//! `nspointer::messages` is the wire; this is what makes it a conversation.
//! A line the owner types into the badge's compose box arrives as an ordinary
//! turn, and the reply to it goes back to the badge instead of to a terminal
//! the owner may not be looking at.
//!
//! Three decisions worth stating, because each could sensibly have gone the
//! other way.
//!
//! **The same session as the wrapping channel, not one of its own.** The point
//! of the box is to say something about what the model is doing *now* — "the
//! dialog is on the second monitor", "no, the other file", "yes, that one". A
//! separate session would give the owner a model with no idea what they were
//! talking about. So a desktop line joins the conversation already in progress,
//! and replies are routed by where the last turn came in rather than by
//! session id.
//!
//! **The queue is not the file.** `take` drains what the agent holds in
//! memory; `outbox.jsonl` is the owner's own record and is left alone. An
//! agent that restarts comes back with an empty queue, and that is deliberate:
//! replaying the file would deliver lines typed in some earlier session at
//! whatever moment the process happened to come back, which is the opposite of
//! what "I am telling you this now" means.
//!
//! **A dead messages service does not take the chat down.** If the agent goes
//! away the poller stops and says so once; stdin keeps working. The desktop is
//! an addition to the conversation, never a dependency of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nscore::{Channel, ChannelError, Incoming, SessionId};
use nspointer::messages::{Message, Messages};
use tokio::sync::{mpsc, Mutex};

/// How often to ask the agent whether anything was typed.
///
/// The box is a person pressing Enter, so this is a human-latency problem, not
/// a throughput one: half a second is imperceptible to the owner and is 2
/// requests a second against a service answering from memory on loopback.
pub const POLL_EVERY: Duration = Duration::from_millis(500);

/// Where a reply goes when the turn came from the badge.
///
/// A trait rather than the client itself so the routing — the one thing in
/// this module that can be got wrong — is testable without a socket.
#[async_trait]
pub trait Say: Send + Sync {
    async fn say(&self, text: &str) -> Result<(), String>;
}

#[async_trait]
impl Say for Arc<Mutex<Messages>> {
    async fn say(&self, text: &str) -> Result<(), String> {
        self.lock().await.say(text).await
    }
}

/// Wraps a channel so the desktop's compose box is a second way in.
///
/// `recv` and `send` take `&self` (multi-conversation plan Phase 2, D2.1):
/// the engine keeps one `recv` pending while replies go out through the same
/// handle. So the receiver sits behind a lock — one `recv` at a time, never
/// contended — and the two routing facts are an atomic and a lock.
pub struct WithDesktop<C> {
    inner: C,
    rx: Mutex<mpsc::Receiver<Message>>,
    badge: Box<dyn Say>,
    /// Which way the last turn came in, and therefore where its reply goes.
    /// The engine answers the turn it was just handed, so this is exactly as
    /// long-lived as it needs to be.
    last_from_desktop: AtomicBool,
    /// The session to put desktop lines on: whatever the wrapped channel last
    /// used, so this holds for any channel rather than only the CLI one. The
    /// initial value only matters if the owner types into the box before
    /// anything has come in the other way.
    session: std::sync::Mutex<SessionId>,
}

impl<C: Channel> WithDesktop<C> {
    /// Takes ownership of a connected client and starts polling it.
    ///
    /// The returned channel and the poller share the one connection: the
    /// service is one line in, one line out, so a `say` must not interleave
    /// with a `take`.
    pub fn spawn(inner: C, client: Messages) -> WithDesktop<C> {
        let client = Arc::new(Mutex::new(client));
        let (tx, rx) = mpsc::channel(64);
        let polling = client.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL_EVERY).await;
                let taken = { polling.lock().await.take().await };
                match taken {
                    Ok(lines) => {
                        for line in lines {
                            // A full receiver means the engine is far behind;
                            // dropping is wrong and blocking is right, since
                            // the agent's queue has already given them up.
                            if tx.send(line).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "desktop: the messages service stopped answering ({e}); \
                             the compose box will not reach this session again. \
                             Typing here still works."
                        );
                        return;
                    }
                }
            }
        });
        WithDesktop {
            inner,
            rx: Mutex::new(rx),
            badge: Box::new(client),
            last_from_desktop: AtomicBool::new(false),
            session: std::sync::Mutex::new(SessionId("cli".into())),
        }
    }
}

#[async_trait]
impl<C: Channel> Channel for WithDesktop<C> {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        // On cancellation: the inner channel is a terminal read, and a
        // terminal delivers a whole line at once when Enter is pressed, so
        // there is no half-read line to lose while the owner is still typing.
        // The window in which a cancel could drop anything is between the line
        // arriving and the read completing.
        let mut rx = self.rx.lock().await;
        tokio::select! {
            typed = rx.recv() => match typed {
                Some(m) => {
                    self.last_from_desktop.store(true, Ordering::SeqCst);
                    // The session is the inner channel's, deliberately: see
                    // the module header. The stamp is dropped here -- it is
                    // the owner's record in `outbox.jsonl`, not something the
                    // model needs in the turn.
                    let session = self.session.lock().expect("session lock").clone();
                    println!("desk> {}", m.text);
                    Ok(Incoming { session, text: m.text })
                }
                // The poller is gone; the desktop is an addition, not a
                // dependency, so fall back to the inner channel for good.
                None => {
                    self.last_from_desktop.store(false, Ordering::SeqCst);
                    self.inner.recv().await
                }
            },
            from_inner = self.inner.recv() => {
                self.last_from_desktop.store(false, Ordering::SeqCst);
                if let Ok(incoming) = &from_inner {
                    // Follow the wrapped channel rather than assuming its id,
                    // so a desktop line lands in whatever conversation this
                    // channel is actually running.
                    *self.session.lock().expect("session lock") = incoming.session.clone();
                }
                from_inner
            }
        }
    }

    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
        if !self.last_from_desktop.load(Ordering::SeqCst) {
            return self.inner.send(session, text).await;
        }
        // Answer where the question was asked. The reply also goes to the
        // terminal, because the owner running both is the normal case and a
        // badge holds one line.
        println!("bot> {text}");
        match self.badge.say(text).await {
            Ok(()) => Ok(()),
            // A badge that will not take the line is worth saying out loud,
            // but the turn itself succeeded.
            Err(e) => {
                eprintln!("desktop: could not put the reply on the badge ({e})");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// An inner channel that hands out scripted lines and records what it was
    /// asked to send, so a test can tell a reply that went to the terminal
    /// from one that went to the badge.
    struct Scripted {
        lines: StdMutex<Vec<String>>,
        sent: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl Channel for Scripted {
        async fn recv(&self) -> Result<Incoming, ChannelError> {
            let next = self.lines.lock().unwrap().pop();
            match next {
                // Deliberately not "cli": the desktop line must pick this up
                // rather than assume the CLI channel's id.
                Some(text) => Ok(Incoming {
                    session: SessionId("wrapped".into()),
                    text,
                }),
                // Nothing scripted: block, so the desktop branch is the only
                // one that can complete.
                None => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
        async fn send(&self, _s: &SessionId, text: &str) -> Result<(), ChannelError> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    /// Records what was put on the badge, and can be told to refuse.
    struct Badge {
        said: Arc<StdMutex<Vec<String>>>,
        refuse: bool,
    }

    #[async_trait]
    impl Say for Badge {
        async fn say(&self, text: &str) -> Result<(), String> {
            self.said.lock().unwrap().push(text.to_string());
            if self.refuse {
                return Err("no badge".into());
            }
            Ok(())
        }
    }

    /// A recording of what one side was told, shared with the test.
    type Said = Arc<StdMutex<Vec<String>>>;

    /// The channel under test, the way to push a desktop line into it, and
    /// what each side received.
    type Rig = (WithDesktop<Scripted>, mpsc::Sender<Message>, Said, Said);

    /// Builds a channel with no socket behind it: scripted lines on the
    /// terminal side, a recording badge on the other.
    fn channel(terminal_lines: Vec<&str>, refuse: bool) -> Rig {
        let to_terminal = Arc::new(StdMutex::new(Vec::new()));
        let to_badge = Arc::new(StdMutex::new(Vec::new()));
        let inner = Scripted {
            lines: StdMutex::new(terminal_lines.iter().rev().map(|s| s.to_string()).collect()),
            sent: to_terminal.clone(),
        };
        let (tx, rx) = mpsc::channel(4);
        let ch = WithDesktop {
            inner,
            rx: Mutex::new(rx),
            badge: Box::new(Badge {
                said: to_badge.clone(),
                refuse,
            }),
            last_from_desktop: AtomicBool::new(false),
            session: std::sync::Mutex::new(SessionId("cli".into())),
        };
        (ch, tx, to_terminal, to_badge)
    }

    fn typed(text: &str) -> Message {
        Message {
            at: "2026-09-07T14:13:27Z".into(),
            text: text.into(),
        }
    }

    /// A desktop line joins the conversation already in progress rather than
    /// starting one of its own, which is the whole reason the box is useful.
    #[tokio::test]
    async fn a_desktop_line_arrives_on_the_inner_channels_session() {
        let (ch, tx, _term, _badge) = channel(vec!["typed at the terminal"], false);

        let first = ch.recv().await.unwrap();
        assert_eq!(first.text, "typed at the terminal");

        tx.send(typed("the dialog is on the second monitor"))
            .await
            .unwrap();
        let second = ch.recv().await.unwrap();
        assert_eq!(second.text, "the dialog is on the second monitor");
        assert_eq!(
            second.session, first.session,
            "a desktop line must join the conversation, not start one"
        );
        assert_eq!(
            second.session,
            SessionId("wrapped".into()),
            "the session is the wrapped channel's, not an assumed \"cli\""
        );
    }

    /// The substance of the module: a reply follows the turn it answers.
    /// Getting this backwards would answer the badge in the terminal, where
    /// the owner is not looking.
    #[tokio::test]
    async fn a_reply_goes_back_the_way_the_turn_came_in() {
        let (ch, tx, term, badge) = channel(vec!["from the terminal"], false);

        let t = ch.recv().await.unwrap();
        ch.send(&t.session, "answer to the terminal").await.unwrap();
        assert_eq!(*term.lock().unwrap(), vec!["answer to the terminal"]);
        assert!(badge.lock().unwrap().is_empty(), "the badge was not asked");

        tx.send(typed("from the badge")).await.unwrap();
        let d = ch.recv().await.unwrap();
        ch.send(&d.session, "answer to the badge").await.unwrap();
        assert_eq!(*badge.lock().unwrap(), vec!["answer to the badge"]);
        assert_eq!(
            term.lock().unwrap().len(),
            1,
            "the badge's answer must not also go to the inner channel"
        );
    }

    /// A badge that will not take the line is worth saying out loud, but the
    /// turn itself succeeded: the model answered, and failing the turn would
    /// lose that answer.
    #[tokio::test]
    async fn a_badge_that_refuses_does_not_fail_the_turn() {
        let (ch, tx, _term, badge) = channel(vec![], true);
        tx.send(typed("from the badge")).await.unwrap();
        let d = ch.recv().await.unwrap();
        assert!(ch.send(&d.session, "an answer").await.is_ok());
        assert_eq!(*badge.lock().unwrap(), vec!["an answer"]);
    }

    /// The desktop is an addition to the conversation, never a dependency of
    /// it: when the poller gives up, the terminal keeps working.
    #[tokio::test]
    async fn a_dead_poller_leaves_the_terminal_working() {
        let (ch, tx, _term, _badge) = channel(vec!["still here"], false);
        drop(tx);
        let t = ch.recv().await.unwrap();
        assert_eq!(t.text, "still here");
    }
}
