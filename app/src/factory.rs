//! One tenant's config in, one `Engine` out.
//!
//! Multi-tenant plan D2: everything that varies per company is already a
//! field of `EngineConfig` or a slot of `HarnessParts`, so the factory is
//! not a design — it is the assembly that used to be inlined in `main`,
//! lifted out unchanged so a second caller can run it. The only difference
//! is the failure mode: where `main` could exit the process on a bad
//! config, the factory returns a `StartupError`, because a shard hosting
//! nineteen good tenants must not die of the twentieth's typo.

use crate::config::{AppConfig, AuthMode, Role, RoleTarget, ServeSection};
use crate::env_override;
use crate::tenant::TenantConfig;
use nscore::{Channel, HarnessBuilder, SessionId, Tool};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use nsidentity::{Hello, Hs256Verifier, IdentityResolver, SharedTokenResolver, TenantAuth};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub(crate) type RulesHandle = Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>>;

/// What the engine is being built for. Today's `serve: bool`, named: the
/// three places it is consulted are the channel, the fact scope and the
/// closing banner.
pub(crate) enum Mode {
    /// The interactive CLI: stdin plus the desktop's compose box, one
    /// `global` fact scope, the session id this run was given.
    Cli { session: String },
    /// `ns-app serve`: the caller's already-bound channel and the address it
    /// landed on, a fact scope per session. The listener belongs to the
    /// caller because one socket feeds every company in the process, and a
    /// factory that bound one for itself could only ever build one engine
    /// (plan A1).
    Serve {
        channel: Arc<dyn Channel>,
        addr: SocketAddr,
    },
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
    /// Plan A6: `auth = "jwt"` with nothing to verify against — either
    /// `[auth] signing_key_envs` names no variable, or one it names is not
    /// exported. Refused by name rather than started with an empty key
    /// table, which would refuse every client instead.
    MissingSigningKeys {
        tenant: String,
        env: String,
    },
    /// Plan A6: the shared token proves the caller read an env var and
    /// nothing else, so it is loopback only. The channel refuses this at
    /// bind; saying it here names the config key rather than a socket.
    SharedAuthOffLoopback {
        listen: String,
    },
    /// Plan B9: one shared token cannot tell two companies apart, so a set
    /// of more than one tenant under `auth = "shared"` could only ever route
    /// every client to whichever company the token was said to stand for.
    /// Refused at startup rather than served as one company wearing
    /// everybody's name.
    SharedAuthManyTenants {
        tenants: Vec<String>,
    },
    /// Plan B9: a message naming a company this shard does not host. The
    /// identity layer refuses an unknown tenant before a queue can exist, so
    /// this is the second answer to the same question: a name with no config
    /// behind it is refused, never built from the base config.
    UnknownTenant {
        tenant: String,
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
            Self::MissingSigningKeys { tenant, env } => write!(
                f,
                "tenant {tenant:?}: {env} is not set — [serve] auth = \"jwt\" verifies every \
                 token against the keys [auth] signing_key_envs names."
            ),
            Self::SharedAuthOffLoopback { listen } => write!(
                f,
                "refusing to serve on {listen:?} with [serve] auth = \"shared\": one token for \
                 everyone proves nothing, so it is loopback only — set auth = \"jwt\" to serve \
                 an address other machines can reach."
            ),
            Self::SharedAuthManyTenants { tenants } => write!(
                f,
                "{} tenants are configured ({}) with [serve] auth = \"shared\": one token for \
                 everyone cannot tell them apart, so every client would arrive as the same \
                 company - set auth = \"jwt\" to serve more than one.",
                tenants.len(),
                tenants.join(", ")
            ),
            Self::UnknownTenant { tenant } => write!(
                f,
                "tenant {tenant:?} is not configured in {}/: a message for a company this shard \
                 does not host is refused, not built from the base config.",
                crate::tenant::TENANT_DIR
            ),
            Self::Serve(e) => write!(f, "serve: {e}"),
            Self::Harness(e) => write!(f, "ns-harness: cannot assemble the harness: {e}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// The resolver the listener will decide identity with, built from one
/// tenant's `[serve] auth` and `[auth]` (plan A6).
///
/// Shared is today: one token from `token_env`, every client it, and a
/// session id the client chooses. JWT is a key table — current key first,
/// the one it replaced second — and a session id derived from the claims.
/// Either way this is the whole difference between the two modes; the
/// listener itself has one code path.
pub(crate) fn serve_resolver(
    tenant: &TenantConfig,
) -> Result<Arc<dyn IdentityResolver<Hello>>, StartupError> {
    let cfg = &tenant.app;
    match cfg.serve.auth {
        AuthMode::Shared => {
            let Some(token) = cfg.serve.token() else {
                return Err(StartupError::MissingServeToken {
                    env: cfg.serve.token_env.clone(),
                });
            };
            // `SHARED_TENANT` in the channel is the CLI's `local`, which is
            // this tenant's id in exactly the case shared auth is for.
            Ok(Arc::new(SharedTokenResolver::new(token, tenant.id.clone())))
        }
        AuthMode::Jwt => {
            let table = HashMap::from([(tenant.id.clone(), tenant_auth(tenant)?)]);
            Ok(Arc::new(Hs256Verifier::new(table)))
        }
    }
}

/// One company's signing material, or the refusal that names what is
/// missing. Split out of [`serve_resolver`] so the shard's table (many
/// companies, one verifier) is built from the same reading of `[auth]` as a
/// single company's.
pub(crate) fn tenant_auth(tenant: &TenantConfig) -> Result<TenantAuth, StartupError> {
    let cfg = &tenant.app;
    if cfg.auth.signing_key_envs.is_empty() {
        return Err(StartupError::MissingSigningKeys {
            tenant: tenant.id.clone(),
            env: "[auth] signing_key_envs".to_string(),
        });
    }
    let keys = cfg
        .auth
        .signing_keys()
        .map_err(|env| StartupError::MissingSigningKeys {
            tenant: tenant.id.clone(),
            env,
        })?;
    let mut keys = keys.into_iter();
    Ok(TenantAuth {
        current: keys.next().expect("signing_key_envs is not empty"),
        previous: keys.next(),
        iat_floor: cfg.auth.iat_floor,
    })
}

/// The platforms this shard answers webhooks for, built out of the set.
///
/// The division is the same one `[serve]` and `[auth]` already draw, and it
/// is worth stating because it is the whole reason a hundred companies can
/// share one endpoint: the *endpoint* is the process's — one URL, one
/// verification token, one file of already-answered message ids — while the
/// *account* is the company's. A delivery is routed to a company by the
/// business number inside its own signed payload, so two companies on one
/// URL are separated by something neither of them can write.
///
/// A company with no `[whatsapp]` section is simply not reachable that way;
/// a company with one whose secrets are not exported is a named refusal
/// rather than an account that would fail every signature at run time.
pub(crate) fn shard_platforms(
    set: &[TenantConfig],
    http: &crate::config::HttpSection,
) -> Result<Vec<Arc<nschannel_http::PlatformEndpoint>>, StartupError> {
    let mut accounts = Vec::new();
    for tenant in set {
        let Some(cfg) = &tenant.app.whatsapp else {
            continue;
        };
        let secret = |env: &str| -> Result<String, StartupError> {
            std::env::var(env)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    StartupError::Config(format!(
                        "tenant {:?} configures [whatsapp] but {env} is unset: an account whose \
                         secrets are missing would refuse every delivery Meta sent it",
                        tenant.id
                    ))
                })
        };
        accounts.push(nschannel_http::whatsapp::Account {
            phone_number_id: cfg.phone_number_id.clone(),
            tenant: tenant.id.clone(),
            access_token: secret(&cfg.access_token_env)?,
            app_secret: secret(&cfg.app_secret_env)?,
            session_salt: secret(&cfg.session_salt_env)?,
        });
    }
    if accounts.is_empty() {
        return Ok(Vec::new());
    }
    // Two companies on one business number would each receive the other's
    // customers. Refused here, with both named, for the same reason two
    // companies on one store path are (plan H9).
    for (i, account) in accounts.iter().enumerate() {
        if let Some(other) = accounts[..i]
            .iter()
            .find(|a| a.phone_number_id == account.phone_number_id)
        {
            return Err(StartupError::Config(format!(
                "tenants {:?} and {:?} both claim WhatsApp number {}: a delivery for it could \
                 only be given to one of them",
                other.tenant, account.tenant, account.phone_number_id
            )));
        }
    }
    let graph_version = set
        .iter()
        .find_map(|t| t.app.whatsapp.as_ref().map(|w| w.graph_version.clone()))
        .unwrap_or_else(crate::config::default_graph_version);
    let verify_env = http.whatsapp_verify_token_env.trim();
    if verify_env.is_empty() {
        return Err(StartupError::Config(
            "a [whatsapp] account is configured but [http] whatsapp_verify_token_env names no \
             variable: Meta will not deliver to an endpoint that cannot answer its verification"
                .into(),
        ));
    }
    let verify_token = std::env::var(verify_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            StartupError::Config(format!(
                "[http] whatsapp_verify_token_env names {verify_env}, which is unset"
            ))
        })?;
    let adapter = nschannel_http::whatsapp::WhatsApp::new(accounts, verify_token)
        .with_graph_version(graph_version);
    let seen = match http.seen_path.trim() {
        // Deliberately in memory: the operator has said they would rather
        // answer the occasional redelivery twice than keep the file.
        "" => nschannel_http::SeenIds::in_memory(),
        path => nschannel_http::SeenIds::open(path),
    };
    Ok(vec![nschannel_http::PlatformEndpoint::new(
        Arc::new(adapter),
        Arc::new(nschannel_http::platform::ReqwestReplyTransport::new()),
        seen,
    )])
}

