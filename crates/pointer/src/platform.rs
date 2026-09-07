//! The one trait a new machine implements.
//!
//! This is the entire per-operating-system surface. Everything else in this
//! crate — coordinate mapping, clamping, path shape, gesture composition,
//! typing rhythm, framing, authentication, rate limiting, the local override,
//! held-key recovery, the audit log — is written once, above this line, and
//! is already tested.
//!
//! Eight required methods, all synchronous, all taking values that need no
//! interpretation: a `Point` is an absolute physical pixel in virtual-desktop
//! coordinates, and it has already been checked against a real screen. The
//! rest have defaults, and every default is the slow or the pessimistic
//! answer rather than a wrong one — override them for speed (`state`,
//! `ui_tree_visible`) or for honesty (`local_hook_ok`, `armed`).
//!
//! Synchronous is a promise about the signature, not about the cost: the
//! agent runs `ui_tree` and the clipboard calls on a blocking thread, so a
//! two-second UI Automation walk or a clipboard held by another process does
//! not stall the connections it is not serving.
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

    /// The display-configuration counter alone, without the enumeration.
    ///
    /// The agent stamps `state` on every `performed`, `position` and `ui`
    /// reply, and the default gets it the only way it can: a full
    /// `screens()`. On Windows that is `EnumDisplayMonitors`,
    /// `QueryDisplayConfig`, two `DisplayConfigGetDeviceInfo` per path and
    /// `GetDpiForMonitor` per monitor, after every click, to read one number.
    /// Override it with something that cannot go stale — a per-call
    /// fingerprint of the monitor rectangles is tens of microseconds — and
    /// skip only the identity queries. Must agree with `screens().state`.
    fn state(&self) -> u64 {
        self.screens().map(|s| s.state).unwrap_or(0)
    }

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
    /// Returning `false` always is a valid, and dangerous, implementation —
    /// which is what `local_hook_ok` exists to make visible.
    fn local_activity(&self) -> bool;

    /// Whether the thing that watches for the user is actually installed and
    /// running.
    ///
    /// `local_activity() -> bool` cannot distinguish *nothing happened* from
    /// *nothing is watching*, and those are the same answer forever if the
    /// hook silently failed to register. This separates them.
    ///
    /// **Defaults to `false`, and that is deliberate.** An agent that has not
    /// said it installed a hook is assumed not to have one, and says so
    /// loudly rather than presenting a dead brake as a working one. Return
    /// `true` only where the raw-input registration or low-level hook
    /// actually succeeded — and return `false` again if it later goes away.
    fn local_hook_ok(&self) -> bool {
        false
    }

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

    /// `ui_tree` without the nodes that are off screen — the one filter the
    /// platform may apply, and only because the caller asked for it
    /// (`Op::UiTree { visible_only: true }`). On a real desktop 73% of the
    /// tree came back `visible: false` and the compressor dropped every one
    /// of them first thing; a provider-side `IsOffscreen == false` condition
    /// halves the walk and returns exactly the 27% that survive.
    ///
    /// The default forwards to `ui_tree`, so an agent that has not
    /// implemented it is merely slower, never wrong: the compressor drops
    /// the same nodes either way. An override must return every node that
    /// `ui_tree` would report as `visible: true` — a stricter filter is the
    /// second copy of the compressor's judgement this seam exists to avoid.
    fn ui_tree_visible(&self) -> Result<Vec<crate::ui::UiNode>, InputError> {
        self.ui_tree()
    }

    /// Whether a person at the machine has armed input for this process,
    /// where such a gate exists. `None` — the default — means "no gate, or
    /// not saying", and is reported to the client as exactly that. Return
    /// `Some` only from an agent that will answer the first `perform` with
    /// `needs_confirmation` until the chord is pressed: then the client can
    /// ask the person before that refusal rather than after it.
    fn armed(&self) -> Option<bool> {
        None
    }
}

fn unsupported(what: &str) -> InputError {
    InputError::Agent {
        kind: crate::wire::ErrorKind::Unsupported,
        detail: format!("{what} is not implemented by this agent"),
    }
}

/// So a `Platform` can be shared without a hand-written wrapper.
///
/// This exists because writing that wrapper went wrong three times in a row,
/// always the same way: it forwards the required methods, inherits the
/// *defaults* for the optional ones, and the capability silently reports as
/// unsupported instead of failing to compile. A blanket forward removes the
/// chance to get it wrong.
impl<P: Platform + ?Sized> Platform for std::sync::Arc<P> {
    fn screens(&self) -> Result<Screens, InputError> {
        (**self).screens()
    }
    fn state(&self) -> u64 {
        (**self).state()
    }
    fn position(&self) -> Result<Point, InputError> {
        (**self).position()
    }
    fn move_to(&self, p: Point) -> Result<(), InputError> {
        (**self).move_to(p)
    }
    fn button(&self, b: Button, down: bool) -> Result<(), InputError> {
        (**self).button(b, down)
    }
    fn scroll(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        (**self).scroll(dx, dy)
    }
    fn key(&self, k: &Key, down: bool) -> Result<(), InputError> {
        (**self).key(k, down)
    }
    fn text(&self, s: &str) -> Result<(), InputError> {
        (**self).text(s)
    }
    fn local_activity(&self) -> bool {
        (**self).local_activity()
    }
    fn local_hook_ok(&self) -> bool {
        (**self).local_hook_ok()
    }
    fn clipboard_read(&self) -> Result<String, InputError> {
        (**self).clipboard_read()
    }
    fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        (**self).clipboard_write(text)
    }
    fn ui_tree(&self) -> Result<Vec<crate::ui::UiNode>, InputError> {
        (**self).ui_tree()
    }
    fn ui_tree_visible(&self) -> Result<Vec<crate::ui::UiNode>, InputError> {
        (**self).ui_tree_visible()
    }
    fn armed(&self) -> Option<bool> {
        (**self).armed()
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
    pub hook_ok: std::sync::atomic::AtomicBool,
    /// When set, every input call fails with it.
    pub refuse: Option<InputError>,
}

impl NullPlatform {
    pub fn new(screens: Screens) -> Self {
        let p = Self {
            screens: Some(screens),
            ..Default::default()
        };
        p.hook_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        p
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

    fn local_hook_ok(&self) -> bool {
        self.hook_ok.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn clipboard_read(&self) -> Result<String, InputError> {
        Ok(self.clipboard.lock().unwrap().clone())
    }

    fn clipboard_write(&self, text: &str) -> Result<(), InputError> {
        *self.clipboard.lock().unwrap() = text.to_string();
        Ok(())
    }
}
