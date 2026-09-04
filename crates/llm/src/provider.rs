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
}