/// The resolver for a whole shard: one listener, every company in the set
/// (plan B9).
///
/// `[serve]` is process-owned (`tenant::PROCESS_OWNED`), so the mode is read
/// off any member and is the same for all of them; `[auth]` is deliberately
/// per company, so the key table has a row each and a token minted by one
/// company reaches only its own engine. Shared auth is the exception and it
/// is a refusal: one token names one company, so it can serve a set of one
/// and nothing larger.
pub(crate) fn shard_resolver(
    set: &[TenantConfig],
) -> Result<Arc<dyn IdentityResolver<Hello>>, StartupError> {
    let Some(first) = set.first() else {
        // `main` refuses an empty set before it reaches this.
        return Err(StartupError::Config("no tenant to serve".into()));
    };
    match first.app.serve.auth {
        AuthMode::Shared if set.len() > 1 => Err(StartupError::SharedAuthManyTenants {
            tenants: set.iter().map(|t| t.id.clone()).collect(),
        }),
        AuthMode::Shared => serve_resolver(first),
        AuthMode::Jwt => {
            let mut table = HashMap::new();
            for tenant in set {
                table.insert(tenant.id.clone(), tenant_auth(tenant)?);
            }
            Ok(Arc::new(Hs256Verifier::new(table)))
        }
    }
}

