//! Waiting out the local override instead of arguing with it.
//!
//! The agent suspends injection whenever it sees the person at the keyboard,
//! and — this is the part that bites — *re-arms* that window on every attempt:
//! `perform` calls `local_activity()` first, and a fresh `suspend_ms` is set
//! before the refusal is returned. So a caller that retries immediately can
//! never get through. Each try pushes the window out again, and the harder it
//! tries the longer it stays shut.
//!
//! That is not hypothetical. Driving a real desktop produced 43 identical
//! `pointer_move` calls in one session, every one of them answered
//! `Suspended: local override, 3000ms remaining`, and the mouse never moved.
//! From the outside it looked like the agent was broken; the agent was doing
//! exactly its job, and the client was the thing behaving badly.
//!
//! The fix is to *believe the number the agent already sends back*. It says
//! how many milliseconds remain; wait that long, then try again. The waiting
//! is the whole point — attempts spaced by the full window cannot livelock the
//! way an immediate retry does, because the window is only re-armed by input
//! that arrives during it.
//!
//! The budget is finite (`MAX_WAITS`). An unattended session should survive
//! the owner typing for a minute without abandoning its task, and should still
//! stop eventually rather than wait on a machine somebody is plainly using:
//! after the budget, "the machine is in use" is the answer, and the refusal
//! goes back up with its reason intact.
//!
//! Only `perform` is wrapped, because only `perform` is suspended — the
//! reads (`screens`, `position`, `ui_tree`, the clipboard pair) do not consult
//! the override.

use std::time::Duration;

use crate::ui::UiNode;
use crate::wire::{ErrorKind, InputError, Step};
use crate::{Point, Pointer, Screens};

/// Longest we will ever wait for one lapse of the override.
///
/// `ns-pointerd` sets its window to 5s, so this clears it with room to spare.
/// It exists because the remaining time is parsed out of a human-readable
/// string: a malformed or hostile detail must not be able to park a model.
const LONGEST_WAIT: Duration = Duration::from_millis(6_000);

