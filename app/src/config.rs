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
    #[serde(default)]
    pub memory: MemorySection,
}

/// [memory] — working memory sizes (M6 spec §4, §10).
#[derive(Debug, serde::Deserialize)]
pub struct MemorySection {
    /// Completed turns rendered verbatim into both model contexts.
    #[serde(default = "default_window_turns")]
    pub window_turns: usize,
    /// Whole rendered turn record, characters.
    #[serde(default = "default_record_max_chars")]
    pub record_max_chars: usize,
    /// Each action line and the reply inside a record, characters.
    #[serde(default = "default_line_max_chars")]
    pub line_max_chars: usize,
    /// Standing facts shown to both models per turn.
    #[serde(default = "default_facts_in_context")]
    pub facts_in_context: usize,
    /// Flag and regenerate once a reply that states numbers, quotes or
    /// names absent from everything the model was shown (M6 §4.5).
    #[serde(default = "default_true")]
    pub reply_grounding_check: bool,
    /// "flag" stores an ungrounded remembered value at half confidence;
    /// "never" refuses it (M6 §6.3).
    #[serde(default = "default_remember_residual")]
    pub remember_residual: String,
    /// Keys with these prefixes are always shown, newest first (M6 §6.5).
    #[serde(default = "default_pinned_prefixes")]
    pub pinned_prefixes: Vec<String>,
    #[serde(default = "default_pinned_max")]
    pub pinned_max: usize,
    /// Facts lexically relevant to the current message (M6 §6.5).
    #[serde(default = "default_relevant_max")]
    pub relevant_max: usize,
    /// Days without use before a fact goes cold (M6 §6.2).
    #[serde(default = "default_fact_stale_days")]
    pub fact_stale_days: u64,
    /// Rolling summary cadence (M6 §5.1); 0 disables the layer.
    #[serde(default = "default_summary_every_turns")]
    pub summary_every_turns: usize,
    #[serde(default = "default_summary_rebuild_every")]
    pub summary_rebuild_every: usize,
    #[serde(default = "default_summary_max_chars")]
    pub summary_max_chars: usize,
    #[serde(default = "default_summary_input_max_chars")]
    pub summary_input_max_chars: usize,
    /// Hits per source the `recall` action returns (M6 §7).
    #[serde(default = "default_recall_top_k")]
    pub recall_top_k: usize,
}

fn default_recall_top_k() -> usize {
    5
}

fn default_summary_every_turns() -> usize {
    4
}
fn default_summary_rebuild_every() -> usize {
    3
}
fn default_summary_max_chars() -> usize {
    800
}
fn default_summary_input_max_chars() -> usize {
    6000
}

fn default_remember_residual() -> String {
    "flag".into()
}
fn default_pinned_prefixes() -> Vec<String> {
    vec!["user.".into()]
}
fn default_pinned_max() -> usize {
    5
}
fn default_relevant_max() -> usize {
    5
}
fn default_fact_stale_days() -> u64 {
    90
}

fn default_window_turns() -> usize {
    6
}
fn default_record_max_chars() -> usize {
    300
}
fn default_line_max_chars() -> usize {
    120
}
fn default_facts_in_context() -> usize {
    10
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            window_turns: default_window_turns(),
            record_max_chars: default_record_max_chars(),
            line_max_chars: default_line_max_chars(),
            facts_in_context: default_facts_in_context(),
            reply_grounding_check: true,
            remember_residual: default_remember_residual(),
            pinned_prefixes: default_pinned_prefixes(),
            pinned_max: default_pinned_max(),
            relevant_max: default_relevant_max(),
            fact_stale_days: default_fact_stale_days(),
            summary_every_turns: default_summary_every_turns(),
            summary_rebuild_every: default_summary_rebuild_every(),
            summary_max_chars: default_summary_max_chars(),
            summary_input_max_chars: default_summary_input_max_chars(),
            recall_top_k: default_recall_top_k(),
        }
    }
}

impl MemorySection {
    pub fn caps(&self) -> nscore::Caps {
        nscore::Caps {
            record_max_chars: self.record_max_chars,
            line_max_chars: self.line_max_chars,
        }
    }

    /// Err names the bad value; config errors are fatal at startup.
    pub fn remember_residual(&self) -> Result<nsengine::turn::RememberResidual, String> {
        match self.remember_residual.as_str() {
            "flag" => Ok(nsengine::turn::RememberResidual::Flag),
            "never" => Ok(nsengine::turn::RememberResidual::Never),
            other => Err(format!(
                "[memory] remember_residual must be \"flag\" or \"never\", got {other:?}"
            )),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the provider API key.
    /// The key itself never lives in config.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Minimum spacing between requests to the provider, shared by every
    /// role. Unset: 1100 ms for api.mistral.ai (free tier is ~1 req/s), 0
    /// elsewhere.
    #[serde(default)]
    pub min_interval_ms: Option<u64>,
    #[serde(default)]
    pub emitter: ModelSection,
    #[serde(default)]
    pub replier: ReplierSection,
    /// The rolling-summary model (M6 §5.1). Defaults to the emitter's model
    /// and provider; each field can point elsewhere so the role can be
    /// swapped without touching the others.
    #[serde(default)]
    pub summarizer: SummarizerSection,
}

fn default_api_key_env() -> String {
    "OPENROUTER_API_KEY".into()
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key_env: default_api_key_env(),
            min_interval_ms: None,
            emitter: ModelSection::default(),
            replier: ReplierSection::default(),
            summarizer: SummarizerSection::default(),
        }
    }
}

