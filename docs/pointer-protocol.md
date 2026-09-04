# ns-pointerd protocol — what the desktop agent implements

**For whoever writes the agent.** Every JSON sample below is printed by
`cargo run -p ns-pointer --example protocol_samples`, so it is generated from
the types rather than written beside them. Protocol version **2**.

Plan: `docs/superpowers/plans/2026-09-04-remote-pointer.md`.

---

## The contract in one paragraph

Newline-delimited JSON over a stream socket: one object per line, requests and
responses paired by `id`. **Six operations** (two of them optional), and six step kinds. Every coordinate the agent ever sees
is an absolute physical pixel in virtual-desktop space, already clamped to a
real screen. The agent performs no coordinate mapping, no clamping, no easing,
no typing rhythm and no gesture composition — all of that happens in `ns-pointer` and is
unit-tested there. Path *shape* (straight, Bézier, Gaussian, Perlin) is chosen
client-side too, and reaches you as nothing but a longer list of points.

**Everything you implement is in the two tables below.**

---

## 1. What you receive

### `hello` — first message on every connection

```json
{"id":1,"op":"hello","token":"<shared secret>","protocol":1}
```

Answer anything else on an unauthenticated connection with `unauthorized`.
Reject a `protocol` you do not implement with `protocol`; do not guess.

### `screens` — the layout

```json
{"id":2,"op":"screens"}
```

### `position` — where the pointer is now

```json
{"id":3,"op":"position"}
```

### `perform` — the only one with substance

```json
{"id":4,"op":"perform","steps":[
  {"step":"move","x":1279,"y":719},
  {"step":"sleep","ms":8},
  {"step":"move","x":1280,"y":720},
  {"step":"button","button":"left","down":true},
  {"step":"sleep","ms":40},
  {"step":"button","button":"left","down":false},
  {"step":"scroll","dx":0,"dy":-3}
]}
```

Replay the steps **in order**, then answer once. Four step kinds, and the
whole implementation is a loop over a match:

| step | fields | meaning |
| --- | --- | --- |
| `move` | `x`, `y` (i32) | Absolute physical pixel, virtual-desktop coordinates. Signed: a monitor left of the primary has negative `x`. |
| `button` | `button` (`left`\|`right`\|`middle`), `down` (bool) | Press or release **where the pointer already is**. Never moves. |
| `scroll` | `dx`, `dy` (i32) | Notches, not pixels. Positive `dy` scrolls down, positive `dx` right. |
| `key` | `key` (object), `down` (bool) | Press or release one key, **where the pointer already is**. See below. |
| `text` | `text` (string) | Type the literal characters. On Windows: `SendInput` with `KEYEVENTF_UNICODE`, `wVk = 0`, `wScan` = the UTF-16 code unit. Non-BMP characters (emoji) are two inputs, one per surrogate. |
| `sleep` | `ms` (u32) | Wait. Typically 8ms between path points, 40ms inside a click, ~70ms between characters. |

A single click arrives as one `perform` of ~40 steps: it is one round trip,
not forty.

### `text` and `key` are different mechanisms, deliberately

A character and a key are not the same thing. The physical key labelled `Q` on
QWERTY is `A` on AZERTY, so "type q" and "press the Q key" have different right
answers, and neither mechanism can do the other's job:

- **`text`** injects the character itself and never consults the layout. It is
  the only reliable way to produce `@`, `#` or an accented letter on a machine
  whose layout you do not know. It cannot express a chord.
- **`key`** presses a key. It is the only way to do Ctrl+C, Enter, F5 or an
  arrow — none of which are characters.

```json
{"step":"key","key":{"k":"enter"},"down":true}
{"step":"key","key":{"k":"ctrl"},"down":true}
{"step":"key","key":{"k":"char","c":"c"},"down":true}
{"step":"key","key":{"k":"f","n":5},"down":true}
{"step":"text","text":"user@example.com"}
```

`{"k":"char","c":"c"}` means *whichever key produces `c` on the target's
current layout* — `VkKeyScanW` on Windows. It is for the `c` in Ctrl+C, not for
typing prose.

Named keys: `enter tab escape backspace delete insert space up down left right
home end page_up page_down ctrl alt shift meta ctrl_right alt_right
shift_right`, plus `{"k":"f","n":1..24}` and `{"k":"char","c":"…"}`.

### Releasing keys is the agent's responsibility too

The client balances every chord it sends and cleans up after a `perform` that
fails partway. That is not sufficient on its own: **release every held key when
a connection drops, and when the local override engages.** A stuck letter is
noise; a stuck Ctrl makes the machine unusable until someone taps the physical
key, and a dropped TCP connection is exactly when nobody is in a position to
send the release.

### `clipboard_read` / `clipboard_write` — optional, protocol 2

```json
{"id":5,"op":"clipboard_read"}
{"id":6,"op":"clipboard_write","text":"a long pasted document"}
```

**You may skip both.** Answer `unsupported` and callers route around it —
that is why they are separate operations rather than step kinds, and why a
missing clipboard is not a broken agent.

Worth implementing anyway, for two reasons that have nothing to do with
convenience. `clipboard_write` then ctrl+v is how bulk text should move:
`text` is per-character, so four thousand characters is eight thousand steps.
And `clipboard_read`, after ctrl+a ctrl+c, is the only way in this protocol to
get **data back off the machine without capturing its screen** — a text field
or a document read as text, no image transport, no DPI registration.

**Log the length, never the contents.** A clipboard holds passwords often
enough that recording it would turn the audit trail into the leak.

---

## 2. What you send back

Exactly one response per request, same `id`.

