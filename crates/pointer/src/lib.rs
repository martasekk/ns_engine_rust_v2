//! Drive another machine's pointer.
//!
//! The split this crate exists to enforce: every decision that can be got
//! wrong — which screen, which pixel, what a normalized coordinate means,
//! where an image's pixels land, how a gesture decomposes — is made here, in
//! code that runs and is tested anywhere. The agent on the target machine
//! receives absolute physical pixels and replays them.
//!
//! That is what makes a second operating system cheap: a new agent implements
//! four operations over its platform's injection API and inherits every
//! coordinate rule already proven here.
//!
//! Plan: `docs/superpowers/plans/2026-09-04-remote-pointer.md`.
//! Wire contract, for whoever writes the agent: `docs/pointer-protocol.md`.

pub mod agent;
pub mod client;
pub mod geom;
pub mod gesture;
pub mod mcp;
pub mod mock;
pub mod motion;
pub mod platform;
pub mod wire;

pub use geom::{Loc, Point, Rect, Screen, ScreenId, Screens};
pub use motion::{Motion, Rng};
pub use wire::{
    Button, ErrorKind, InputError, Key, Op, Request, Response, ResultBody, Step, PROTOCOL,
};

use async_trait::async_trait;

/// One machine's pointer, seen from here.
///
/// Three operations, because the fourth — `Hello` — is a property of a
/// connection rather than of a pointer. Implemented by the RPC client
/// (phase 1) and by `mock::MockPointer`.
#[async_trait]
pub trait Pointer: Send + Sync {
    async fn screens(&self) -> Result<Screens, InputError>;
    async fn position(&self) -> Result<Point, InputError>;
    /// Replay `steps` in order. Returns the screen-state token as of
    /// completion, so a caller can tell the layout moved under it.
    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError>;

    /// Optional (protocol 2). Defaults to `Unsupported` so a double or an
    /// older agent needs no change.
    async fn clipboard_read(&self) -> Result<String, InputError> {
        Err(unsupported())
    }
    async fn clipboard_write(&self, _text: &str) -> Result<(), InputError> {
        Err(unsupported())
    }
}

fn unsupported() -> InputError {
    InputError::Agent {
        kind: ErrorKind::Unsupported,
        detail: "clipboard is not available on this agent".into(),
    }
}

/// So a `Session` can be built over an `Arc<dyn Pointer>` and shared by
/// several owners — the engine's tools each hold one, and they must all drive
/// the same connection.
#[async_trait]
impl<P: Pointer + ?Sized> Pointer for std::sync::Arc<P> {
    async fn screens(&self) -> Result<Screens, InputError> {
        (**self).screens().await
    }
    async fn position(&self) -> Result<Point, InputError> {
        (**self).position().await
    }
    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
        (**self).perform(steps).await
    }
    async fn clipboard_read(&self) -> Result<String, InputError> {
        (**self).clipboard_read().await
    }
    async fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        (**self).clipboard_write(text).await
    }
}

/// Gesture timings. Defaults are unremarkable on purpose: they are what a
/// hand does, and anything watching for synthetic input notices instant
/// teleports and zero-length button presses.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub move_ms: u32,
    pub step_ms: u32,
    pub press_ms: u32,
    pub click_gap_ms: u32,
    pub settle_ms: u32,
    /// Mean gap between characters when typing, and how much it varies. ~70ms
    /// is a brisk 170 wpm; the jitter is there because perfectly uniform
    /// keystrokes drop characters in applications with debounced or
    /// autocomplete-driven input handling.
    pub key_ms: u32,
    pub key_jitter_ms: u32,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            move_ms: 300,
            step_ms: gesture::STEP_MS,
            press_ms: 40,
            click_gap_ms: 80,
            settle_ms: 30,
            key_ms: 70,
            key_jitter_ms: 30,
        }
    }
}

/// A pointer plus the layout it is being aimed at. Holding the two together
/// is what lets a `Loc` be resolved without a round trip per coordinate, and
/// what lets `state` be compared against the layout a caller reasoned about.
pub struct Session<P: Pointer> {
    pointer: P,
    screens: Screens,
    timing: Timing,
    motion: Motion,
    /// Seeded, so a session's paths are reproducible from its log. Behind a
    /// mutex because gestures are `&self` — the alternative is threading
    /// `&mut` through every call for the sake of a path generator.
    rng: std::sync::Mutex<Rng>,
}

