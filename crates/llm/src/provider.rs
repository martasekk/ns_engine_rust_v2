//! Named provider presets: one word swaps every role's backend.
//!
//! Every provider listed here speaks POST `{base_url}/v1/chat/completions`
//! with OpenAI-style function tools, the only protocol this client
//! implements. A preset carries what the harness must know beyond the URL:
//! which env var holds the key, how hard the free tier lets us push,
//! whether Anthropic cache breakpoints survive the hop, and whether the key
//! is a formality (local servers).
//!
//! Prior art: LiteLLM's provider registry (a named provider is a base_url
//! plus an `api_key_env`), and the `provider:model` string of LangChain's
//! `init_chat_model` and the Vercel AI SDK provider registry. The split
//! here is first-colon-only *and* the prefix must name a known provider —
//! otherwise `qwen2.5:3b` parses as provider `qwen2.5` (vercel/ai#2056).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub name: &'static str,
    pub base_url: &'static str,
    /// The key itself never lives in config, only the variable's name.
    pub api_key_env: &'static str,
    /// Used when the config names the provider but no model. `None` where
    /// the catalogue moves too fast for a default to stay honest.
    pub default_model: Option<&'static str>,
    /// Minimum spacing between requests, ms — free tiers rate-limit per
    /// second and one turn with a tool call fires several requests.
    pub min_interval_ms: u64,
    /// Anthropic prompt-cache breakpoints are only forwarded by OpenRouter;
    /// stricter providers reject the unknown field.
    pub prompt_cache: bool,
    /// Local server: any non-empty key works, so an unset key env is not a
    /// startup error.
    pub local: bool,
    /// Whether the endpoint honours `response_format: {"type": "json_schema"}`
    /// (M11 T0.5). Only the summarizer uses it, and only as a stronger
    /// spelling of what its prompt already asks for: `strip_fence` stays the
    /// parser, so a provider that advertises the field and then ignores it
    /// costs nothing.
    pub structured_output: bool,
    pub note: &'static str,
}

/// The presets. Adding one is a row here plus a line in config.example.toml.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        name: "openrouter",
        base_url: "https://openrouter.ai/api",
        api_key_env: "OPENROUTER_API_KEY",
        default_model: Some("openrouter/free"),
        min_interval_ms: 0,
        prompt_cache: true,
        local: false,
        structured_output: true,
        note: "many models behind one key; free tier 50 requests/day",
    },
    Provider {
        name: "mistral",
        base_url: "https://api.mistral.ai",
        api_key_env: "MISTRAL_API_KEY",
        default_model: Some("mistral-small-latest"),
        // Seen live: the free Experiment tier (~1 req/s) 429s on bursts.
        min_interval_ms: 1100,
        prompt_cache: false,
        local: false,
        structured_output: false,
        note: "free Experiment tier, ~1 req/s, does function calling",
    },
    Provider {
        name: "ollama",
        base_url: "http://localhost:11434",
        api_key_env: "OLLAMA_API_KEY",
        default_model: Some("qwen2.5:3b"),
        min_interval_ms: 0,
        prompt_cache: false,
        local: true,
        structured_output: false,
        note: "local; pick a non-thinking model, the OpenAI endpoint ignores `think`",
    },
    Provider {
        name: "lmstudio",
        base_url: "http://localhost:1234",
        api_key_env: "LMSTUDIO_API_KEY",
        default_model: None,
        min_interval_ms: 0,
        prompt_cache: false,
        local: true,
        structured_output: false,
        note: "local; model id is whatever LM Studio has loaded",
    },
    Provider {
        name: "llamacpp",
        base_url: "http://localhost:8080",
        api_key_env: "LLAMACPP_API_KEY",
        default_model: None,
        min_interval_ms: 0,
        prompt_cache: false,
        local: true,
        structured_output: false,
        note: "local; llama-server --jinja for tool calls",
    },
    Provider {
        name: "openai",
        base_url: "https://api.openai.com",
        api_key_env: "OPENAI_API_KEY",
        default_model: None,
        min_interval_ms: 0,
        prompt_cache: false,
        local: false,
        structured_output: true,
        note: "name the model explicitly; the catalogue moves",
    },
    Provider {
        name: "groq",
        base_url: "https://api.groq.com/openai",
        api_key_env: "GROQ_API_KEY",
        default_model: None,
        min_interval_ms: 0,
        prompt_cache: false,
        local: false,
        structured_output: false,
        note: "fast, but the free tier is ~6K tokens/min per model",
    },
];

/// Preset by name, case-insensitively.
pub fn find(name: &str) -> Option<&'static Provider> {
    PROVIDERS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
}

pub fn names() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.name).collect()
}

