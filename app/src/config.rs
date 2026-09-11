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
    #[serde(default)]
    pub router: RouterSection,
    /// [models] — the local model service (nsmodels) the evaluation lane may
    /// use. Absent, or `enabled = false`, means nothing reaches for it.
    #[serde(default)]
    pub models: ModelsSection,
    /// [pointer] — a desktop to drive, through the ns-pointer agent on it.
    /// Absent means no pointer actions are registered.
    #[serde(default)]
    pub pointer: Option<PointerSection>,
    /// [serve] — `ns-app serve`: where the TCP channel listens and which env
    /// var holds its token.
    #[serde(default)]
    pub serve: ServeSection,
}

/// [models] — the local CPU model service, for the evaluation lane only
/// (M8 T0.3).
///
/// `~/models` (nsmodels) serves embeddings and a cross-encoder reranker on
/// loopback, warm, because a cold load is ~30 s and a per-call subprocess is
/// a thousand times the cost of a warm request. What it is here for is the
/// free tier: `openrouter/free` allows about fifty requests a day, and an
/// evaluation lane that spends one of them per graded turn competes with the
/// work it is grading.
///
/// What it may not be is a decision. M6 §13 stands unchanged — no model in
/// the guard chain and no model-decided applies — and a local model does not
/// acquire an exception by being free. Its output enters as a signal and
/// becomes a gate only on a measured true-positive rate, which is what
/// `evaluator_min_kappa` will be for.
///
/// Off by default, and a service that is not answering degrades the lane
/// rather than failing it: this is the behaviour `adgen-eval::local_vision`
/// and `ns-pointerd`'s OCR tools already have, and it is why they stay usable
/// on a machine where the server is not always up.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ModelsSection {
    /// Whether anything may call the service at all.
    #[serde(default)]
    pub enabled: bool,
    /// Where it listens. 7374 is nsmodels' own default, one past
    /// `ns-pointerd`'s 7373.
    #[serde(default = "default_models_base_url")]
    pub base_url: String,
    /// How long one request may take before the signal is counted
    /// unavailable. Short on purpose: the pass runs while the harness waits
    /// for the next message, and a hung scorer would hold that wait open.
    #[serde(default = "default_models_timeout_ms")]
    pub timeout_ms: u64,
    /// Cosine at or above which a follow-up is the same question again
    /// (M8 T2.3).
    ///
    /// **Chosen on the development half of `nstestkit::grading` and nowhere
    /// else** — `ns-app grade --sweep` is what picks it, and the flag refuses
    /// to run against the held-out half. A cut fitted on the data a number is
    /// quoted from is the failure the 2026 transfer audit measured at 0.172
    /// AUROC of regret.
    #[serde(default = "default_reask_cosine")]
    pub reask_cosine: f32,
    /// Cross-encoder score below which the reply is not about the question.
    /// Chosen the same way, on the same half.
    #[serde(default = "default_relevance_cut")]
    pub relevance_cut: f32,
    /// Not-yet-graded turns one evolution pass may grade (M8 T2.8, M9 T1.3).
    ///
    /// It lives under `[models]` rather than in an `[eval]` section of its
    /// own because that is what the budget is *about*: grading is only
    /// expensive when a scorer is dialled, and the scorer is configured
    /// here. A graded turn is remembered by its `Graded` event, so this pays
    /// for new turns only — a second pass over the same log grades nothing.
    #[serde(default = "default_evaluate_budget_turns")]
    pub evaluate_budget_turns: u32,
}

impl Default for ModelsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_models_base_url(),
            timeout_ms: default_models_timeout_ms(),
            reask_cosine: default_reask_cosine(),
            relevance_cut: default_relevance_cut(),
            evaluate_budget_turns: default_evaluate_budget_turns(),
        }
    }
}

/// 40, the same number `probe_budget_turns` uses. One idle pass, one budget
/// of the same order: the two lanes compete for the same wait.
fn default_evaluate_budget_turns() -> u32 {
    40
}

