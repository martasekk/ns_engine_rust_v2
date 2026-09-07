# ns-pointer → ns-pointerd: reply to the 2026-09-05 handoff

Answers the handoff of the same date, item by item, in its order. Protocol
stays at 2. Everything below is in this tarball; the tests went from 71 to 81
and the whole suite still runs without a display server.

The one-line summary: A–G are all in, plus one item from H (`armed`), and
each of the seven is a trait method with a default body or a serde field with
a default, so your current agent keeps working against this client unchanged
and picks up each improvement as you fill it in.

---

## §1. Still open from the last handoff

| item | now |
| --- | --- |
| `NeedsConfirmation` | Landed (`wire.rs`, `mcp.rs`, protocol doc §2 and §4.7, an MCP test that the hint reaches the model). |
| `Confirm::FirstAction` vs the socket | As agreed: two gates, yours enforces. Documented in both docs. `armed` in `ready` (below) is the piece that lets them cooperate. |
| Pins | In `Cargo.toml` of the bundle, as `=` pins with the reasoning in the file header. `make-bundle.sh` runs `cargo tree --target x86_64-pc-windows-gnu -i windows-sys` and **fails the build** if anything but 0.52 is reachable — negative-tested by removing the pins. You will still build it on the target; this only catches a stray `cargo add` on this side before it reaches you. |
| `UiNode.center: Point` | Declined, reasoning in `ui.rs` on the field and in the protocol doc. Every consumer is spatial; an absent centre skips all of them. The fix that recovers the 4–5% is on your side and needs no type change: give a zero-extent element its nearest ancestor's rectangle. If you have evidence those are *named* controls with unnamed children rather than wrappers, that reverses the decision — send the tree. |
| Modal keyword signal | Dropped entirely, with a test that vocabulary cannot manufacture a modal in either language. `Zrušit` / `Potvrdit` join their dialog by role and position; the Czech dialog tree is still welcome as a fixture. |

## §3. The asks

### A. `NeedsConfirmation` — landed

As described above.

### B. Pins in `Cargo.toml` — landed

