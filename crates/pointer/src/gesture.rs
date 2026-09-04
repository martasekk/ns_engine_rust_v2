//! Gestures as step sequences. Everything a caller asks for decomposes here,
//! on this side of the wire, into the four primitives the agent knows.
//!
//! This is why the agent stays small enough to rewrite for a new OS in an
//! afternoon: an eased 400 ms move is a list of points and sleeps computed
//! and clamped here, sent once, and replayed there by a `for` loop.

use crate::geom::{Point, Rect};
use crate::motion::{Motion, Rng};
use crate::wire::{Button, Key, Step};

/// Milliseconds between points of a moved path. ~120 Hz: fine enough that the
/// motion reads as continuous, coarse enough that a 400 ms move is 48 points
/// rather than a flood.
pub const STEP_MS: u32 = 8;

/// Points along a path from `from` to `to`, excluding `from` and always
/// ending exactly on `to` — rounding must never leave the pointer one pixel
/// short of where the caller aimed. Each point is clamped into `within`, so a
/// path cannot stray into the gap between two monitors on an L-shaped desktop.
///
/// The route's shape comes from `motion` (see that module); the sampling rate
/// and the pacing are the same whichever is chosen. Consecutive duplicate
/// points are dropped — a slow move over a short distance otherwise spends
/// most of its steps telling the agent to stay where it is.
pub fn path(
    from: Point,
    to: Point,
    duration_ms: u32,
    step_ms: u32,
    within: Rect,
    motion: &Motion,
    rng: &mut Rng,
) -> Vec<Step> {
    let step_ms = step_ms.max(1);
    if duration_ms == 0 || from == to {
        return vec![Step::move_to(within.clamp(to))];
    }
    let n = (duration_ms / step_ms).max(1);
    let pts = motion.points(
        (from.x as f64, from.y as f64),
        (to.x as f64, to.y as f64),
        n,
        rng,
    );
    let mut steps = Vec::with_capacity(pts.len() * 2);
    let mut last_pt = from;
    for (i, (x, y)) in pts.iter().enumerate() {
        let p = within.clamp(Point::new(x.round() as i32, y.round() as i32));
        if p != last_pt {
            if !steps.is_empty() {
                steps.push(Step::Sleep { ms: step_ms });
            }
            steps.push(Step::move_to(p));
            last_pt = p;
        }
        let _ = i;
    }
    let last = within.clamp(to);
    if last_pt != last {
        if !steps.is_empty() {
            steps.push(Step::Sleep { ms: step_ms });
        }
        steps.push(Step::move_to(last));
    }
    if steps.is_empty() {
        steps.push(Step::move_to(last));
    }
    steps
}

/// `count` presses of `button` where it already is. The gap between press and
/// release is deliberate: some targets ignore a down/up pair in the same
/// event tick, and a double click needs the pair spaced under the system's
/// double-click time.
pub fn click(button: Button, count: u32, press_ms: u32, gap_ms: u32) -> Vec<Step> {
    let mut steps = Vec::new();
    for i in 0..count.max(1) {
        if i > 0 {
            steps.push(Step::Sleep { ms: gap_ms });
        }
        steps.push(Step::Button { button, down: true });
        steps.push(Step::Sleep { ms: press_ms });
        steps.push(Step::Button {
            button,
            down: false,
        });
    }
    steps
}

/// Press at `from`, move, release at `to`. The settle sleeps matter: a drag
/// whose button goes down and moves in the same tick is read as a click by a
/// good deal of software.
#[allow(clippy::too_many_arguments)]
pub fn drag(
    from: Point,
    to: Point,
    button: Button,
    duration_ms: u32,
    settle_ms: u32,
    within: Rect,
    motion: &Motion,
    rng: &mut Rng,
) -> Vec<Step> {
    let mut steps = vec![Step::move_to(within.clamp(from))];
    steps.push(Step::Sleep { ms: settle_ms });
    steps.push(Step::Button { button, down: true });
    steps.push(Step::Sleep { ms: settle_ms });
    steps.extend(path(from, to, duration_ms, STEP_MS, within, motion, rng));
    steps.push(Step::Sleep { ms: settle_ms });
    steps.push(Step::Button {
        button,
        down: false,
    });
    steps
}

/// Type `text` a character at a time, with a jittered pause between.
///
/// Per-character rather than one `Text` step because the pacing belongs on
/// this side of the wire, like every other timing decision here — an agent
/// that owns typing rhythm is an agent that has to be rewritten per OS to
/// keep it. Uniform machine-gun keystrokes also drop characters in real
/// applications: autocomplete popups, IME candidate windows and web fields
/// with debounced handlers all lose input typed faster than a person can.
///
/// For bulk text — pasting a file, filling a large field — build a single
/// `Step::Text` directly instead. 4000 characters is 8000 steps this way.
pub fn type_text(text: &str, per_key_ms: u32, jitter_ms: u32, rng: &mut Rng) -> Vec<Step> {
    let mut steps = Vec::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 {
            let j = if jitter_ms == 0 {
                0.0
            } else {
                rng.signed() * jitter_ms as f64
            };
            let ms = (per_key_ms as f64 + j).max(1.0).round() as u32;
            steps.push(Step::Sleep { ms });
        }
        steps.push(Step::Text {
            text: c.to_string(),
        });
    }
    steps
}

/// A chord: hold `modifiers`, tap `key`, release in reverse order.
///
/// Reverse order is not cosmetic. Releasing Ctrl before C in a Ctrl+C can be
/// seen by the target as a bare `c` keystroke, which types a character into
/// whatever had focus instead of copying.
pub fn chord(modifiers: &[Key], key: Key, press_ms: u32) -> Vec<Step> {
    let mut steps = Vec::new();
    for m in modifiers {
        steps.push(Step::Key {
            key: m.clone(),
            down: true,
        });
    }
    steps.push(Step::Sleep { ms: press_ms });
    steps.push(Step::Key {
        key: key.clone(),
        down: true,
    });
    steps.push(Step::Sleep { ms: press_ms });
    steps.push(Step::Key { key, down: false });
    for m in modifiers.iter().rev() {
        steps.push(Step::Key {
            key: m.clone(),
            down: false,
        });
    }
    steps
}

/// One key, pressed and released.
pub fn press(key: Key, press_ms: u32) -> Vec<Step> {
    vec![
        Step::Key {
            key: key.clone(),
            down: true,
        },
        Step::Sleep { ms: press_ms },
        Step::Key { key, down: false },
    ]
}

/// Every key `steps` leaves held, in the order they must be released.
///
/// The client cannot rely on the agent to clean up after a batch it never
/// finished — a `perform` that stops halfway on a `blocked` leaves whatever
/// it had pressed still down. A stuck Ctrl makes a machine unusable, so this
/// exists to let a caller check its own batches and to build the recovery
/// sequence when one fails.
pub fn held_after(steps: &[Step]) -> Vec<Key> {
    let mut held: Vec<Key> = Vec::new();
    for s in steps {
        if let Step::Key { key, down } = s {
            if *down {
                if !held.contains(key) {
                    held.push(key.clone());
                }
            } else {
                held.retain(|k| k != key);
            }
        }
    }
    held.reverse();
    held
}

/// Release everything `steps` left held. Empty when the batch was balanced.
pub fn release_all(steps: &[Step]) -> Vec<Step> {
    held_after(steps)
        .into_iter()
        .map(|key| Step::Key { key, down: false })
        .collect()
}
