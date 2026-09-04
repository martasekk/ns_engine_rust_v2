# Remote Pointer — drive another machine's mouse, over MCP

**Goal:** move and click the mouse on a different PC, exposed as MCP tools so
any MCP client can drive it. Windows first, with the port to a second OS being
one trait implementation rather than a second project.

**Status, 2026-09-04:** phases 0, 1, 3, 4 and 5 built and green — 292 tests
across the workspace, on a machine with no display server. Remaining: the
Windows `Platform` impl (yours), then UI Automation (§8). Keyboard folded in (§6). The MCP survey changed
three decisions and one of those was later withdrawn (§8). Remaining: the
Windows `Platform` impl (yours), then clipboard and UI Automation.

---

## 1. Shape

The load-bearing decision is *where the MCP server runs*. Two options, and the
second is better for what you asked for.

**(a) MCP server on the Windows box.** The client reaches it over Streamable
HTTP. One process, but it puts the whole MCP surface — schemas, session
handling, auth — on every machine you ever want to control, and it forces the
network transport into OAuth 2.1 territory.

**(b) MCP server local, thin agent remote.** Chosen.

```
   MCP client (Claude Code, …)          ns-app's own emitter
        │ stdio, JSON-RPC 2.0                  │ nscore::Tool
        └───────────────┬──────────────────────┘
                        ▼
                  ns-pointer          core: coordinate model, validation,
                  (a library)         easing, rate limit, audit. No platform
                        │             code, no I/O. Testable anywhere.
                        │ authenticated RPC, one hop
                        ▼
                  ns-pointerd         the agent on the target PC
                        │
                        │ trait Pointer
                        ▼
     Windows SendInput  │  X11 XTEST  │  macOS CGEvent
```

Why (b):

- **stdio is the transport that actually works everywhere.** MCP stdio is
  JSON-RPC 2.0 over a subprocess's stdin/stdout, configured by environment
  variables, and needs no authentication *because* it is a subprocess. Keeping
  the MCP surface on stdio means exactly one hop is network-exposed and we
  define it ourselves, rather than inheriting OAuth 2.1 with dynamic client
  registration, PKCE and resource indicators for what is a personal LAN tool.
- **The remote agent stays small.** Porting to macOS is one `Pointer` impl,
  not a second MCP server. That is the "make adding more systems possible"
  requirement, discharged structurally.
- **Almost all of it is testable on a headless box.** Only the `Pointer` impls
  need real hardware. This machine has no display server at all — no
  `DISPLAY`, no Wayland socket, no Xvfb — so anything not behind that trait
  could never be tested here.

## 2. Prior-art pass

