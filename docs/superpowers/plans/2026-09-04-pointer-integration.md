# Connecting the halves — from a verified `Platform` to a driven desktop

**Status:** plan. Follows the Windows-side verification report of 2026-09-04.
Prior plan: `2026-09-04-remote-pointer.md`.

---

## 1. What the Windows verification actually established

The report is worth reading as evidence about the *design*, not only about
that machine. Three decisions were made blind on a box with no display server
and are now confirmed against real Win32:

- **Virtual-desktop normalization.** A move landed pixel-exact at `x = 2032`,
  past the primary's 1920. Normalizing 0–65535 against the primary alone would
  have put that click on the wrong monitor **and reported success** — the
  silent-failure case `docs/pointer-protocol.md` §3 names.
- **Surrogate pairs.** `Příliš žluťoučký kůň 🐎` round-tripped as 22 characters
  and 23 UTF-16 units. The extra unit is the horse: the pair went out as two
  `KEYEVENTF_UNICODE` inputs inside one batch and Windows reassembled it.
- **`ui_tree` works**, returning the taskbar as 30 controls.

And one thing it correctly refused to claim: **`local_activity` is only
half-verified, and it is the wrong half.** 330 injected moves and a click from
a separate process produced zero activity events, which proves the injected-
input filter holds cross-process — and is exactly what a `fn local_activity()
-> bool { false }` would also produce. §4 below is about making that
distinction impossible to lose rather than about testing it harder.

### One gap the report revealed by accident

The taskbar came back as **"Hlavní panel"**. `ui.rs`'s `MODAL_WORDS` are
English only — `accept`, `cancel`, `confirm`, `ok`. On a Czech desktop, modal
detection is simply off: a `Zrušit` / `Potvrdit` dialog scores 0.6 from
nothing and stays in the background list. Phase C fixes it; nobody would have
found it without a non-English machine.

---

## 2. The step runner is the wrong next thing

The Windows session proposed `ns-pointerd steps`, reading protocol JSON from
stdin, so that it could act as the MCP client itself. It named the cost
honestly — a duplicate of `agent.rs`'s interpreter — and judged it acceptable
because a stdin harness has no socket and so owes none of §4's duties.

That reasoning is right about the harness and wrong about the priority. The
blocker is not that the steps cannot be *expressed*; it is that
**`ns-pointerd` does not listen.** `netstat` showed nothing on the pointer
port because nothing in either codebase binds one: `Agent::serve` takes a
stream and there is no accept loop in the workspace. Meanwhile
`ns-pointer-mcp` is written as a *client* — it dials `NS_POINTER_ADDR`.

So the step runner builds a second execution path to avoid a missing
fifteen-line one, and leaves the first still missing. Worse, two paths into
the same `Platform` is the shape that drifts: the stdin one has no rate limit,
no local override, no held-key release, and the day one of them gains a step
kind the other does not is the day a conformance suite stops meaning anything.

**Build the accept loop instead.** It is smaller than the step runner, it
deletes the duplication rather than accepting it, and it arrives with all six
§4 duties already written and tested.

---

## 3. Phases

### A — make `ns-pointerd` listen  *(the blocker; ~15 lines each side)*

**A1, here — done.** `nspointer::agent::{bind, serve_listener, serve_tcp}`,
five tests over real ephemeral ports.

Building it surfaced a bug that only an accept loop could reveal: **the rate
limit was per connection.** `Bucket` was constructed inside `serve`, so a
caller could reset its budget by opening a second socket — defeating, by
reconnecting, the enforcement §3b argues must live where a caller cannot
reach it. The bucket, the suspension deadline and the connection count are now
all machine-wide, and three tests say so. The thing being protected is one
desktop; nothing about it is per socket.

Two hard refusals rather than warnings, since both failure modes are silent
and permanent: an empty token, and a non-loopback bind without `allow_remote`.
A caller past the connection cap is told *why* before the hangup — otherwise a
full agent is indistinguishable from a dead one.

A smaller trap, fixed on the way: the bucket was primed on first connect,
guarded by `last_ms == 0`. A test clock that starts at zero re-primed on every
connection, which is exactly the per-connection budget being removed. It is
primed at construction now, and `with_clock` re-primes, so the budget is
always measured on the clock actually installed.

