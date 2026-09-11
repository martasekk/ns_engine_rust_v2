mod budget;
mod config;
mod eval;
mod grade;
mod models;

use config::{AppConfig, Role, RoleTarget};
use nscore::{HarnessBuilder, SessionId, Tool};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

type RulesHandle = Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>>;

/// Every spec `ns-app budget` can price a recorded tool array with (M10
/// T0.1): the engine's seven synthetic actions, the desktop set, and the one
/// built-in tool. Deliberately *not* `build_tools` — that dials the pointer
/// daemon and reads the http component config, and `budget` reads a log on a
/// box where neither has to be up. A name the snapshot does not hold prints
/// `n/a` rather than a guess.
pub(crate) fn budget_specs(profile: nscore::SchemaProfile) -> Vec<nscore::ActionSpec> {
    let mut specs = nsengine::turn::synthetic_specs(profile);
    specs.extend(nscomponents_std::pointer_tool::specs(profile));
    specs.push(
        nscomponents_std::time_tool::GetTimeTool::new()
            .spec()
            .clone(),
    );
    specs
}

async fn build_tools(cfg: &AppConfig, profile: nscore::SchemaProfile) -> Vec<Arc<dyn Tool>> {
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
        // with no key: exit rather than run without the thing that was asked
        // for. An unreachable one is a warning: the machine being off must
        // not take the chat down with it, but it must be said.
        let Some(token) = target.token() else {
            eprintln!(
                "{} is not set — [pointer] addr = {:?} needs the agent's token.",
                target.token_env, target.addr
            );
            std::process::exit(1);
        };
        match connect_pointer(&target.addr, &token, profile).await {
            Ok(more) => tools.extend(more),
            Err(e) => eprintln!("pointer: {e}\npointer: the desktop actions are not registered."),
        }
    }
    tools
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

/// `ns-app evolve [--dry-run] [--spend]` → Ok((dry_run, spend))
///
/// M12 T0.2: `--dry-run` alone now spends nothing — no judge, no note
/// proposer, no probes — and `--spend` is the only way to buy those lanes
/// back out of one. Without `--dry-run` the pass spends anyway, so
/// `--spend` there is accepted and says nothing new.
fn parse_evolve_args(args: &[String]) -> Result<(bool, bool), String> {
    let usage = || format!("usage: ns-app evolve [--dry-run] [--spend] (got {args:?})");
    let (mut dry_run, mut spend) = (false, false);
    for arg in args {
        let flag = match arg.as_str() {
            "--dry-run" => &mut dry_run,
            "--spend" => &mut spend,
            _ => return Err(usage()),
        };
        if *flag {
            return Err(usage());
        }
        *flag = true;
    }
    Ok((dry_run, spend))
}

/// One throttle per endpoint: roles sharing a base URL share the pacing,
/// so the provider sees one paced stream (seen live: Mistral 429s on
/// bursts). Roles on different providers are paced independently.
fn throttle_for(target: &RoleTarget) -> Arc<nsllm::client::Throttle> {
    type Registry =
        std::sync::Mutex<std::collections::HashMap<String, Arc<nsllm::client::Throttle>>>;
    static THROTTLES: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    let mut map = THROTTLES
        .get_or_init(Registry::default)
        .lock()
        .expect("throttle registry");
    map.entry(target.base_url_or_default().to_string())
        .or_insert_with(|| Arc::new(nsllm::client::Throttle::new(target.min_interval_ms)))
        .clone()
}

/// The wire log, opened once when NS_TRACE names a path. A path that cannot
/// be opened is fatal: a trace the user asked for and did not get would let
/// them debug against a file that is silently never written.
fn trace_sink() -> Option<Arc<nsllm::trace::Trace>> {
    static SINK: std::sync::OnceLock<Option<Arc<nsllm::trace::Trace>>> = std::sync::OnceLock::new();
    SINK.get_or_init(|| {
        let path = env_override("NS_TRACE")?;
        match nsllm::trace::Trace::open(&path) {
            Ok(t) => {
                eprintln!("tracing every provider request to {path}");
                Some(Arc::new(t))
            }
            Err(e) => {
                eprintln!("NS_TRACE={path}: {e}");
                std::process::exit(1);
            }
        }
    })
    .clone()
}