fn normalize(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// The preset a resolved base URL belongs to, so a config that spells the
/// URL out still gets the provider's throttle and prompt-cache behaviour.
pub fn for_base_url(url: &str) -> Option<&'static Provider> {
    let url = normalize(url);
    PROVIDERS.iter().find(|p| normalize(p.base_url) == url)
}

/// Split a `provider:model` spec. The prefix counts only when it names a
/// known provider, so bare model ids that contain a colon (`qwen2.5:3b`,
/// `gemma3:4b`) stay intact. The returned model may be empty, meaning
/// "the provider's default".
pub fn split_model(spec: &str) -> (Option<&'static Provider>, &str) {
    let spec = spec.trim();
    match spec.split_once(':') {
        Some((prefix, rest)) => match find(prefix) {
            Some(p) => (Some(p), rest.trim()),
            None => (None, spec),
        },
        None => (None, spec),
    }
}

// ── Request shaping (M11 T0.1–T0.3) ──────────────────────────────────────
//
// Every role has always sent one fixed request shape, chosen for a small
// model: `temperature: 0` on the emitter and summarizer, no sampling params
// on the replier, `max_tokens` a constant. Claude Sonnet 5 rejects a
// non-default `temperature`, `top_p`, `top_k` or a manual thinking budget
// with a 400 and replaces budgets with `reasoning: {effort}` — so the shape
// has to become per-role config rather than a literal in three `json!`
// blocks. Everything here is optional: with nothing set, each role's own
// default shape is the request it has always sent, byte for byte.

/// Whether a role sends sampling params at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    /// The role's own default — `temperature: 0` where it had one.
    Default,
    /// Send no sampling param at all. Not `temperature: 1`: the models that
    /// need this reject the *presence* of the key, not its value.
    None,
}

impl Sampling {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Sampling::Default),
            "none" => Ok(Sampling::None),
            other => Err(format!(
                "sampling must be \"default\" or \"none\", got {other:?}"
            )),
        }
    }
}

/// `low` | `medium` | `high`, the only spellings OpenRouter's `reasoning`
/// block takes. Err names the bad value rather than dropping the knob: a
/// typo would otherwise look exactly like a model that ignores effort.
pub fn parse_effort(s: &str) -> Result<String, String> {
    let e = s.trim().to_ascii_lowercase();
    match e.as_str() {
        "low" | "medium" | "high" => Ok(e),
        other => Err(format!(
            "reasoning must be \"low\", \"medium\" or \"high\", got {other:?}"
        )),
    }
}

/// The shape one role's request takes, after config and the safety net.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestShape {
    /// The value to send as `temperature`, or `None` to omit the key
    /// entirely. A `Value` rather than an `f32` because the request that
    /// exists today carries the integer `0`, and "byte for byte" includes
    /// not turning it into `0.0`.
    pub temperature: Option<serde_json::Value>,
    pub reasoning_effort: Option<String>,
    /// `Some(false)` sends `reasoning: {"enabled": false}` — for models
    /// (Qwen3 and kin) that think by default when the emitter needs a tool
    /// call, not a monologue.
    pub thinking_enabled: Option<bool>,
    pub max_tokens: u32,
}

impl RequestShape {
    /// Determinism pinned at `temperature: 0` — the emitter's and the
    /// summarizer's shape since M2.
    pub fn pinned(max_tokens: u32) -> Self {
        Self {
            temperature: Some(serde_json::json!(0)),
            reasoning_effort: None,
            thinking_enabled: None,
            max_tokens,
        }
    }

    /// No sampling param at all — the replier's shape since M6.
    pub fn unsampled(max_tokens: u32) -> Self {
        Self {
            temperature: None,
            reasoning_effort: None,
            thinking_enabled: None,
            max_tokens,
        }
    }

    /// Insert this shape's keys into a request object, and omit the ones it
    /// does not carry. The one place any of these four keys is written, so
    /// "omit `temperature` entirely" is a property of the type rather than
    /// of three `json!` blocks agreeing.
    pub fn apply(&self, request: &mut serde_json::Value) {
        let obj = request
            .as_object_mut()
            .expect("a chat-completions request is a JSON object");
        obj.insert("max_tokens".into(), serde_json::json!(self.max_tokens));
        match &self.temperature {
            Some(t) => obj.insert("temperature".into(), t.clone()),
            None => obj.remove("temperature"),
        };
        // One `reasoning` block, never two. An explicit effort wins over
        // `thinking = false`: asking for effort and switching thinking off
        // is a contradiction, and the effort is the narrower instruction.
        if let Some(effort) = &self.reasoning_effort {
            obj.insert("reasoning".into(), serde_json::json!({ "effort": effort }));
        } else if self.thinking_enabled == Some(false) {
            obj.insert("reasoning".into(), serde_json::json!({"enabled": false}));
        }
    }
}