/// Plan A6: shared auth on an address other machines can reach, refused
/// before the port is taken.
///
/// The channel refuses the same thing at bind
/// (`BindError::SharedAuthOffLoopback`); doing it here means the operator is
/// told which config key to change rather than which socket failed, and the
/// port is still free when they are told. `allow_remote` does not buy it:
/// that knob means "I meant this address", not "one token is enough".
pub(crate) async fn check_serve_address(cfg: &ServeSection) -> Result<(), StartupError> {
    if cfg.auth != AuthMode::Shared {
        return Ok(());
    }
    // A name that will not resolve is the listener's own error to report,
    // with the words it has always used.
    let Ok(addrs) = tokio::net::lookup_host(&cfg.listen).await else {
        return Ok(());
    };
    for addr in addrs {
        if !addr.ip().is_loopback() {
            return Err(StartupError::SharedAuthOffLoopback {
                listen: cfg.listen.clone(),
            });
        }
    }
    Ok(())
}

/// One tenant, built: the engine and the two things running it takes.
///
/// `Engine::run` consumes the engine, and `config`/`parts` are the engine's
/// own, so an `Arc<Engine>` can be neither run nor asked for its channel or
/// its slot count. A registry holding one engine per company needs all
/// three, and the factory is where all three are already in hand, so it
/// hands them back rather than the engine exporting its internals for two
/// scalars (plan B2). The caller builds `Dispatcher::new(engine, channel,
/// worker_slots)`, which is what `Engine::run` built for itself.
pub(crate) struct BuiltTenant {
    pub engine: Arc<Engine>,
    pub channel: Arc<dyn Channel>,
    pub worker_slots: usize,
}

