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
    fn local_hook_ok(&self) -> bool;      // defaults to FALSE — see below

    // Optional, protocol 2 — both default to `Unsupported`, so skipping them
    // costs a capability rather than a compile error.
    fn clipboard_read(&self)              -> Result<String, InputError>;
    fn clipboard_write(&self, s: &str)    -> Result<(), InputError>;
    fn ui_tree(&self)                     -> Result<Vec<UiNode>, InputError>;

    // Defaulted to the slow or the silent answer; override for speed or honesty.
    fn state(&self) -> u64;                       // default: screens().state
    fn ui_tree_visible(&self) -> Result<Vec<UiNode>, InputError>;  // default: ui_tree()
    fn armed(&self) -> Option<bool>;              // default: None — "no gate"
}
```

The last three exist because of what the Windows side measured. `state` is
stamped on every `performed`, `position` and `ui` reply, and the default gets
it through a full monitor enumeration after every click; a fingerprint of the
monitor rectangles is tens of microseconds. `ui_tree_visible` is the one
filter you may apply, because the caller asked: 73% of a real tree is
off-screen and dropped on arrival. `armed` lets the client hear at `hello`
that the first click will be refused, instead of finding out from the refusal.

Three of the methods are slow and synchronous — `ui_tree` and the clipboard
pair — and the agent runs them on a blocking thread. Implement them as plain
blocking calls; do not spin up your own threads or runtimes for them.

**Still eight required methods.** The clipboard pair is defaulted, so an agent
that ignores it builds and runs; callers get `unsupported` and route around.
Worth doing when you get to it, in this order:

1. **`ui_tree`** — UI Automation, and the highest-value thing you can add.
   Return everything, filtered by nothing: the caller compresses. It turns
   "click at (812, 344)" into "click the control named Save", which removes
   the entire coordinate-registration problem rather than solving it.
2. **`clipboard_read`** after ctrl+a ctrl+c is the only other way to read
   data off the machine without capturing its screen.
3. **`clipboard_write`** + ctrl+v is how long text should move, since typing
   is per-character.

### `local_hook_ok` defaults to `false`, and that is the point

`local_activity() -> bool` cannot tell *nothing happened* from *nothing is
watching*, and those are the same answer forever if the hook silently failed
to register. `local_hook_ok` separates them, and its default is the
pessimistic one: an agent that has not said it installed a hook is assumed not
to have one.

Return `true` only where the registration actually succeeded, and `false`
again if it goes away. The agent then reports it in `Ready`, logs
`no_local_override`, and `ns-pointer` warns on every command — instead of a
dead brake looking exactly like a quiet user.

That turns the wave-the-mouse check from the whole guarantee into a one-time
acceptance.

**"It installed" is a fact about the past.** Windows silently unhooks a thread
whose callback overruns `LowLevelHooksTimeout`, long after
`SetWindowsHookExW` returned success, so a `local_hook_ok` that reports the
registration result reports something that stopped being true. Report
*delivery* instead: count callbacks — including our own injected events, which
are the only events we can cause on demand — and treat a counter that has not
moved after an injection plus a grace period as a dead hook.

The ordering is the whole trick, and getting it backwards is silent.
**Sample the counter before `SendInput`, never after.** Sampled after, it
already contains the events it is waiting for, can never grow again, and so
reports a healthy hook as dead the moment anything is injected. Unit tests
pass either way — this was found on a live connection, by `ns-pointer`
warning `this agent reports no local override` against a hook that was fine.
Worth a regression test, because the next person will not be that lucky.

### One trap, and one that is now handled for you

**If you wrap a `Platform` in another `Platform`, forward the defaulted
methods explicitly.** A wrapper that omits them silently reports the
capability unsupported rather than failing to compile — this went wrong three
times here before it was worth fixing structurally.

`Arc<P>` now forwards everything, so `Arc<WindowsPlatform>` is safe to share
without writing a wrapper at all. Any other wrapper is still yours to get
right.

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
| `ui_tree` | Everything, filtered by nothing, invisible and disabled **flagged rather than dropped**. Roles from a control-type id map, never `CurrentLocalizedControlType` — that returns `Tlačítko` on a Czech desktop, and a role nobody can match on is worse than no role. Ask UIA for a cached request: measured 250× here, 11ms against 2.8s over ~2700 nodes. Fill `focused`, `focusable`, `depth` and `window` — all four come out of the same cached fetch — and the compressor gets containment and a focus marker for free. |
| `ui_tree_visible` | `ui_tree` with a provider-side `IsOffscreen == false` condition. Must return every node `ui_tree` would flag `visible: true`; a stricter filter is the second copy of the caller's judgement the seam forbids. |
| `state` | A per-call fingerprint of the monitor rectangles, so it cannot go stale. Skip only the identity queries. Must agree with `screens().state`. |
| `key(_, false)` | May be refused (secure desktop, elevated foreground). Return the error; the agent keeps the key on a machine-wide stuck list and retries before the next `perform`. Do not swallow it and do not retry inside `Platform`. |

Plus: return `Blocked` when the OS refuses. On Windows `SendInput` **returns
success under UIPI and does nothing**, so a backend that trusts the return
value reports clicks that never happened. Detect the foreground window's
integrity level and the secure desktop (UAC prompt, lock screen, Ctrl+Alt+Del).

Plus: answer `NeedsConfirmation` until a person at the machine has armed the
session — the lower half of the split described at the end of this document,
and the only half a caller cannot go around. Gate what commits (button and key
presses, `text`); never gate a release, or a refusal mid-chord leaves Ctrl
down. Watch for the arming chord somewhere the socket cannot reach it: a
low-level keyboard hook already ignores injected input, so nothing sent over
the wire can arm the machine.

And outside the code: run in the **user's session**, not as a service — a
service lands in session 0 with no interactive desktop and drives nothing.

One more thing UIA will lie to you about: a minimized window is parked at
roughly (-32000, -32000) by Windows convention and still reports as on
screen. Flag those `visible: false`. Left alone they reach the caller as a
click target on no monitor, which either clicks nothing or comes back
`out_of_bounds` for a control you just advertised.

---

## Mine: everything else

| | where | state |
| --- | --- | --- |
| Coordinate model, DPI/image registration, clamping, screen hit-testing | `geom.rs` | done |
| Gestures: click, drag, chord, typing rhythm, modifier recovery | `gesture.rs` | done |
| Path shapes: eased / Bézier / Gaussian / Perlin, seeded | `motion.rs` | done |
| Wire types and framing | `wire.rs` | done |
| **Agent guards**: auth, step cap, rate limit, local override, held-key release, refused-release retry, audit | `agent.rs` | done |
| Blocking platform calls off the runtime (`spawn_blocking` for `ui_tree`, clipboard) | `agent.rs` | done |
| Client (`RemotePointer`), `Session` | `client.rs`, `lib.rs` | done |
| MCP stdio server, 12 tools, `ns-pointer-mcp` binary; `armed` and `local_override` reported to the model at `initialize` and in `screens_list` | `mcp.rs`, `bin/` | done |
| `nscore::Tool` adapter | `components-std/pointer_tool.rs` | done |
| **Confirmation gate, upper half**: `Confirm`, the `confirm` argument, the sentence a model reads | `mcp.rs` | done — but advisory, see below |

| UI-tree compression (A11y-Compressor pipeline) | `ui.rs` | done |

| TCP accept loop (`serve_tcp`) | `agent.rs` | done |

81 tests, all green without a display server.

Once your `Platform` exists, the whole chain runs:

```
MCP client ──stdio──> ns-pointer-mcp ──TCP+token──> your agent ──> Platform
ns-app [pointer] ────────────────────> TCP+token ──> your agent ──> Platform
```

The second line is the engine's own emitter driving the desktop: a
`[pointer] addr = "host:port"` section in `config.toml` (token from
`NS_POINTER_TOKEN`) dials the agent at startup and registers the ten
`pointer_*` actions. Clicks and typing are staged behind the harness's
confirmation flow with the coordinates named, so nothing there needs the MCP
layer's `confirm` argument.

---

## The two ways to use this, and the overlap each avoids

**If you write the agent in Rust** — implement `Platform`, hand it to
`Agent::new`, and serve. The guards, framing, auth, the accept loop and the
interpreter are already written and tested. This is ~200 lines of Win32 and
nothing else.

```rust
use nspointer::agent::{serve_tcp, Agent, AgentConfig, Limits, Listen};

