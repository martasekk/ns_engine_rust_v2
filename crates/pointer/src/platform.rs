//! The one trait a new machine implements.
//!
//! This is the entire per-operating-system surface. Everything else in this
//! crate — coordinate mapping, clamping, path shape, gesture composition,
//! typing rhythm, framing, authentication, rate limiting, the local override,
//! held-key recovery, the audit log — is written once, above this line, and
//! is already tested.
//!
//! Eight methods, all synchronous, all taking values that need no
//! interpretation: a `Point` is an absolute physical pixel in virtual-desktop
//! coordinates, and it has already been checked against a real screen.
//!
//! Implementing this for Windows is the whole of the Windows work.

use crate::geom::{Point, Screens};
use crate::wire::{Button, InputError, Key};

pub trait Platform: Send + Sync {
    /// The current display layout.
    ///
    /// `Screen::id` **must be stable** across reboots, driver updates and
    /// replugging — derive it from the monitor's device path or EDID, never a
    /// GDI device index, which Windows reassigns on all three. `bounds` are
    /// physical pixels with a signed origin; `scale` is 1.5 at 150%.
    ///
    /// `Screens::state` increments on any display-configuration change.
    fn screens(&self) -> Result<Screens, InputError>;

    fn position(&self) -> Result<Point, InputError>;

    /// Absolute, already clamped to a real screen by the caller. On Windows:
    /// `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`,
    /// normalized 0–65535 across the **virtual desktop**, not the primary
    /// monitor, from a per-monitor-DPI-aware thread.
    fn move_to(&self, p: Point) -> Result<(), InputError>;

    /// Press or release, wherever the pointer currently is. Never moves.
    fn button(&self, b: Button, down: bool) -> Result<(), InputError>;

    /// Notches, not pixels. Positive `dy` scrolls down.
    fn scroll(&self, dx: i32, dy: i32) -> Result<(), InputError>;

    /// One key. `Key::Char` means whichever key produces that character on
    /// the *current layout* — `VkKeyScanW` on Windows.
    fn key(&self, k: &Key, down: bool) -> Result<(), InputError>;

    /// Literal characters, layout-independent. On Windows: `SendInput` with
    /// `KEYEVENTF_UNICODE`, `wVk = 0`, `wScan` = the UTF-16 code unit — and
    /// a non-BMP character (an emoji) is two inputs, one per surrogate.
    fn text(&self, s: &str) -> Result<(), InputError>;

    /// Whether a human has touched this machine since the last call:
    /// physical mouse movement over a threshold, or a keystroke that did not
    /// come from us. Drives the local override, which is the only reason the
    /// person at the keyboard can take their machine back.
    ///
    /// Returning `false` always is a valid, and dangerous, implementation.
    fn local_activity(&self) -> bool;

    /// The target's clipboard, as text. **Optional**: the default answers
    /// `Unsupported`, so an agent can ship without it and the capability
    /// degrades to an error rather than a compile failure.
    ///
    /// Worth having because it is the only way to get *data* back off the
    /// machine without capturing its screen — select-all, copy, read.
    fn clipboard_read(&self) -> Result<String, InputError> {
        Err(unsupported("clipboard_read"))
    }

    /// Replace the target's clipboard. **Optional**, as above. The sane way
    /// to move bulk text: `Step::Text` is right for a search box and wrong
    /// for four thousand characters, which is eight thousand steps.
    fn clipboard_write(&self, _text: &str) -> Result<(), InputError> {
        Err(unsupported("clipboard_write"))
    }

    /// The target's controls, flattened. **Optional**, defaulted to
    /// `Unsupported`. On Windows this is UI Automation.
    ///
    /// Return everything you can see and do **no** filtering: the caller
    /// compresses (`ui::compress`), and a filter here would be a second,
    /// untested, per-platform copy of that judgement. Bounds come back as a
    /// centre point plus a height, both in absolute virtual-desktop pixels.
    fn ui_tree(&self) -> Result<Vec<crate::ui::UiNode>, InputError> {
        Err(unsupported("ui_tree"))
    }
}

fn unsupported(what: &str) -> InputError {
    InputError::Agent {
        kind: crate::wire::ErrorKind::Unsupported,
        detail: format!("{what} is not implemented by this agent"),
    }
}

/// A `Platform` that records and never touches anything, for exercising the
/// agent's guards without a desktop.
#[derive(Default)]
pub struct NullPlatform {
    pub screens: Option<Screens>,
    pub applied: std::sync::Mutex<Vec<String>>,
    pub local: std::sync::atomic::AtomicBool,
    pub clipboard: std::sync::Mutex<String>,
    /// When set, every input call fails with it.
    pub refuse: Option<InputError>,
}

impl NullPlatform {
    pub fn new(screens: Screens) -> Self {
        Self {
            screens: Some(screens),
            ..Default::default()
        }
    }

    pub fn refusing(screens: Screens, e: InputError) -> Self {
        Self {
            screens: Some(screens),
            refuse: Some(e),
            ..Default::default()
        }
    }

    pub fn log(&self) -> Vec<String> {
        self.applied.lock().unwrap().clone()
    }

    fn note(&self, s: String) -> Result<(), InputError> {
        if let Some(e) = &self.refuse {
            return Err(e.clone());
        }
        self.applied.lock().unwrap().push(s);
        Ok(())
    }
}

impl Platform for NullPlatform {
    fn screens(&self) -> Result<Screens, InputError> {
        self.screens
            .clone()
            .ok_or_else(|| InputError::Transport("no screens".into()))
    }

    fn position(&self) -> Result<Point, InputError> {
        Ok(Point::new(0, 0))
    }

    fn move_to(&self, p: Point) -> Result<(), InputError> {
        self.note(format!("move {},{}", p.x, p.y))
    }

    fn button(&self, b: Button, down: bool) -> Result<(), InputError> {
        self.note(format!("button {b:?} {down}"))
    }

    fn scroll(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        self.note(format!("scroll {dx},{dy}"))
    }

    fn key(&self, k: &Key, down: bool) -> Result<(), InputError> {
        self.note(format!("key {k:?} {down}"))
    }

    fn text(&self, s: &str) -> Result<(), InputError> {
        self.note(format!("text {s}"))
    }

    fn local_activity(&self) -> bool {
        self.local.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    fn clipboard_read(&self) -> Result<String, InputError> {
        Ok(self.clipboard.lock().unwrap().clone())
    }

    fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        *self.clipboard.lock().unwrap() = text.to_string();
        Ok(())
    }
}