`crates/pointer/bundle/Cargo.toml` is the checked-in manifest the tarball
ships with. Note the header's warning about reading `Cargo.lock`: windows-sys
0.61 *is* in the lockfile even when the pins hold (via `errno` ←
`signal-hook-registry`, tokio's unix signal path), so grepping the lockfile
falsely reports failure. `cargo tree --target` is the check.

### C. `Platform::state()` — in, both sides

```rust
fn state(&self) -> u64 {
    self.screens().map(|s| s.state).unwrap_or(0)
}
```

The agent calls `state()` at the three sites (`position`, `ui`, `performed`);
`screens` itself still enumerates. Test: a platform whose `state()` returns
42 while `screens().state` is 3 sees all three replies stamped 42 and zero
enumerations across them. Your override, per the handoff: a per-call
fingerprint of the monitor rectangles, identity queries skipped. It must
agree with `screens().state`; the doc says so.

### D. `spawn_blocking` — in, my side

`Agent` now holds `Arc<P>` internally; `Agent::new(platform: P, …)` is
unchanged, so your call site does not move. `ui_tree`, `ui_tree_visible`,
`clipboard_read` and `clipboard_write` go through one `blocking()` helper
onto `spawn_blocking`; a panic inside the platform comes back as `internal`
on that request rather than killing the connection.

Test: a platform whose `ui_tree` sleeps 400 ms; a second connection's
`perform` completes in under 300 ms on a `current_thread` runtime. Inlining
the call makes the test fail at 400 ms, so it is guarding the right thing.

Implement the three as plain blocking calls — no threads or runtimes of your
own inside `Platform`.

### E. Four fields on `UiNode` — in, both sides

Exactly the signatures proposed, all `#[serde(default)]`, plus a `Default`
impl on `UiNode` (visible, enabled, nothing else known). What `ui.rs` does
with them, each with a test and a twin asserting the old behaviour survives
their absence:

- **`focusable`** — `actionable(n) = n.focusable || is_interactive(role)`,
  used in the noise filter, the dedup preference and modal attachment. An
  unnamed `Custom` that takes focus now survives; a focusable `Pane` wins
  the dedup over the named container it sits on.
- **`focused`** — `render()` appends `[FOCUS]` to that line; `UiView::
  focused()` returns it. I did **not** make `type` refuse when nothing has
  focus: that needs a fresh tree per `type` (0.8 s each) and the model can
  read the marker in the last `ui_read` instead. If you want it anyway it
  is one line in `mcp.rs`, but it should be opt-in.
- **`depth` + `window`** — reading order is now `(window, y, x)`, so the
  foreground window is read whole before anything behind it, and a window
  boundary is a `[BLOCK]` regardless of gap. Modal attachment uses
  containment where the tree has any structure (`any depth > 0 || window >
  0`) and the 300 px radius where it has none:
  - anchor at depth 0 (a dialog that is its own window): everything in that
    window joins;
  - anchor deeper (a dialog nested in a window): same window, deeper, **and**
    near — depth is not a parent link, and a deep control across the window
    is not the dialog's.

  A wide Czech dialog's far `Zrušit` now joins; the toolbar button 250 px
  above it in the window behind does not. Both are tested.

Fill all four and the rest happens here. `window` numbering: 0 = foreground,
then walk order; `depth`: 0 for the top-level window node itself.

### F. `visible_only` — in, both sides

`Op::UiTree { visible_only: bool }` with `#[serde(default)]`; a test pins
that `{"op":"ui_tree"}` still parses as "everything". `Platform::
ui_tree_visible()` defaults to `ui_tree()`, so until you override it you are
only slower. `Session::ui_read` always asks for visible only, because
`compress` drops the rest on arrival; anyone holding the `Pointer` directly
can still ask for everything (`Pointer::ui_tree(false)`).

The contract on your override, in the doc: return every node `ui_tree` would
have flagged `visible: true`, and nothing stricter. `IsOffscreen == false`
provider-side is exactly that.

### G. Refused releases — in, my side

`Agent::release()` no longer discards. A refused key-up or button-up is
recorded as `release_refused` (with the keys and buttons by name) and kept on
a **machine-wide** stuck list; the next `perform` on *any* connection retries
them first — before its own batch, so a stuck Ctrl does not turn that batch's
clicks into Ctrl+clicks — and records `release_retried` with counts either
way. The batch proceeds even if some are still stuck: whatever refused the
key-up refuses the batch too, and that is the more useful error.

Machine-wide rather than per-connection because the connection that pressed
the key has usually just disconnected — that is the case that produced the
refusal. Test: connection one presses Ctrl and drops while key-ups are
refused; the OS relents; connection two's unrelated `perform` releases Ctrl
before its first step.

Your `release_all` keeping refused keys is the right complement: `Platform::
key(_, false)` should just return the error, and neither swallow it nor retry
inside the platform.

### H. The three ideas

- **Validate before acting.** Yes, for protocol 3, as a step. But the cheap
  implementation is the one worth planning for: not a fresh walk, but
  `ElementFromPoint` at the step's target and a compare against the expected
  role/name/enabled — microseconds, and it answers the actual question ("is
  what I am about to click still what I think it is"). A `Step::Expect {
  role, name, x, y }` that fails the `perform` with a new kind (`stale`?) and
  releases what the batch had pressed. Not in this drop; say if you want the
  wire shape now so both sides can prototype.
- **`armed` in `ready`.** Taken now, as an optional field: `ResultBody::Ready
  { armed: Option<bool> }` (`skip_serializing_if` none), `Platform::armed()
  -> Option<bool>` defaulting to `None`, `RemotePointer::armed()`, and the
  `ns-pointer` CLI prints a note at connect when it is `Some(false)`. `None`
  is "no gate, or not saying" and is deliberately not read as either answer;
  a test pins that `NullPlatform` yields `None` and a gated platform's
  `Some(false)` reaches the client. Return `Some(self.armed.load())` from
  your platform and it is done. The MCP server does not surface it yet —
  once you fill it I will put it in the `initialize` result or the first tool
  reply, whichever the model reads.
- **Arming expiry.** Idle expiry, yes — measured from the last accepted
  `perform`, not wall-clock since the chord, so an active session never
  disarms under the model's hands. Disarm on the manual brake, yes. The MCP
  side needs nothing for either: a disarm mid-session shows up as the next
  `needs_confirmation`, and the hint already tells the model to relay the
  chord and stop. One request: when you disarm on idle, say so in the
  `detail` ("disarmed after N minutes idle; press … to arm") so the model
  can tell the person *why* it is asking again.

## §4. Your changes

Nothing there touches this side. Two notes:

- **Timeouts on the UIA client**: a window skipped for hanging is invisible
  to the compressor by construction — it just is not there — so `find` on
  its controls comes back empty. If you can, emit one node for the skipped
  window with its title and `enabled: false` (role `window`, its rectangle);
  the compressor keeps named nodes, and "Firefox (not responding)" in the
  list is a better answer than an absence.
- **`text` on character boundaries**: good. Note the client already sends
  `Step::Text` per character (typing rhythm), so the chunking only matters
  for `clipboard_write`-then-paste and a raw `perform` from a shell.

## §5. Rollout

Unchanged from your list. On this side the order was A, B, C–G, `armed`, then
the bundle. On yours: build on the target, fill the four `UiNode` fields,
override `state()` and `ui_tree_visible()`, return `armed()`, return refused
releases from `key()` instead of swallowing them.

## What I need back

- The build result on the target — the `raw-dylib` problem only shows there.
- The Czech dialog tree, and if you have it, one of the 4–5% zero-extent
  elements with its parent's name and rectangle. That decides the
  `Option<Point>` question for good.
- Whether owned dialogs appear under their owner's subtree in UIA (your open
  check in §2). If they do not, `window` numbering needs a rule for them, and
  I would rather know before the modal path depends on it.
- Whether you want the `Step::Expect` wire shape now.

---

## Addendum, 2026-09-07: `armed` now reaches the model

The one thing §H left for later on this side is done. `ns-pointer-mcp` reads
`armed` and `local_override` off `ready` and:

- puts both in the `initialize` result's `instructions` — the field the MCP
  specification gives a server to tell the model how it should be used, and
  the one place a client hands to the model before any tool is called. Not
  armed reads as "a person at that machine must press its arming chord before
  the first click, key or text is accepted; tell the user before you start,
  and do not retry in a loop";
- repeats them in the `screens_list` reply (`"armed"`, `"local_override"`, a
  `"note"` while not armed), since every tool description says to call that
  first;
- keeps `armed` current from what `perform` answers: an accepted commit sets
  it, `needs_confirmation` clears it, and a move or a read says nothing. An
  agent that never sent `armed` is never claimed to be armed, whatever its
  performs accept;
- mentions the machine's gate in its own `confirm` refusal, so a model does
  not learn about the second gate from a second refusal.

Nothing changed on the wire; protocol stays 2. Your idle-expiry `detail` is
still the one request: when you disarm, say why, so the model can tell the
person why it is asking again.
