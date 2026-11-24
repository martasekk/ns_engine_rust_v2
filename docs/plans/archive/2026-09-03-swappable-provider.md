# Swappable Agent Backend — provider presets

**Goal:** swap the agent's model/provider in one word, in config or for a
single run, per role — and run the whole harness on the local model.

**Shipped:** `crates/llm/src/provider.rs` (preset registry),
`app/src/config.rs` (`Role`, `RoleTarget`, per-role resolution),
`app/src/main.rs` (per-endpoint throttles, `ns-app providers`, banner),
`crates/llm/src/emitter.rs` (empty-response fallback, see §4).

## 1. Prior-art pass

| Source | Decision |
| --- | --- |
| **LiteLLM provider registry** — a named provider is `base_url` + `api_key_env` (+ per-provider param mappings, `api_base_env` overrides) | **Adopt the shape.** A preset here is `base_url`, `api_key_env`, `default_model`, `min_interval_ms`, `prompt_cache`, `local`. |
| **LangChain `init_chat_model("openai:o1")`** and **Vercel AI SDK `createProviderRegistry`** (`providerId:modelId`) | **Adopt the string convention** for per-role swaps. **Amended:** split on the *first* colon only *and* require the prefix to name a known provider — otherwise `qwen2.5:3b` parses as provider `qwen2.5` (the bug in vercel/ai#2056). Covered by a test. |
| **rig-core, rust-genai, llm-connector** (multi-provider Rust clients) | **Rejected for now.** The harness already owns a 230-line tested `OpenRouterClient` over an `HttpTransport` seam, and the engine depends on its retry/throttle/error semantics (repeat gate, provenance, replay). Swapping it for a framework would hide exactly the behaviour the engine reasons about, for providers we do not use. **Revisit** `llm-connector` (smallest, protocol/provider split) if a *native* non-OpenAI protocol is ever needed. |
| **Model-gateway / lock-in writing** (TrueFoundry, Augment, nhimg; adjacent: arXiv 2506.23978) — the lock-in that bites is behavioural (prompts and evals tuned to one model), not the API | **Design consequence.** The swap is deliberately *visible*: the startup banner prints each role's model and endpoint, `ns-app providers` prints what the config resolves to, and provider quirks live in one table rather than scattered `url.contains(...)` checks. The existing replay/evolution corpus is what makes a swap measurable. |

## 2. Model

Three roles (emitter, replier, summarizer) each resolve to a `RoleTarget`
independently. Precedence, most specific first:

1. the role's own `model` / `base_url` / `api_key_env`
2. the provider named in the role's `provider:model` prefix
3. the `[llm]` literals (`base_url`, `api_key_env`, `min_interval_ms`, `prompt_cache`)
4. the `[llm] provider` preset
5. the built-in default (OpenRouter)

`min_interval_ms`, `prompt_cache` and `local` follow the **resolved base
URL**, so a config that spells the endpoint out gets the same treatment as
one that names the preset — this is what keeps the pre-preset configs
behaving exactly as before. A loopback URL no preset knows still counts as
local. Env overrides (`NS_PROVIDER`, `NS_MODEL`) are applied after parsing,
so `AppConfig::parse` stays pure and the rules stay testable.

Two failure modes are now fatal at startup instead of silent: an unknown
provider name (listing the known ones) and a model that no preset can
supply. Both spell out the fix.

## 3. Throttling

Was: one process-wide throttle at `[llm] min_interval_ms`. Now: one
throttle **per endpoint**, so roles sharing a provider still present one
paced stream (the reason it exists — Mistral 429s on bursts) while a role
on a different provider is paced separately.

## 4. Emitter: empty response after work is done

Found while smoke-testing the swap. With `qwen2.5:3b`: the model calls
`get_time`, then re-proposes the identical call; the repeat gate rejects it
and drops the action from the legal set; the model calls it anyway; Ollama's
OpenAI shim **drops a tool call whose name is not in `tools`**, returning a
message with neither `tool_calls` nor `content` (`finish_reason: "stop"`,
16 completion tokens). Three retries later the turn settled on the canned
failure — with the correct answer sitting in the trace.

The emitter already treats a plain-text answer as `respond_directly`. It now
treats an *empty* answer the same way **when this turn has already produced
a trace** — the results are the answer, so the replier narrates them. With
an empty trace there is nothing to narrate, so it stays `Malformed` and the
engine's retries are preserved. No guard is skipped: the proposal goes
through legality, validation and the guards like any other.

## 5. Verified live (2026-09-03, CPU-only VM)

- `ns-app providers` — 7 presets, key state, resolved roles.
- `provider = "ollama"`, no key exported: `what time is it?` → `get_time`
  runs → `2026-09-03 19:56:45 UTC (Thursday)`; conversational turn also OK.
- `NS_PROVIDER=mistral ns-app` — same binary, same config file, cloud
  models, replies normally.
- `cargo test --workspace` green (+16 new tests), `cargo clippy
  --workspace --all-targets -D warnings` clean.

## Sources

- LiteLLM — [Setting API Keys, Base, Version](https://docs.litellm.ai/docs/set_keys), [Adding OpenAI-Compatible Providers](https://docs.litellm.ai/docs/contributing/adding_openai_compatible_providers), [Integrate as a Model Provider](https://docs.litellm.ai/docs/provider_registration/)
- LangChain — [`init_chat_model`](https://reference.langchain.com/python/langchain/chat_models/base/init_chat_model), [Providers and models](https://docs.langchain.com/oss/python/concepts/providers-and-models)
- Vercel AI SDK — [Provider & Model Management](https://ai-sdk.dev/docs/ai-sdk-core/provider-management), [`createProviderRegistry`](https://ai-sdk.dev/docs/reference/ai-sdk-core/provider-registry), [colon in model ids (vercel/ai#2056)](https://github.com/vercel/ai/issues/2056)
- Rust clients considered — [rig-core](https://docs.rs/rig-core/latest/rig_core/), [rust-genai](https://github.com/jeremychone/rust-genai), [llm-connector](https://crates.io/crates/llm-connector/0.1.0)
- Lock-in / gateways — [TrueFoundry: vendor lock-in prevention](https://www.truefoundry.com/blog/vendor-lock-in-prevention), [Augment: model-agnostic AI](https://www.augmentcode.com/guides/model-agnostic-ai-why-provider-lock-in-is-so-expensive), [nhimg: portable code, data and evals](https://nhimg.org/articles/avoiding-llm-provider-lock-in-requires-portable-code-data-and-evals/), [arXiv:2506.23978](https://arxiv.org/html/2506.23978v2)
