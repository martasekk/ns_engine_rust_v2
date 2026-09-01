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
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub emitter: ModelSection,
    #[serde(default)]
    pub replier: ReplierSection,
}

#[derive(Debug, serde::Deserialize)]
pub struct ModelSection {
    pub model: String,
}

impl Default for ModelSection {
    fn default() -> Self {
        Self { model: "z-ai/glm-5.2:free".into() }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ReplierSection {
    pub model: String,
}

impl Default for ReplierSection {
    fn default() -> Self {
        Self { model: "z-ai/glm-5.2:free".into() }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct EngineSection {
    pub max_iterations: u32,
    pub max_emit_retries: u32,
}

impl Default for EngineSection {
    fn default() -> Self {
        Self { max_iterations: 5, max_emit_retries: 3 }
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
        Self { path: "ns.sqlite".into() }
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
    fn empty_config_gets_all_defaults() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.llm.emitter.model, "z-ai/glm-5.2:free");
        assert_eq!(cfg.llm.replier.model, "z-ai/glm-5.2:free");
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
        assert_eq!(cfg.templates.get("cant_help").map(String::as_str), Some("Sorry."));
        assert!(AppConfig::parse("").unwrap().templates.is_empty());
    }
}