```json
{"id":1,"ok":true,"result":{"kind":"ready","agent":"ns-pointerd 0.1.0","platform":"windows","protocol":1}}
{"id":3,"ok":true,"result":{"kind":"position","x":1280,"y":720,"state":7}}
{"id":4,"ok":true,"result":{"kind":"performed","steps":7,"state":7}}
{"id":5,"ok":true,"result":{"kind":"clipboard","text":"a long pasted document"}}
{"id":4,"ok":false,"error":{"kind":"blocked","detail":"target window is elevated (UIPI)"}}
```

```json
{"id":2,"ok":true,"result":{"kind":"screens","state":7,"screens":[
  {"id":"PRIMARY-EDID-A1","bounds":{"x":0,"y":0,"w":2560,"h":1440},"scale":1.5,"primary":true,"label":"Dell U2723"},
  {"id":"LEFT-EDID-B2","bounds":{"x":-1920,"y":0,"w":1920,"h":1080},"scale":1.0,"primary":false,"label":"ASUS VG248"}
]}}
```

### The four fields of a screen that matter

| field | why it is not optional |
| --- | --- |
| `id` | **Must be stable across reboots, driver updates and replugging.** Derive it from the monitor's device path or EDID — never a GDI device index, which Windows reassigns on all three. An agent keyed by index eventually clicks confidently on the wrong monitor. |
| `bounds` | Physical pixels, virtual-desktop coordinates, signed origin. `w`/`h` are the real pixel dimensions, not the DPI-scaled ones. |
| `scale` | 1.0 at 96 dpi, 1.5 at 150%. Reported so a caller can register an externally captured screenshot against the screen. |
| `primary` | Which screen the virtual desktop's origin belongs to. |

### `state`

A counter you increment whenever the **display configuration** changes —
resolution, arrangement, a monitor connected or removed. Returned on every
response. It lets a caller notice the layout moved between reading `screens`
and acting on it. It does **not** track screen *contents*; nothing here does.

### Errors

| `kind` | when |
| --- | --- |
| `unauthorized` | no valid `hello`, or a bad token |
| `suspended` | the local override is active — the machine's owner has taken control back. Resolves on its own; distinct from `blocked` for that reason. |
| `blocked` | the OS refused the injection. See §3. |
| `out_of_bounds` | a `move` landed on no screen. The caller clamps, so this means the layout changed underneath it — compare `state`. |
| `unsupported` | known operation you have not implemented — the clipboard, typically. A legitimate permanent answer, not a failure |
| `protocol` | unparseable, unknown op, or a version you do not implement |
| `internal` | anything else, with `detail` |

---

## 3. Windows behaviours that must not be swallowed

Each of these is a silent failure if the agent trusts the API's return value.

- **UIPI.** A medium-integrity process cannot inject into a window owned by an
  elevated one. `SendInput` **returns success and nothing happens.** Detect the
  foreground window's integrity level and answer `blocked` — otherwise you
  report a click that never occurred, which is worse than an error.
- **The secure desktop.** UAC prompts, the lock screen and Ctrl+Alt+Del accept
  no injected input by design. `blocked`, not a bug to fix.
- **Session 0 isolation.** As a Windows *service* the agent has no interactive
  desktop and can drive nothing. Run it in the user's session — a scheduled
  task at logon, not a service.
- **DPI.** Without per-monitor DPI awareness Windows silently scales your
  coordinates. `enigo` switches the calling thread's DPI context per call; a
  hand-rolled `SendInput` backend must do the same, and must use
  `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` with the 0–65535
  normalization taken across the **virtual desktop**, not the primary monitor.

## 4. Non-negotiables — the agent's own duties

Everything in §1 is coordinate work the client has already done for you. This
section is the part **only** the agent can do, and it is the real work.

The client limits what it sends. That is worth nothing as a guarantee:
**anything that can open this socket can speak this protocol**, so every rule
below has to be enforced where a caller cannot reach it.

1. **Authenticate.** Loopback unless explicitly configured otherwise, and never
   an unauthenticated mode — not even behind a flag, because the flag is what
   ends up running.
2. **Cap and rate-limit.** A maximum `steps` per `perform`, and a ceiling on
   performs per second. An agent in a loop should hit a wall, not flood the
   input queue. Reject over the cap with `protocol`; answer over the rate with
   `internal` and a `detail` saying so.
3. **A local override.** Physical mouse movement over a threshold, or a
   hotkey, suspends remote input and answers `suspended` until it lapses. The
   person at the keyboard outranks the socket.
4. **Release held keys and buttons** on disconnect and when suspending. The
   client balances what it sends and recovers modifiers from a failed
   `perform`, but a dropped connection is exactly when there is no client left
   to send the release, and a stuck Ctrl is not a failed operation — it is an
   unusable machine.
5. **Detect OS refusals** rather than trusting a return value (§3), and report
   `blocked`.
6. **Be visible while live**, and keep an append-only log of what was actually
   injected. A log written by the client records intent; only yours records
   effect.

Items 1–6 are why "the agent is a `match` in a loop" is true of the step
interpreter and false of the agent.

## 5. A minimal agent, in shape

```
loop over lines:
    req = parse(line)                       -> on failure: protocol
    if not authed and req.op != hello       -> unauthorized
    match req.op:
        hello    -> check token+protocol, reply ready
        screens  -> enumerate monitors, reply screens
        position -> reply position
        clipboard_read  -> reply clipboard, or unsupported
        clipboard_write -> set it, reply clipboard, or unsupported
        perform  -> for step in steps:
                        move(x, y) | button(b, down) | scroll(dx, dy)
                        key(k, down) | text(s)       | sleep(ms)
                        on OS refusal -> blocked, stop
                    reply performed
on disconnect / on suspend:
    release every key and button still held
```

That is the whole agent. The interesting parts are the monitor enumeration in
§2 and the refusal detection in §3; the rest is transcription.
