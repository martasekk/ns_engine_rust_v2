//! One tenant's config in, one `Engine` out.
//!
//! Multi-tenant plan D2: everything that varies per company is already a
//! field of `EngineConfig` or a slot of `HarnessParts`, so the factory is
//! not a design — it is the assembly that used to be inlined in `main`,
//! lifted out unchanged so a second caller can run it. The only difference
//! is the failure mode: where `main` could exit the process on a bad
//! config, the factory returns a `StartupError`, because a shard hosting
//! nineteen good tenants must not die of the twentieth's typo.

use crate::config::{AppConfig, Role, RoleTarget};
use crate::env_override;
use nscore::{HarnessBuilder, SessionId, Tool};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

pub(crate) type RulesHandle = Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>>;

/// What the engine is being built for. Today's `serve: bool`, named: the
/// three places it is consulted are the channel, the fact scope and the
/// closing banner.
pub(crate) enum Mode {
    /// The interactive CLI: stdin plus the desktop's compose box, one
    /// `global` fact scope, the session id this run was given.
    Cli { session: String },
    /// `ns-app serve`: the TCP channel, a fact scope per session.
    Serve,
}

/// A startup the tenant refused, with the text `main` used to print before
/// exiting on it. Every variant is exit code 1; `code` exists so a caller
/// never has to remember that.
#[derive(Debug)]
pub(crate) enum StartupError {
    /// A semantic error in `config.toml` — an unknown provider, an
    /// unparseable knob, a shape that would not resolve.
    Config(String),
    /// A `[llm]` fact the whole process reads: the schema profile and the
    /// capability tier. Printed bare, as it always was.
    Llm(String),
    /// `learned.toml` would not load. Fatal, because the evolution pass
    /// would otherwise propose against the wrong base.
    Rules {
        path: String,
        detail: String,
    },
    /// A configured desktop with no token, which is a config error rather
    /// than the warning an unreachable desktop gets.
    MissingPointerToken {
        env: String,
        addr: String,
    },
    MissingApiKey {
        env: String,
        role: String,
    },
    MissingServeToken {
        env: String,
    },
    /// The listener would not bind.
    Serve(String),
    /// Assembly is a gate: a missing slot, a duplicated one, two tools
    /// claiming one name.
    Harness(String),
}

impl StartupError {
    pub(crate) fn code(&self) -> i32 {
        1
    }