fn default_models_base_url() -> String {
    "http://127.0.0.1:7374".into()
}

/// Swept on the development half, 2026-09-09: the exact value the sweep
/// picked, not a rounded one. Rounding a fitted cut changes the setting and
/// quietly makes the held-out number a measurement of something else.
fn default_reask_cosine() -> f32 {
    0.627_304_9
}
/// Likewise. Small because the cross-encoder emits a probability, not a
/// logit, and an off-topic reply scores very near zero: the development half
/// put the median at 0.050 and the 25th percentile at 0.001.
fn default_relevance_cut() -> f32 {
    0.000_484_392
}
fn default_models_timeout_ms() -> u64 {
    2000
}

/// [router] — the cue lists that decide a turn's tier (M7 Phase 3).
///
/// Configuration rather than code because the lists are exactly what the
/// evolution pass should be able to propose an addition to and gate: a
/// `Misrouted` signature names the message that routed wrong, and a cue is
/// the smallest patch that fixes it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct RouterSection {
    /// Off means every turn is `Task` — the whole legal set, the whole
    /// context, exactly as the engine behaved before the router existed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Words that mean the answer is probably outside the window, so the
    /// engine should search before proposing rather than spend an iteration
    /// being asked to.
    #[serde(default)]
    pub recall_cues: Vec<String>,
    /// Words that mean something is to be done, so the tools must be legal.
    #[serde(default)]
    pub task_cues: Vec<String>,
}

impl Default for RouterSection {
    fn default() -> Self {
        Self {
            enabled: true,
            recall_cues: Vec::new(),
            task_cues: Vec::new(),
        }
    }
}

impl RouterSection {
    /// Empty lists mean "keep the built-in ones", not "no cues": a `[router]`
    /// section written to set `enabled` should not silently disarm the
    /// routing it just switched on.
    pub fn router(&self) -> nsengine::router::KeywordRouter {
        let mut r = nsengine::router::KeywordRouter::default();
        if !self.recall_cues.is_empty() {
            r.recall_cues = self.recall_cues.clone();
        }
        if !self.task_cues.is_empty() {
            r.task_cues = self.task_cues.clone();
        }
        r
    }
}

/// [pointer] — where the ns-pointer agent listens and which env var holds
/// its token. The token never goes in the file, for the same reason the
/// provider keys do not.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PointerSection {
    /// `host:port` of the agent, reachable from this process. The agent
    /// binds loopback unless told `allow_remote`, so a remote address means
    /// it was started to accept one.
    pub addr: String,
    #[serde(default = "default_pointer_token_env")]
    pub token_env: String,
    /// Whether to read the agent's compose box, so the person at that machine
    /// can talk back mid-task. On by default: the service answers from memory
    /// on loopback, and an agent that does not offer it costs one refused
    /// connection and a warning.
    #[serde(default = "default_true")]
    pub messages: bool,
    /// `host:port` of the messages service. Unset means the agent's default,
    /// which is the pointer port plus one.
    #[serde(default)]
    pub messages_addr: Option<String>,
}

fn default_pointer_token_env() -> String {
    "NS_POINTER_TOKEN".into()
}