/// Parse the `Nms remaining` the agent puts in a suspension detail.
///
/// Returns `None` when the detail is not shaped that way, in which case the
/// refusal is passed straight through rather than guessed at: waiting a made-up
/// amount of time is worse than not waiting.
fn remaining_ms(detail: &str) -> Option<u64> {
    let (before, _) = detail.split_once("ms remaining")?;
    let digits: String = before
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

/// How long to wait before the next attempt, or `None` to not retry at all.
fn wait_for(e: &InputError) -> Option<Duration> {
    match e {
        InputError::Agent {
            kind: ErrorKind::Suspended,
            detail,
        } => {
            let ms = remaining_ms(detail)?;
            // The window is closed *for* ms; waiting exactly that long can
            // land on the boundary, so add a little.
            Some(Duration::from_millis(ms + 50).min(LONGEST_WAIT))
        }
        _ => None,
    }
}

/// How many times to wait the window out before reporting the refusal.
///
/// One retry is enough for a person who brushed the mouse once. It is not
/// enough for an unattended session, where the owner may type for a minute
/// and every keystroke re-arms the window: giving up after one wait would end
/// the task because somebody answered an email. Twelve waits of ~5s is about
/// a minute of continuous typing, after which "the machine is in use" really
/// is the answer.
///
/// This cannot spin: each attempt is preceded by a wait of the full remaining
/// window, so the attempts are spaced by the window itself. That spacing is
/// the entire difference between this and the immediate retry that produced
/// 43 refusals in a row.
pub const MAX_WAITS: usize = 12;

/// A `Pointer` that waits out a local override before giving up.
pub struct WaitOutOverride<P> {
    inner: P,
    max_waits: usize,
}

impl<P> WaitOutOverride<P> {
    pub fn new(inner: P) -> Self {
        WaitOutOverride {
            inner,
            max_waits: MAX_WAITS,
        }
    }

    /// How many times to wait the window out. Zero passes the first refusal
    /// straight up.
    pub fn with_max_waits(mut self, max_waits: usize) -> Self {
        self.max_waits = max_waits;
        self
    }
}

#[async_trait::async_trait]
impl<P: Pointer> Pointer for WaitOutOverride<P> {
    async fn screens(&self) -> Result<Screens, InputError> {
        self.inner.screens().await
    }

    async fn position(&self) -> Result<Point, InputError> {
        self.inner.position().await
    }

    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
        let mut waited = 0usize;
        loop {
            let refusal = match self.inner.perform(steps).await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            // Anything but a suspension, or a budget spent: the caller gets
            // the refusal with its reason intact.
            let Some(wait) = wait_for(&refusal) else {
                return Err(refusal);
            };
            if waited >= self.max_waits {
                return Err(refusal);
            }
            waited += 1;
            tokio::time::sleep(wait).await;
        }
    }

    async fn clipboard_read(&self) -> Result<String, InputError> {
        self.inner.clipboard_read().await
    }

    async fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        self.inner.clipboard_write(text).await
    }

    async fn ui_tree(&self, visible_only: bool) -> Result<Vec<UiNode>, InputError> {
        self.inner.ui_tree(visible_only).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn suspended(ms: u64) -> InputError {
        InputError::Agent {
            kind: ErrorKind::Suspended,
            detail: format!("local override, {ms}ms remaining"),
        }
    }

    /// Counts calls and refuses the first `refusals` of them. The waits in
    /// these tests are 1ms, so the retry is real but costs nothing.
    struct Flaky {
        refusals: usize,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Pointer for Flaky {
        async fn screens(&self) -> Result<Screens, InputError> {
            Err(InputError::Transport("not used".into()))
        }
        async fn position(&self) -> Result<Point, InputError> {
            Err(InputError::Transport("not used".into()))
        }
        async fn perform(&self, _steps: &[Step]) -> Result<u64, InputError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.refusals {
                Err(suspended(1))
            } else {
                Ok(7)
            }
        }
    }

    fn flaky(refusals: usize) -> Flaky {
        Flaky {
            refusals,
            calls: AtomicUsize::new(0),
        }
    }

    #[test]
    fn the_remaining_time_is_read_off_the_agents_own_detail() {
        assert_eq!(remaining_ms("local override, 2400ms remaining"), Some(2400));
        assert_eq!(remaining_ms("local override, 0ms remaining"), Some(0));
    }

    /// A detail in another shape is not guessed at: waiting a made-up amount
    /// of time is worse than passing the refusal straight up.
    #[test]
    fn an_unparseable_detail_means_no_retry() {
        assert_eq!(remaining_ms("local override"), None);
        assert_eq!(remaining_ms("ms remaining"), None);
        assert_eq!(remaining_ms("about threems remaining"), None);
        assert!(wait_for(&InputError::Agent {
            kind: ErrorKind::Suspended,
            detail: "local override".into(),
        })
        .is_none());
    }

    /// The cap is the whole reason the parse is allowed to be loose.
    #[test]
    fn the_wait_is_capped() {
        assert_eq!(wait_for(&suspended(600_000)), Some(LONGEST_WAIT));
    }

    #[test]
    fn only_a_suspension_is_waited_out() {
        for kind in [
            ErrorKind::Blocked,
            ErrorKind::OutOfBounds,
            ErrorKind::Unsupported,
        ] {
            assert!(
                wait_for(&InputError::Agent {
                    kind,
                    detail: "0ms remaining".into(),
                })
                .is_none(),
                "{kind:?} must not be retried"
            );
        }
        assert!(wait_for(&InputError::Transport("gone".into())).is_none());
    }

    #[tokio::test]
    async fn a_suspension_that_lapses_is_waited_out_and_then_succeeds() {
        let p = WaitOutOverride::new(flaky(1));
        assert_eq!(p.perform(&[]).await.expect("the retry succeeds"), 7);
        assert_eq!(p.inner.calls.load(Ordering::SeqCst), 2);
    }

    /// The budget is what lets an unattended run survive someone typing for a
    /// while: eleven refusals in a row is not the end of the task.
    #[tokio::test]
    async fn a_run_of_refusals_inside_the_budget_still_gets_through() {
        let p = WaitOutOverride::new(flaky(11));
        assert_eq!(p.perform(&[]).await.expect("gets through"), 7);
        assert_eq!(p.inner.calls.load(Ordering::SeqCst), 12);
    }

    /// The loop this module exists to prevent. A person who still has their
    /// hand on the mouse is eventually told so, and the attempts are bounded
    /// rather than forty-three in a row.
    #[tokio::test]
    async fn a_person_who_is_still_there_is_reported_once_the_budget_is_spent() {
        let p = WaitOutOverride::new(flaky(usize::MAX)).with_max_waits(3);
        let e = p.perform(&[]).await.expect_err("still suspended");
        assert!(
            matches!(
                e,
                InputError::Agent {
                    kind: ErrorKind::Suspended,
                    ..
                }
            ),
            "the reason must survive: {e:?}"
        );
        assert_eq!(
            p.inner.calls.load(Ordering::SeqCst),
            4,
            "the first attempt plus one per wait in the budget, and no more"
        );
    }

    /// Zero budget is the old behaviour, and the escape hatch for a caller
    /// that would rather hear about the refusal immediately.
    #[tokio::test]
    async fn a_zero_budget_passes_the_first_refusal_straight_up() {
        let p = WaitOutOverride::new(flaky(usize::MAX)).with_max_waits(0);
        assert!(p.perform(&[]).await.is_err());
        assert_eq!(p.inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_call_that_works_first_time_is_not_delayed() {
        let p = WaitOutOverride::new(flaky(0));
        assert_eq!(p.perform(&[]).await.expect("succeeds"), 7);
        assert_eq!(p.inner.calls.load(Ordering::SeqCst), 1);
    }
}