| Source | Decision |
| --- | --- |
| **MCP transports** — stdio (JSON-RPC over a child process's stdio, env-var config, no auth by construction) vs **Streamable HTTP** (spec 2025-03-26, retained in the Nov 2025 revision; one endpoint serving POST and GET with optional SSE; OAuth 2.1 + Resource Indicators from 2025-06-18) | **Adopt stdio for the MCP hop.** Streamable HTTP is the right answer for a multi-tenant hosted server and the wrong one here: it would put an OAuth stack in front of a tool whose entire threat model is "one person, one LAN". Revisit only if the server must serve clients it does not launch. |
| **"A First Measurement Study on Authentication Security in Real-World Remote MCP Servers"** — arXiv 2605.22333 (abstract read; the PDF's numbers did not extract cleanly, so this is cited qualitatively) | **Design consequence.** The study's finding is that deployed remote MCP servers commonly ship with no authentication or with broken authentication. This one injects synthetic input into a desktop: unauthenticated, it is not a leaky tool, it is a remote-control backdoor for anyone who can reach the port. Auth is a phase-1 requirement, not a hardening pass (§5). |
| **enigo** (cross-platform input simulation, Rust) — on Windows it temporarily switches the calling thread to per-monitor DPI awareness for coordinate and display queries, so the *process* DPI mode need not change; cartesian, origin top-left, physical pixels | **Adopt as the first backend.** It discharges the DPI trap for free and brings X11 and macOS nearly free, which is exactly the extensibility you asked for. Kept behind our own `Pointer` trait so a hand-rolled `windows-rs` backend can replace it the day we hit a limit — the same call this repo made for `OpenRouterClient` over adopting a multi-provider framework. |
| **windows-rs** (`SendInput`, `GetDpiForMonitor`) | **Held in reserve.** Direct use means owning DPI-awareness contexts and cross-monitor translation by hand. Worth it only for something enigo cannot express (raw scan codes, injection flags, per-device input). |
| **Synergy / Input Leap / Barrier** (network KVM) | **Adopt one behaviour: the local override.** They all yield control the instant the physical mouse moves. Without it, a stuck remote session means you cannot wrestle back your own machine (§5). |
| **RustDesk** input handling, **VNC/RFB** | **Reference for the wire model, not adopted.** Both stream frames; we are not building a remote desktop (§6). Their coordinate handling is the part worth reading. |
| **Windows display APIs** — GDI device indices are volatile: a reboot, a driver update, or unplugging a monitor reassigns them | **Design consequence.** Screens get a stable identity (device path + geometry), never an index. A tool that clicks on "screen 2" and means a different monitor after a reboot is worse than one that errors. |

## 3. The seam

```rust
/// One machine's pointer. Everything above this line is platform-free and
/// unit-testable; everything below it is per-OS glue. Adding an OS means
/// implementing this and nothing else.
pub trait Pointer: Send + Sync {
    fn screens(&self) -> Result<Vec<Screen>, InputError>;
    fn position(&self) -> Result<Point, InputError>;
    fn move_to(&self, p: Point) -> Result<(), InputError>;
    fn button(&self, b: Button, down: bool) -> Result<(), InputError>;
    fn scroll(&self, dx: i32, dy: i32) -> Result<(), InputError>;
}
```

Everything else lives above it and is tested against a `MockPointer` that
records calls: normalized→physical coordinate mapping, clamping, `move_to`
easing over a duration, `drag` as move→down→move→up, `click` as down→up with a
count, rate limiting, and the audit record. That is most of the code, and none
of it needs a display.

**Coordinates.** The default over the wire is a **named screen plus normalized
0.0–1.0**, with absolute virtual-desktop pixels available. A caller that says
"click (812, 344)" on a machine whose resolution it has never seen is guessing;
`screens_list` comes first, always. On Windows, absolute `SendInput` normalizes
to 0–65535 across the **virtual desktop** (`MOUSEEVENTF_VIRTUALDESK`), not the
primary monitor — the single most common multi-monitor bug in this space, and
it lives inside the backend where it belongs.

## 3b. Division of labour — and where the split is not a choice

§3 argues for pushing work to the client. That argument holds for *correctness*
concerns and fails completely for *enforcement* ones, and the two must not be
confused: **a limit the client applies to itself is not a limit.** Anything
that can open a socket can speak this protocol, so every rule that exists to
constrain a caller has to live where the caller cannot reach it.

| Duty | Side | Why it cannot sit on the other side |
| --- | --- | --- |
| Coordinate mapping, clamping, image registration | client | Needs the layout the client already holds, and per-platform coordinate maths is exactly where this class of tool goes wrong |
| Path shape, easing, typing rhythm | client | Seeded reproducibility for audit and test; otherwise four algorithms reimplemented per platform |
| Gesture composition (click, drag, chord) | client | Keeps the agent's interpreter to six cases |
| **Authentication** | **agent** | Anything can open the socket |
| **Rate limit, step cap** | **agent** | A caller that limits itself is not limited. This is the one that was missing from the contract |
| **Local override** | **agent** | Only the agent can see physical input arriving |
| **Release held keys on disconnect / suspend** | **agent** | The client is gone precisely when this is needed |
| **Audit log** | **agent** | It records what was *injected*, not what was *intended*. A client-side log is a record of hopes |
| **OS-refusal detection** (UIPI, secure desktop) | **agent** | Only it can see `SendInput` succeed and do nothing |
| Balanced chords, modifier recovery on error | both | Client handles the ordinary failure; the agent is the backstop for the case where there is no client left |

### A correction to the estimate

"The agent is a `match` in a loop" is true of the **step interpreter** and
false of the agent. The interpreter is perhaps forty lines. The duties in bold
above are the actual work, and they are not optional — an agent with the
interpreter and none of them is a remote-input backdoor with a JSON parser in
front of it.

Phase 1 therefore builds the agent-side guards as a reference implementation
generic over `Pointer`, so they exist and are tested whether or not the final
agent is written in Rust.

## 4. Windows realities to design around

Not edge cases; each one is a silent failure if unhandled.

- **UIPI.** A medium-integrity process cannot inject input into a window owned
  by an elevated one. `SendInput` *succeeds* and nothing happens. Detect the
  target's integrity level and return an error rather than reporting success.
- **The secure desktop.** UAC prompts, the lock screen and Ctrl+Alt+Del accept
  no injected input by design. Not a bug to fix — a state to report.
- **Session 0 isolation.** Run as a Windows service and the agent lands in
  session 0, which has no interactive desktop. It must run in the user's
  session; a scheduled task at logon, not a service.
- **DPI.** Without per-monitor awareness Windows silently scales coordinates.
  enigo handles this per-call; a hand-rolled backend must not forget it.

## 5. Security, as requirements

This accepts messages over a socket and injects synthetic input into someone's
desktop. The bar is therefore higher than for a normal tool, and the study in
§2 says this is exactly where comparable software fails.

1. **Loopback by default.** Listening on a routable address requires an
   explicit config key. The default build is useless to a stranger.
2. **Authenticated always.** mTLS, or a pre-shared key with a nonce and a
   monotonic counter against replay. There is no unauthenticated mode, not
   even behind a flag — the flag is what ends up in production.
3. **Local override.** Physical mouse movement over a threshold, or a hotkey,
   suspends remote input for N seconds and answers subsequent calls with
   `Suspended`. The machine's owner outranks the network.
4. **Visible when live.** A tray icon or console banner while a session holds
   control. Silent remote control of a desktop is the property that separates
   this tool from malware, and it is one line of code.
5. **Bounded.** Rate limit, bounded queue, and a session that expires. An
   agent in a loop should hit a ceiling, not flood the input queue.
6. **Audited.** Append-only JSONL of every injected event with its source.
   This repo already thinks in event logs with provenance; the same instinct
   applies, and it is what makes "what did it just do to my machine?"
   answerable.

## 6. Scope

**In:** pointer position, move, click, drag, scroll; screen enumeration;
suspend/status; **keystrokes and text**. Windows backend. MCP stdio server. A
`nscore::Tool` adapter so `ns-app`'s own emitter can drive the same core.

**Keyboard — added 2026-09-04, and the earlier "costs nothing" was wrong.**
It was scoped out on the grounds that a `Keyboard` sibling could be bolted on
later at no cost. Adding it before the agent is written cost two step kinds
rather than one, because a *character* and a *key* are not the same thing and
neither mechanism can do the other's job:

- `Step::Text` injects the character itself (`KEYEVENTF_UNICODE`) and never
  consults the target's layout — the only reliable way to type `@` on a
  machine whose layout is unknown. It cannot express a chord.
- `Step::Key` presses a key, which is the only way to do Ctrl+C, Enter or F5 —
  none of which are characters. The physical key labelled `Q` on QWERTY is `A`
  on AZERTY, so "type q" and "press the Q key" have different right answers.

Two consequences that reach the agent. Chords release in **reverse order**,
because releasing Ctrl before C is read by some targets as a bare `c` that
types into whatever had focus. And **held keys must be released on disconnect
and on suspend**: the client balances what it sends and recovers modifiers
from a failed `perform`, but a dropped connection is exactly when nobody is
left to send the release, and a stuck Ctrl is not a failed operation — it is
an unusable machine.

Doing this *before* the agent exists was the whole saving. After, it is a
protocol bump.

**Out, deliberately:**
- **Screen capture. Decided 2026-09-04: out, and the caller supplies images
  by its own route.** Agent-driven GUI automation is wanted later; capture is
  not this module's job. Correct call — it changes nothing structural. The
  architecture, the `Pointer` trait, auth, the local override and the phase
  order all stand, and adding capture later is a sibling `Capture` trait plus
  one tool, disturbing nothing already built.

  What it *does* change is the wire contract, in two ways worth settling in
  phase 0 rather than after a client depends on them.

  **(a) `screens_list` must be enough to register an externally captured
  image against.** A screenshot taken by a process that is not per-monitor
  DPI aware is *virtualized* by Windows: a 2560×1440 monitor at 150% scaling
  captures as 1707×960. enigo clicks in physical pixels. A model looking at
  that image says "click (800, 400)" and every click lands at 0.667× the
  intended offset — consistently wrong, and it presents as a model that
  cannot aim rather than as a unit mismatch. So each `Screen` carries: stable
  id, physical bounds **in virtual-desktop coordinates**, DPI scale factor,
  and a primary flag. That is enough for a caller to map image pixels to
  physical pixels without guessing. It is a data-model decision, not a
  feature.

  **(b) Responses carry a screen-state token** (a counter or timestamp), so a
  caller can detect that the screen moved between the image it reasoned about
  and the click it sent. Unnecessary for teleoperation, nearly free now,
  awkward to retrofit into a protocol later. Include it; ignore it until it
  matters.

  And one contract with no code behind it: **the capture must originate on the
  target machine, in the interactive session, DPI-aware.** An image from any
  other machine has no coordinate frame in common with this agent.
- **Remote desktop.** Not building VNC.

## 7. Phases

| # | Work | Testable here? | State |
| --- | --- | --- | --- |
| 0 | `ns-pointer`: `Pointer` trait, `Screen` (stable id, virtual-desktop bounds, DPI scale, primary), coordinate mapping, image registration, clamping, gestures, keystrokes, screen-state token, `MockPointer`, wire types | **yes**, fully | **done** — 21 tests |
| 0b | `motion.rs`: OxyMouse-shaped path algorithms (bezier / gaussian / perlin) behind a seeded RNG | **yes**, fully | **done** |
| 1 | `agent` + `client`: NDJSON framing, `hello` auth, protocol version, step cap, token-bucket rate limit, local override, held-key release on disconnect and on failed batches, audit log; `RemotePointer`. Driven over an in-memory duplex with an injected clock, so rate limit and override are proven without sleeping | **yes**, fully | **done** — 10 tests |
| 2 | Windows backend: `impl Pointer` over enigo — screen identity from device path/EDID, UIPI and secure-desktop detection, `KEYEVENTF_UNICODE` with surrogate pairs | **no** — your machine | |
| 3 | `ns-pointer-mcp`: stdio JSON-RPC (MCP 2025-06-18), eight typed tools, absolute pixels by default with `screen` switching to fractions. Caller mistakes are JSON-RPC errors; machine refusals are `isError` tool results the model can read | **yes** | **done** — 9 tests, plus the binary |
| 4 | `nscore::Tool` adapter — six actions in `components-std`, one shared session. `pointer_click` and `pointer_type` are `SideEffect::Irreversible`, so `SideEffectGate` stages them and `stage()` names the coordinates and text in the prompt | **yes** | **done** — 8 tests |
| 5 | Clipboard read/write (§8), protocol 2. Optional on the agent — defaulted `Platform` methods, so it costs a capability rather than a compile error. Contents never reach the audit log, only the length | **yes** | **done** — 4 tests |
| 6 | **UI Automation** (§8): `ui_tree`, `find_element`, `click_element`. Evaluate **before** screen capture — naming a control sidesteps DPI registration, image transport and stale screenshots at once | partly | |
| 7 | Second platform (X11 or macOS) — one trait impl, the proof that §3 worked | partly | |

Phases 0, 1, 3 and 4 are the bulk of the code and all land on a box with no
display server. Phase 2 is small by construction and is the only one that
needs Windows in front of you. The ordering front-loads everything verifiable
before anything has to be tested by hand.

**Phase 1 is built as a reference agent, not only as a client.** The server
loop is generic over `Pointer`, so it runs here against `MockPointer` and on
Windows against the real backend. If the agent is written in another language
instead, the same code is a conformance target to test it against — either way
the protocol gets exercised end to end before hardware is involved.

---

## 8. Against the field — how existing MCP servers are built

Surveyed 2026-09-04. The comparison changed three decisions and added two
items to the roadmap.

| | **Anthropic computer-use tool** | **zavora-ai/computer-use-mcp** (Rust, Win+macOS — the closest peer) | **nuphus-mcp**, **tanob/mcp-desktop-automation** | **this design** |
| --- | --- | --- | --- | --- |
| Surface | One tool, `action` enum: `key`, `type`, `mouse_move`, `left_click`, `left_click_drag`, `screenshot`, `cursor_position`, later `scroll`, `hold_key`, `wait`, `triple_click`, `zoom` | 64 tools in 9 groups | 15 desktop tools (+23 browser) | ~7 tools planned |
| Coordinates | `coordinate: [x, y]`, "pixels from the left edge" | absolute pixels; `list_displays`, `get_display_size` | absolute pixels | screen id + normalized, DPI scale exposed, absolute available |
| Where it runs | in the sandbox it drives | on the machine it drives | on the machine it drives | **beside the client, driving a remote agent** |
| Auth | container isolation | loopback only; "remote serving requires an embedding host with authentication" | none | built in, required |
| Screenshot | yes | yes | yes | **no** (§6) |
| UI tree | no | **yes** — `get_ui_tree`, `find_element`, `click_element`, `set_value`, `fill_form` | no | no |
| Motion | teleport | teleport | teleport | eased/Bézier/Gaussian/Perlin |

### What this changes

**1. Conform at the MCP boundary, keep the richer model underneath.** Every
peer takes absolute pixels, and the canonical tool literally documents
`coordinate: [x, y]` as "pixels from the left edge". A model reaching for this
server will produce that shape by default, and a tool surface that refuses it
in favour of `{screen, x: 0.5, y: 0.5}` is a tool surface that gets called
wrong. Phase 3 therefore accepts `[x, y]` as a first-class form — resolved
against the virtual desktop — with screen+normalized as the precise option.
The internal model does not change; only what the MCP layer will accept.

**2. ~~Consider the single-tool-with-`action`-enum shape.~~ Withdrawn on a
closer read of the evidence.** The first draft of this row argued for one tool
with an `action` enum, on the grounds that it is the form models have most
exposure to. That conflated two different conventions. The single-tool
`action` enum is Anthropic's **API tool definition** (`computer_20241022`),
not an MCP server; every MCP server in the survey exposes *multiple named
tools* — 64 for zavora, 15 for nuphus. Adopting the one-tool shape would have
made us the odd one out among the very servers being copied. **Built as eight
typed tools**, which is both the MCP convention and the better engineering.

**3. Nobody ships authenticated remote operation; we are building the thing
they punt on.** zavora's HTTP listener is loopback-only and explicitly defers
auth to "an embedding host". Anthropic's runs inside a container it owns. That
is not an argument against §5, it is confirmation that §5 is the load-bearing
part: the remote requirement is exactly what has no prior art to copy, and it
is where a mistake is a backdoor rather than a bug.

### Two things the survey added

**Accessibility tree, and it may make screenshots unnecessary.** zavora's
`get_ui_tree` / `find_element` / `click_element` group is the capability this
design lacks entirely, and it is a *better* answer to GUI automation than
coordinates: asking for the button named "Save" sidesteps DPI registration,
image transport, resolution differences and stale screenshots in one move. On
Windows this is UI Automation (UIA), which is also the API that answers
"what is under the cursor" without capturing anything. Given that agent-driven
GUI automation is the eventual goal (§6), **this is a stronger next capability
than screen capture** and it should be evaluated before capture is built.

**Clipboard.** Several peers expose read/write. It is the pragmatic way to move
bulk text: `Step::Text` per character is right for a search box and wrong for
four thousand characters, which is 8000 steps. Two operations, and it removes
the only case where the typing design is awkward.

### Where this design is unusual, for better and worse

The motion work (§`motion.rs`) has no counterpart in any surveyed server —
they all teleport. That is a real differentiator for teleoperation and for UI
with hover and drag-threshold behaviour, and it is mild evidence that nothing
about *automation* requires it. Worth keeping — it is already built and
tested — without claiming it is essential.

The screen model is better than the field's: every peer assumes absolute
pixels on an implied single screen, and none of them expose DPI scale, which
is the value a caller needs to register an externally captured image. That
advantage is only worth having if the MCP surface still accepts the
conventional form, which is change 1.

---

## 12. Phase 4 — why the harness adapter is not just a second front door

The MCP server and the `nscore::Tool` adapter expose the same capability, and
the second is worth having because the engine already owns machinery an MCP
client does not.

**The classification is the substance.** `SideEffect` was there for exactly
this:

| action | effect | reasoning |
| --- | --- | --- |
| `pointer_screens`, `pointer_position` | `Pure` | reads |
| `pointer_move`, `pointer_scroll` | `Reversible` | moves a cursor; activates nothing |
| `pointer_click`, `pointer_type` | **`Irreversible`** | you cannot know what a click activated. There is no undo for "sent the email" |

`SideEffectGate` turns `Irreversible` into a staged action the user must
confirm, so the harness gets a human in the loop for remote input without a
line of new guard code — which is also what the MCP specification asks clients
to provide and cannot itself enforce.

`stage()` is where the care goes. A prompt reading *"'pointer_click' is
irreversible. Confirm to proceed."* is not a question anyone can answer, so the
staged description carries the specifics:

```
right-click 2 times at (0.5, 0.5) of screen S1 on the remote machine
type "rm -rf /" on the remote machine
press ctrl+c on the remote machine
```

Reversible actions are deliberately not staged: a gate that asks about
everything is a gate nobody reads.

Two smaller things this phase fixed, both caught by tests rather than by
inspection: `{}` on a `serde_json::Value` renders a string with its JSON
quotes, which would have put `screen "S1"` in a prompt read by a person; and
`pointer_screens` renders its output as prose (`S1 1920x1080 at (0,0) scale 1
primary`) rather than JSON, because the entrainment plan's phase-1 invariant —
no engine syntax in model-visible text — applies to every tool, not only to
`recall`.