impl PointerSection {
    pub fn token(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Where the messages service is, or `None` when it is switched off or
    /// the pointer address is too odd to derive one from.
    ///
    /// The agent puts the two services one port apart, so deriving costs the
    /// owner nothing in the normal case and `messages_addr` is there for when
    /// it is not the normal case.
    pub fn messages_target(&self) -> Option<String> {
        if !self.messages {
            return None;
        }
        if let Some(explicit) = &self.messages_addr {
            return Some(explicit.clone());
        }
        // Rightmost colon: an IPv6 literal has its own.
        let (host, port) = self.addr.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        Some(format!("{host}:{}", port.checked_add(1)?))
    }
}

/// [serve] — `ns-app serve`: the TCP channel (`nschannel_tcp`) that carries
/// many conversations at once, one session per connection (multi-conversation
/// plan Phase 3). The token never goes in the file, for the same reason the
/// provider keys and the pointer's do not.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ServeSection {
    /// `host:port` to listen on. Loopback unless `allow_remote`.
    #[serde(default = "default_serve_listen")]
    pub listen: String,
    /// The env var holding the token every client presents in its first
    /// line. Unset or blank means `serve` refuses to start.
    #[serde(default = "default_serve_token_env")]
    pub token_env: String,
    /// Connections at once, machine-wide; the next one is closed at accept.
    #[serde(default = "default_serve_max_connections")]
    pub max_connections: usize,
    /// Whether a non-loopback `listen` is meant. Off, a bind to one is
    /// refused at startup: the channel has no auth beyond the token and no
    /// TLS, so on the network it would be the whole conversation in clear.
    #[serde(default)]
    pub allow_remote: bool,
}

fn default_serve_listen() -> String {
    "127.0.0.1:7375".into()
}

fn default_serve_token_env() -> String {
    "NS_SERVE_TOKEN".into()
}

fn default_serve_max_connections() -> usize {
    8
}

impl Default for ServeSection {
    fn default() -> Self {
        Self {
            listen: default_serve_listen(),
            token_env: default_serve_token_env(),
            max_connections: default_serve_max_connections(),
            allow_remote: false,
        }
    }
}

