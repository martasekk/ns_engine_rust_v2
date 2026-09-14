# Providers — running the agent on a different model

Everything the harness sends is `POST {base_url}/v1/chat/completions` with
OpenAI-style function tools. Any provider that speaks that works; seven are
built in as **presets** so a swap is one word.

```
cargo run -p ns-app -- providers
```

lists them, shows which API keys are present, marks the one this config
uses (`→`), and prints what each role resolves to. Start there.

---

## 1. The two roles

| Role | What it does | Needs |
| --- | --- | --- |
| `emitter` | turns your message into one tool call, or into the reply | **function calling**, and prose |
| `summarizer` | folds old turns into a rolling summary | JSON out; cheap is fine |

There were three until 2026-09-14, when the `replier` — a second model that
wrote the sentence you read from the turn's results — was folded into the
first. One call now chooses the action and writes the reply, so the model
that does it needs both function calling and prose worth reading.

They resolve **independently**. An unset role falls back to `[llm]`; the
summarizer falls back to the emitter. So the turn can run on a cloud model
while the summarizer stays local, or the other way round.

`[llm.replier]` is refused at startup by name rather than ignored: a config
that still pins a reply model would otherwise go on paying for a model it no
longer names.

## 2. Three ways to swap

```toml
# config.toml — the standing choice
[llm]
provider = "ollama"
```

```bash
# just this run, no edit
NS_PROVIDER=mistral cargo run -p ns-app
NS_PROVIDER=ollama NS_MODEL=qwen2.5:7b cargo run -p ns-app
```

```toml
# just one role — "provider:model" in that role's model field
[llm]
provider = "ollama"                              # emitter + summarizer local
[llm.summarizer]
model = "mistral:mistral-small-latest"           # the summarizer in the cloud
```

Only a **known provider name** counts as a `provider:` prefix, so model ids
that contain a colon (`qwen2.5:3b`, `gemma3:4b`) are left alone.

A preset fills in `base_url`, `api_key_env`, a default model, the request
spacing and prompt-cache support. Override any of them field by field:

```toml
[llm]
provider = "ollama"
base_url = "http://gpu-box:11434"   # same preset, different host
min_interval_ms = 0                 # request spacing (ms) for this provider
prompt_cache = false                # Anthropic cache breakpoints (OpenRouter only)
```

**Keys never live in config** — a provider names the *environment variable*
that holds its key. Local providers need no key at all.

## 3. The presets

| Name | Endpoint | Key env | Default model | Notes |
| --- | --- | --- | --- | --- |
| `openrouter` | `https://openrouter.ai/api` | `OPENROUTER_API_KEY` | `openrouter/free` | Many models behind one key. Free tier: 20 req/min, 50 req/day (1000/day once $10 of credit has ever been bought). The only provider that forwards Anthropic prompt-cache breakpoints. |
| `mistral` | `https://api.mistral.ai` | `MISTRAL_API_KEY` | `mistral-small-latest` | Free "Experiment" tier ≈ 1 req/s, 500K tokens/min. Does function calling. Requests are spaced 1100 ms apart automatically — it 429s on bursts otherwise. |
| `ollama` | `http://localhost:11434` | `OLLAMA_API_KEY` (optional) | `qwen2.5:3b` | Local, unlimited, no key. Pick a **non-thinking** model: Ollama's OpenAI endpoint ignores `think:false`, so qwen3 spends its whole output budget reasoning (minutes per turn on CPU). |
| `lmstudio` | `http://localhost:1234` | `LMSTUDIO_API_KEY` (optional) | — | Local. Model id is whatever LM Studio has loaded. |
| `llamacpp` | `http://localhost:8080` | `LLAMACPP_API_KEY` (optional) | — | Local. Start `llama-server` with `--jinja` or tool calls won't parse. |
| `openai` | `https://api.openai.com` | `OPENAI_API_KEY` | — | Name the model explicitly; the catalogue moves. |
| `groq` | `https://api.groq.com/openai` | `GROQ_API_KEY` | — | Fast, but the free tier is ~6K tokens/min per model — tight for this harness's context. |