**A2, Windows:** replace the canned probe subcommands with

```rust
let agent = Agent::new(WindowsPlatform::new()?, AgentConfig {
    token: std::env::var("NS_POINTER_TOKEN")?,
    limits: Limits::default(),
});
serve_tcp(agent, "127.0.0.1:7373").await?;
```

That is the whole change, and it retires the probe demos: every canned
`click`/`move` becomes expressible as a `perform`, which is what the step
runner was for.

**A3:** run the chain. `ns-pointer-mcp` on the machine that has the MCP
client, `NS_POINTER_ADDR` pointing at the Windows box.

**Risk, unresolved:** I do not know whether this Linux box can reach that
Windows machine at all. If it cannot, `ns-pointer-mcp` is built for Windows
too and runs beside the agent over loopback — the MCP client and the agent on
one host, which is the arrangement every comparable server uses anyway
(plan §8). That is a build target, not a redesign.

### B — the first real task

With A up, "open a browser and search for dog images" is executed by the MCP
client, not by this protocol: `ui_read` → find the browser → `pointer_click`
at the point it returns → `type_text` → `key_press enter`. The reason to
expect it to work is that `ui_find` returns a point rather than a guess, and
the report already shows `ui_tree` populated.

Acceptance is one task, end to end, with the compression ratio recorded:
every `ui_read` reports `raw_controls` against `controls`, and that number
against a real Windows tree is the first measurement of `ui.rs` that means
anything (plan §13 — the A11y-Compressor figures are motivation, not a claim
about this implementation).

### C — modal detection beyond English

`MODAL_WORDS` becomes a configurable list with a non-English default set, or
the keyword signal is dropped in favour of role plus the temporal difference,
which are language-independent and already implemented. Leaning toward the
second: a hand-maintained keyword list per locale is the same species of
magic-number table this design rejected in §13, and the temporal half needs no
vocabulary at all. Decide with a real Czech dialog in front of us.

### D — make a silent `local_activity` impossible

§4 below. Independent of A; do it whenever.

### E — still unexercised

DPI other than 100%, and the two `Blocked` paths, which need an elevated
foreground window and a UAC prompt. Both are single manual checks once A is
up, not development.

---

## 4. `local_activity`: change the shape, not the test

The Windows session is right that the positive direction needs a physical
hand, and right that this is the failure that matters — the local override is
the only brake the machine's owner has. But "run the watcher and wave the
mouse" is a check that passes once and then rots, because the same zero
appears whether the hook is working or was never installed.

Two changes make the silent case impossible:

1. **The hook's installation is a startup error, not a runtime default.** If
   the raw-input registration or the low-level hook fails, the agent refuses
   to start, or starts with a loud, logged, repeated warning that the override
   is dead. A `Platform` that cannot see the user must not silently pretend
   the user is absent.
2. **Report last-seen, not a bare bool.** `local_activity() -> bool` cannot
   distinguish "nothing happened" from "nothing is watching". Something like
   `local_state() -> LocalState { last_event_ms: Option<u64>, hook_ok: bool }`
   can, and the agent can then say `suspended` *or* warn that it does not
   know. This is a change to `Platform`, so it needs agreeing before their
   impl hardens around the current signature — which is why it is in this plan
   and not deferred.

Then the manual check is a one-time acceptance rather than the whole
guarantee, and `hook_ok: false` is visible in the audit log forever after.

---

## 5. Consent, and where the gate actually is

The Windows session raised per-task agreement before driving a live desktop
from natural language rather than treating one "yes" as standing permission.
That is the right instinct and it matches an asymmetry already recorded in
plan §12: **the harness path has a confirmation gate and the MCP path does
not.**

`pointer_click` and `pointer_type` are `SideEffect::Irreversible`, so
`SideEffectGate` stages them and `stage()` names the coordinates and the text
before anything happens. An MCP client gets none of that — the specification
asks *clients* to keep a human in the loop and cannot enforce it, and the
survey found no server that does.

So the per-task agreement is not caution to be talked out of; it is standing
in for a gate that the MCP path structurally lacks. If this becomes routine
rather than occasional, the durable fix is to give the MCP server the same
staging the harness already has — an `Irreversible` tool answering `isError`
with "confirm by calling again with `confirm: true`" — rather than relying on
the operator to remember.