impl ServeSection {
    /// The token, or `None` when the variable is unset or blank.
    pub fn token(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
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
    /// M9 T3.1 — the activation prior's weight `w` in
    /// `hits + w · ln(1 + uses) · exp(−Δdays / half_life)`, and the same `w`
    /// on `search_turns`' recency term.
    ///
    /// **Default 0.0, which is pre-M9 ranking exactly.** vstash's negative
    /// result on BEIR is why a recency×frequency prior ships off; T3.3's
    /// suites at `w ∈ {0, 0.5, 1}` are what may move it, not taste.
    #[serde(default = "default_activation_weight")]
    pub activation_weight: f32,
    /// Days for the fact term to halve (M9 T3.1). The turn term's half-life
    /// is a constant in turns (`nscore::RECENCY_HALF_LIFE_TURNS`): a turn
    /// number is not a clock.
    #[serde(default = "default_activation_half_life_days")]
    pub activation_half_life_days: f32,
    /// Obligations extracted from one user message (M9 T2.1); 0 renders no
    /// block.
    #[serde(default = "default_obligations_max")]
    pub obligations_max: usize,
    /// Regenerate a reply once when an `answer:` obligation went
    /// unaddressed (M9 T2.1). Off until T0.4's ablation arm has priced the
    /// block: the block renders whatever this says.
    #[serde(default)]
    pub obligation_check: bool,
    /// Guidance notes rendered into either context, at most (M9 T2.2).
    #[serde(default = "default_guidance_max")]
    pub guidance_max: usize,
    /// Days without use before a fact goes cold (M6 §6.2).
    #[serde(default = "default_fact_stale_days")]
    pub fact_stale_days: u64,
    /// How many of a turn's own tool outcomes stay verbatim in the prompt
    /// (M7 T1.3). Older ones fold into one counted line; refusals never
    /// fold. 0 turns the fold off.
    #[serde(default = "default_trace_verbatim_lines")]
    pub trace_verbatim_lines: usize,
    /// How many characters of one tool result reach the prompt before the
    /// clip leaves a handle in its place (M7 T1.1, settable since M8 T0.2).
    /// The right value depends on the tools a deployment runs: a control
    /// tree and a two-line shell result do not want the same cap.
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,
    /// Ceiling for the whole composed context, in estimated tokens (M7
    /// T2.1). 0 turns the budget off.
    #[serde(default = "default_prompt_budget_tokens")]
    pub prompt_budget_tokens: u32,
    /// "report" measures the drops and makes none; "enforce" applies them.
    #[serde(default = "default_budget_mode")]
    pub budget_mode: String,
    /// Show the emitter its own size against its ceiling (M7 T2.3).
    #[serde(default)]
    pub show_budget_line: bool,
    /// How many earlier sessions of the scope `recall` searches beyond this
    /// one (M7 Phase 4). 0 keeps recall inside the current conversation.
    #[serde(default = "default_recall_sessions")]
    pub recall_sessions: usize,
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
fn default_obligations_max() -> usize {
    5
}

fn default_guidance_max() -> usize {
    6
}

fn default_relevant_max() -> usize {
    5
}

/// Off. The prior ships inert and a measurement turns it on (M9 T3.1).
fn default_activation_weight() -> f32 {
    0.0
}

fn default_activation_half_life_days() -> f32 {
    7.0
}
fn default_fact_stale_days() -> u64 {
    90
}

/// Five outcomes. A desktop turn runs to twelve iterations and most of them
/// are moves and clicks whose whole content is "it worked"; five keeps the
/// recent screen reads, which are the ones with anything in them.
fn default_trace_verbatim_lines() -> usize {
    5
}

/// The engine's own default, not a second opinion about it: a copy here
/// would drift, and the drift would show up as a budget report that
/// disagreed with the prompt it claimed to measure.
fn default_tool_result_max_chars() -> usize {
    nsengine::trace::DEFAULT_TOOL_RESULT_MAX_CHARS
}

/// Six thousand tokens of composed context. Not a model's limit — it is a
/// working ceiling for the blocks the engine controls, chosen so the pieces
/// M6 sizes (window ≈1k, facts ≈300, summary ≈250) plus a desktop turn's
/// trace sit inside it with room, and so going over is a signal rather than
/// a routine event.
fn default_prompt_budget_tokens() -> u32 {
    6000
}

/// Report, not enforce. The drops are recorded and not made until the share
/// of them that leave the next turn unaffected has been measured on real
/// sessions; Self-GC puts that at about 85% for its own prunes, and below
/// something like it the priority order is wrong rather than the idea.
fn default_budget_mode() -> String {
    "report".into()
}

/// Three earlier conversations. Recall is precision-bound, not
/// coverage-bound — irrelevant memory measurably degrades a reply
/// (findings 2026-09-02 §1) — and a lexical index over every session anyone
/// ever had would return its best match whether or not it meant anything.
fn default_recall_sessions() -> usize {
    3
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
            activation_weight: default_activation_weight(),
            activation_half_life_days: default_activation_half_life_days(),
            obligations_max: default_obligations_max(),
            obligation_check: false,
            guidance_max: default_guidance_max(),
            fact_stale_days: default_fact_stale_days(),
            trace_verbatim_lines: default_trace_verbatim_lines(),
            tool_result_max_chars: default_tool_result_max_chars(),
            prompt_budget_tokens: default_prompt_budget_tokens(),
            budget_mode: default_budget_mode(),
            show_budget_line: false,
            recall_sessions: default_recall_sessions(),
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

    /// Err names the bad value. A typo here would otherwise mean the budget
    /// silently stayed in report mode, which looks exactly like a budget
    /// that found nothing to drop.
    pub fn budget_mode(&self) -> Result<nscore::BudgetMode, String> {
        nscore::BudgetMode::parse(&self.budget_mode).ok_or_else(|| {
            format!(
                "[memory] budget_mode must be \"report\" or \"enforce\", got {:?}",
                self.budget_mode
            )
        })
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
    /// Whether an irreversible action is staged and confirmed before it runs.
    ///
    /// True for a session someone is sitting in front of. False for one meant
    /// to run unattended: there is nobody to answer the prompt, so the gate
    /// stops being a safeguard and becomes a stall. Turning it off means every
    /// irreversible action the model proposes happens — clicks and typing on a
    /// real desktop included, where there is no undo for "sent the email".
    /// The brakes that remain are the machine's own: the local override, the
    /// arming chord, and the badge's pie menu.
    #[serde(default = "default_confirm_irreversible")]
    pub confirm_irreversible: bool,
    /// How many turns may run at once across all sessions (multi-conversation
    /// plan Phase 2). Each session is still one turn at a time; this bounds
    /// how many sessions are mid-turn. `1` is the CLI's serial behaviour. A
    /// larger number overlaps the waiting of several conversations — it does
    /// not add requests: the per-provider throttle and the daily allowance
    /// stay global. Read through `worker_slots()`, which refuses 0.
    #[serde(default = "default_worker_slots")]
    pub worker_slots: usize,
}

fn default_confirm_irreversible() -> bool {
    true
}

fn default_worker_slots() -> usize {
    1
}

impl Default for EngineSection {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            max_emit_retries: 3,
            confirm_irreversible: default_confirm_irreversible(),
            worker_slots: default_worker_slots(),
        }
    }
}

impl EngineSection {
    /// Err names the bad value; config errors are fatal at startup. Zero
    /// slots would park every turn forever.
    pub fn worker_slots(&self) -> Result<usize, String> {
        match self.worker_slots {
            0 => Err("[engine] worker_slots must be at least 1, got 0".into()),
            n => Ok(n),
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
    /// M8 T2.1. Jaccard over content tokens at or above which two user turns
    /// are the same question in different words. A key rather than a
    /// constant because the number is a guess with no corpus behind it yet
    /// (`2026-09-09-symbolic-evaluation-findings.md` §1) — the exact-repeat
    /// band, which is what T2.7 will calibrate against, does not depend on it.
    #[serde(default = "default_reask_jaccard")]
    pub reask_jaccard: f32,
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
fn default_reask_jaccard() -> f32 {
    0.6
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
            reask_jaccard: default_reask_jaccard(),
        }
    }
}

impl EvolutionSection {
    pub fn pass_config(
        &self,
        dry_run: bool,
        fact_stale_days: u64,
        evaluate_budget_turns: u32,
    ) -> nsevolution::pass::PassConfig {
        nsevolution::pass::PassConfig {
            regression_budget: self.regression_budget,
            probe_budget_turns: self.probe_budget_turns,
            max_notes: self.max_notes,
            regression_replay_cap: self.regression_replay_cap,
            dry_run,
            fact_stale_days,
            // The CLI maps every session to one scope (M6 §15), so digests
            // are written under it. A multi-user channel replaces this with
            // the same mapping `scope_for` applies.
            digest_scope: "global".into(),
            evaluate: nsevolution::evaluate::EvaluateConfig {
                reask_jaccard: self.reask_jaccard,
                budget_turns: evaluate_budget_turns,
                ..Default::default()
            },
            // The symbolic checks are the baseline every other evaluator is
            // calibrated against (T2.7), so they are what the gate believes
            // until a κ threshold says otherwise.
            authoritative_evaluator: "symbolic".into(),
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

    /// The agent to drive, if any. `NS_POINTER_ADDR` — the same variable
    /// `ns-pointer-mcp` and the CLI read — repoints a configured section or
    /// stands in for a missing one, so one export tries a different machine
    /// without editing the file.
    pub fn pointer_target(&self, env_addr: Option<String>) -> Option<PointerSection> {
        match (&self.pointer, env_addr) {
            (Some(p), Some(addr)) => Some(PointerSection {
                addr,
                token_env: p.token_env.clone(),
                messages: p.messages,
                // Deliberately not carried over: the override names a port on
                // the machine that was configured, and the env var has just
                // pointed everything at a different one.
                messages_addr: None,
            }),
            (Some(p), None) => Some(p.clone()),
            (None, Some(addr)) => Some(PointerSection {
                addr,
                token_env: default_pointer_token_env(),
                messages: true,
                messages_addr: None,
            }),
            (None, None) => None,
        }
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

    /// The cap M7 left as a `const`: a default deployment sees exactly the
    /// number the engine was measured at, and a deployment whose tools
    /// produce something else can say so without a rebuild.
    #[test]
    fn tool_result_max_chars_defaults_to_the_engine_constant_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(
            cfg.memory.tool_result_max_chars,
            nsengine::trace::DEFAULT_TOOL_RESULT_MAX_CHARS
        );
        assert_eq!(cfg.memory.tool_result_max_chars, 1200);
        let cfg = AppConfig::parse("[memory]\ntool_result_max_chars = 400\n").unwrap();
        assert_eq!(cfg.memory.tool_result_max_chars, 400);
        // The other M7 knob, asserted beside it: both are settable now, and
        // a regression that unwires one should not be reported as passing
        // because the other still parses.
        let cfg = AppConfig::parse("[memory]\ntrace_verbatim_lines = 2\n").unwrap();
        assert_eq!(cfg.memory.trace_verbatim_lines, 2);
    }

    /// Absent `[models]` is the normal case and must mean "reach for
    /// nothing": the service is a machine-local convenience, and a config
    /// written before it existed has to keep behaving as it did.
    #[test]
    fn models_section_is_off_unless_asked_for() {
        let cfg = AppConfig::parse("").unwrap();
        assert!(!cfg.models.enabled);
        assert_eq!(cfg.models.base_url, "http://127.0.0.1:7374");
        assert_eq!(cfg.models.timeout_ms, 2000);

        // Naming the section is not the same as switching it on.
        let cfg = AppConfig::parse("[models]\n").unwrap();
        assert!(!cfg.models.enabled);
        assert_eq!(cfg.models.evaluate_budget_turns, 40);

        let cfg = AppConfig::parse(
            "[models]\nenabled = true\nbase_url = \"http://127.0.0.1:9999\"\ntimeout_ms = 500\n",
        )
        .unwrap();
        assert!(cfg.models.enabled);
        assert_eq!(cfg.models.base_url, "http://127.0.0.1:9999");
        assert_eq!(cfg.models.timeout_ms, 500);
    }

    /// M9 T1.3. The grading budget is settable, and it reaches the pass.
    #[test]
    fn evaluate_budget_turns_parses_and_reaches_the_pass() {
        let cfg = AppConfig::parse("[models]\nevaluate_budget_turns = 7\n").unwrap();
        assert_eq!(cfg.models.evaluate_budget_turns, 7);
        let pc = cfg
            .evolution
            .pass_config(true, 90, cfg.models.evaluate_budget_turns);
        assert_eq!(pc.evaluate.budget_turns, 7);
        // And the gate's default belief is the symbolic baseline.
        assert_eq!(pc.authoritative_evaluator, "symbolic");
    }

    #[test]
    fn memory_section_defaults_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.memory.window_turns, 6);
        assert_eq!(cfg.memory.facts_in_context, 10);
        assert_eq!(cfg.memory.pinned_prefixes, vec!["user.".to_string()]);
        assert_eq!(cfg.memory.fact_stale_days, 90);
        assert_eq!(cfg.memory.caps(), nscore::Caps::default());
        // M9 T2.1/T2.2: the block renders by default, the interceptor does
        // not fire until T0.4's arm has priced it.
        assert_eq!(cfg.memory.obligations_max, 5);
        assert!(!cfg.memory.obligation_check);
        assert_eq!(cfg.memory.guidance_max, 6);
        let tuned = AppConfig::parse(
            "[memory]\nobligations_max = 2\nobligation_check = true\nguidance_max = 3\n",
        )
        .unwrap();
        assert_eq!(tuned.memory.obligations_max, 2);
        assert!(tuned.memory.obligation_check);
        assert_eq!(tuned.memory.guidance_max, 3);
        // M9 T3.1: the activation prior ships inert. A config that never
        // heard of it must rank exactly as it did before M9.
        assert_eq!(cfg.memory.activation_weight, 0.0);
        assert_eq!(cfg.memory.activation_half_life_days, 7.0);
        let prior = AppConfig::parse(
            "[memory]\nactivation_weight = 0.5\nactivation_half_life_days = 14.0\n",
        )
        .unwrap();
        assert_eq!(prior.memory.activation_weight, 0.5);
        assert_eq!(prior.memory.activation_half_life_days, 14.0);
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

    /// `[engine] worker_slots` defaults to the CLI's one slot, takes a larger
    /// number, and refuses 0 — which would park every turn forever — by name.
    #[test]
    fn engine_worker_slots_defaults_to_one_and_rejects_zero() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.engine.worker_slots().unwrap(), 1);
        let cfg = AppConfig::parse("[engine]\nmax_iterations = 5\nmax_emit_retries = 3\n").unwrap();
        assert_eq!(cfg.engine.worker_slots().unwrap(), 1);
        let cfg = AppConfig::parse(
            "[engine]\nmax_iterations = 5\nmax_emit_retries = 3\nworker_slots = 4\n",
        )
        .unwrap();
        assert_eq!(cfg.engine.worker_slots().unwrap(), 4);
        let cfg = AppConfig::parse(
            "[engine]\nmax_iterations = 5\nmax_emit_retries = 3\nworker_slots = 0\n",
        )
        .unwrap();
        let err = cfg.engine.worker_slots().unwrap_err();
        assert!(err.contains("[engine] worker_slots"), "{err}");
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
        assert_eq!(cfg.evolution.pass_config(true, 90, 40).probe_budget_turns, 7);
        assert!(cfg.evolution.pass_config(true, 90, 40).dry_run);
        let cfg = AppConfig::parse("[evolution]\nidle_after_secs = 0\n").unwrap();
        assert_eq!(cfg.evolution.idle_after(), None);
    }