fn client_for(
    target: &RoleTarget,
    transport: Arc<nsllm::transport::ReqwestTransport>,
    key: &str,
) -> nsllm::client::OpenRouterClient {
    let c = nsllm::client::OpenRouterClient::new(transport, key.to_string())
        .with_throttle(throttle_for(target));
    let c = match &target.base_url {
        Some(url) => c.with_base_url(url.clone()),
        None => c,
    };
    match trace_sink() {
        Some(trace) => c.with_trace(trace, target.role.as_str()),
        None => c,
    }
}

/// Resolve a role, or exit: an unknown provider or a missing model must not
/// fall back silently to someone else's endpoint.
fn role_or_exit(cfg: &AppConfig, role: Role) -> RoleTarget {
    match cfg.llm.role(role) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve one role's request shape, or exit: an unparseable `reasoning` or
/// `sampling` must not be dropped silently — a shape that did not take looks
/// exactly like a model that ignores the knob (M11 T0.2/T0.3).
///
/// The coercion line prints here, once per role at startup, so a request
/// that quietly lost its `temperature` is never a mystery in a later 400.
fn shape_or_exit(
    cfg: &AppConfig,
    target: &RoleTarget,
    base: nsllm::provider::RequestShape,
) -> nsllm::provider::RequestShape {
    match cfg.llm.shape(target.role, &target.model, base) {
        Ok((shape, note)) => {
            if let Some(line) = note {
                println!("{line}");
            }
            shape
        }
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    }
}

fn key_or_exit(target: &RoleTarget) -> String {
    match target.key() {
        Some(k) => k,
        None => {
            eprintln!(
                "{} is not set — the {} role needs a provider API key.",
                target.api_key_env,
                target.role.as_str()
            );
            eprintln!(
                "export {}=... , or switch to a local backend: NS_PROVIDER=ollama (ns-app providers).",
                target.api_key_env
            );
            std::process::exit(1);
        }
    }
}

/// A non-empty env var, trimmed.
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `ns-app providers`: the presets, which keys are present, which one this
/// config is on, and where the full guide lives.
fn render_providers(cfg: &AppConfig) -> String {
    let roles = cfg.llm.roles();
    // A preset is "active" when a role actually resolves onto its endpoint.
    let active: std::collections::HashSet<&str> = match &roles {
        Ok(targets) => targets
            .iter()
            .filter_map(|t| nsllm::provider::for_base_url(t.base_url_or_default()))
            .map(|p| p.name)
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    };

    let mut s = String::from(
        "Providers — one word swaps the agent.\n\n\
         \x20 in config.toml   [llm] provider = \"<name>\"\n\
         \x20 for one run      NS_PROVIDER=<name> [NS_MODEL=<id>] cargo run -p ns-app\n\
         \x20 for one role     [llm.replier] model = \"<name>:<model-id>\"\n\n",
    );
    let row = |mark: &str, name: &str, url: &str, env: &str, key: &str, model: &str| {
        format!("  {mark:<2}{name:<12}{url:<32}{env:<21}{key:<9}{model}\n")
    };
    s.push_str(&row(
        "",
        "NAME",
        "ENDPOINT",
        "KEY (env var)",
        "KEY?",
        "DEFAULT MODEL",
    ));
    for p in nsllm::provider::PROVIDERS {
        let key = if p.local {
            "local"
        } else if env_override(p.api_key_env).is_some() {
            "set"
        } else {
            "MISSING"
        };
        s.push_str(&row(
            if active.contains(p.name) { "→" } else { "" },
            p.name,
            p.base_url,
            p.api_key_env,
            key,
            p.default_model.unwrap_or("(name one)"),
        ));
    }
    s.push_str("\n  → = what this config uses.  local = key optional, any value works.\n\n");
    match &roles {
        Ok(targets) => {
            s.push_str("Active roles:\n");
            for t in targets {
                s.push_str(&format!(
                    "  {:<12}{} @ {}\n",
                    t.role.as_str(),
                    t.model,
                    t.base_url_or_default()
                ));
            }
        }
        Err(e) => s.push_str(&format!("Active roles: config.toml: {e}\n")),
    }
    s.push_str("\nSetup, per-provider notes and troubleshooting: docs/providers.md\n");
    s
}

/// Load learned.toml (fatal when unparsable: a bad rule set must not be
/// silently ignored — the pass would then propose against the wrong base).
fn load_rules_or_exit(cfg: &AppConfig) -> RulesHandle {
    match nsevolution::files::load_rules(std::path::Path::new(&cfg.evolution.learned_path)) {
        Ok(r) => Arc::new(nsengine::arc_swap::ArcSwap::from_pointee(r)),
        Err(e) => {
            eprintln!("{}: {e}", cfg.evolution.learned_path);
            std::process::exit(1);
        }
    }
}

fn build_pass(
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
                client_for(emitter, transport.clone(), &key)
                    .with_usage_sink(judge_sink.clone(), "judge"),
                |c| nsevolution::client_eval::JudgeConfig {
                    // Sonnet 5's shape, and harmless on anything else: no
                    // sampling key at all, one short reasoning block, a
                    // 1,024-token cap on a two-field answer.
                    unsampled: true,
                    structured_output: nsllm::provider::for_base_url(
                        emitter.base_url_or_default(),
                    )
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
                let c = client_for(&factory_target, factory_transport.clone(), &factory_key)
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
                client: client_for(emitter, transport, &key)
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

#[tokio::main]
async fn main() {
    let cfg_text = std::fs::read_to_string("config.toml").unwrap_or_default();
    let mut cfg = match AppConfig::parse(&cfg_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    // A swap without editing config: NS_PROVIDER=ollama ns-app.
    cfg.llm
        .apply_overrides(env_override("NS_PROVIDER"), env_override("NS_MODEL"));
    let cfg = cfg;
    // M10 T1.3: resolved once, here, because every subcommand that prices or
    // sends a tool array has to price or send the same one.
    let schema_profile = match cfg.llm.schema_profile() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    // M12 T1.1, resolved beside it: the emitter's preamble, the engine and
    // the reply path all read the same one fact about the model in play.
    let capability = match cfg.llm.capability() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    // M10 T2.3, resolved here for the same reason: an unreadable depth is a
    // startup error, not a turn that silently runs at the default.
    if let Err(e) = cfg.router.depth() {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().collect();

    // `ns-app providers`: which backends exist, which keys are present, and
    // what this config resolves to. Needs no key and no network.
    if args.get(1).map(String::as_str) == Some("providers") {
        print!("{}", render_providers(&cfg));
        return;
    }

    // `ns-app dump <session_id>`: print the session log as JSONL and exit.
    // Needs no API key — the log is local.
    if args.get(1).map(String::as_str) == Some("dump") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app dump <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        println!("{}", render_dump(&events));
        return;
    }

    // `ns-app echo <session_id>`: per-turn copy ratio for the session log.
    // Needs no API key — the metric is lexical and the log is local.
    if args.get(1).map(String::as_str) == Some("echo") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app echo <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        print!(
            "{}",
            render_echo(&events, cfg.memory.window_turns, cfg.memory.caps())
        );
        return;
    }

    // `ns-app budget <session_id>`: requests, tokens and characters per turn
    // (M7 T0.2). Needs no API key — the cost was recorded when the calls
    // were made, and a session recorded before `ModelCall` existed is
    // reconstructed from the log rather than re-run.
    if args.get(1).map(String::as_str) == Some("budget") {
        let Some(session) = args.get(2) else {
            eprintln!("usage: ns-app budget <session_id>");
            std::process::exit(2);
        };
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        let events = nscore::MemoryStore::load(&store, &SessionId(session.clone()))
            .await
            .expect("load session");
        print!(
            "{}",
            budget::render_budget(
                &events,
                cfg.memory.window_turns,
                cfg.memory.caps(),
                cfg.memory.trace_verbatim_lines,
                cfg.memory.tool_result_max_chars,
                cfg.persona.text.len(),
                &budget_specs(schema_profile),
            )
        );
        return;
    }

    // `ns-app eval [<ledger-path>]`: the six memory abilities against a fixed
    // model (M7 T5.2). Needs no API key and no network — every model in the
    // set is a scripted double, which is the point: a number that moves
    // between two runs moved because the harness changed. Exits non-zero when
    // an ability failed, so a release script can gate on it.
    if args.get(1).map(String::as_str) == Some("eval") {
        let parsed = match eval::parse_args(&args[2..]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if parsed.paraphrase {
            std::process::exit(eval::run_paraphrase(parsed.activation, parsed.facts).await);
        }
        // M10 T5.4. The same shape: a report, not a gate.
        if parsed.obligations {
            std::process::exit(eval::run_knob(eval::Knob::Obligations).await);
        }
        if parsed.guidelines {
            std::process::exit(eval::run_knob(eval::Knob::Guidelines).await);
        }
        // M9 T0.4. A report, not a gate: exits 0 whatever the delta.
        if let Some(block) = parsed.ablate {
            std::process::exit(eval::run_ablate(block, parsed.activation).await);
        }
        std::process::exit(eval::run_at(&parsed.ledger, parsed.activation, parsed.depth).await);
    }

    // `ns-app grade [--local] [--split dev|held|all]`: what an evaluator is
    // worth against a hundred labelled turns (M8 T2.7). No key. The symbolic
    // arm needs no network either; `--local` dials nsmodels and degrades if it
    // is not there.
    if args.get(1).map(String::as_str) == Some("grade") {
        let parsed = match grade::parse_args(&args[2..]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let code = grade::run_cmd(
            &parsed,
            &cfg.models.base_url,
            cfg.models.timeout_ms,
            cfg.models.reask_cosine,
            cfg.models.relevance_cut,
        )
        .await;
        std::process::exit(code);
    }

    // `ns-app evolve [--dry-run]`: driver A (spec M5 §5). The symbolic lane
    // needs no key; without one the notes lane is skipped with a warning.
    if args.get(1).map(String::as_str) == Some("evolve") {
        let (dry_run, spend) = match parse_evolve_args(&args[2..]) {
            Ok(flags) => flags,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if dry_run && !spend {
            eprintln!(
                "dry run: the judge, the note proposer and the probes are skipped \
                 (add --spend to buy them)."
            );
        }
        let rules = load_rules_or_exit(&cfg);
        let tools = build_tools(&cfg, schema_profile).await;
        let emitter = role_or_exit(&cfg, Role::Emitter);
        let pass = build_pass(&cfg, rules, &tools, &emitter, dry_run, spend);
        // M8 T3.1: `evolve` is the idle pass run by hand, and the embeddings
        // backfill is one of its steps — so this store needs the encoder the
        // running harness's does, or `ns-app evolve` would be the one place
        // the backfill never happens.
        let store = models::store(&cfg);
        match pass.run_report(&store).await {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("evolve: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // `ns-app serve`: the chat assembly below, on the TCP channel instead of
    // stdin (multi-conversation plan Phase 3). Not a separate path — the
    // harness is built exactly as for the chat, and the flag is consulted
    // at the points that differ: the channel, the fact scope, the banner.
    let serve = args.get(1).map(String::as_str) == Some("serve");

    // Each role resolves on its own, so the emitter can sit on a local
    // model while the replier stays in the cloud (or the other way round).
    let emitter_target = role_or_exit(&cfg, Role::Emitter);
    let replier_target = role_or_exit(&cfg, Role::Replier);
    let emitter_key = key_or_exit(&emitter_target);
    let replier_key = key_or_exit(&replier_target);

    // Settled before anything is built or dialled. `build_tools` opens the
    // pointer socket and arms an agent; failing after that on a typo in
    // `[memory]` means the config was rejected only once it had already
    // reached out to another machine. Every semantic check the chat path
    // needs happens here, in one place, while the process still holds
    // nothing.
    let remember_residual = match cfg.memory.remember_residual() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    let budget_mode = match cfg.memory.budget_mode() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    let worker_slots = match cfg.engine.worker_slots() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    // The listener too, for the same reason: a missing token or a bad
    // address is refused here, before the pointer has been dialled.
    let tcp = if serve {
        let Some(token) = cfg.serve.token() else {
            eprintln!(
                "{} is not set — `ns-app serve` needs a token; every client presents it in \
                 its first line.",
                cfg.serve.token_env
            );
            std::process::exit(1);
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
            Err(e) => {
                eprintln!("serve: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
    let rules = load_rules_or_exit(&cfg);
    let tools = build_tools(&cfg, schema_profile).await;

    // M7 T0.1: the engine hands each of its own calls a sink of its own
    // through the call's context, so this one is only the fallback for calls
    // made outside a turn — and it is what names the role in every record.
    let usage = Arc::new(nscore::UsageSink::new());

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(
        nsllm::emitter::CloudEmitter::new(
            client_for(&emitter_target, transport.clone(), &emitter_key)
                .with_usage_sink(usage.clone(), "emitter"),
            emitter_target.model.clone(),
        )
        .with_shape(shape_or_exit(
            &cfg,
            &emitter_target,
            nsllm::emitter::default_shape(),
        ))
        // M10 P4. Both halves have to hold: the endpoint must forward a
        // breakpoint at all, and the operator must have said the emitter
        // prefix is worth one. Either off means the request is today's.
        .with_prompt_cache(emitter_target.prompt_cache && cfg.llm.prompt_cache_emitter)
        // M12 T1.1.
        .with_capability(capability),
    ));
    b.set_replier(Box::new(
        nsllm::replier::CloudReplier::new(
            client_for(&replier_target, transport.clone(), &replier_key)
                .with_usage_sink(usage.clone(), "replier"),
            replier_target.model.clone(),
        )
        .with_shape(shape_or_exit(
            &cfg,
            &replier_target,
            nsllm::replier::default_shape(),
        ))
        .with_prompt_cache(replier_target.prompt_cache),
    ));
    b.set_memory(Arc::new(models::store(&cfg)));
    // Says whether the local model service is answering, when one is asked
    // for. Before the channel so the line lands with the other startup
    // reports rather than in the middle of the first turn.
    models::announce(&cfg.models).await;
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
        let cli = nschannel_cli::CliChannel::new_stdio();
        match desktop_messages(&cfg).await {
            Some(client) => b.set_channel(Box::new(
                nscomponents_std::desktop_channel::WithDesktop::spawn(cli, client),
            )),
            None => b.set_channel(Box::new(cli)),
        };
    }
    // M6 §5.1: the rolling summary runs on its own role (model, provider,
    // key), so it can be swapped without touching the emitter or replier.
    if cfg.memory.summary_every_turns > 0 {
        let target = role_or_exit(&cfg, Role::Summarizer);
        match target.key() {
            Some(role_key) => {
                let c = client_for(&target, transport.clone(), &role_key)
                    .with_usage_sink(usage.clone(), "summarizer");
                b.set_summarizer(Box::new(
                    nsllm::summarizer::CloudSummarizer::new(c, target.model.clone())
                        .with_shape(shape_or_exit(
                            &cfg,
                            &target,
                            nsllm::summarizer::default_shape(),
                        ))
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
    if cfg.evolution.enabled {
        // Driver B: the idle timer runs this pass during quiet periods.
        // Driver B is not a dry run, so it spends by the same rule it
        // always did: `spend` only ever gates a dry run.
        b.set_consolidator(Box::new(build_pass(
            &cfg,
            rules.clone(),
            &tools,
            &emitter_target,
            false,
            true,
        )));
    } else {
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
    let parts = match b.build() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ns-harness: cannot assemble the harness: {e}");
            std::process::exit(1);
        }
    };
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
    };
    let engine = Engine::new(parts, engine_cfg);
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
    if let Err(e) = engine.run().await {
        eprintln!("engine stopped: {e}");
    }
}

/// `echo_ratio` per replied turn, reconstructed from a stored log: the
/// acceptance number for the reply-entrainment work (plan §3, phase 0).
///
/// The material is what the log preserves — the window as of that turn, the
/// summary in force, and the turn's own trace. Standing facts are not in it:
/// the log records what the engine decided, not which facts were selected,
/// and the facts table holds only current values. So this **under**-reports
/// against the runtime check, which sees the rendered facts too. It is a
/// floor on the copying in a session, not a ceiling.
fn render_echo(events: &[nscore::Event], window_turns: usize, caps: nscore::Caps) -> String {
    let mut out = String::new();
    let mut ratios: Vec<f32> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        let nscore::EventKind::Replied { text } = &ev.kind else {
            continue;
        };
        // Fold everything before this turn: records exist only for completed
        // turns, so this is exactly the window the reply was shown.
        let before: Vec<nscore::Event> = events[..i]
            .iter()
            .filter(|e| e.turn < ev.turn)
            .cloned()
            .collect();
        let state = nsengine::state::fold(&before);
        let window = state.window(window_turns);
        let mut material = nsengine::trace::turn_trace(events, ev.turn);
        if let Some(s) = &state.summary {
            material.push('\n');
            material.push_str(&nscore::render_summary(s));
        }
        material.push('\n');
        material.push_str(&nscore::render_window(&window, window.len(), &caps));
        let ratio = nsengine::echo::echo_ratio(text, &material);
        ratios.push(ratio);
        let head: String = text.replace('\n', " ").chars().take(70).collect();
        out.push_str(&format!("t{:<5} {ratio:.2}  {head}\n", ev.turn));
    }
    let n = ratios.len();
    let over = ratios.iter().filter(|r| **r >= 0.6).count();
    let mean = if n == 0 {
        0.0
    } else {
        ratios.iter().sum::<f32>() / n as f32
    };
    out.push_str(&format!(
        "\n{n} replied turns · {over} at or over 0.60 · mean {mean:.3}\n"
    ));
    out
}

/// JSONL: one serialized event per line (spec §7 — eyeball any session).
fn render_dump(events: &[nscore::Event]) -> String {
    events
        .iter()
        .map(|e| serde_json::to_string(e).expect("event serialization is infallible"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dump_is_one_json_line_per_event() {
        let mut log = nscore::EventLog::new(nscore::SessionId("d".into()));
        log.append(
            1,
            nscore::Timestamp(1),
            nscore::EventKind::UserSaid { text: "hi".into() },
        );
        log.append(
            1,
            nscore::Timestamp(2),
            nscore::EventKind::Replied { text: "ho".into() },
        );
        let out = render_dump(log.events());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"]["type"], "UserSaid");
    }

    #[test]
    fn providers_listing_names_every_preset_and_the_active_roles() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"ollama\"").unwrap();
        let out = render_providers(&cfg);
        for p in nsllm::provider::PROVIDERS {
            assert!(out.contains(p.name), "{} missing from:\n{out}", p.name);
            assert!(out.contains(p.base_url), "{} url missing", p.name);
        }
        assert!(
            out.contains("emitter     qwen2.5:3b @ http://localhost:11434"),
            "{out}"
        );
        assert!(
            out.contains("replier     qwen2.5:3b @ http://localhost:11434"),
            "{out}"
        );
        assert!(
            out.contains("OLLAMA_API_KEY       local"),
            "local key is optional: {out}"
        );
        assert!(
            out.contains("docs/providers.md"),
            "points at the guide: {out}"
        );
        // The preset in use is marked; the others are not.
        let ollama_row = out.lines().find(|l| l.contains("ollama ")).unwrap();
        assert!(ollama_row.trim_start().starts_with('→'), "{ollama_row:?}");
        let openai_row = out.lines().find(|l| l.contains("openai ")).unwrap();
        assert!(!openai_row.contains('→'), "{openai_row:?}");
    }

    /// A bad provider name must surface in the listing, not panic it.
    #[test]
    fn providers_listing_reports_a_config_error_instead_of_roles() {
        let cfg = AppConfig::parse("[llm]\nprovider = \"nope\"").unwrap();
        let out = render_providers(&cfg);
        assert!(out.contains("is unknown"), "{out}");
    }

    /// M12 T0.2: `--dry-run` is free by default, and `--spend` is the only
    /// way to buy requests out of one.
    #[test]
    fn evolve_args_accept_dry_run_and_spend() {
        let arg = |flags: &[&str]| {
            parse_evolve_args(&flags.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(arg(&[]), Ok((false, false)));
        assert_eq!(arg(&["--dry-run"]), Ok((true, false)));
        assert_eq!(arg(&["--spend"]), Ok((false, true)));
        assert_eq!(arg(&["--dry-run", "--spend"]), Ok((true, true)));
        assert_eq!(arg(&["--spend", "--dry-run"]), Ok((true, true)));
        assert!(arg(&["--wat"]).is_err());
        assert!(arg(&["--dry-run", "--dry-run"]).is_err());
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

        let tools = connect_pointer(&addr, "t0k", nscore::SchemaProfile::Full).await.unwrap();
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