Run end-to-end on this box: **ollama** and **mistral** (2026-09-03);
**openrouter** is what the harness was originally built against. The rest
are endpoint definitions following the same protocol — treat a first run on
them as a smoke test.

## 4. Setup: local (Ollama)

```bash
curl -fsSL https://ollama.com/install.sh | sh     # systemd service
ollama pull qwen2.5:3b
```

Give it room for the harness's context (facts + summary + 6 verbatim turns
+ tool schemas is ~1.1K tokens on a quiet session, more as facts pile up):

```bash
sudo mkdir -p /etc/systemd/system/ollama.service.d
printf '[Service]\nEnvironment="OLLAMA_CONTEXT_LENGTH=16384"\n' \
  | sudo tee /etc/systemd/system/ollama.service.d/override.conf
sudo systemctl daemon-reload && sudo systemctl restart ollama
```

Then `[llm] provider = "ollama"` and run. No key, no quota, no network.

**Model choice matters more than anything else here.** See §7.

## 5. Setup: a cloud provider

```bash
export MISTRAL_API_KEY=...        # the variable the preset names
cargo run -p ns-app -- providers  # confirm it shows "set"
cargo run -p ns-app
```

Put the `export` in your shell profile, never in a file in this repo.
(`~/.bashrc` returns early for non-interactive shells, so scripts need
`MISTRAL_API_KEY=$(bash -ic 'echo $MISTRAL_API_KEY' 2>/dev/null) cargo run …`.)

## 6. A provider that isn't a preset

Spell the endpoint out. Everything else still applies:

```toml
[llm]
base_url = "https://api.example.com/v1-compat"   # no trailing /v1
api_key_env = "EXAMPLE_API_KEY"
min_interval_ms = 500
[llm.emitter]
model = "their-model-id"
[llm.summarizer]
model = "their-model-id"
```

`base_url` is the part **before** `/v1/chat/completions`. A loopback URL is
treated as local (no key required) even when no preset matches it.

Requirements: OpenAI-shaped `/v1/chat/completions`, `tools` with function
schemas, and ideally `tool_choice: "required"`. A provider that ignores
`tool_choice` still works — the emitter treats a plain-text answer as
`respond_directly`.

To make it a preset instead, add one row to `PROVIDERS` in
`crates/llm/src/provider.rs`; the table there is the whole registry.

## 7. Picking a model (this box: 4 vCPU, 11 GB RAM, no GPU)

Choosing one tool call is the easy half. Turning a structured trace (recall
hits, tool outputs, fact lines) into a sentence is where small models fall
apart — and since 2026-09-14 the same call does both, so the model you pick
has to be good at the hard half.

- `qwen2.5:3b` — ~12 s/turn on CPU. Calls tools correctly. But it **copies
  its context instead of writing prose**: asked "whats my name" it has
  replied `fact user.name = "Peter"` verbatim, and because each reply is fed
  back into the next turn's verbatim window, the format sticks and every
  later turn answers the same way. It also continues the transcript
  (answering `hi` with `hi\nwhat time is it?`). Fine for exercising the
  engine; poor as a conversationalist.
- Larger local (7–8B) — noticeably better prose, ~3× slower on CPU. Worth it
  if the replies matter more than latency.
- **Put the turn on a cloud model and keep the summarizer local** — the
  summarizer's output is never read by a person, so it is the half that can
  stay cheap:
  ```toml
  [llm]
  provider = "mistral"
  [llm.summarizer]
  model = "ollama:qwen2.5:3b"
  ```
  Splitting the other way was the advice until the reply model went: it is no
  longer available, because there is no role left that writes prose nobody
  reads.
- Avoid reasoning/"thinking" models through Ollama's OpenAI endpoint (see
  the `ollama` row above).

## 8. Verifying a swap

```bash
cargo run -p ns-app -- providers        # → marks the active preset, roles listed
cargo run -p ns-app                     # the banner names each role's model
```

The banner is the ground truth:

```
ns-harness — emitter: qwen2.5:3b @ http://localhost:11434
```

Then ask something that uses a tool (`what time is it?`) and something that
doesn't (`hi`). `cargo run -p ns-app -- dump cli` prints the session log as
JSONL — every proposal, rejection and tool result for the turn.

## 9. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `X_API_KEY is not set — the emitter role needs a provider API key.` | No key in the environment for the resolved provider | `export X_API_KEY=…`, or switch to a local backend: `NS_PROVIDER=ollama` |
| `[llm] provider "ollamma" is unknown — known providers: …` | Typo in the preset name | Use a listed name (startup fails deliberately rather than falling back to someone else's endpoint — and someone else's bill) |
| `[llm.emitter] model is not set and provider "openai" has no default` | That preset ships no default model | Add `[llm.emitter] model = "…"` (and `[llm.summarizer]`) |
| `[llm.emitter] model is not set and base_url "…" matches no preset` | Custom endpoint, no model named | Name the model per role |
| `Sorry, I couldn't complete that. Reason: … transport: status 429` | Rate limit | Raise `min_interval_ms`, or move to a local provider. 429/5xx already retry at 1 s / 2 s / 4 s |
| `… transport: status 402` | Out of credit/quota | Provider-side, not the harness |
| `… transport: status 422` | Provider rejected the request shape | Usually a strict provider and an unknown field: try `prompt_cache = false` |
| Replies take minutes locally | A thinking model on CPU | Use a non-thinking model (`qwen2.5:3b`, `gemma3`) |
| Replies echo the harness's internals (`fact user.name = "Peter"`), repeat, or continue the transcript | The model is too small to compose from structured context, and its own output is fed back through the verbatim window | A bigger model for the turn, or put the turn on a cloud provider (§7) |
| `ran out of steps after 5 actions` | The emitter loops on tool calls instead of answering | Small-model behaviour; raise `[engine] max_iterations`, or use a stronger emitter |
| Everything answers "Sorry, I couldn't complete that." | Usually the provider, not the engine | `cargo run -p ns-app -- dump cli \| tail` shows the real `Rejected` reason |

### Seeing what the model was actually sent

The event log (`ns-app dump <session>`) records what the *engine* decided.
To see the prompts and completions themselves — which is what you need when a
model answers oddly — set `NS_TRACE`:

```bash
NS_TRACE=wire.jsonl cargo run -p ns-app
```

One JSON line per provider request: `role`, `model`, `attempt`, `ms`, the
full request body, and the response (including the provider's `usage` token
counts, so this doubles as a spend meter). Retries and transport failures get
their own lines. Headers are never written — the API key lives in one — but
the bodies hold the whole conversation, so treat the file like the session
log. Handy summary of a run:

```bash
jq -s 'map(.response.usage.total_tokens // 0) | add' wire.jsonl   # tokens
jq -r '"\(.role)\t\(.ms)ms\t\(.status)"' wire.jsonl            # per call
```

Since M7 the log answers the token question by itself, without a wire trace:
one `ModelCall` event per provider call carries the usage the provider
reported (or a flagged `chars/4` estimate when it reported none), the
requests the call really cost including retries, and the size of the tool
schemas sent with it.

```bash
ns-app budget <session>     # requests, tokens and trace size, per turn
```

`ns-app eval` runs the fixed task set against scripted doubles — no key, no
network, no requests spent — and exits non-zero if an ability regressed. It
is the "fix the model, vary the harness" check (2607.03691): run it before
each harness release, and the ledger row says which release moved which
ability.

```bash
ns-app eval [<ledger-path>]  # default eval-ledger.json
```

`ns-app providers`, `ns-app dump <session>`, `ns-app budget <session>`,
`ns-app eval` and `ns-app evolve --dry-run` all run **in a terminal**, not at
the `you>` prompt — typing them into the chat just sends them to the model as
text.

---

Related: `config.example.toml` (every option, commented),
`docs/superpowers/plans/2026-09-03-swappable-provider.md` (why the
resolution rules are what they are).