    /// `[serve]` defaults to loopback 7375, `NS_SERVE_TOKEN`, eight
    /// connections and no remote bind; each field parses on its own, and
    /// the token is read from the named variable, trimmed, blank meaning
    /// unset.
    #[test]
    fn serve_section_defaults_and_parses() {
        let cfg = AppConfig::parse("").unwrap();
        assert_eq!(cfg.serve, ServeSection::default());
        assert_eq!(cfg.serve.listen, "127.0.0.1:7375");
        assert_eq!(cfg.serve.token_env, "NS_SERVE_TOKEN");
        assert_eq!(cfg.serve.max_connections, 8);
        assert!(!cfg.serve.allow_remote);

        let cfg = AppConfig::parse(
            "[serve]\nlisten = \"0.0.0.0:9000\"\ntoken_env = \"MY_TOKEN\"\n\
             max_connections = 2\nallow_remote = true\n",
        )
        .unwrap();
        assert_eq!(cfg.serve.listen, "0.0.0.0:9000");
        assert_eq!(cfg.serve.token_env, "MY_TOKEN");
        assert_eq!(cfg.serve.max_connections, 2);
        assert!(cfg.serve.allow_remote);

        let cfg = AppConfig::parse("[serve]\nmax_connections = 3\n").unwrap();
        assert_eq!(
            cfg.serve.listen, "127.0.0.1:7375",
            "the rest keep their defaults"
        );
        assert_eq!(cfg.serve.max_connections, 3);

        // A variable no other test touches, so this cannot race one.
        let cfg =
            AppConfig::parse("[serve]\ntoken_env = \"NS_TEST_SERVE_TOKEN_2026_09_10\"\n").unwrap();
        std::env::remove_var("NS_TEST_SERVE_TOKEN_2026_09_10");
        assert_eq!(cfg.serve.token(), None);
        std::env::set_var("NS_TEST_SERVE_TOKEN_2026_09_10", "  ");
        assert_eq!(cfg.serve.token(), None, "blank is unset");
        std::env::set_var("NS_TEST_SERVE_TOKEN_2026_09_10", " s3cret ");
        assert_eq!(cfg.serve.token().as_deref(), Some("s3cret"));
        std::env::remove_var("NS_TEST_SERVE_TOKEN_2026_09_10");
    }

