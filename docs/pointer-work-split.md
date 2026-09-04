# Who implements what

So the two halves do not overlap. **Your entire job is one trait.** Everything
else is written, tested and running on a machine with no display server.

---

## Yours: `Platform` — 8 methods, `crates/pointer/src/platform.rs`

```rust
pub trait Platform: Send + Sync {
    fn screens(&self)  -> Result<Screens, InputError>;
    fn position(&self) -> Result<Point,   InputError>;
    fn move_to(&self, p: Point)                 -> Result<(), InputError>;
    fn button (&self, b: Button, down: bool)    -> Result<(), InputError>;
    fn scroll (&self, dx: i32, dy: i32)         -> Result<(), InputError>;
    fn key    (&self, k: &Key, down: bool)      -> Result<(), InputError>;
    fn text   (&self, s: &str)                  -> Result<(), InputError>;
    fn local_activity(&self) -> bool;

    // Optional, protocol 2 — both default to `Unsupported`, so skipping them
    // costs a capability rather than a compile error.
    fn clipboard_read(&self)              -> Result<String, InputError>;
    fn clipboard_write(&self, s: &str)    -> Result<(), InputError>;
}
```

**Still eight required methods.** The clipboard pair is defaulted, so an agent
that ignores it builds and runs; callers get `unsupported` and route around.
Worth doing when you get to it: `clipboard_read` after ctrl+a ctrl+c is the
only way in this protocol to read data off the machine without capturing its
screen, and `clipboard_write` + ctrl+v is how long text should move, since
typing is per-character.

One trap, which cost a test here: **if you wrap a `Platform` in another
`Platform`, forward the defaulted methods explicitly.** A wrapper that omits
them silently reports the capability as unsupported rather than failing to
compile.

Every value arrives needing no interpretation. A `Point` is an absolute
physical pixel in virtual-desktop coordinates **that has already been checked
against a real screen**. There is no mapping, clamping, easing, pacing or
gesture logic on your side — those are done, and tested, above this line.

The four that carry real work:

| method | the actual difficulty |
| --- | --- |
| `screens` | `Screen::id` **must be stable** across reboots, driver updates and replugging — device path or EDID, never a GDI index, which Windows reassigns on all three. Report `bounds` as physical pixels with a signed origin, `scale` as 1.5 at 150%, and bump `state` on any display-config change. |
| `move_to` | `SendInput` with `MOUSEEVENTF_ABSOLUTE \| MOUSEEVENTF_VIRTUALDESK`, normalized 0–65535 across the **virtual desktop**, not the primary monitor, from a per-monitor-DPI-aware thread. |
| `text` | `KEYEVENTF_UNICODE`, `wVk = 0`, `wScan` = the UTF-16 code unit. A non-BMP character is **two** inputs, one per surrogate. |
| `local_activity` | Has a human touched this machine since the last call? Physical mouse movement over a threshold, or a keystroke that was not ours. Returning `false` always compiles, works, and removes the only means by which the person at the keyboard can take their machine back. |

Plus: return `Blocked` when the OS refuses. On Windows `SendInput` **returns
success under UIPI and does nothing**, so a backend that trusts the return
value reports clicks that never happened. Detect the foreground window's
integrity level and the secure desktop (UAC prompt, lock screen, Ctrl+Alt+Del).

And outside the code: run in the **user's session**, not as a service — a
service lands in session 0 with no interactive desktop and drives nothing.

---

## Mine: everything else

| | where | state |
| --- | --- | --- |
| Coordinate model, DPI/image registration, clamping, screen hit-testing | `geom.rs` | done |
| Gestures: click, drag, chord, typing rhythm, modifier recovery | `gesture.rs` | done |
| Path shapes: eased / Bézier / Gaussian / Perlin, seeded | `motion.rs` | done |
| Wire types and framing | `wire.rs` | done |
| **Agent guards**: auth, step cap, rate limit, local override, held-key release, audit | `agent.rs` | done |
| Client (`RemotePointer`), `Session` | `client.rs`, `lib.rs` | done |
| MCP stdio server, 8 tools, `ns-pointer-mcp` binary | `mcp.rs`, `bin/` | done |
| `nscore::Tool` adapter | phase 4 | next |

47 tests, all green without a display server.

Once your `Platform` exists, the whole chain runs:

```
MCP client ──stdio──> ns-pointer-mcp ──TCP+token──> your agent ──> Platform
```

---

## The two ways to use this, and the overlap each avoids

**If you write the agent in Rust** — implement `Platform`, hand it to
`Agent::new`, done. The guards, framing, auth and interpreter are already
there and already tested. This is ~200 lines of Win32 and nothing else.

```rust
let agent = Agent::new(WindowsPlatform::new()?, AgentConfig {
    token: std::env::var("NS_POINTER_TOKEN")?,
    limits: Limits::default(),
});
agent.serve(read, write).await?;
```

**If you write it in another language** — implement the whole protocol from
`docs/pointer-protocol.md`, and treat `crates/pointer/tests/agent.rs` as the
conformance suite: every assertion in it is a rule that document states.
Notably you must reimplement §4's duties yourself, because they are the half
that cannot live on my side:

- authenticate; there is no unauthenticated mode
- cap steps per `perform`, and rate-limit performs
- suspend on local activity, and answer `suspended`
- **release every held key and button on disconnect and on suspend**
- report OS refusals as `blocked` rather than trusting a return value
- log what was actually injected

Either way, nothing in the list above is something I will also write for the
target machine, and nothing in `Platform` is something you have to decide
about coordinates.

---

## The seam, in one line

**You answer "what is on this machine and how do I poke it."** Everything
about *where*, *when*, *how fast*, *whether allowed*, and *what happens when
it goes wrong* is already on my side.
