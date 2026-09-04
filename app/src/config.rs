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
    /// Reporting threshold for `ReplyEchoed`: a reply at or over this
    /// fraction of one verbatim run out of its own prompt is logged and then
    /// sent as-is. Measured, never acted on (plan §8). Rides the
    /// `reply_grounding_check` gate; above 1.0 nothing is logged.
    #[serde(default = "default_max_echo_ratio")]
    pub max_echo_ratio: f32,
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
            max_echo_ratio: default_max_echo_ratio(),
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

/// The three model roles. Each resolves independently, so one can sit on a
/// local model while another stays in the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Emitter,
    Replier,
    Summarizer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Emitter => "emitter",
            Role::Replier => "replier",
            Role::Summarizer => "summarizer",
        }
    }
}

/// The provider the client falls back to when nothing names one.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";
pub const DEFAULT_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Everything one role needs to reach a provider, after the preset, the
/// `[llm]` defaults, the per-role overrides and the env overrides are
/// folded together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTarget {
    pub role: Role,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: String,
    pub min_interval_ms: u64,
    pub prompt_cache: bool,
    /// Local server: any non-empty key works, so an unset key env is not a
    /// startup error.
    pub local: bool,
}

impl RoleTarget {
    /// The URL the client will actually POST to.
    pub fn base_url_or_default(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    /// The key from the environment; a local server gets a placeholder.
    pub fn key(&self) -> Option<String> {
        match std::env::var(&self.api_key_env)
            .ok()
            .filter(|k| !k.is_empty())
        {
            Some(k) => Some(k),
            None if self.local => Some("local".into()),
            None => None,
        }
    }

    /// One line for the startup banner: which model, where.
    pub fn describe(&self) -> String {
        format!(
            "{}: {} @ {}",
            self.role.as_str(),
            self.model,
            self.base_url_or_default()
        )
    }
}

/// One model role in config. Every field is optional: unset falls back to
/// `[llm]`, and for the summarizer to the emitter.
#[derive(Debug, serde::Deserialize, Default, Clone)]
pub struct RoleSection {
    /// `"model-id"`, or `"provider:model-id"` to point this role at a
    /// different provider than the rest.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct LlmConfig {
    /// Named preset — `ns-app providers` lists them. Supplies base_url,
    /// api_key_env, a default model, the provider's rate limit and whether
    /// prompt-cache breakpoints survive the hop. One word swaps the agent.
    #[serde(default)]
    pub provider: Option<String>,
    /// Spelled-out endpoint; overrides the preset's base_url.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the provider API key.
    /// The key itself never lives in config.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Minimum spacing between requests to one provider, shared by every
    /// role. Unset: the provider's own value (0 for most).
    #[serde(default)]
    pub min_interval_ms: Option<u64>,
    /// Send Anthropic cache breakpoints. Unset: the provider's own value.
    #[serde(default)]
    pub prompt_cache: Option<bool>,
    #[serde(default)]
    pub emitter: RoleSection,
    #[serde(default)]
    pub replier: RoleSection,
    /// The rolling-summary model (M6 §5.1). Falls back to the emitter's
    /// model and provider; any field can point it elsewhere.
    #[serde(default)]
    pub summarizer: RoleSection,
}

fn unknown_provider(name: &str, where_: &str) -> String {
    format!(
        "{where_} provider {name:?} is unknown — known providers: {}",
        nsllm::provider::names().join(", ")
    )
}

fn is_loopback(url: &str) -> bool {
    ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"]
        .iter()
        .any(|h| url.contains(h))
}

impl LlmConfig {
    fn section(&self, role: Role) -> &RoleSection {
        match role {
            Role::Emitter => &self.emitter,
            Role::Replier => &self.replier,
            Role::Summarizer => &self.summarizer,
        }
    }