    /// Absent is "no desktop"; present needs an address, and the token is an
    /// env var name, never the token.
    #[test]
    fn pointer_section_is_optional_and_env_addr_repoints_it() {
        let cfg = AppConfig::parse("").unwrap();
        assert!(cfg.pointer.is_none());
        assert!(cfg.pointer_target(None).is_none());
        // One export tries a machine without a config edit.
        let t = cfg.pointer_target(Some("10.0.0.5:7373".into())).unwrap();
        assert_eq!(t.addr, "10.0.0.5:7373");
        assert_eq!(t.token_env, "NS_POINTER_TOKEN");

        let cfg =
            AppConfig::parse("[pointer]\naddr = \"192.168.1.40:7373\"\ntoken_env = \"DESK_TOKEN\"")
                .unwrap();
        let t = cfg.pointer_target(None).unwrap();
        assert_eq!(t.addr, "192.168.1.40:7373");
        assert_eq!(t.token_env, "DESK_TOKEN");
        // The env address wins, the configured token env stays.
        let t = cfg.pointer_target(Some("127.0.0.1:7373".into())).unwrap();
        assert_eq!(t.addr, "127.0.0.1:7373");
        assert_eq!(t.token_env, "DESK_TOKEN");

        assert!(
            AppConfig::parse("[pointer]\ntoken_env = \"X\"").is_err(),
            "a section without an address is a mistake, not a default"
        );
    }

