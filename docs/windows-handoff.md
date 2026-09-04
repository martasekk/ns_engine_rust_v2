# What's left on the Windows machine

Everything on the Linux side is built and tested. This is the whole remaining
list, in order. The first item is the only one that blocks anything.

---

## 1. Make `ns-pointerd` listen  *(the blocker — five lines)*

`Agent::serve` takes a stream and, until now, nothing in the workspace bound a
port. That is why `netstat` was empty: not a failure to start, but an accept
loop nobody had written. It exists now.

Add `ns-pointer` as a dependency and replace the canned probe subcommands with:

```rust
use nspointer::agent::{serve_tcp, Agent, AgentConfig, Limits, Listen};

let agent = Agent::new(WindowsPlatform::new()?, AgentConfig {
    token: std::env::var("NS_POINTER_TOKEN")?,
    limits: Limits::default(),
});
serve_tcp(agent, &Listen::loopback(7373)).await?;
```

`serve_tcp` binds, accepts, and gives every connection its own task. It
refuses to start on an empty token, and refuses a non-loopback address unless
`allow_remote` says you meant it — both of those fail silently and permanently
otherwise.

**The local override, the rate ceiling and the connection cap are machine-wide,
not per socket.** Writing the accept loop is what exposed that: the token
bucket used to be created per connection, so a caller could reset its budget by
reconnecting. Three tests hold the line now.

### This replaces the step runner

`ns-pointerd steps` is no longer worth building. Everything a canned `click` or
`move` demo did is a `perform` over this socket, and going through the socket
means it inherits the rate limit, the override, held-key release and the audit
log — none of which a stdin interpreter beside the `Platform` would have had.
One execution path, so nothing drifts.

## 2. Build `ns-pointer` and drive it from a shell

```
cargo build -p ns-pointer --bins      # ns-pointer, ns-pointer-mcp
```

```
NS_POINTER_TOKEN=… ns-pointer screens
NS_POINTER_TOKEN=… ns-pointer move 2032 1126
NS_POINTER_TOKEN=… ns-pointer type Příliš žluťoučký kůň 🐎
NS_POINTER_TOKEN=… ns-pointer key ctrl+c
NS_POINTER_TOKEN=… ns-pointer ui
NS_POINTER_TOKEN=… ns-pointer find Firefox
```

`ns-pointer type` reports both counts — `typed 22 chars (23 UTF-16 units)` —
because they differ exactly when a surrogate pair is involved, which is the
case worth seeing.

**Run it beside the agent, over loopback.** That is not a fallback from a
cross-machine setup, it is the right shape: no network hop, no `allow_remote`,
no reachability to arrange, and it is what every comparable MCP server does.

### The end-to-end proof

With the agent running, `ns-pointer find Firefox` returning a point and
`ns-pointer click X Y` acting on it is the whole chain working, before any MCP
client is involved. That is the acceptance test for item 1.

## 3. `local_activity` — change the shape *(agree this before the impl hardens)*

The verification is right that the positive direction needs a hand, and right
that this is the failure that matters: the override is the only brake the
machine's owner has. But "run the watcher and wave the mouse" passes once and
then rots, because the same zero appears whether the hook works or was never
installed.

**Done on this side, non-breaking.** `Platform` gains one optional method:

```rust
fn local_hook_ok(&self) -> bool { false }   // note the default
```

Your existing impl compiles untouched and reports `false`, which is the safe
reading: an agent that has not said it installed a hook is assumed not to have
one. **Return `true` only where the raw-input registration or the low-level
hook actually succeeded**, and `false` again if it later goes away.

The agent then puts it in `Ready`, logs `no_local_override`, and `ns-pointer`
prints a warning on every command:

```
warning: this agent reports no local override — moving the physical mouse
will not interrupt anything sent from here.
```

That is the whole change, and it turns the wave-the-mouse check from the whole
guarantee into a one-time acceptance: after it, a dead hook is visible in the
audit log and on every connection rather than indistinguishable from a quiet
user.

## 4. `ui_tree` — the highest-value optional method

Optional and defaulted to `Unsupported`, so it does not block anything, but it
is what turns "click at (812, 344)" into "click the control named Save" and
removes the coordinate-registration problem rather than solving it.

Return **everything you can see, filtered by nothing** — including invisible
and disabled nodes with the flags set. The caller compresses (`ui::compress`,
built to the A11y-Compressor pipeline); a filter on your side would be a
second, untested, per-platform copy of that judgement. `center` in absolute
virtual-desktop pixels, `h` used only to derive the layout-block threshold.

Every `ui_read` reports `raw_controls` against `controls`, so the first real
Windows tree is also the first measurement of whether the compression is worth
anything.

## 5. A gap the verification found by accident

The taskbar came back as **"Hlavní panel"**. `ui.rs`'s modal keywords are
English only — `accept`, `cancel`, `confirm`, `ok`. On a Czech desktop, modal
detection is simply off: a `Zrušit` / `Potvrdit` dialog scores nothing and
stays in the background list.

**Fixed on this side, and better than the option I proposed.** I had suggested
dropping the keyword signal; the real bug was narrower and the fix is
strictly better.

A dialog was already found by *role*, which carries no vocabulary — that part
worked in Czech all along. What failed was the second pass that attaches a
dialog's own buttons to it: it looked for English decision keywords, so
`Zrušit` and `Potvrdit` stayed in the background list while the dialog they
belong to was announced. That is the half a caller actually needs.

It now attaches any **interactive** control near a detected modal. Role and
proximity carry no vocabulary, so it works in every language, and the keyword
list is demoted to one weak extra signal for a banner with no dialog-ish role
at all. There is a Czech test and an English one, asserting identical
behaviour.

## 6. Still unexercised, and cheap once item 1 is up

- **DPI other than 100%.** One check on a scaled monitor.
- **The two `Blocked` paths** — an elevated foreground window, and a UAC
  prompt. Both are `SendInput` returning success and doing nothing, so the
  test is that `ns-pointer click` reports `Blocked` rather than success.

---

## What is already proven

From the verification report, and worth keeping because each one confirms a
decision made blind on a machine with no display:

| | result |
| --- | --- |
| `move_to` | pixel-exact, including `x = 2032` on the second monitor — normalizing 0–65535 against the primary alone would have clicked the wrong screen **and reported success** |
| `text` | `Příliš žluťoučký kůň 🐎` round-tripped, 22 chars / 23 UTF-16 units — the surrogate pair went out as two inputs in one batch |
| injected-input filter | 330 injected moves and a click from a separate process produced zero activity events; holds cross-process |
| `ui_tree` | populated, 30 controls for the taskbar |
| `local_activity` | **half-verified, and the wrong half** — see item 3 |