impl<P: Pointer> Session<P> {
    /// Reads the layout once. Call `refresh` after a `state` change.
    pub async fn open(pointer: P) -> Result<Self, InputError> {
        let screens = pointer.screens().await?;
        Ok(Self {
            pointer,
            screens,
            timing: Timing::default(),
            motion: Motion::default(),
            rng: std::sync::Mutex::new(Rng::seed(0x5EED)),
        })
    }

    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Path shape. `Motion::Ease` is the deterministic one.
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.motion = motion;
        self
    }

    pub fn with_seed(self, seed: u64) -> Self {
        *self.rng.lock().unwrap() = Rng::seed(seed);
        self
    }

    pub fn screens(&self) -> &Screens {
        &self.screens
    }

    pub async fn refresh(&mut self) -> Result<(), InputError> {
        self.screens = self.pointer.screens().await?;
        Ok(())
    }

    /// A caller's location as a physical point, or an error naming what was
    /// wrong with it. Never silently clamps a bad screen id or an
    /// out-of-range fraction into something plausible: a click in the wrong
    /// place is worse than a refusal.
    pub fn resolve(&self, loc: &Loc) -> Result<Point, InputError> {
        match loc {
            Loc::Absolute { x, y } => {
                let p = Point::new(*x, *y);
                match self.screens.hit(p) {
                    Some(_) => Ok(p),
                    None => Err(InputError::NoSuchLocation(format!(
                        "({x}, {y}) is on no screen"
                    ))),
                }
            }
            Loc::Normalized { screen, x, y } => {
                let s = self.screens.get(screen).ok_or_else(|| {
                    InputError::NoSuchLocation(format!("no screen {:?}", screen.0))
                })?;
                s.normalized_to_physical(*x, *y).ok_or_else(|| {
                    InputError::NoSuchLocation(format!("({x}, {y}) is outside 0.0..=1.0"))
                })
            }
        }
    }

    /// The screen a point belongs to, for clamping a path.
    fn bounds_for(&self, p: Point) -> Rect {
        self.screens
            .hit(p)
            .map(|s| s.bounds)
            .or_else(|| self.screens.primary().map(|s| s.bounds))
            .unwrap_or(Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            })
    }

    pub async fn move_to(&self, loc: &Loc) -> Result<u64, InputError> {
        let to = self.resolve(loc)?;
        let from = self.pointer.position().await?;
        let steps = gesture::path(
            from,
            to,
            self.timing.move_ms,
            self.timing.step_ms,
            self.bounds_for(to),
            &self.motion,
            &mut self.rng.lock().unwrap(),
        );
        self.pointer.perform(&steps).await
    }

    /// Where the pointer is now.
    pub async fn position(&self) -> Result<Point, InputError> {
        self.pointer.position().await
    }

    /// Click without moving — for a target already under the pointer, and for
    /// the second click of a sequence where moving would break it.
    pub async fn click_here(&self, button: Button, count: u32) -> Result<u64, InputError> {
        let steps = gesture::click(
            button,
            count,
            self.timing.press_ms,
            self.timing.click_gap_ms,
        );
        self.pointer.perform(&steps).await
    }

    /// Move, then click. One round trip.
    pub async fn click_at(&self, loc: &Loc, button: Button, count: u32) -> Result<u64, InputError> {
        let to = self.resolve(loc)?;
        let from = self.pointer.position().await?;
        let mut steps = gesture::path(
            from,
            to,
            self.timing.move_ms,
            self.timing.step_ms,
            self.bounds_for(to),
            &self.motion,
            &mut self.rng.lock().unwrap(),
        );
        steps.push(Step::Sleep {
            ms: self.timing.settle_ms,
        });
        steps.extend(gesture::click(
            button,
            count,
            self.timing.press_ms,
            self.timing.click_gap_ms,
        ));
        self.pointer.perform(&steps).await
    }

    pub async fn drag(&self, from: &Loc, to: &Loc, button: Button) -> Result<u64, InputError> {
        let a = self.resolve(from)?;
        let b = self.resolve(to)?;
        let steps = gesture::drag(
            a,
            b,
            button,
            self.timing.move_ms,
            self.timing.settle_ms,
            self.bounds_for(b),
            &self.motion,
            &mut self.rng.lock().unwrap(),
        );
        self.pointer.perform(&steps).await
    }

    /// Read the target's clipboard.
    pub async fn clipboard_read(&self) -> Result<String, InputError> {
        self.pointer.clipboard_read().await
    }

    /// Replace the target's clipboard.
    pub async fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        self.pointer.clipboard_write(text).await
    }

    pub async fn scroll(&self, dx: i32, dy: i32) -> Result<u64, InputError> {
        self.pointer.perform(&[Step::Scroll { dx, dy }]).await
    }

    /// Type text into whatever has focus. Layout-independent: the characters
    /// are injected directly rather than mapped through the target's keyboard
    /// layout, so `@` arrives as `@` on a machine set to any layout.
    pub async fn type_text(&self, text: &str) -> Result<u64, InputError> {
        let steps = gesture::type_text(
            text,
            self.timing.key_ms,
            self.timing.key_jitter_ms,
            &mut self.rng.lock().unwrap(),
        );
        self.pointer.perform(&steps).await
    }

    /// One key, pressed and released — Enter, Tab, F5, an arrow.
    pub async fn press(&self, key: Key) -> Result<u64, InputError> {
        self.pointer
            .perform(&gesture::press(key, self.timing.press_ms))
            .await
    }

    /// Hold `modifiers`, tap `key`, release in reverse — Ctrl+C, Alt+Tab.
    ///
    /// If the agent refuses partway through, the modifiers it had already
    /// pressed are still down on the target. That is recovered here rather
    /// than left to the caller: a stuck Ctrl is not a failed operation, it is
    /// an unusable machine.
    pub async fn chord(&self, modifiers: &[Key], key: Key) -> Result<u64, InputError> {
        let steps = gesture::chord(modifiers, key, self.timing.press_ms);
        match self.pointer.perform(&steps).await {
            Ok(state) => Ok(state),
            Err(e) => {
                let release: Vec<Step> = modifiers
                    .iter()
                    .rev()
                    .map(|k| Step::Key {
                        key: k.clone(),
                        down: false,
                    })
                    .collect();
                // Best effort: if this fails too the connection is gone, and
                // the agent's own disconnect handler is the backstop.
                let _ = self.pointer.perform(&release).await;
                Err(e)
            }
        }
    }
}
