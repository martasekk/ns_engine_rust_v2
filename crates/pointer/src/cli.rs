//! A shell for the pointer protocol: one command, one connection, one line
//! of output.
//!
//! This is the "do this specific thing" the Windows session wanted a step
//! runner for, arrived at from the other direction. It goes through the real
//! socket, so it inherits the rate limit, the local override, held-key
//! release and the audit log — none of which a stdin interpreter beside the
//! `Platform` would have had. And there is only ever one execution path, so
//! nothing drifts.
//!
//! Meant to run **beside the agent, over loopback**. There is no network hop
//! to arrange, no `allow_remote`, and the machine that has the desktop is the
//! machine that has the shell.

use crate::ui::UiView;
use crate::wire::{Button, InputError, Key};
use crate::{Loc, Pointer, Session};

pub const USAGE: &str = "\
ns-pointer — drive a machine through a running ns-pointerd

  ns-pointer screens                     the displays, with ids and DPI scale
  ns-pointer position                    where the pointer is
  ns-pointer move X Y                    move (absolute virtual-desktop pixels)
  ns-pointer click [X Y] [--right|--middle] [--double]
  ns-pointer drag X1 Y1 X2 Y2
  ns-pointer scroll DX DY                notches; positive DY scrolls down
  ns-pointer type TEXT                   layout-independent, handles @ and emoji
  ns-pointer key KEY                     enter, f5, ctrl+c, ctrl+shift+s
  ns-pointer ui [QUERY]                  the controls, as text
  ns-pointer find NAME                   a control's click point
  ns-pointer clip [TEXT]                 read the clipboard, or set it

  NS_POINTER_ADDR   default 127.0.0.1:7373
  NS_POINTER_TOKEN  required — there is no unauthenticated mode
";

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Screens,
    Position,
    Move {
        x: i32,
        y: i32,
    },
    Click {
        at: Option<(i32, i32)>,
        button: Button,
        count: u32,
    },
    Drag {
        from: (i32, i32),
        to: (i32, i32),
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Type(String),
    Key {
        mods: Vec<Key>,
        key: Key,
    },
    Ui(Option<String>),
    Find(String),
    ClipRead,
    ClipWrite(String),
}

/// `ctrl+shift+s` — everything before the last `+` is a modifier.
fn parse_chord(s: &str) -> Result<(Vec<Key>, Key), String> {
    // Empty parts are not skipped: `ctrl+` means the key was forgotten, and
    // silently reading it as "press ctrl" would hold a modifier down as if
    // that had been asked for.
    let parts: Vec<&str> = s.split('+').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!(
            "{s:?} has an empty part — a trailing + is a missing key"
        ));
    }
    let Some((last, mods)) = parts.split_last() else {
        return Err("empty key".into());
    };
    let key = crate::mcp::parse_key(last).ok_or_else(|| format!("unknown key: {last}"))?;
    let mut out = Vec::new();
    for m in mods {
        let k = crate::mcp::parse_key(m).ok_or_else(|| format!("unknown modifier: {m}"))?;
        if !k.is_modifier() {
            return Err(format!("{m} is not a modifier"));
        }
        out.push(k);
    }
    Ok((out, key))
}

fn num(v: Option<&String>, what: &str) -> Result<i32, String> {
    v.ok_or_else(|| format!("missing {what}"))?
        .parse()
        .map_err(|_| format!("{what} must be a whole number"))
}