    /// Print and exit exactly as the inlined assembly did. For `main`,
    /// which owns the process; a shard hosting many tenants logs the error
    /// and keeps the others running instead.
    pub(crate) fn exit(self) -> ! {
        eprintln!("{self}");
        std::process::exit(self.code())
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "config.toml: {e}"),
            Self::Llm(e) => write!(f, "{e}"),
            Self::Rules { path, detail } => write!(f, "{path}: {detail}"),
            Self::MissingPointerToken { env, addr } => write!(
                f,
                "{env} is not set — [pointer] addr = {addr:?} needs the agent's token."
            ),
            Self::MissingApiKey { env, role } => write!(
                f,
                "{env} is not set — the {role} role needs a provider API key.\n\
                 export {env}=... , or switch to a local backend: NS_PROVIDER=ollama (ns-app providers)."
            ),
            Self::MissingServeToken { env } => write!(
                f,
                "{env} is not set — `ns-app serve` needs a token; every client presents it in \
                 its first line."
            ),
            Self::Serve(e) => write!(f, "serve: {e}"),
            Self::Harness(e) => write!(f, "ns-harness: cannot assemble the harness: {e}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// One tenant's config and one mode in, one running-ready `Engine` out.
///
/// The order of construction is load-bearing and is the order `main` used:
/// every semantic check the chat path needs happens before anything is
/// dialled, the listener binds before the pointer socket is opened, and the
/// three startup banners print between `Engine::new` and the first turn.
pub(crate) async fn build_engine(
    cfg: &AppConfig,
    mode: Mode,
    max_requests: Option<u32>,
) -> Result<Arc<Engine>, StartupError> {
    // Both were resolved in `main` before the subcommands, which is where
    // they are still reported from; re-resolving here is pure, and keeps
    // the factory a function of the tenant's config alone.
    let schema_profile = cfg.llm.schema_profile().map_err(StartupError::Llm)?;
    let capability = cfg.llm.capability().map_err(StartupError::Llm)?;
    let serve = matches!(mode, Mode::Serve);
    let cli_session = match &mode {
        Mode::Cli { session } => session.clone(),
        Mode::Serve => String::new(),
    };

    // Each role resolves on its own, so the emitter can sit on a local
    // model while the replier stays in the cloud (or the other way round).
    let emitter_target = role(cfg, Role::Emitter)?;
    let replier_target = role(cfg, Role::Replier)?;
    let emitter_key = key(&emitter_target)?;
    let replier_key = key(&replier_target)?;

    // Settled before anything is built or dialled. `build_tools` opens the
    // pointer socket and arms an agent; failing after that on a typo in
    // `[memory]` means the config was rejected only once it had already
    // reached out to another machine. Every semantic check the chat path
    // needs happens here, in one place, while the process still holds
    // nothing.
    let remember_residual = cfg
        .memory
        .remember_residual()
        .map_err(StartupError::Config)?;
    let budget_mode = cfg.memory.budget_mode().map_err(StartupError::Config)?;
    let worker_slots = cfg.engine.worker_slots().map_err(StartupError::Config)?;
    // The listener too, for the same reason: a missing token or a bad
    // address is refused here, before the pointer has been dialled.
    let tcp = if serve {
        let Some(token) = cfg.serve.token() else {
            return Err(StartupError::MissingServeToken {
                env: cfg.serve.token_env.clone(),
            });
        };
        match nschannel_tcp::TcpChannel::bind(
            &cfg.serve.listen,
            token,
            cfg.serve.max_connections,
            cfg.serve.allow_remote,
        )
        .await
        {
            Ok(channel) => Some(channel),
            Err(e) => return Err(StartupError::Serve(e.to_string())),
        }
    } else {
        None
    };

    let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
    let rules = load_rules(cfg)?;
    let tools = build_tools(cfg, schema_profile).await?;

    // M7 T0.1: the engine hands each of its own calls a sink of its own
    // through the call's context, so this one is only the fallback for calls
    // made outside a turn — and it is what names the role in every record.
    let usage = Arc::new(nscore::UsageSink::new());

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(
        nsllm::emitter::CloudEmitter::new(
            crate::client_for(&emitter_target, transport.clone(), &emitter_key)
                .with_usage_sink(usage.clone(), "emitter"),
            emitter_target.model.clone(),
        )
        .with_shape(shape(
            cfg,
            &emitter_target,
            nsllm::emitter::default_shape(),
        )?)
        // M10 P4. Both halves have to hold: the endpoint must forward a
        // breakpoint at all, and the operator must have said the emitter
        // prefix is worth one. Either off means the request is today's.
        .with_prompt_cache(emitter_target.prompt_cache && cfg.llm.prompt_cache_emitter)
        // M12 T1.1.
        .with_capability(capability),
    ));
    b.set_replier(Box::new(
        nsllm::replier::CloudReplier::new(
            crate::client_for(&replier_target, transport.clone(), &replier_key)
                .with_usage_sink(usage.clone(), "replier"),
            replier_target.model.clone(),
        )
        .with_shape(shape(
            cfg,
            &replier_target,
            nsllm::replier::default_shape(),
        )?)
        .with_prompt_cache(replier_target.prompt_cache),
    ));
    b.set_memory(Arc::new(crate::models::store(cfg)));
    // Says whether the local model service is answering, when one is asked
    // for. Before the channel so the line lands with the other startup
    // reports rather than in the middle of the first turn.
    crate::models::announce(&cfg.models).await;
    let serve_addr = tcp.as_ref().map(|c| c.local_addr());
    if let Some(channel) = tcp {
        // `serve`: the TCP channel and nothing else — no stdin, and no
        // compose box, which joins a desktop to *one* session.
        b.set_shared_channel(channel);
    } else {
        // stdin, plus the desktop's compose box when there is one to read.
        // The agent has offered that channel since 2026-09-05 and nothing
        // collected it; a line typed into the badge went into the outbox and
        // stopped there.
        let cli = nschannel_cli::CliChannel::new_stdio().with_session(cli_session.clone());
        match desktop_messages(cfg).await {
            Some(client) => b.set_channel(Box::new(
                nscomponents_std::desktop_channel::WithDesktop::spawn(cli, client),
            )),
            None => b.set_channel(Box::new(cli)),
        };
    }
    // M6 §5.1: the rolling summary runs on its own role (model, provider,
    // key), so it can be swapped without touching the emitter or replier.
    if cfg.memory.summary_every_turns > 0 {
        let target = role(cfg, Role::Summarizer)?;
        match target.key() {
            Some(role_key) => {
                let c = crate::client_for(&target, transport.clone(), &role_key)
                    .with_usage_sink(usage.clone(), "summarizer");
                b.set_summarizer(Box::new(
                    nsllm::summarizer::CloudSummarizer::new(c, target.model.clone())
                        .with_shape(shape(cfg, &target, nsllm::summarizer::default_shape())?)
                        // M11 T0.5: the fixed fields as a schema, where the
                        // endpoint the summarizer actually reaches knows the
                        // field. The prompt keeps asking for JSON either way.
                        .with_structured_output(
                            nsllm::provider::for_base_url(target.base_url_or_default())
                                .is_some_and(|p| p.structured_output),
                        )
                        .with_guidelines(cfg.memory.summary_guidelines.clone()),
                ));
            }
            None => eprintln!(
                "{} is not set — rolling summary disabled.",
                target.api_key_env
            ),
        }
    }
    if idle_pass_allowed(max_requests, cfg.evolution.enabled) {
        // Driver B: the idle timer runs this pass during quiet periods.
        // Driver B is not a dry run, so it spends by the same rule it
        // always did: `spend` only ever gates a dry run.
        b.set_consolidator(Box::new(build_pass(
            cfg,
            rules.clone(),
            &tools,
            &emitter_target,
            false,
            true,
        )));
    } else {
        if let (Some(cap), true) = (max_requests, cfg.evolution.enabled) {
            println!("metered run: the idle evolution pass is off (cap {cap})");
        }
        b.set_consolidator(Box::new(NoopConsolidator));
    }
    for t in &tools {
        b.add_tool(t.clone());
    }

    // Assembly is a gate, not a formality: it refuses a missing slot, a
    // duplicated one, two tools claiming the same name, and an action whose
    // arguments would outrank the rationale in the compiled schema. An
    // `expect` here reported all four as a panic with a backtrace, which is
    // the least useful form for the only errors a user can actually fix.
    let parts = b
        .build()
        .map_err(|e| StartupError::Harness(e.to_string()))?;
    // M6 §6.6: the fact scope. The CLI is single-user, so every session
    // shares `global`. `serve` is a multi-user channel, and a global scope
    // there is a leak — what one client tells the engine would surface as a
    // standing fact in every other client's context — so each session is
    // its own scope. (§6.6 named the Telegram target; the TCP channel is the
    // same shape.)
    let scope_for: Arc<dyn Fn(&SessionId) -> String + Send + Sync> = if serve {
        Arc::new(|sid| sid.0.clone())
    } else {
        Arc::new(|_| "global".to_string())
    };
    let engine_cfg = EngineConfig {
        max_iterations: cfg.engine.max_iterations,
        max_emit_retries: cfg.engine.max_emit_retries,
        confirm_irreversible: cfg.engine.confirm_irreversible,
        persona: cfg.persona.text.clone(),
        templates: cfg.templates.clone(),
        learned: rules,
        idle_after: cfg.evolution.idle_after(),
        window_turns: cfg.memory.window_turns,
        caps: cfg.memory.caps(),
        facts_in_context: cfg.memory.facts_in_context,
        reply_grounding_check: cfg.memory.reply_grounding_check,
        // M12 T1.2: a strong model is flagged and logged, never regenerated
        // at — the second call buys nothing it did not already do.
        reply_regenerate: capability != nscore::Capability::Strong,
        // M12 T4.3: chat-tier act-or-answer, on unless `[llm]` says otherwise.
        chat_act_or_answer: cfg.llm.chat_act_or_answer,
        // M13 T2.1: and the same offer on Task and Deep, off unless asked.
        act_or_answer_every_tier: cfg.llm.act_or_answer_every_tier,
        // M13 T3.1: and the third branch, act *and* answer, likewise.
        act_and_answer: cfg.llm.act_and_answer,
        max_echo_ratio: cfg.memory.max_echo_ratio,
        scope_for,
        remember_residual,
        pinned_prefixes: cfg.memory.pinned_prefixes.clone(),
        pinned_max: cfg.memory.pinned_max,
        relevant_max: cfg.memory.relevant_max,
        activation_weight: cfg.memory.activation_weight,
        activation_half_life_days: cfg.memory.activation_half_life_days,
        obligations_max: cfg.memory.obligations_max,
        obligation_check: cfg.memory.obligation_check,
        guidance_max: cfg.memory.guidance_max,
        // M12 T3.2: the engine has no model id of its own, so the resolved
        // emitter target is what names the model notes are kept for.
        archive_foreign_notes: cfg.memory.archive_foreign_notes,
        learning_model: Some(emitter_target.model.clone()),
        // M9 T0.4 is an evaluation knob with no config key: the live harness
        // never ablates a block.
        ablate: None,
        summary_every_turns: cfg.memory.summary_every_turns,
        summary_rebuild_every: cfg.memory.summary_rebuild_every,
        summary_max_chars: cfg.memory.summary_max_chars,
        summary_input_max_chars: cfg.memory.summary_input_max_chars,
        recall_top_k: cfg.memory.recall_top_k,
        trace_verbatim_lines: cfg.memory.trace_verbatim_lines,
        tool_result_max_chars: cfg.memory.tool_result_max_chars,
        recall_sessions: cfg.memory.recall_sessions,
        schema_profile,
        capability,
        prune_inapplicable: true,
        router: cfg
            .router
            .enabled
            .then(|| Arc::new(cfg.router.router()) as Arc<dyn nsengine::router::Router>),
        prompt_budget_tokens: cfg.memory.prompt_budget_tokens,
        budget_mode,
        show_budget_line: cfg.memory.show_budget_line,
        worker_slots,
        // M8 T3.2/T3.3 and M10 T3.6. Both off by default, and both inert
        // without `[models] enabled` — the store gets no encoder, so the
        // hybrid path *is* the lexical path and there are no digest vectors
        // to be near.
        recall_hybrid: cfg.recall.hybrid,
        exemplars_max: cfg.memory.exemplars_max,
        // M12 T6.1: no ceiling unless this run asked for one.
        max_requests,
    };
    let engine = Arc::new(Engine::new(parts, engine_cfg));
    println!(
        "ns-harness — {}  |  {}",
        emitter_target.describe(),
        replier_target.describe()
    );
    if !cfg.engine.confirm_irreversible {
        // Said out loud because it is the one thing a glance at the process
        // cannot tell you, and because the gate it names is the one that would
        // otherwise have asked before anything irreversible happened.
        eprintln!(
            "ns-harness: AUTONOMOUS — irreversible actions run without asking. \
             Clicks and typing on the desktop happen unattended; the brakes left \
             are on the machine itself (touch its mouse or keyboard to suspend \
             input for 5s, or use the badge's pie menu)."
        );
    }
    match serve_addr {
        Some(addr) => println!(
            "serving on {addr} — one session per connection, facts scoped per session  ·  \
             `ns-app providers` lists the backends"
        ),
        None => println!("type text, /quit to exit  ·  `ns-app providers` lists the backends"),
    }
    Ok(engine)
}

pub(crate) async fn build_tools(
    cfg: &AppConfig,
    profile: nscore::SchemaProfile,
) -> Result<Vec<Arc<dyn Tool>>, StartupError> {
    let mut tools: Vec<Arc<dyn Tool>> =
        vec![Arc::new(nscomponents_std::time_tool::GetTimeTool::new())];
    let tool_transport = Arc::new(nscomponents_std::transport::ReqwestToolTransport::new());
    for hc in &cfg.http_components {
        tools.push(Arc::new(nscomponents_std::http_tool::HttpTool::new(
            hc.clone(),
            tool_transport.clone(),
        )));
    }
    if let Some(target) = cfg.pointer_target(env_override("NS_POINTER_ADDR")) {
        // A configured desktop with no token is a config error, like a role
        // with no key: refuse rather than run without the thing that was
        // asked for. An unreachable one is a warning: the machine being off
        // must not take the chat down with it, but it must be said.
        let Some(token) = target.token() else {
            return Err(StartupError::MissingPointerToken {
                env: target.token_env.clone(),
                addr: target.addr.clone(),
            });
        };
        match connect_pointer(&target.addr, &token, profile).await {
            Ok(more) => tools.extend(more),
            Err(e) => eprintln!("pointer: {e}\npointer: the desktop actions are not registered."),
        }
    }
    Ok(tools)
}

/// Dial the agent's messages service, so the person at that machine can talk
/// back mid-task.
///
/// `None` on every failure, and each one says why: the compose box is an
/// addition to the conversation and must never be the reason there is no
/// conversation. An agent built before the service existed simply refuses the
/// connection, and that is worth one line, not an exit.
async fn desktop_messages(cfg: &AppConfig) -> Option<nspointer::messages::Messages> {
    let target = cfg.pointer_target(env_override("NS_POINTER_ADDR"))?;
    let addr = target.messages_target()?;
    let token = target.token()?;
    match nspointer::messages::Messages::connect(&addr, &token).await {
        Ok(m) => {
            println!(
                "desktop: reading the compose box on {addr} — press ctrl+shift+T on that \
                 machine to type a line into this conversation."
            );
            Some(m)
        }
        Err(e) => {
            eprintln!("desktop: no messages service on {addr} ({e});");
            eprintln!("desktop: the compose box will not reach this session.");
            None
        }
    }
}

/// Dial the ns-pointer agent and turn the connection into harness actions.
///
/// The same hop `ns-pointer-mcp` makes, minus the MCP layer: the engine's
/// own gates do what that layer's `confirm` argument approximates. What the
/// agent said in `ready` is printed here, since the engine has no
/// `initialize` to carry it, and a session that starts not armed or with no
/// local brake is something the person at this end should know before the
/// emitter's first click.
async fn connect_pointer(
    addr: &str,
    token: &str,
    profile: nscore::SchemaProfile,
) -> Result<Vec<Arc<dyn Tool>>, String> {
    use nspointer::client::RemotePointer;
    let dial = tokio::net::TcpStream::connect(addr);
    let stream = tokio::time::timeout(std::time::Duration::from_secs(5), dial)
        .await
        .map_err(|_| format!("no answer from the agent at {addr} within 5s"))?
        .map_err(|e| format!("cannot reach the agent at {addr}: {e}"))?;
    let _ = stream.set_nodelay(true);
    let (r, w) = stream.into_split();
    let pointer = RemotePointer::connect(tokio::io::BufReader::new(r), w, token)
        .await
        .map_err(|e| format!("agent at {addr} refused the connection: {e}"))?;
    if !pointer.local_override() {
        eprintln!(
            "pointer: warning — the agent at {addr} has no local override; nobody at that \
             machine can interrupt input sent from here by touching the mouse."
        );
    }
    if pointer.armed() == Some(false) {
        eprintln!(
            "pointer: note — the agent at {addr} is not armed; the first click, key or text \
             will be refused with needs_confirmation until someone presses the arming chord \
             on the machine."
        );
    }
    let shared: Arc<dyn nspointer::Pointer> = Arc::new(pointer);
    let tools = nscomponents_std::pointer_tool::tools(shared, profile)
        .await
        .map_err(|e| format!("could not read the screen layout from {addr}: {e}"))?;
    eprintln!("pointer: {} desktop actions on {addr}", tools.len());
    Ok(tools)
}

/// Resolve a role, or refuse: an unknown provider or a missing model must
/// not fall back silently to someone else's endpoint.
pub(crate) fn role(cfg: &AppConfig, role: Role) -> Result<RoleTarget, StartupError> {
    cfg.llm.role(role).map_err(StartupError::Config)
}

/// Resolve one role's request shape, or refuse: an unparseable `reasoning`
/// or `sampling` must not be dropped silently — a shape that did not take
/// looks exactly like a model that ignores the knob (M11 T0.2/T0.3).
///
/// The coercion line prints here, once per role at startup, so a request
/// that quietly lost its `temperature` is never a mystery in a later 400.
fn shape(
    cfg: &AppConfig,
    target: &RoleTarget,
    base: nsllm::provider::RequestShape,
) -> Result<nsllm::provider::RequestShape, StartupError> {
    match cfg.llm.shape(target.role, &target.model, base) {
        Ok((shape, note)) => {
            if let Some(line) = note {
                println!("{line}");
            }
            Ok(shape)
        }
        Err(e) => Err(StartupError::Config(e)),
    }
}

fn key(target: &RoleTarget) -> Result<String, StartupError> {
    match target.key() {
        Some(k) => Ok(k),
        None => Err(StartupError::MissingApiKey {
            env: target.api_key_env.clone(),
            role: target.role.as_str().to_string(),
        }),
    }
}

/// Load learned.toml (fatal when unparsable: a bad rule set must not be
/// silently ignored — the pass would then propose against the wrong base).
pub(crate) fn load_rules(cfg: &AppConfig) -> Result<RulesHandle, StartupError> {
    match nsevolution::files::load_rules(std::path::Path::new(&cfg.evolution.learned_path)) {
        Ok(r) => Ok(Arc::new(nsengine::arc_swap::ArcSwap::from_pointee(r))),
        Err(e) => Err(StartupError::Rules {
            path: cfg.evolution.learned_path.clone(),
            detail: e.to_string(),
        }),
    }
}

/// Whether driver B — the idle evolution pass — may be installed.
///
/// It may not on a metered run. `--max-requests` is enforced inside the turn
/// loop (`EngineConfig::max_requests`), and the idle pass runs *between*
/// turns, on its own timer, spending real requests that the cap never sees.
/// A run told to spend at most N would quietly spend more than N, and the
/// whole point of the flag is that the number it prints is the number.
fn idle_pass_allowed(max_requests: Option<u32>, enabled: bool) -> bool {
    enabled && max_requests.is_none()
}

pub(crate) fn build_pass(
    cfg: &AppConfig,
    rules: RulesHandle,
    tools: &[Arc<dyn Tool>],
    emitter: &RoleTarget,
    dry_run: bool,
    spend: bool,
) -> nsevolution::pass::EvolutionPass {
    let specs: Vec<nscore::ActionSpec> = tools.iter().map(|t| t.spec().clone()).collect();
    let mut pass_cfg = cfg.evolution.pass_config(dry_run, &cfg.memory, &cfg.models);
    // M12 T0.2: the flag the pass consults before it enters a paid lane.
    pass_cfg.spend = spend;
    let evaluate_cfg = pass_cfg.evaluate.clone();
    let mut pass = nsevolution::pass::EvolutionPass::new(
        rules,
        specs.clone(),
        std::path::PathBuf::from(&cfg.evolution.learned_path),
        std::path::PathBuf::from(&cfg.evolution.ledger_path),
        pass_cfg,
    );
    // M10 T5.3: the local scorer joins the always-present symbolic one when
    // `[models] enabled`, and only then. It needs no key — that is the whole
    // point of it — so it is added before the notes lane's key check, and a
    // pass with no API key still grades with it.
    //
    // Nothing here checks whether the service is up. It should not: the lane
    // disables itself after two unreachable calls and reports every signal
    // `Unavailable`, so a service that is down costs two timeouts and prints
    // `unavailable (service down)` beside its κ. A reachability probe at
    // startup would only be a third way to learn the same thing, one pass
    // earlier.
    if cfg.models.enabled {
        pass = pass.with_evaluator(std::sync::Arc::new(
            nsevolution::local::LocalEvaluator::new(
                nsevolution::local::LocalConfig {
                    base_url: cfg.models.base_url.clone(),
                    timeout_ms: cfg.models.timeout_ms,
                    reask_cosine: cfg.models.reask_cosine,
                    relevance_cut: cfg.models.relevance_cut,
                    embed_model: cfg.recall.embed_model.clone(),
                },
                // The local scorer keeps the structural half of the symbolic
                // checks rather than re-deriving it: I6 is two logged facts
                // and grounding is span attribution, and an embedding
                // improves on neither.
                nsevolution::evaluate::SymbolicEvaluator {
                    cfg: evaluate_cfg.clone(),
                },
            ),
        ));
    }
    match emitter.key() {
        None => {
            eprintln!(
                "{} is not set — notes lane skipped (symbolic lane needs no key).",
                emitter.api_key_env
            );
            pass
        }
        Some(key) => {
            let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
            let model = emitter.model.clone();
            // M12 T0.3: one sink per paid lane. The judge counts its own
            // grades, but nothing counted the notes lane, which is where a
            // pass actually spends — so each lane's client records into a
            // sink of its own and the report sums them per run.
            let judge_sink = Arc::new(nscore::UsageSink::new());
            let proposer_sink = Arc::new(nscore::UsageSink::new());
            let probe_sink = Arc::new(nscore::UsageSink::new());
            let mut lane_sinks: Vec<(String, Arc<nscore::UsageSink>)> = Vec::new();
            // M11 T1.3: the paid judge, and only when `[models] judge_model`
            // names one. `for_model` is the gate — `None` in, `None` out —
            // so an unset id cannot reach a request, and `pass` is handed
            // back unchanged. It rides the emitter's endpoint, key and
            // throttle because it is the same provider account; what makes
            // it not a role is that it is added here, to the idle pass, and
            // nowhere a turn can see it.
            let mut pass = pass;
            if let Some(judge) = nsevolution::client_eval::ClientEvaluator::for_model(
                cfg.models.judge_model.as_deref(),
                crate::client_for(emitter, transport.clone(), &key)
                    .with_usage_sink(judge_sink.clone(), "judge"),
                |c| nsevolution::client_eval::JudgeConfig {
                    // Sonnet 5's shape, and harmless on anything else: no
                    // sampling key at all, one short reasoning block, a
                    // 1,024-token cap on a two-field answer.
                    unsampled: true,
                    structured_output: nsllm::provider::for_base_url(emitter.base_url_or_default())
                        .is_some_and(|p| p.structured_output),
                    ..c
                },
                nsevolution::evaluate::SymbolicEvaluator { cfg: evaluate_cfg },
            ) {
                eprintln!(
                    "judge: {} grades up to {} turns per idle pass, κ-gated at {:.2}.",
                    cfg.models.judge_model.as_deref().unwrap_or_default(),
                    cfg.models.evaluate_budget_turns,
                    cfg.models.evaluator_min_kappa
                );
                pass = pass.with_evaluator(std::sync::Arc::new(judge));
                lane_sinks.push(("judge".into(), judge_sink));
            }
            // The probe builds a fresh emitter per run, on the same target.
            // Each of those clients records into the one probe sink, so a
            // lane that builds a client per probed session is still one
            // number in the report.
            let factory_target = emitter.clone();
            let factory_transport = transport.clone();
            let factory_key = key.clone();
            let factory_model = model.clone();
            let factory_sink = probe_sink.clone();
            let emitter_factory: nsevolution::notes::EmitterFactory = Arc::new(move || {
                let c = crate::client_for(&factory_target, factory_transport.clone(), &factory_key)
                    .with_usage_sink(factory_sink.clone(), "probe");
                Box::new(nsllm::emitter::CloudEmitter::new(c, factory_model.clone()))
                    as Box<dyn nscore::Emitter>
            });
            let probe = nsevolution::notes::LiveProbe {
                emitter: emitter_factory,
                known_specs: specs,
                persona: cfg.persona.text.clone(),
            };
            let proposer = nsevolution::notes::ClientNoteProposer {
                client: crate::client_for(emitter, transport, &key)
                    .with_usage_sink(proposer_sink.clone(), "notes-proposer"),
                model,
            };
            lane_sinks.push(("proposer".into(), proposer_sink));
            lane_sinks.push(("probes".into(), probe_sink));
            pass.with_notes(Box::new(probe), Box::new(proposer))
                .with_lane_sinks(lane_sinks)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cap the idle pass never sees is not a cap. Driver B runs between
    /// turns on its own timer and spends real requests, so a metered run
    /// turns it off entirely rather than hoping the quiet never comes.
    #[test]
    fn a_metered_run_installs_no_idle_evolution_pass() {
        assert!(idle_pass_allowed(None, true), "the unmetered run keeps it");
        assert!(!idle_pass_allowed(Some(60), true));
        assert!(!idle_pass_allowed(Some(0), true));
        // And evolution being off still wins, metered or not.
        assert!(!idle_pass_allowed(None, false));
        assert!(!idle_pass_allowed(Some(60), false));
    }

    /// The whole client hop, against an agent that records and touches
    /// nothing: the ten desktop actions arrive, and a bad token is refused
    /// with the agent's reason rather than a hang.
    #[tokio::test]
    async fn a_configured_pointer_agent_becomes_ten_harness_actions() {
        use nspointer::agent::{bind, serve_listener, Agent, AgentConfig, Limits, Listen};
        use nspointer::platform::NullPlatform;
        use nspointer::{Rect, Screen, ScreenId, Screens};
        let platform = NullPlatform {
            screens: Some(Screens {
                screens: vec![Screen {
                    id: ScreenId::from("S1"),
                    bounds: Rect {
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                    },
                    scale: 1.0,
                    primary: true,
                    label: "main".into(),
                }],
                state: 1,
            }),
            ..Default::default()
        };
        let cfg = AgentConfig {
            token: "t0k".into(),
            limits: Limits::default(),
        };
        let listener = bind(&cfg, &Listen::loopback(0)).await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let agent = Arc::new(Agent::new(platform, cfg));
        tokio::spawn(async move {
            let _ = serve_listener(agent, listener).await;
        });

        let tools = connect_pointer(&addr, "t0k", nscore::SchemaProfile::Full)
            .await
            .unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.spec().name.as_str()).collect();
        assert_eq!(names.len(), 10, "{names:?}");
        assert!(names.contains(&"pointer_click") && names.contains(&"pointer_ui_read"));

        let err = match connect_pointer(&addr, "wrong", nscore::SchemaProfile::Full).await {
            Err(e) => e,
            Ok(_) => panic!("a wrong token must be refused"),
        };
        assert!(err.contains("refused"), "{err}");
    }
}