/// One tenant's config and one mode in, one running-ready `Engine` out.
///
/// The order of construction is load-bearing and is the order `main` used:
/// every semantic check the chat path needs happens before anything is
/// dialled, and the three startup banners print between `Engine::new` and
/// the first turn. The listener is no longer among them: `serve` hands one
/// in already bound (plan A1).
pub(crate) async fn build_engine(
    tenant: &TenantConfig,
    mode: Mode,
    max_requests: Option<u32>,
) -> Result<BuiltTenant, StartupError> {
    // The tenant's resolved config — the shared `config.toml` with this
    // company's overlay already laid over it (plan T1.2). Everything below
    // reads it exactly as it read the process-wide config before.
    let cfg = &tenant.app;
    // Both were resolved in `main` before the subcommands, which is where
    // they are still reported from; re-resolving here is pure, and keeps
    // the factory a function of the tenant's config alone.
    let schema_profile = cfg.llm.schema_profile().map_err(StartupError::Llm)?;

    let (tcp, cli_session) = match mode {
        Mode::Cli { session } => (None, session),
        Mode::Serve { channel, addr } => (Some((channel, addr)), String::new()),
    };
    let serve = tcp.is_some();
    // Plan B7: which company's file this engine's clients trace into. The
    // terminal is `None` — one tenant on one box, where NS_TRACE names the
    // file itself and always has.
    let trace_tenant: Option<&str> = serve.then_some(tenant.id.as_str());

    // One model writes the reply and chooses the actions, so there is one
    // target to resolve. The summarizer still resolves on its own, and can
    // still sit on a different model from the turn loop.
    let emitter_target = role(cfg, Role::Emitter)?;
    let emitter_key = key(&emitter_target)?;

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
    // Which knob this is depends on the mode: the terminal is one person
    // typing and stays serial, serving overlaps the waiting of several
    // conversations (plan B6).
    let worker_slots = cfg.engine.slots_for(serve).map_err(StartupError::Config)?;

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
            crate::client_for(
                &emitter_target,
                transport.clone(),
                &emitter_key,
                trace_tenant,
            )
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
        .with_prompt_cache(emitter_target.prompt_cache && cfg.llm.prompt_cache_emitter),
    ));
    b.set_memory(Arc::new(crate::models::store(cfg)));
    // Says whether the local model service is answering, when one is asked
    // for. Before the channel so the line lands with the other startup
    // reports rather than in the middle of the first turn.
    crate::models::announce(&cfg.models).await;
    let serve_addr = tcp.as_ref().map(|(_, addr)| *addr);
    // Kept, not just handed over: the dispatcher the caller builds reads the
    // same channel the engine was built on, and the engine will not give it
    // back (plan B2).
    let channel: Arc<dyn Channel> = match tcp {
        // `serve`: the caller's channel and nothing else — no stdin, and no
        // compose box, which joins a desktop to *one* session.
        Some((channel, _)) => channel,
        None => {
            // stdin, plus the desktop's compose box when there is one to
            // read. The agent has offered that channel since 2026-09-05 and
            // nothing collected it; a line typed into the badge went into
            // the outbox and stopped there.
            let cli = nschannel_cli::CliChannel::new_stdio().with_session(cli_session.clone());
            match desktop_messages(cfg).await {
                Some(client) => Arc::new(nscomponents_std::desktop_channel::WithDesktop::spawn(
                    cli, client,
                )),
                None => Arc::new(cli),
            }
        }
    };
    b.set_shared_channel(channel.clone());
    // M6 §5.1: the rolling summary runs on its own role (model, provider,
    // key), so it can be swapped without touching the emitter or replier.
    if cfg.memory.summary_every_turns > 0 {
        let target = role(cfg, Role::Summarizer)?;
        match target.key() {
            Some(role_key) => {
                let c = crate::client_for(&target, transport.clone(), &role_key, trace_tenant)
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
            trace_tenant,
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
        // A flagged draft is logged and stands as written. This was the
        // `Strong` half of a capability knob that no longer exists
        // (2026-09-14): every deployment is now the strong one, and the
        // second call the weak setting bought was a rewrite by the same
        // model that had just been told what it got wrong.
        reply_regenerate: false,

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
    println!("ns-harness — {}", emitter_target.describe());
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
    Ok(BuiltTenant {
        engine,
        channel,
        worker_slots,
    })
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
    // Plan B7: the company whose trace file the pass's own paid lanes append
    // to. `None` is the terminal and `ns-app evolve`, which are one tenant.
    trace_tenant: Option<&str>,
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
                crate::client_for(emitter, transport.clone(), &key, trace_tenant)
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
            // The factory outlives this call, so the tenant it traces under
            // is owned rather than borrowed from the config.
            let factory_trace = trace_tenant.map(str::to_string);
            let emitter_factory: nsevolution::notes::EmitterFactory = Arc::new(move || {
                let c = crate::client_for(
                    &factory_target,
                    factory_transport.clone(),
                    &factory_key,
                    factory_trace.as_deref(),
                )
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
                client: crate::client_for(emitter, transport, &key, trace_tenant)
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

    /// A fixture tenant set whose base config reaches nothing: the local
    /// provider resolves a key without one being exported, and every path it
    /// writes to lives under `root`. `listen` is the address the caller has
    /// already bound, which is the whole point of the serve tests below.
    fn serve_fixture(
        root: &std::path::Path,
        listen: SocketAddr,
        ids: &[&str],
    ) -> Vec<TenantConfig> {
        std::fs::create_dir_all(root.join(crate::tenant::TENANT_DIR)).expect("tenants dir");
        let base = format!(
            "[llm]\nprovider = \"ollama\"\n\
             [serve]\nlisten = {:?}\n\
             [evolution]\nlearned_path = {:?}\n",
            listen.to_string(),
            root.join("learned.toml").display().to_string()
        );
        for id in ids {
            // A store, a learned file and a ledger of this tenant's own:
            // guard G1 refuses a set that shares any of the three, and the
            // base names one of each.
            std::fs::write(
                root.join(crate::tenant::TENANT_DIR)
                    .join(format!("{id}.toml")),
                format!(
                    "[store]\npath = {:?}\n\
                     [evolution]\nlearned_path = {:?}\nledger_path = {:?}\n",
                    root.join(format!("ns-{id}.sqlite")).display().to_string(),
                    root.join(format!("learned-{id}.toml"))
                        .display()
                        .to_string(),
                    root.join(format!("ledger-{id}.json")).display().to_string()
                ),
            )
            .expect("overlay");
        }
        crate::tenant::load_set(&base, root, "local").expect("the fixture tenants")
    }

    /// A tenant set where each named company has a WhatsApp account on the
    /// number given, with its secrets in env vars named after it.
    /// Every environment variable is named for `scope` as well as for the
    /// company, because the environment is the process's and these tests run
    /// beside each other: one that unsets a variable to prove a refusal would
    /// otherwise unset it under another test that is mid-assertion.
    fn whatsapp_fixture(
        root: &std::path::Path,
        scope: &str,
        accounts: &[(&str, &str)],
    ) -> Vec<crate::tenant::TenantConfig> {
        let addr: SocketAddr = "127.0.0.1:7399".parse().expect("a literal address");
        let ids: Vec<&str> = accounts.iter().map(|(id, _)| *id).collect();
        let mut set = serve_fixture(root, addr, &ids);
        for (tenant, (id, number)) in set.iter_mut().zip(accounts) {
            assert_eq!(&tenant.id, id, "the fixture keeps the order it was given");
            let up = format!("{}_{}", scope.to_uppercase(), id.to_uppercase());
            tenant.app.whatsapp = Some(crate::config::WhatsAppSection {
                phone_number_id: (*number).to_string(),
                access_token_env: format!("NS_TEST_WA_TOKEN_{up}"),
                app_secret_env: format!("NS_TEST_WA_SECRET_{up}"),
                session_salt_env: format!("NS_TEST_WA_SALT_{up}"),
                graph_version: "v21.0".into(),
            });
            std::env::set_var(format!("NS_TEST_WA_TOKEN_{up}"), "a-graph-token");
            std::env::set_var(format!("NS_TEST_WA_SECRET_{up}"), "an-app-secret");
            std::env::set_var(format!("NS_TEST_WA_SALT_{up}"), "a-salt");
        }
        set
    }

    fn http_with_verify(env: &str) -> crate::config::HttpSection {
        std::env::set_var(env, "verify-me");
        crate::config::HttpSection {
            listen: "127.0.0.1:0".into(),
            whatsapp_verify_token_env: env.into(),
            // In memory: a test must not write a dedupe file beside the
            // repository, and what the file does is `SeenIds`' own test.
            seen_path: String::new(),
            ..crate::config::HttpSection::default()
        }
    }

    /// A company with no `[whatsapp]` is simply not reachable that way, and
    /// a shard of such companies opens no webhook endpoint at all.
    #[test]
    fn a_set_with_no_platform_account_serves_no_webhooks() {
        let root = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:7399".parse().expect("a literal address");
        let set = serve_fixture(root.path(), addr, &["acme"]);
        let platforms = shard_platforms(&set, &http_with_verify("NS_TEST_WA_VERIFY_NONE"))
            .expect("nothing to configure");
        assert!(platforms.is_empty());
    }

    #[test]
    fn two_tenants_claiming_one_whatsapp_number_are_refused_naming_both() {
        let root = tempfile::tempdir().expect("tempdir");
        // The copy-pasted overlay, which is the likeliest way one company's
        // customers ever reach another's engine.
        let set = whatsapp_fixture(root.path(), "dup", &[("acme", "111"), ("globex", "111")]);
        let err = match shard_platforms(&set, &http_with_verify("NS_TEST_WA_VERIFY_DUP")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two companies on one business number must be refused"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("globex"), "{err}");
        assert!(err.contains("111"), "{err}");

        // Their own numbers, and the same set is fine.
        let set = whatsapp_fixture(root.path(), "dup", &[("acme", "111"), ("globex", "222")]);
        assert_eq!(
            shard_platforms(&set, &http_with_verify("NS_TEST_WA_VERIFY_DUP"))
                .expect("two accounts, one endpoint")
                .len(),
            1,
            "one endpoint serves every company on the platform"
        );
    }

    #[test]
    fn a_whatsapp_account_whose_secret_is_unset_is_refused_by_name() {
        let root = tempfile::tempdir().expect("tempdir");
        let set = whatsapp_fixture(root.path(), "secret", &[("acme", "111")]);
        std::env::remove_var("NS_TEST_WA_SECRET_SECRET_ACME");
        let err = match shard_platforms(&set, &http_with_verify("NS_TEST_WA_VERIFY_SECRET")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an account with no secret would refuse every delivery"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("NS_TEST_WA_SECRET_SECRET_ACME"), "{err}");
    }

    /// Meta will not deliver to an endpoint that cannot answer its
    /// verification handshake, so a shard that could never be subscribed to
    /// says so at startup rather than waiting silently for traffic.
    #[test]
    fn a_platform_with_no_verification_token_is_refused_at_startup() {
        let root = tempfile::tempdir().expect("tempdir");
        let set = whatsapp_fixture(root.path(), "verify", &[("acme", "111")]);
        let mut http = http_with_verify("NS_TEST_WA_VERIFY_MISSING");
        http.whatsapp_verify_token_env = String::new();
        let err = match shard_platforms(&set, &http) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an endpoint Meta cannot verify must be refused"),
        };
        assert!(err.contains("whatsapp_verify_token_env"), "{err}");

        // Named but unset is the same refusal, with the variable in it.
        http.whatsapp_verify_token_env = "NS_TEST_WA_VERIFY_UNSET".into();
        std::env::remove_var("NS_TEST_WA_VERIFY_UNSET");
        let err = match shard_platforms(&set, &http) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unset verification token must be refused"),
        };
        assert!(err.contains("NS_TEST_WA_VERIFY_UNSET"), "{err}");
    }

    /// One company's side of a listener the caller has bound: what a
    /// registry hands each engine, and what `Mode::Serve` takes. The queue
    /// only exists once somebody has spoken for that company, so the
    /// fixture speaks - a hello and one line, exactly as a client would.
    async fn tenant_channel_of(listener: &nschannel_tcp::TcpChannel) -> Arc<dyn Channel> {
        use tokio::io::AsyncWriteExt;
        let mut client = tokio::net::TcpStream::connect(listener.local_addr())
            .await
            .expect("connect to the caller's listener");
        client
            .write_all(b"{\"token\":\"t\",\"session\":\"s\"}\n{\"text\":\"hi\"}\n")
            .await
            .expect("the hello and one line");
        let tenant = listener
            .next_active_tenant()
            .await
            .expect("the wake stream outlives the listener");
        listener
            .tenant_channel(&tenant)
            .expect("a woken company has its receiver parked")
    }

    /// Plan A6. `auth = "jwt"` and nothing to verify against is a process
    /// that could only ever refuse every client, so it is refused at
    /// startup instead — with the tenant and the variable named, in both
    /// shapes the mistake takes: no variable named at all, and one named
    /// that is not exported.
    #[test]
    fn jwt_auth_with_no_signing_keys_is_refused_by_name() {
        let root = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:7375".parse().expect("a literal address");

        // Named nothing.
        let mut set = serve_fixture(root.path(), addr, &["acme"]);
        set[0].app.serve.auth = crate::config::AuthMode::Jwt;
        let err = match serve_resolver(&set[0]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("jwt with no signing keys must be refused"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("signing_key_envs"), "{err}");

        // Named a variable nobody exported. A variable no other test
        // touches, so this cannot race one.
        std::env::remove_var("NS_TEST_A6_SIGNING_KEY");
        set[0].app.auth.signing_key_envs = vec!["NS_TEST_A6_SIGNING_KEY".into()];
        let err = match serve_resolver(&set[0]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a key env that is not set must be refused"),
        };
        assert!(err.contains("acme"), "{err}");
        assert!(err.contains("NS_TEST_A6_SIGNING_KEY"), "{err}");

        // Exported, and the resolver builds.
        std::env::set_var("NS_TEST_A6_SIGNING_KEY", "a signing key");
        serve_resolver(&set[0]).expect("the tenant's verifier");
        std::env::remove_var("NS_TEST_A6_SIGNING_KEY");
    }

    /// Plan B9. One listener decides identity for every company on it, so
    /// the key table has a row per company and each row is read from that
    /// company's own `[auth]`. A set whose second member names a key nobody
    /// exported is refused by that member's name, which is what proves the
    /// table is built from the whole set rather than from its first entry.
    #[test]
    fn the_shard_resolver_reads_every_tenants_keys() {
        let root = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:7379".parse().expect("a literal address");
        let mut set = serve_fixture(root.path(), addr, &["acme", "globex"]);
        for tenant in &mut set {
            tenant.app.serve.auth = crate::config::AuthMode::Jwt;
            tenant.app.auth.signing_key_envs =
                vec![format!("NS_TEST_B9_KEY_{}", tenant.id.to_uppercase())];
        }
        std::env::set_var("NS_TEST_B9_KEY_ACME", "acme's signing key");
        std::env::remove_var("NS_TEST_B9_KEY_GLOBEX");

        let err = match shard_resolver(&set) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a company with no key to verify against must be refused"),
        };
        assert!(err.contains("globex"), "{err}");
        assert!(err.contains("NS_TEST_B9_KEY_GLOBEX"), "{err}");

        std::env::set_var("NS_TEST_B9_KEY_GLOBEX", "globex's signing key");
        shard_resolver(&set).expect("the shard's verifier");
        std::env::remove_var("NS_TEST_B9_KEY_ACME");
        std::env::remove_var("NS_TEST_B9_KEY_GLOBEX");
    }

    /// Plan B9. The shared token is one secret that names one company, so a
    /// shard of several under it would answer every client as whichever
    /// company the token was said to stand for. Refused at startup, with
    /// the companies named, rather than served as one wearing all their
    /// names. One company under shared auth is unchanged.
    #[test]
    fn shared_auth_cannot_serve_more_than_one_tenant() {
        let root = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:7380".parse().expect("a literal address");
        let set = serve_fixture(root.path(), addr, &["acme", "globex"]);
        let err = match shard_resolver(&set) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("one token cannot tell two companies apart"),
        };
        assert!(err.contains("acme") && err.contains("globex"), "{err}");

        // And one company under the same auth still builds its resolver,
        // which is `ns-app serve` on a laptop exactly as it was. Its own
        // root, since a fixture's overlays stay in the directory.
        let alone = tempfile::tempdir().expect("tempdir");
        let mut set = serve_fixture(alone.path(), addr, &["acme"]);
        // A variable no other test touches, so this cannot race one.
        std::env::set_var("NS_TEST_B9_SHARED_TOKEN", "t0k");
        set[0].app.serve.token_env = "NS_TEST_B9_SHARED_TOKEN".into();
        shard_resolver(&set).expect("one company under shared auth");
        std::env::remove_var("NS_TEST_B9_SHARED_TOKEN");
    }

    /// Plan A6. One token for everyone proves the caller read an env var and
    /// nothing else, so it may not be the only thing between the network and
    /// a conversation. The channel refuses it at bind; refusing it here
    /// leaves the port free and names the config key.
    #[tokio::test]
    async fn a_non_loopback_bind_under_shared_auth_is_refused_at_startup() {
        let mut cfg = ServeSection {
            listen: "0.0.0.0:9000".into(),
            ..ServeSection::default()
        };
        let err = match check_serve_address(&cfg).await {
            Err(e) => e.to_string(),
            Ok(()) => panic!("shared auth off loopback must be refused"),
        };
        assert!(err.contains("0.0.0.0:9000"), "{err}");
        assert!(err.contains("shared"), "{err}");

        // `allow_remote` says "I meant this address", not "one token is
        // enough", so it does not buy the refusal off.
        cfg.allow_remote = true;
        assert!(
            check_serve_address(&cfg).await.is_err(),
            "allow_remote does not make a shared token sufficient"
        );

        // Loopback, which is every deployment today, is untouched; and jwt
        // is what an address other machines can reach is for.
        cfg.allow_remote = false;
        cfg.listen = "127.0.0.1:7375".into();
        check_serve_address(&cfg).await.expect("loopback is fine");
        cfg.listen = "0.0.0.0:9000".into();
        cfg.auth = AuthMode::Jwt;
        check_serve_address(&cfg)
            .await
            .expect("jwt proves who the caller is");
    }

    /// The listener is the caller's (plan A1). The factory installs the
    /// channel it was handed rather than opening one of its own, so the
    /// caller's handle is still alive — and shared — once the engine is up.
    #[tokio::test]
    async fn build_engine_takes_its_serve_channel_from_the_caller() {
        let root = tempfile::tempdir().expect("tempdir");
        let listener = nschannel_tcp::TcpChannel::bind_shared("127.0.0.1:0", "t".into(), 4, false)
            .await
            .expect("the caller's listener");
        let addr = listener.local_addr();
        let channel = tenant_channel_of(&listener).await;
        let set = serve_fixture(root.path(), addr, &["acme"]);
        assert_eq!(
            Arc::strong_count(&channel),
            1,
            "only the caller holds it yet"
        );

        let built = build_engine(
            &set[0],
            Mode::Serve {
                channel: channel.clone(),
                addr,
            },
            None,
        )
        .await
        .expect("the tenant's engine");

        assert_eq!(Arc::strong_count(&built.engine), 1);
        assert!(
            Arc::strong_count(&channel) > 1,
            "the engine holds the caller's channel, not one it bound itself"
        );
    }

    /// Plan B2. What a dispatcher needs is an `Arc<Engine>`, the channel
    /// that engine reads and its slot count, and the factory is the one
    /// place all three are already in hand. Handing them back means the
    /// caller drives the engine through a handle it keeps, instead of
    /// taking the engine back out of its `Arc` first - the unwrap that
    /// panicked the moment a second handle existed, which is exactly what
    /// the tenant registry will hold.
    ///
    /// The clone held across the whole run is the proof.
    #[tokio::test]
    async fn the_cli_runs_its_dispatcher_from_an_arc_it_still_holds() {
        // Closed before the first read, so the dispatcher built here ends
        // on its own.
        struct ClosedChannel;
        #[async_trait::async_trait]
        impl Channel for ClosedChannel {
            async fn recv(&self) -> Result<nscore::Incoming, nscore::ChannelError> {
                Err(nscore::ChannelError::Closed)
            }
            async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), nscore::ChannelError> {
                Ok(())
            }
        }

        let root = tempfile::tempdir().expect("tempdir");
        let addr: SocketAddr = "127.0.0.1:9".parse().expect("a fixture address");
        let mut set = serve_fixture(root.path(), addr, &["acme"]);
        // Not either default, so a hard-coded slot count cannot pass for it.
        // Both knobs, because which one a build reads is its mode's (B6) and
        // what is asserted below is that the tenant's own number comes back.
        set[0].app.engine.worker_slots = 3;
        set[0].app.engine.serve_worker_slots = 3;
        let channel: Arc<dyn Channel> = Arc::new(ClosedChannel);

        let built = build_engine(
            &set[0],
            Mode::Serve {
                channel: channel.clone(),
                addr,
            },
            None,
        )
        .await
        .expect("the tenant's engine");

        assert!(
            Arc::ptr_eq(&built.channel, &channel),
            "the dispatcher must read the very channel the engine was built on"
        );
        assert_eq!(built.worker_slots, 3, "this tenant's own slot count");

        // The handle the caller keeps - a registry keeps one per company.
        let held = built.engine.clone();
        nsengine::dispatch::Dispatcher::new(built.engine, built.channel, built.worker_slots)
            .run()
            .await
            .expect("a closed channel ends the run");
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "the run is over and the caller's handle outlived it"
        );
    }

    /// Why the bind moved out: one engine per company, in one process. The
    /// factory binding for itself would reach for the same socket a second
    /// time and fail with `StartupError::Serve`, so the fixture points both
    /// tenants at the address the caller has already taken.
    #[tokio::test]
    async fn build_engine_no_longer_binds_a_socket() {
        let root = tempfile::tempdir().expect("tempdir");
        let listener = nschannel_tcp::TcpChannel::bind_shared("127.0.0.1:0", "t".into(), 4, false)
            .await
            .expect("the caller's listener");
        let addr = listener.local_addr();
        let channel = tenant_channel_of(&listener).await;
        let set = serve_fixture(root.path(), addr, &["acme", "beta"]);
        assert_eq!(set.len(), 2);

        for tenant in &set {
            build_engine(
                tenant,
                Mode::Serve {
                    channel: channel.clone(),
                    addr,
                },
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("{} could not be built: {e}", tenant.id));
        }
    }

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

    /// Two companies, one base config, one process: each engine is built
    /// from its own overlay, so each gets its own persona, its own HTTP tool
    /// and its own store file (plan T1.2, D4). The base's own persona and
    /// store reach neither.
    #[tokio::test]
    async fn factory_builds_two_tenants_with_different_personas_and_tools() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(root.path().join(crate::tenant::TENANT_DIR)).expect("tenants dir");
        let store = |id: &str| root.path().join(format!("ns-{id}.sqlite"));
        // Ollama is the local provider: a key resolves without one being
        // exported, so nothing here reaches for a network or a secret.
        let base = format!(
            "[llm]\nprovider = \"ollama\"\n\
             [persona]\ntext = \"the shared persona\"\n\
             [store]\npath = {:?}\n\
             [evolution]\nlearned_path = {:?}\n",
            store("base").display().to_string(),
            root.path().join("learned.toml").display().to_string()
        );
        for (id, persona, tool) in [
            ("acme", "acme's persona", "acme_orders"),
            ("beta", "beta's persona", "beta_tickets"),
        ] {
            let overlay = format!(
                "[persona]\ntext = {persona:?}\n\
                 [store]\npath = {:?}\n\
                 [evolution]\nlearned_path = {:?}\nledger_path = {:?}\n\
                 [[http_component]]\nname = {tool:?}\n\
                 description = \"one company's own tool\"\n\
                 url = \"https://example.invalid/{tool}\"\n\
                 side_effect = \"Pure\"\n\
                 args_schema = {{ type = \"object\" }}\n",
                store(id).display().to_string(),
                root.path()
                    .join(format!("learned-{id}.toml"))
                    .display()
                    .to_string(),
                root.path()
                    .join(format!("ledger-{id}.json"))
                    .display()
                    .to_string()
            );
            std::fs::write(
                root.path()
                    .join(crate::tenant::TENANT_DIR)
                    .join(format!("{id}.toml")),
                overlay,
            )
            .expect("overlay");
        }

        let set = crate::tenant::load_set(&base, root.path(), "local").expect("two tenants");
        assert_eq!(set.len(), 2);
        for t in &set {
            let tools = build_tools(&t.app, nscore::SchemaProfile::Full)
                .await
                .expect("the tenant's tools");
            let names: Vec<&str> = tools.iter().map(|x| x.spec().name.as_str()).collect();
            // Its own tool, and not the other company's.
            assert!(
                names.contains(&format!("{}_{}", t.id, tool_suffix(&t.id)).as_str()),
                "{names:?}"
            );
            assert_eq!(names.len(), 2, "the time tool plus its own: {names:?}");
            assert_eq!(t.app.persona.text, format!("{}'s persona", t.id));

            let built = build_engine(
                t,
                Mode::Cli {
                    session: format!("{}-s1", t.id),
                },
                None,
            )
            .await
            .expect("the tenant's engine");
            // The engine is built and holds the only handle there is; what is
            // observable from here is that it opened this tenant's store and
            // no other. The channel now returned alongside is a handle to the
            // channel, not to the engine, so the count is still one.
            assert_eq!(Arc::strong_count(&built.engine), 1);
            assert!(store(&t.id).exists(), "{} has its own store", t.id);
        }
        assert!(
            !store("base").exists(),
            "the base store path belongs to no tenant once overlays name their own"
        );
    }

    /// The tool each fixture tenant owns, by tenant id.
    fn tool_suffix(id: &str) -> &'static str {
        match id {
            "acme" => "orders",
            _ => "tickets",
        }
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