pub fn parse(args: &[String]) -> Result<Cmd, String> {
    let flags: Vec<&String> = args.iter().filter(|a| a.starts_with("--")).collect();
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let at = |i: usize| pos.get(i).copied();
    let verb = at(0).ok_or_else(|| "no command".to_string())?.as_str();

    Ok(match verb {
        "screens" => Cmd::Screens,
        "position" | "pos" => Cmd::Position,
        "move" => Cmd::Move {
            x: num(at(1), "X")?,
            y: num(at(2), "Y")?,
        },
        "click" => {
            let button = if flags.iter().any(|f| *f == "--right") {
                Button::Right
            } else if flags.iter().any(|f| *f == "--middle") {
                Button::Middle
            } else {
                Button::Left
            };
            let count = if flags.iter().any(|f| *f == "--double") {
                2
            } else {
                1
            };
            let at_ = match (at(1), at(2)) {
                (Some(_), Some(_)) => Some((num(at(1), "X")?, num(at(2), "Y")?)),
                (None, None) => None,
                // Half a coordinate is a typo, and clicking "where the pointer
                // happens to be" because one number was dropped is the kind of
                // mistake that lands on something.
                _ => return Err("click takes both X and Y, or neither".into()),
            };
            Cmd::Click {
                at: at_,
                button,
                count,
            }
        }
        "drag" => Cmd::Drag {
            from: (num(at(1), "X1")?, num(at(2), "Y1")?),
            to: (num(at(3), "X2")?, num(at(4), "Y2")?),
        },
        "scroll" => Cmd::Scroll {
            dx: num(at(1), "DX")?,
            dy: num(at(2), "DY")?,
        },
        // Everything after the verb, so quoting is the shell's problem and a
        // sentence does not need it.
        "type" => {
            let text = pos[1..]
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                return Err("type needs some text".into());
            }
            Cmd::Type(text)
        }
        "key" => {
            let (mods, key) = parse_chord(at(1).ok_or("key needs a key")?)?;
            Cmd::Key { mods, key }
        }
        "ui" => Cmd::Ui(at(1).map(|s| s.to_string())),
        "find" => Cmd::Find(
            at(1)
                .ok_or_else(|| "find needs a name".to_string())?
                .to_string(),
        ),
        "clip" => match pos.len() {
            1 => Cmd::ClipRead,
            _ => Cmd::ClipWrite(
                pos[1..]
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        },
        other => return Err(format!("unknown command: {other}")),
    })
}

fn render_screens(s: &crate::Screens) -> String {
    s.screens
        .iter()
        .map(|x| {
            format!(
                "{:<24} {}x{} at ({},{})  scale {}{}",
                x.id.0,
                x.bounds.w,
                x.bounds.h,
                x.bounds.x,
                x.bounds.y,
                x.scale,
                if x.primary { "  primary" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_find(v: &UiView, name: &str) -> String {
    let hits = v.find(name);
    if hits.is_empty() {
        return format!("no control matching {name:?} among {} shown", v.nodes.len());
    }
    hits.iter()
        .take(10)
        .map(|n| {
            format!(
                "{:<14} {:<40} click {} {}",
                n.role, n.name, n.center.x, n.center.y
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run<P: Pointer>(s: &Session<P>, cmd: Cmd) -> Result<String, InputError> {
    Ok(match cmd {
        Cmd::Screens => render_screens(s.screens()),
        Cmd::Position => {
            let p = s.position().await?;
            format!("{} {}", p.x, p.y)
        }
        Cmd::Move { x, y } => {
            s.move_to(&Loc::absolute(x, y)).await?;
            format!("moved to {x} {y}")
        }
        Cmd::Click { at, button, count } => match at {
            Some((x, y)) => {
                s.click_at(&Loc::absolute(x, y), button, count).await?;
                format!("clicked {x} {y}")
            }
            None => {
                s.click_here(button, count).await?;
                "clicked where the pointer was".into()
            }
        },
        Cmd::Drag { from, to } => {
            s.drag(
                &Loc::absolute(from.0, from.1),
                &Loc::absolute(to.0, to.1),
                Button::Left,
            )
            .await?;
            format!("dragged {} {} -> {} {}", from.0, from.1, to.0, to.1)
        }
        Cmd::Scroll { dx, dy } => {
            s.scroll(dx, dy).await?;
            format!("scrolled {dx} {dy}")
        }
        Cmd::Type(t) => {
            s.type_text(&t).await?;
            // Both counts, because they differ exactly when a surrogate pair
            // is involved and that is the case worth seeing.
            format!(
                "typed {} chars ({} UTF-16 units)",
                t.chars().count(),
                t.encode_utf16().count()
            )
        }
        Cmd::Key { mods, key } => {
            if mods.is_empty() {
                s.press(key).await?;
            } else {
                s.chord(&mods, key).await?;
            }
            "pressed".into()
        }
        Cmd::Ui(q) => {
            let v = s.ui_read(q.as_deref()).await?;
            format!(
                "{}\n{} of {} controls after compression",
                v.render().trim_end(),
                v.nodes.len() + v.modals.len(),
                v.raw_count
            )
        }
        Cmd::Find(name) => {
            let v = s.ui_read(Some(&name)).await?;
            render_find(&v, &name)
        }
        Cmd::ClipRead => s.clipboard_read().await?,
        Cmd::ClipWrite(t) => {
            s.clipboard_write(&t).await?;
            format!("clipboard set, {} chars", t.chars().count())
        }
    })
}