    /// Env overrides, applied after parsing so `parse` stays pure:
    /// `NS_PROVIDER` repoints every role, `NS_MODEL` sets every role's model.
    ///
    /// Switching backends drops the model ids and endpoints belonging to the
    /// old one: `NS_PROVIDER=ollama` must not go on asking for
    /// "google/gemini-3.8-flash". Naming the provider the config already
    /// resolves to changes nothing, and an explicit `NS_MODEL` always wins.
    pub fn apply_overrides(&mut self, provider: Option<String>, model: Option<String>) {
        if let Some(name) = provider {
            let target = nsllm::provider::find(&name);
            let current = self
                .role(Role::Emitter)
                .ok()
                .and_then(|t| nsllm::provider::for_base_url(t.base_url_or_default()));
            // Only a preset with a default model can replace what it clears.
            let switching = match (target, current) {
                (Some(t), Some(c)) => t.name != c.name,
                (Some(_), None) => true,
                (None, _) => false,
            } && target.is_some_and(|t| t.default_model.is_some());
            self.provider = Some(name);
            if switching {
                self.base_url = None;
                self.api_key_env = None;
                for s in [&mut self.emitter, &mut self.replier, &mut self.summarizer] {
                    s.model = None;
                    s.base_url = None;
                    s.api_key_env = None;
                }
            }
        }
        if let Some(m) = model {
            for s in [&mut self.emitter, &mut self.replier, &mut self.summarizer] {
                s.model = Some(m.clone());
            }
        }
    }

    /// The `[llm] provider` preset, if one is named. Err on an unknown name:
    /// silently falling back to OpenRouter would hide a typo behind a bill.
    pub fn preset(&self) -> Result<Option<&'static nsllm::provider::Provider>, String> {
        match self.provider.as_deref() {
            None => Ok(None),
            Some(name) => nsllm::provider::find(name)
                .map(Some)
                .ok_or_else(|| unknown_provider(name, "[llm]")),
        }
    }

    /// Resolve one role. Precedence, most specific first: the role's own
    /// fields, the provider named in its `provider:model` prefix, the
    /// `[llm]` literals, the `[llm] provider` preset.
    pub fn role(&self, role: Role) -> Result<RoleTarget, String> {
        let global = self.preset()?;
        let section = self.section(role);
        // The summarizer inherits the emitter's spec — prefix included.
        let spec = section.model.clone().or_else(|| {
            (role == Role::Summarizer)
                .then(|| self.emitter.model.clone())
                .flatten()
        });
        let (prefix, model) = match spec.as_deref() {
            Some(s) => {
                let (p, m) = nsllm::provider::split_model(s);
                (p, m.to_string())
            }
            None => (None, String::new()),
        };

        let base_url = section
            .base_url
            .clone()
            .or_else(|| prefix.map(|p| p.base_url.to_string()))
            .or_else(|| self.base_url.clone())
            .or_else(|| global.map(|p| p.base_url.to_string()));
        let api_key_env = section
            .api_key_env
            .clone()
            .or_else(|| prefix.map(|p| p.api_key_env.to_string()))
            .or_else(|| self.api_key_env.clone())
            .or_else(|| global.map(|p| p.api_key_env.to_string()))
            .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());

        // Rate limit, prompt cache and key-optionality follow the URL that
        // is actually used, so a config that spells the endpoint out gets
        // the same treatment as one that names the preset.
        let url = base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        let effective = nsllm::provider::for_base_url(url);
        let min_interval_ms = self
            .min_interval_ms
            .or(effective.map(|p| p.min_interval_ms))
            .unwrap_or(0);
        let prompt_cache = self
            .prompt_cache
            .or(effective.map(|p| p.prompt_cache))
            .unwrap_or(false);
        let local = effective
            .map(|p| p.local)
            .unwrap_or_else(|| is_loopback(url));

        let model = if model.is_empty() {
            let named = prefix.or(global).or(effective);
            named
                .and_then(|p| p.default_model)
                .map(str::to_string)
                .ok_or_else(|| match named {
                    Some(p) => format!(
                        "[llm.{}] model is not set and provider {:?} has no default — \
                         set model = \"…\"",
                        role.as_str(),
                        p.name
                    ),
                    None => format!(
                        "[llm.{}] model is not set and base_url {url:?} matches no preset — \
                         set model = \"…\", or name a preset: [llm] provider = \"…\"",
                        role.as_str()
                    ),
                })?
        } else {
            model
        };

