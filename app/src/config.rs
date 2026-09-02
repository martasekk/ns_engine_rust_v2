#[derive(Debug, serde::Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub engine: EngineSection,
    #[serde(default)]
    pub persona: PersonaSection,
    #[serde(default)]
    pub store: StoreSection,
    #[serde(default, rename = "http_component")]
    pub http_components: Vec<nscomponents_std::http_tool::HttpToolConfig>,
    /// [templates] table: id = "text with {placeholders}".
    #[serde(default)]
    pub templates: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub evolution: EvolutionSection,
}

#[derive(Debug, serde::Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the provider API key.
    /// The key itself never lives in config.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default)]
    pub emitter: ModelSection,
    #[serde(default)]
    pub replier: ReplierSection,
}

fn default_api_key_env() -> String {
    "OPENROUTER_API_KEY".into()
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: default_api_key_env(),
            emitter: ModelSection::default(),
            replier: ReplierSection::default(),
        }
    }
}

impl LlmConfig {
    /// Anthropic prompt-cache breakpoints are only forwarded by OpenRouter;
    /// other OpenAI-compatible providers may reject the unknown field.
    pub fn prompt_cache(&self) -> bool {
        self.base_url
            .as_deref()
            .is_none_or(|u| u.contains("openrouter.ai"))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelSection {
    pub model: String,
}

impl Default for ModelSection {
    fn default() -> Self {
        Self {
            model: "openrouter/free".into(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ReplierSection {
    pub model: String,
}

impl Default for ReplierSection {
    fn default() -> Self {
        Self {
            model: "openrouter/free".into(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct EngineSection {
    pub max_iterations: u32,
    pub max_emit_retries: u32,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            max_emit_retries: 3,
        }
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct PersonaSection {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct StoreSection {
    pub path: String,
}

impl Default for StoreSection {
    fn default() -> Self {
        Self {
            path: "ns.sqlite".into(),
        }
    }
}

/// [evolution] — the self-improvement pass (spec 2026-09-02).
#[derive(Debug, serde::Deserialize)]
pub struct EvolutionSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_learned_path")]
    pub learned_path: String,
    #[serde(default = "default_ledger_path")]
    pub ledger_path: String,
    /// 0 = batch (`ns-app evolve`) only.
    #[serde(default = "default_idle_after_secs")]
    pub idle_after_secs: u64,
    #[serde(default)]
    pub regression_budget: u32,
    #[serde(default = "default_probe_budget")]
    pub probe_budget_turns: u32,
    #[serde(default = "default_max_notes")]
    pub max_notes: usize,
    #[serde(default = "default_replay_cap")]
    pub regression_replay_cap: usize,
}

fn default_true() -> bool {
    true
}
fn default_learned_path() -> String {
    "learned.toml".into()
}
fn default_ledger_path() -> String {
    "evolution-ledger.json".into()
}
fn default_idle_after_secs() -> u64 {
    300
}
fn default_probe_budget() -> u32 {
    40
}
fn default_max_notes() -> usize {
    20
}
fn default_replay_cap() -> usize {
    200
}

impl Default for EvolutionSection {
    fn default() -> Self {
        Self {
            enabled: true,
            learned_path: default_learned_path(),
            ledger_path: default_ledger_path(),
            idle_after_secs: default_idle_after_secs(),
            regression_budget: 0,
            probe_budget_turns: default_probe_budget(),
            max_notes: default_max_notes(),
            regression_replay_cap: default_replay_cap(),
        }
    }
}

impl EvolutionSection {
    pub fn pass_config(&self, dry_run: bool) -> nsevolution::pass::PassConfig {
        nsevolution::pass::PassConfig {
            regression_budget: self.regression_budget,
            probe_budget_turns: self.probe_budget_turns,
            max_notes: self.max_notes,
            regression_replay_cap: self.regression_replay_cap,
            dry_run,
        }
    }
    /// Driver B interval; None when disabled or set to 0.
    pub fn idle_after(&self) -> Option<std::time::Duration> {
        (self.enabled && self.idle_after_secs > 0)
            .then(|| std::time::Duration::from_secs(self.idle_after_secs))
    }
}

impl AppConfig {
    pub fn parse(toml_text: &str) -> Result<AppConfig, String> {
        toml::from_str(toml_text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_config_parses() {
        let cfg = AppConfig::parse(
            r#"
            [llm]
            base_url = "http://localhost:9999"
            [llm.emitter]
            model = "anthropic/claude-haiku-4.5"
            [llm.replier]
            model = "anthropic/claude-sonnet-5"
            [engine]
            max_iterations = 4
            max_emit_retries = 2
            [persona]
            text = "You are Tomáš."
            [store]
            path = "/tmp/ns.sqlite"
            [[http_component]]
            name = "check_stock"
            description = "Check stock"
            url = "https://example.test/stock"
            side_effect = "Pure"
            args_schema = { type = "object", properties = { product = { type = "string" } }, required = ["product"] }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm.base_url.as_deref(), Some("http://localhost:9999"));
        assert_eq!(cfg.llm.emitter.model, "anthropic/claude-haiku-4.5");
        assert_eq!(cfg.engine.max_iterations, 4);
        assert_eq!(cfg.persona.text, "You are Tomáš.");
        assert_eq!(cfg.http_components.len(), 1);
        assert_eq!(cfg.http_components[0].name, "check_stock");
    }

    #[test]
    fn api_key_env_defaults_to_openrouter_and_is_configurable() {
        let d = AppConfig::parse("").unwrap();
        assert_eq!(d.llm.api_key_env, "OPENROUTER_API_KEY");
        let m = AppConfig::parse("[llm]\napi_key_env = \"MISTRAL_API_KEY\"").unwrap();
        assert_eq!(m.llm.api_key_env, "MISTRAL_API_KEY");
    }

    #[test]
    fn prompt_cache_is_enabled_only_for_openrouter() {
        let default = AppConfig::parse("").unwrap();
        assert!(default.llm.prompt_cache(), "default base_url is OpenRouter");
        let explicit = AppConfig::parse("[llm]\nbase_url = \"https://openrouter.ai/api\"").unwrap();
        assert!(explicit.llm.prompt_cache());
        let mistral = AppConfig::parse("[llm]\nbase_url = \"https://api.mistral.ai\"").unwrap();
        assert!(!mistral.llm.prompt_cache());
        let ollama = AppConfig::parse("[llm]\nbase_url = \"http://localhost:11434\"").unwrap();
        assert!(!ollama.llm.prompt_cache());
    }

    #[test]
    fn empty_config_gets_all_defaults() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.llm.emitter.model, "openrouter/free");
        assert_eq!(cfg.llm.replier.model, "openrouter/free");
        assert_eq!(cfg.engine.max_iterations, 5);
        assert_eq!(cfg.engine.max_emit_retries, 3);
        assert_eq!(cfg.store.path, "ns.sqlite");
        assert!(cfg.http_components.is_empty());
    }

    #[test]
    fn bad_toml_is_a_readable_error() {
        assert!(AppConfig::parse("[llm").is_err());
    }

    #[test]
    fn templates_section_parses_and_defaults_empty() {
        let cfg = AppConfig::parse("[templates]\ncant_help = \"Sorry.\"\n").unwrap();
        assert_eq!(
            cfg.templates.get("cant_help").map(String::as_str),
            Some("Sorry.")
        );
        assert!(AppConfig::parse("").unwrap().templates.is_empty());
    }

    #[test]
    fn evolution_section_defaults_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert!(cfg.evolution.enabled);
        assert_eq!(cfg.evolution.learned_path, "learned.toml");
        assert_eq!(
            cfg.evolution.idle_after(),
            Some(std::time::Duration::from_secs(300))
        );
        let cfg =
            AppConfig::parse("[evolution]\nenabled = false\nprobe_budget_turns = 7\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
        assert_eq!(cfg.evolution.pass_config(true).probe_budget_turns, 7);
        assert!(cfg.evolution.pass_config(true).dry_run);
        let cfg = AppConfig::parse("[evolution]\nidle_after_secs = 0\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
    }
}
