//! The protocol between this crate and the agent on the target machine.
//!
//! Newline-delimited JSON, one object per line, request and response paired
//! by `id`. Not JSON-RPC and not MCP: the agent is expected to be written by
//! hand, possibly not in Rust, so the format is the one that costs least to
//! implement correctly. MCP lives one layer up, where the client speaks it
//! over stdio.
//!
//! The agent implements four operations, and every coordinate it ever sees is
//! an absolute physical pixel in virtual-desktop space. It performs no
//! mapping, no clamping and no easing.

use crate::geom::{Point, Screen};
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. The agent rejects what it does not know
/// rather than guessing.
pub const PROTOCOL: u32 = 2;

/// One primitive the agent replays in order. `Perform` carries a list, so a
/// whole gesture — an eased move, a click, a drag — is a single round trip
/// however many primitives it decomposes into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Absolute physical pixel, virtual-desktop coordinates. Already clamped
    /// to a real screen by the caller.
    Move {
        x: i32,
        y: i32,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// Positive `dy` scrolls down, positive `dx` scrolls right. Units are
    /// notches, not pixels.
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// Press or release one key. For chords and for keys that produce no
    /// character — Enter, F5, Ctrl. Layout-*dependent* by nature: see `Key`.
    Key {
        key: Key,
        down: bool,
    },
    /// Type literal text, layout-independently. On Windows this is
    /// `SendInput` with `KEYEVENTF_UNICODE`, which injects the character
    /// itself and never consults the keyboard layout — the only reliable way
    /// to type `@` on a machine whose layout you do not know.
    Text {
        text: String,
    },
    Sleep {
        ms: u32,
    },
}

/// A key to press.
///
/// The distinction that matters, and the reason both this and `Step::Text`
/// exist: a *character* and a *key* are not the same thing. The physical key
/// labelled `Q` on QWERTY is `A` on AZERTY, so "press the Q key" and "type
/// the letter q" are different requests with different right answers.
///
/// - Typing an address, a path, a sentence -> `Step::Text`. Layout-independent,
///   and the only thing that reliably produces `@`, `#` or an accented letter.
/// - Ctrl+C, Enter, Tab, F5, arrow keys -> `Step::Key`. Unicode injection
///   cannot express a chord or a key that produces no character.
///
/// `Char` means "whichever key produces this character on the target's
/// current layout" — on Windows, `VkKeyScanW`. It is for the `c` in Ctrl+C,
/// not for typing prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Key {
    Char {
        c: char,
    },
    /// F1–F24.
    F {
        n: u8,
    },
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl,
    Alt,
    Shift,
    /// Windows key / Command.
    Meta,
    CtrlRight,
    AltRight,
    ShiftRight,
}

impl Key {
    pub fn ch(c: char) -> Self {
        Key::Char { c }
    }

    pub fn f(n: u8) -> Self {
        Key::F { n }
    }

    /// Whether releasing this key matters more than releasing an ordinary
    /// one. A stuck letter is noise; a stuck Ctrl makes the machine unusable
    /// until someone taps the physical key.
    pub fn is_modifier(&self) -> bool {
        matches!(
            self,
            Key::Ctrl
                | Key::Alt
                | Key::Shift
                | Key::Meta
                | Key::CtrlRight
                | Key::AltRight
                | Key::ShiftRight
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// First message on every connection. An agent that has not seen a valid
    /// `Hello` answers everything else with `Unauthorized`.
    Hello {
        token: String,
        protocol: u32,
    },
    Screens,
    Position,
    Perform {
        steps: Vec<Step>,
    },
    /// Read the target's clipboard. Protocol 2. The only way to get *data*
    /// back off the machine without capturing its screen: select-all, copy,
    /// read.
    ClipboardRead,
    /// Replace the target's clipboard. Protocol 2. The sane way to move bulk
    /// text — `Step::Text` per character is right for a search box and wrong
    /// for four thousand characters, which is eight thousand steps.
    ClipboardWrite {
        text: String,
    },
    /// The target's controls, uncompressed. Protocol 2, optional.
    UiTree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultBody {
    Ready {
        agent: String,
        platform: String,
        protocol: u32,
    },
    Screens {
        screens: Vec<Screen>,
        state: u64,
    },
    Position {
        x: i32,
        y: i32,
        state: u64,
    },
    Performed {
        steps: u32,
        state: u64,
    },
    Clipboard {
        text: String,
    },
    Ui {
        nodes: Vec<crate::ui::UiNode>,
        state: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// No valid `Hello`, or a bad token.
    Unauthorized,
    /// The machine's owner has taken control back: the local override is
    /// active. Distinct from `Blocked` because it resolves on its own.
    Suspended,
    /// The OS refused the injection. On Windows this is UIPI (a
    /// medium-integrity process cannot drive an elevated window) or the
    /// secure desktop (a UAC prompt, the lock screen, Ctrl+Alt+Del). It must
    /// be reported: `SendInput` returns success in the UIPI case and simply
    /// does nothing, so an agent that trusts the return value reports a click
    /// that never happened.
    Blocked,
    /// A `Move` landed on no screen. The caller clamps, so this means the
    /// layout changed underneath it — compare `state`.
    OutOfBounds,
    /// Known operation, not available on this platform.
    Unsupported,
    /// Unparseable, unknown op, or wrong protocol version.
    Protocol,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireError {
    pub kind: ErrorKind,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Response {
    pub fn ok(id: u64, result: ResultBody) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(WireError {
                kind,
                detail: detail.into(),
            }),
        }
    }
}

/// Anything that can go wrong between a caller and a pointer.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InputError {
    #[error("{kind:?}: {detail}")]
    Agent { kind: ErrorKind, detail: String },
    /// No screen by that id, or a normalized coordinate outside 0.0..=1.0.
    #[error("no such location: {0}")]
    NoSuchLocation(String),
    #[error("transport: {0}")]
    Transport(String),
}

impl From<WireError> for InputError {
    fn from(e: WireError) -> Self {
        InputError::Agent {
            kind: e.kind,
            detail: e.detail,
        }
    }
}

impl Step {
    pub fn move_to(p: Point) -> Self {
        Step::Move { x: p.x, y: p.y }
    }
}