let agent = Agent::new(WindowsPlatform::new()?, AgentConfig {
    token: std::env::var("NS_POINTER_TOKEN")?,
    limits: Limits::default(),
});
serve_tcp(agent, &Listen::loopback(7373)).await?;
```

`serve_tcp` binds, accepts, and gives each connection its own task. It refuses
to start on an empty token, and refuses a non-loopback address unless
`allow_remote` says you meant it — both failure modes are silent and permanent
otherwise. The local override, the rate ceiling and the connection cap are
**machine-wide**, not per socket: they protect one desktop, and a limit a
caller can reset by reconnecting is not a limit.

You do not need a step runner. Everything a canned `click` or `move` demo did
is a `perform` over this socket, driven by whatever holds the other end.

**If you write it in another language** — implement the whole protocol from
`docs/pointer-protocol.md`, and treat `crates/pointer/tests/agent.rs` as the
conformance suite: every assertion in it is a rule that document states.
Notably you must reimplement §4's duties yourself, because they are the half
that cannot live on my side:

- authenticate; there is no unauthenticated mode
- cap steps per `perform`, and rate-limit performs
- suspend on local activity, and answer `suspended`
- **release every held key and button on disconnect and on suspend**, and
  keep and retry the ones the OS refused
- report OS refusals as `blocked` rather than trusting a return value
- log what was actually injected

Either way, nothing in the list above is something I will also write for the
target machine, and nothing in `Platform` is something you have to decide
about coordinates.

---

## The one correction to the split above

"Whether allowed" was listed on my side, and for one case that was wrong.

`Confirm::FirstAction` lives in the MCP layer, so it gates a model driving
through MCP and nothing else: `ns-pointer click 900 500` on a raw socket ran
three times with no prompt while it was in place. That is the §4 argument of
the protocol doc turned on its author — anything that can open this socket can
speak this protocol, so a guard above the socket is a guard a caller can
decline to use.

So the confirmation gate is **two gates, and they are not redundant**:

| | where | knows | can be bypassed |
| --- | --- | --- | --- |
| upper | `mcp.rs`, mine | what the model *intends* — "this would type your password into a chat window" | yes, by not using MCP |
| lower | `Platform`, yours | only what reached the OS | no |

Keep both. The upper one writes the sentence a person can actually judge; the
lower one is the one that enforces. The lower one answers
`needs_confirmation`, which exists in `wire.rs` for this purpose and is
documented in §4.7 of the protocol.

---

## The seam, in one line

**You answer "what is on this machine and how do I poke it."** Everything
about *where*, *when* and *how fast* is on my side; *whether allowed* is
shared, and the half that enforces is yours.