        Ok(RoleTarget {
            role,
            model,
            base_url,
            api_key_env,
            min_interval_ms,
            prompt_cache,
            local,
        })
    }

    /// Emitter, replier, summarizer — the order the banner prints them in.
    pub fn roles(&self) -> Result<Vec<RoleTarget>, String> {
        [Role::Emitter, Role::Replier, Role::Summarizer]
            .into_iter()
            .map(|r| self.role(r))
            .collect()
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

/// 0.6: a reply more than half of which is one lifted run is a copy, not
/// an answer. Below that, shared phrasing is ordinary language.
fn default_max_echo_ratio() -> f32 {
    0.6
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
        let emitter = cfg.llm.role(Role::Emitter).unwrap();
        assert_eq!(emitter.model, "anthropic/claude-haiku-4.5");
        assert_eq!(emitter.base_url.as_deref(), Some("http://localhost:9999"));
        assert_eq!(cfg.engine.max_iterations, 4);
        assert_eq!(cfg.persona.text, "You are Tomáš.");
        assert_eq!(cfg.http_components.len(), 1);
        assert_eq!(cfg.http_components[0].name, "check_stock");
    }

    #[test]
    fn api_key_env_defaults_to_openrouter_and_is_configurable() {
        let d = AppConfig::parse("").unwrap();
        assert_eq!(
            d.llm.role(Role::Emitter).unwrap().api_key_env,
            "OPENROUTER_API_KEY"
        );
        let m = AppConfig::parse("[llm]\napi_key_env = \"MISTRAL_API_KEY\"").unwrap();
        assert_eq!(
            m.llm.role(Role::Emitter).unwrap().api_key_env,
            "MISTRAL_API_KEY"
        );
    }

    /// A config that spells the endpoint out instead of naming the preset
    /// still gets that provider's rate limit and prompt-cache behaviour.
    #[test]
    fn min_interval_and_prompt_cache_follow_the_resolved_base_url() {
        let emitter = |toml: &str| {
            AppConfig::parse(toml)
                .unwrap()
                .llm
                .role(Role::Emitter)
                .unwrap()
        };
        let d = emitter("");
        assert_eq!(d.min_interval_ms, 0);
        assert!(d.prompt_cache, "default base_url is OpenRouter");
        let mistral = emitter("[llm]\nbase_url = \"https://api.mistral.ai\"");
        assert_eq!(mistral.min_interval_ms, 1100);
        assert!(!mistral.prompt_cache);
        let explicit = emitter("[llm]\nbase_url = \"https://api.mistral.ai\"\nmin_interval_ms = 0");
        assert_eq!(explicit.min_interval_ms, 0);
        let ollama = emitter("[llm]\nbase_url = \"http://localhost:11434\"");
        assert!(!ollama.prompt_cache);
        assert!(ollama.local);
        // An endpoint no preset knows: loopback still counts as local, and
        // the model must be named because no preset can supply one.
        let custom =
            emitter("[llm]\nbase_url = \"http://127.0.0.1:9999\"\n[llm.emitter]\nmodel = \"x\"");
        let err = AppConfig::parse("[llm]\nbase_url = \"http://127.0.0.1:9999\"")
            .unwrap()
            .llm
            .role(Role::Emitter)
            .unwrap_err();
        assert!(err.contains("matches no preset"), "{err}");
        assert!(custom.local);
        assert!(!custom.prompt_cache);
        assert_eq!(custom.min_interval_ms, 0);
    }

    #[test]
    fn a_named_preset_supplies_url_key_env_and_default_model() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"ollama\"").unwrap();
        for t in cfg.llm.roles().unwrap() {
            assert_eq!(t.model, "qwen2.5:3b", "{:?}", t.role);
            assert_eq!(t.base_url.as_deref(), Some("http://localhost:11434"));
            assert_eq!(t.api_key_env, "OLLAMA_API_KEY");
            assert!(t.local);
            assert!(!t.prompt_cache);
        }
    }

    #[test]
    fn preset_fields_can_be_overridden_one_at_a_time() {
        let cfg = AppConfig::parse(
            "[llm]\nprovider = \"ollama\"\nbase_url = \"http://gpu-box:11434\"\n[llm.emitter]\nmodel = \"gemma3:4b\"\n",
        )
        .unwrap();
        let e = cfg.llm.role(Role::Emitter).unwrap();
        assert_eq!(
            e.model, "gemma3:4b",
            "a colon in a model id is not a prefix"
        );
        assert_eq!(e.base_url.as_deref(), Some("http://gpu-box:11434"));
        assert_eq!(
            e.api_key_env, "OLLAMA_API_KEY",
            "preset still supplies the key env"
        );
        // The replier keeps the preset's default model.
        assert_eq!(cfg.llm.role(Role::Replier).unwrap().model, "qwen2.5:3b");
    }

    #[test]
    fn a_role_prefix_points_one_role_at_another_provider() {
        let cfg = AppConfig::parse(
            "[llm]\nprovider = \"ollama\"\n[llm.replier]\nmodel = \"mistral:mistral-small-latest\"\n",
        )
        .unwrap();
        let e = cfg.llm.role(Role::Emitter).unwrap();
        assert_eq!(e.base_url.as_deref(), Some("http://localhost:11434"));
        let r = cfg.llm.role(Role::Replier).unwrap();
        assert_eq!(r.model, "mistral-small-latest");
        assert_eq!(r.base_url.as_deref(), Some("https://api.mistral.ai"));
        assert_eq!(r.api_key_env, "MISTRAL_API_KEY");
        assert_eq!(r.min_interval_ms, 1100);
        assert!(!r.local);
        // The summarizer inherits the emitter, not the replier.
        assert_eq!(
            cfg.llm.role(Role::Summarizer).unwrap().base_url.as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn a_bare_provider_name_as_the_model_means_its_default() {
        let cfg = AppConfig::parse("[llm.emitter]\nmodel = \"ollama:\"").unwrap();
        let e = cfg.llm.role(Role::Emitter).unwrap();
        assert_eq!(e.model, "qwen2.5:3b");
        assert_eq!(e.base_url.as_deref(), Some("http://localhost:11434"));
    }

    #[test]
    fn unknown_provider_is_a_readable_error_not_a_silent_fallback() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"ollamma\"").unwrap();
        let err = cfg.llm.role(Role::Emitter).unwrap_err();
        assert!(err.contains("ollamma"), "{err}");
        assert!(err.contains("ollama"), "lists the known names: {err}");
    }

    #[test]
    fn a_provider_without_a_default_model_demands_one() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"openai\"").unwrap();
        let err = cfg.llm.role(Role::Emitter).unwrap_err();
        assert!(err.contains("[llm.emitter]"), "{err}");
        let ok = AppConfig::parse("[llm]\nprovider = \"openai\"\n[llm.emitter]\nmodel = \"some-model\"\n[llm.replier]\nmodel = \"some-model\"\n").unwrap();
        assert_eq!(ok.llm.role(Role::Emitter).unwrap().model, "some-model");
        assert_eq!(
            ok.llm.role(Role::Summarizer).unwrap().model,
            "some-model",
            "summarizer inherits the emitter"
        );
    }

    #[test]
    fn env_overrides_repoint_every_role() {
        let mut cfg = AppConfig::parse(
            "[llm]\nprovider = \"mistral\"\n[llm.emitter]\nmodel = \"mistral-small-latest\"\n",
        )
        .unwrap();
        cfg.llm
            .apply_overrides(Some("ollama".into()), Some("qwen2.5:3b".into()));
        for t in cfg.llm.roles().unwrap() {
            assert_eq!(t.model, "qwen2.5:3b");
            assert_eq!(t.base_url.as_deref(), Some("http://localhost:11434"));
            assert_eq!(t.api_key_env, "OLLAMA_API_KEY");
        }
        // Switching backends drops the old backend's model ids: asking
        // Ollama for "google/gemini-3.8-flash" is a 404, not a swap.
        let mut cloud = AppConfig::parse(
            "[llm]\nprovider = \"openrouter\"\n[llm.emitter]\nmodel = \"google/gemini-3.8-flash\"\n[llm.replier]\nmodel = \"google/gemini-3.8-flash\"\n",
        )
        .unwrap();
        cloud.llm.apply_overrides(Some("ollama".into()), None);
        for t in cloud.llm.roles().unwrap() {
            assert_eq!(t.model, "qwen2.5:3b", "{:?}", t.role);
            assert_eq!(t.base_url.as_deref(), Some("http://localhost:11434"));
        }
        // Naming the provider it is already on changes nothing.
        let mut same = AppConfig::parse(
            "[llm]\nprovider = \"openrouter\"\n[llm.emitter]\nmodel = \"google/gemini-3.8-flash\"\n",
        )
        .unwrap();
        same.llm.apply_overrides(Some("openrouter".into()), None);
        assert_eq!(
            same.llm.role(Role::Emitter).unwrap().model,
            "google/gemini-3.8-flash"
        );
        // An explicit NS_MODEL still wins over the preset default.
        let mut both = AppConfig::parse("[llm]\nprovider = \"openrouter\"").unwrap();
        both.llm
            .apply_overrides(Some("ollama".into()), Some("gemma3:4b".into()));
        assert_eq!(both.llm.role(Role::Emitter).unwrap().model, "gemma3:4b");
    }

    #[test]
    fn a_local_target_needs_no_key_a_remote_one_does() {
        let local = AppConfig::parse("[llm]\nprovider = \"ollama\"")
            .unwrap()
            .llm
            .role(Role::Emitter)
            .unwrap();
        assert_eq!(local.key().as_deref(), Some("local"));
        let remote = AppConfig::parse(
            "[llm]\nprovider = \"mistral\"\napi_key_env = \"NS_TEST_KEY_ENV_THAT_IS_UNSET\"",
        )
        .unwrap()
        .llm
        .role(Role::Emitter)
        .unwrap();
        assert_eq!(remote.key(), None);
    }

    #[test]
    fn empty_config_gets_all_defaults() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(
            cfg.llm.role(Role::Emitter).unwrap().model,
            "openrouter/free"
        );
        assert_eq!(
            cfg.llm.role(Role::Replier).unwrap().model,
            "openrouter/free"
        );
        assert_eq!(cfg.engine.max_iterations, 5);
        assert_eq!(cfg.engine.max_emit_retries, 3);
        assert_eq!(cfg.store.path, "ns.sqlite");
        assert!(cfg.http_components.is_empty());
    }

    /// The shipped example must parse and resolve — it is the first thing
    /// anyone copies.
    #[test]
    fn config_example_parses_and_resolves_every_role() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.example.toml");
        let text = std::fs::read_to_string(path).expect("config.example.toml");
        let cfg = AppConfig::parse(&text).expect("example parses");
        for t in cfg.llm.roles().expect("example resolves") {
            assert!(!t.model.is_empty());
        }
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
        let s = cfg.llm.role(Role::Summarizer).unwrap();
        assert_eq!(s.model, "mistral-small-latest");
        assert_eq!(s.base_url.as_deref(), Some("https://api.mistral.ai"));
        assert_eq!(s.api_key_env, "MISTRAL_API_KEY");

        let cfg = AppConfig::parse(
            "[llm.summarizer]\nmodel = \"qwen2.5:3b\"\nbase_url = \"http://localhost:11434\"\napi_key_env = \"OLLAMA_API_KEY\"\n",
        )
        .unwrap();
        let s = cfg.llm.role(Role::Summarizer).unwrap();
        assert_eq!(s.model, "qwen2.5:3b");
        assert_eq!(s.base_url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(s.api_key_env, "OLLAMA_API_KEY");
        // …and the shorter spelling of the same swap.
        let cfg = AppConfig::parse("[llm.summarizer]\nmodel = \"ollama:qwen2.5:3b\"\n").unwrap();
        assert_eq!(cfg.llm.role(Role::Summarizer).unwrap(), s);
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
