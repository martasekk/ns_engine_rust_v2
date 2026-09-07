# ns-engine

A Rust workspace for an agent that emits **actions**, not prose. A language
model proposes structured actions against a declared schema; a deterministic
engine validates, guards, stages and executes them; a second model writes the
reply from the trace of what actually happened. The model never gets to say
that something was done — the trace does.

One of the action families is **pointer control**: moving the mouse, clicking,
typing and reading the UI tree of *another machine*, over an authenticated
socket, behind two independent confirmation gates.

```
crates/core           actions, events, values, validation, learned rules
crates/engine         the turn loop: emit → validate → guard → execute → reply
crates/llm            provider-agnostic chat client, emitter/replier/summarizer
crates/memory-sqlite  the event log and an FTS5 index over it
crates/provenance     where a fact came from
crates/evolution      the self-improvement pass: mine → propose → gate → apply
crates/channel-cli    a REPL channel, generic over reader/writer
crates/components-std time, HTTP and pointer tools
crates/pointer        remote pointer, keyboard, clipboard and UI reading
app                   ns-app: wires a config file into a running harness
```

## The turn loop

The engine is the part that must not be clever. Each turn:

1. The **emitter** model receives the persona, the working-memory window, the
   relevant facts and the action schema, and returns actions as JSON.
2. Every action is **validated** against its schema, then run past the guards
   and the learned rules.
3. Anything irreversible is **staged**: the engine answers with a synthetic
   `confirm_pending` action that is valid for exactly one turn. Nothing with a
   side effect happens without a confirmation crossing the loop.
4. Surviving actions execute; each outcome is appended to the log.
5. The **replier** model writes the user-facing text from the trace, and a
   grounding check regenerates a reply once if it invents numbers or names
   that appear nowhere in its material.

Working memory keeps the last few turns verbatim, pins user facts, and folds
everything older into a rolling summary. `recall` searches the whole log.

## Mouse control — the `pointer` crate

The design rule: **every decision that can be got wrong is made on this side.**
Which screen, which pixel, what a normalized coordinate means, how a gesture
decomposes into steps — all of it is resolved here, in code that runs and is
tested on a machine with no display server. The agent on the target machine
receives absolute physical pixels and replays them.

That makes a second operating system cheap. A new agent implements one trait
— `Platform`, eight required methods over the OS injection API — and inherits
every coordinate rule already proven here. See `docs/pointer-work-split.md`
for exactly who owns what, and `docs/pointer-protocol.md` for the wire format.

What ships:

- **`nspointer` (library)** — geometry, motion synthesis, gestures, the RPC
  client, a mock pointer, and `ui::compress`, which turns a raw accessibility
  tree into text a model can read (a real tree is ~73% off-screen; that part
  is dropped on arrival).
- **`ns-pointerd`-side `agent`** — the accept loop, machine-wide limits, and
  the local-override brake.
- **`ns-pointer` (CLI)** — drive an agent from a shell.
- **`ns-pointer-mcp`** — a hand-rolled JSON-RPC-over-stdio MCP server exposing
  12 tools to any MCP client. Coordinates are absolute by default, with one
  optional `screen` argument rather than two schemas.
- **the ns-app path** — a `[pointer]` section registers `pointer_click`,
  `pointer_type`, `pointer_ui_read` and the rest as ordinary harness actions,
  staged behind the same confirmation flow as everything else.

### Two gates, cooperating

The MCP gate is **advisory**: it is a property of one client session, and a raw
socket can ignore it. The machine's own gate is not. Both are reported, so a
refusal can say which one is closed:

- the **agent** arms once per session (`Confirm::FirstAction` by default) and
  names the action in its refusal;
- the **local override** — a low-level input hook on the target machine — is a
  physical brake. `local_hook_ok` defaults to *false*, because an agent that
  has not said it installed a hook is assumed not to have one, and a dead brake
  looks exactly like a quiet user.

Arming state reaches `hello`, `screens_list`, the MCP `initialize`
instructions and the CLI, so a caller hears that the first click will be
refused instead of finding out from the refusal.

## Running it

```sh
cargo build --workspace
cargo test  --workspace          # 334 tests, no network required

cp config.example.toml config.toml
cargo run -p ns-app -- providers # which providers exist, which keys are set
cargo run -p ns-app              # the REPL
cargo run -p ns-app -- evolve --dry-run
```

API keys never live in the config file. A provider names the *environment
variable* that holds its key (`api_key_env`), and the same pattern covers the
pointer token (`token_env`). Presets ship for openrouter, mistral, ollama,
lmstudio, llamacpp, openai and groq; anything speaking OpenAI-style
`POST {base_url}/v1/chat/completions` with function tools works by spelling
out `base_url`. `NS_PROVIDER` / `NS_MODEL` override for a single run.
`docs/providers.md` has the full guide.

To drive a desktop, uncomment `[pointer]` in `config.toml`, point `addr` at a
machine running the agent (started with `allow_remote` for a non-loopback
bind), and export the token. `NS_POINTER_ADDR` overrides the address.

## Docs

| | |
|---|---|
| `docs/pointer-protocol.md` | the wire contract, for whoever writes an agent |
| `docs/pointer-work-split.md` | which half implements what, and why |
| `docs/windows-handoff.md` | notes to and from the Windows agent maintainer |
| `docs/providers.md` | swapping the model, per role or per run |
| `docs/superpowers/` | specs and plans |
| `docs/research/` | the literature the design leans on |

## License

Not yet licensed — all rights reserved.