/// The four optional `[llm.<role>]` shaping fields, parsed. All unset — the
/// default — means the role's own shape, unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleShaping {
    pub reasoning: Option<String>,
    pub max_tokens: Option<u32>,
    pub sampling: Option<Sampling>,
    pub thinking: Option<bool>,
}

/// Model ids that 400 on any sampling param. Prefix match, because the
/// catalogue appends dated and `:thinking` suffixes to the same model.
pub const SAMPLING_REJECTED_BY: &[&str] = &["anthropic/claude-sonnet-5"];

/// The safety net (T0.3): does this model reject sampling params outright?
pub fn rejects_sampling(model: &str) -> bool {
    let m = model.trim();
    SAMPLING_REJECTED_BY.iter().any(|p| m.starts_with(p))
}

impl RoleShaping {
    /// Fold the config onto the role's default shape.
    ///
    /// Returns the shape and, when the safety net fired, the one line to
    /// print at startup — a coercion nobody asked for has to be visible, or
    /// the next 400 is unexplainable.
    pub fn resolve(
        &self,
        role: &str,
        model: &str,
        base: RequestShape,
    ) -> (RequestShape, Option<String>) {
        let mut shape = base;
        let mut coercion = None;
        let sampling = match self.sampling {
            Some(s) => s,
            None if rejects_sampling(model) => {
                // Only worth a line when it actually removes something: the
                // replier already sends none, and saying so every start
                // would be noise.
                if shape.temperature.is_some() {
                    coercion = Some(format!(
                        "{role}: {model} rejects sampling params — sending none \
                         (set [llm.{role}] sampling = \"default\" to override)"
                    ));
                }
                Sampling::None
            }
            None => Sampling::Default,
        };
        if sampling == Sampling::None {
            shape.temperature = None;
        }
        if let Some(effort) = &self.reasoning {
            shape.reasoning_effort = Some(effort.clone());
        }
        if let Some(t) = self.thinking {
            shape.thinking_enabled = Some(t);
        }
        if let Some(m) = self.max_tokens {
            shape.max_tokens = m;
        }
        (shape, coercion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_is_case_insensitive_and_trims() {
        assert_eq!(find("ollama").unwrap().name, "ollama");
        assert_eq!(find(" Ollama ").unwrap().name, "ollama");
        assert!(find("nope").is_none());
    }

    #[test]
    fn model_ids_containing_a_colon_are_not_provider_prefixes() {
        // The vercel/ai#2056 case: only a known provider name counts.
        let (p, m) = split_model("qwen2.5:3b");
        assert!(p.is_none());
        assert_eq!(m, "qwen2.5:3b");
        let (p, m) = split_model("anthropic/claude-haiku-4.5");
        assert!(p.is_none());
        assert_eq!(m, "anthropic/claude-haiku-4.5");
    }

    #[test]
    fn known_prefix_selects_the_provider_and_keeps_the_rest_verbatim() {
        let (p, m) = split_model("ollama:qwen2.5:3b");
        assert_eq!(p.unwrap().name, "ollama");
        assert_eq!(m, "qwen2.5:3b");
        let (p, m) = split_model("mistral:mistral-small-latest");
        assert_eq!(p.unwrap().name, "mistral");
        assert_eq!(m, "mistral-small-latest");
    }

    #[test]
    fn bare_provider_name_means_its_default_model() {
        let (p, m) = split_model("ollama:");
        assert_eq!(p.unwrap().name, "ollama");
        assert_eq!(m, "");
        assert_eq!(p.unwrap().default_model, Some("qwen2.5:3b"));
    }

    #[test]
    fn base_urls_map_back_to_their_preset() {
        assert_eq!(
            for_base_url("https://api.mistral.ai").unwrap().name,
            "mistral"
        );
        assert_eq!(
            for_base_url("http://localhost:11434/").unwrap().name,
            "ollama"
        );
        assert!(for_base_url("https://example.test").is_none());
    }

    #[test]
    fn every_preset_is_uniquely_named_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for p in PROVIDERS {
            assert!(seen.insert(p.name), "duplicate preset {}", p.name);
            assert_eq!(p.name, p.name.to_lowercase());
            assert!(!p.base_url.ends_with('/'), "{} base_url", p.name);
            assert!(!p.api_key_env.is_empty());
        }
    }

    #[test]
    fn only_openrouter_forwards_prompt_cache_breakpoints() {
        for p in PROVIDERS {
            assert_eq!(p.prompt_cache, p.name == "openrouter", "{}", p.name);
        }
    }

    /// M11 T0.5. The two OpenAI-shaped cloud endpoints take `json_schema`;
    /// the local servers and the smaller clouds do not, and a preset that
    /// claims it wrongly means a 400 on every summary.
    #[test]
    fn structured_output_is_advertised_only_by_openrouter_and_openai() {
        for p in PROVIDERS {
            assert_eq!(
                p.structured_output,
                p.name == "openrouter" || p.name == "openai",
                "{}",
                p.name
            );
        }
    }

    /// M11 T0.1/T0.2. Nothing set is today's request: the role's own shape
    /// reaches the wire untouched.
    #[test]
    fn an_unset_shaping_leaves_the_roles_own_shape_alone() {
        let (shape, note) =
            RoleShaping::default().resolve("emitter", "google/gemini-3.8-flash", pinned_4096());
        assert_eq!(shape, pinned_4096());
        assert!(note.is_none());
        let mut req = serde_json::json!({"model": "google/gemini-3.8-flash"});
        shape.apply(&mut req);
        assert_eq!(req["temperature"], 0);
        assert_eq!(req["max_tokens"], 4096);
        assert!(req.get("reasoning").is_none());
    }

    fn pinned_4096() -> RequestShape {
        RequestShape::pinned(4096)
    }

    /// M11 T0.3, the exit criterion: the 400 is unreachable from a default
    /// config that names Sonnet in a role. Built the way `main.rs` builds
    /// it — the role's default shape folded through `resolve` with the
    /// resolved model id and the section's (empty) shaping — so the test
    /// fails if that path stops going through here.
    #[test]
    fn sonnet_five_never_receives_a_sampling_param() {
        for model in [
            "anthropic/claude-sonnet-5",
            "anthropic/claude-sonnet-5:thinking",
            "anthropic/claude-sonnet-5-20260514",
        ] {
            for (role, base) in [
                ("emitter", RequestShape::pinned(4096)),
                ("replier", RequestShape::unsampled(4096)),
                ("summarizer", RequestShape::pinned(400)),
            ] {
                let (shape, note) = RoleShaping::default().resolve(role, model, base.clone());
                let mut req = serde_json::json!({"model": model, "messages": []});
                shape.apply(&mut req);
                assert!(
                    req.get("temperature").is_none(),
                    "{role} {model} still carries a sampling param: {req}"
                );
                assert!(!req.to_string().contains("top_p"));
                // The coercion is announced exactly where it changed
                // something, and stays quiet where it did not.
                assert_eq!(
                    note.is_some(),
                    base.temperature.is_some(),
                    "{role}: {note:?}"
                );
                if let Some(line) = note {
                    assert!(line.contains(model) && line.contains(role), "{line}");
                }
            }
        }
        // A model that is not Sonnet keeps its temperature.
        let (shape, note) =
            RoleShaping::default().resolve("emitter", "anthropic/claude-haiku-4.5", pinned_4096());
        assert_eq!(shape.temperature, Some(serde_json::json!(0)));
        assert!(note.is_none());
    }

    /// An explicit `sampling = "default"` overrides the net — the operator
    /// gets to be wrong on purpose, e.g. when the id is a proxy alias.
    #[test]
    fn an_explicit_sampling_beats_the_safety_net() {
        let shaping = RoleShaping {
            sampling: Some(Sampling::Default),
            ..Default::default()
        };
        let (shape, note) = shaping.resolve("emitter", "anthropic/claude-sonnet-5", pinned_4096());
        assert_eq!(shape.temperature, Some(serde_json::json!(0)));
        assert!(note.is_none(), "nothing was coerced");
    }

    #[test]
    fn shaping_values_parse_case_insensitively_and_reject_typos() {
        assert_eq!(Sampling::parse(" None ").unwrap(), Sampling::None);
        assert_eq!(Sampling::parse("default").unwrap(), Sampling::Default);
        assert!(Sampling::parse("off").unwrap_err().contains("\"none\""));
        assert_eq!(parse_effort("HIGH").unwrap(), "high");
        assert!(parse_effort("maximum").unwrap_err().contains("\"medium\""));
    }

    /// `reasoning` and `thinking` share one block, and the effort wins.
    #[test]
    fn reasoning_and_thinking_share_one_block() {
        let mut req = serde_json::json!({});
        RequestShape {
            reasoning_effort: Some("low".into()),
            thinking_enabled: Some(false),
            ..RequestShape::pinned(2048)
        }
        .apply(&mut req);
        assert_eq!(req["reasoning"], serde_json::json!({"effort": "low"}));

        let mut req = serde_json::json!({});
        RequestShape {
            thinking_enabled: Some(false),
            ..RequestShape::pinned(2048)
        }
        .apply(&mut req);
        assert_eq!(req["reasoning"], serde_json::json!({"enabled": false}));
        assert_eq!(req["max_tokens"], 2048);
    }
}