impl LlmConfig {
    /// Explicit value, else a provider default (seen live: Mistral's free
    /// tier 429s on back-to-back requests).
    pub fn min_interval_ms(&self) -> u64 {
        self.min_interval_ms.unwrap_or_else(|| {
            if self
                .base_url
                .as_deref()
                .is_some_and(|u| u.contains("mistral.ai"))
            {
                1100
            } else {
                0
            }
        })
    }
}

/// Per-role provider override: `[llm.summarizer] model = "…"`, optionally
/// with its own `base_url` and `api_key_env`. Unset fields fall back to the
/// emitter model and the global provider.
#[derive(Debug, serde::Deserialize, Default)]
pub struct SummarizerSection {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

impl LlmConfig {
    /// (model, base_url, api_key_env) the summarizer actually uses.
    pub fn summarizer_role(&self) -> (String, Option<String>, String) {
        (
            self.summarizer
                .model
                .clone()
                .unwrap_or_else(|| self.emitter.model.clone()),
            self.summarizer
                .base_url
                .clone()
                .or_else(|| self.base_url.clone()),
            self.summarizer
                .api_key_env
                .clone()
                .unwrap_or_else(|| self.api_key_env.clone()),
        )
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
    pub fn pass_config(
        &self,
        dry_run: bool,
        fact_stale_days: u64,
    ) -> nsevolution::pass::PassConfig {
        nsevolution::pass::PassConfig {
            regression_budget: self.regression_budget,
            probe_budget_turns: self.probe_budget_turns,
            max_notes: self.max_notes,
            regression_replay_cap: self.regression_replay_cap,
            dry_run,
            fact_stale_days,
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
    fn min_interval_defaults_per_provider_and_is_configurable() {
        assert_eq!(AppConfig::parse("").unwrap().llm.min_interval_ms(), 0);
        let mistral = AppConfig::parse("[llm]\nbase_url = \"https://api.mistral.ai\"").unwrap();
        assert_eq!(mistral.llm.min_interval_ms(), 1100);
        let explicit =
            AppConfig::parse("[llm]\nbase_url = \"https://api.mistral.ai\"\nmin_interval_ms = 0")
                .unwrap();
        assert_eq!(explicit.llm.min_interval_ms(), 0);
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
    fn summarizer_role_falls_back_to_emitter_and_global_provider() {
        let cfg = AppConfig::parse(
            "[llm]\nbase_url = \"https://api.mistral.ai\"\napi_key_env = \"MISTRAL_API_KEY\"\n[llm.emitter]\nmodel = \"mistral-small-latest\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.llm.summarizer_role(),
            (
                "mistral-small-latest".to_string(),
                Some("https://api.mistral.ai".to_string()),
                "MISTRAL_API_KEY".to_string()
            )
        );
        let cfg = AppConfig::parse(
            "[llm.summarizer]\nmodel = \"qwen2.5:3b\"\nbase_url = \"http://localhost:11434\"\napi_key_env = \"OLLAMA_API_KEY\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.llm.summarizer_role(),
            (
                "qwen2.5:3b".to_string(),
                Some("http://localhost:11434".to_string()),
                "OLLAMA_API_KEY".to_string()
            )
        );
        assert_eq!(cfg.memory.summary_every_turns, 4);
        assert_eq!(cfg.memory.summary_rebuild_every, 3);
        let off = AppConfig::parse("[memory]\nsummary_every_turns = 0\n").unwrap();
        assert_eq!(off.memory.summary_every_turns, 0);
    }

    #[test]
    fn memory_section_defaults_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.memory.window_turns, 6);
        assert_eq!(cfg.memory.facts_in_context, 10);
        assert_eq!(cfg.memory.pinned_prefixes, vec!["user.".to_string()]);
        assert_eq!(cfg.memory.fact_stale_days, 90);
        assert_eq!(cfg.memory.caps(), nscore::Caps::default());
        let cfg = AppConfig::parse("[memory]\nwindow_turns = 2\nrecord_max_chars = 50\n").unwrap();
        assert_eq!(cfg.memory.window_turns, 2);
        assert_eq!(cfg.memory.caps().record_max_chars, 50);
        assert_eq!(cfg.memory.caps().line_max_chars, 120);
        assert!(cfg.memory.reply_grounding_check);
        let cfg = AppConfig::parse("[memory]\nreply_grounding_check = false\n").unwrap();
        assert!(!cfg.memory.reply_grounding_check);
        assert_eq!(
            cfg.memory.remember_residual().unwrap(),
            nsengine::turn::RememberResidual::Flag
        );
        let cfg = AppConfig::parse("[memory]\nremember_residual = \"never\"\n").unwrap();
        assert_eq!(
            cfg.memory.remember_residual().unwrap(),
            nsengine::turn::RememberResidual::Never
        );
        let cfg = AppConfig::parse("[memory]\nremember_residual = \"maybe\"\n").unwrap();
        assert!(cfg.memory.remember_residual().is_err());
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
        assert_eq!(cfg.evolution.pass_config(true, 90).probe_budget_turns, 7);
        assert!(cfg.evolution.pass_config(true, 90).dry_run);
        let cfg = AppConfig::parse("[evolution]\nidle_after_secs = 0\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
    }
}