    /// The agent puts its two services one port apart, so a config that names
    /// only the pointer still finds the compose box.
    #[test]
    fn the_messages_service_is_derived_from_the_pointer_port() {
        let cfg = AppConfig::parse("[pointer]\naddr = \"127.0.0.1:7373\"").unwrap();
        let t = cfg.pointer_target(None).unwrap();
        assert_eq!(t.messages_target().as_deref(), Some("127.0.0.1:7374"));
    }

    #[test]
    fn an_explicit_messages_address_wins() {
        let cfg = AppConfig::parse(
            "[pointer]\naddr = \"10.0.0.5:7373\"\nmessages_addr = \"10.0.0.5:9999\"",
        )
        .unwrap();
        let t = cfg.pointer_target(None).unwrap();
        assert_eq!(t.messages_target().as_deref(), Some("10.0.0.5:9999"));
    }

    #[test]
    fn the_compose_box_can_be_switched_off() {
        let cfg =
            AppConfig::parse("[pointer]\naddr = \"127.0.0.1:7373\"\nmessages = false").unwrap();
        assert_eq!(cfg.pointer_target(None).unwrap().messages_target(), None);
    }

    /// `NS_POINTER_ADDR` repoints everything at another machine, so an override
    /// naming a port on the machine that was configured must not survive it.
    #[test]
    fn an_env_override_drops_a_stale_messages_address() {
        let cfg = AppConfig::parse(
            "[pointer]\naddr = \"10.0.0.5:7373\"\nmessages_addr = \"10.0.0.5:9999\"",
        )
        .unwrap();
        let t = cfg
            .pointer_target(Some("192.168.1.40:7373".into()))
            .unwrap();
        assert_eq!(t.messages_target().as_deref(), Some("192.168.1.40:7374"));
    }

    /// An IPv6 literal has colons of its own; the port is the rightmost one.
    #[test]
    fn an_ipv6_address_keeps_its_own_colons() {
        let cfg = AppConfig::parse("[pointer]\naddr = \"[::1]:7373\"").unwrap();
        let t = cfg.pointer_target(None).unwrap();
        assert_eq!(t.messages_target().as_deref(), Some("[::1]:7374"));
    }

    /// An address with no port cannot have one derived, and guessing would
    /// dial something arbitrary.
    #[test]
    fn an_underivable_address_yields_no_messages_service() {
        let cfg = AppConfig::parse("[pointer]\naddr = \"desktop.local\"").unwrap();
        assert_eq!(cfg.pointer_target(None).unwrap().messages_target(), None);
    }
}
