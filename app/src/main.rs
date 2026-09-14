mod budget;
mod config;
mod eval;
mod factory;
mod grade;
mod models;
mod tenant;

use config::{AppConfig, Role, RoleTarget};
use nscore::{SessionId, Tool};
use std::sync::Arc;

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

/// `ns-app [--max-requests N] [--session ID]` → Ok((max_requests, session))
///
/// M12 T6.1: the two things a metered live session needs that the config
/// file cannot give it — a ceiling on what this run may spend, and an id of
/// its own so the reading is separable from every other CLI session in the
/// store. `NS_MAX_REQUESTS` and `NS_SESSION` are the fallback for each; the
/// flag wins when both are given.
fn parse_repl_args(
    args: &[String],
    env_max_requests: Option<String>,
    env_session: Option<String>,
) -> Result<(Option<u32>, String), String> {
    let usage = || format!("usage: ns-app [--max-requests N] [--session ID] (got {args:?})");
    let mut max_requests = env_max_requests;
    let mut session = env_session;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let slot = match arg.as_str() {
            "--max-requests" => &mut max_requests,
            "--session" => &mut session,
            _ => return Err(usage()),
        };
        let Some(value) = rest.next() else {
            return Err(format!("{arg} needs a value — {}", usage()));
        };
        *slot = Some(value.clone());
    }
    let max_requests = match max_requests {
        Some(n) => Some(
            n.parse::<u32>()
                .map_err(|_| format!("--max-requests wants a number, got {n:?}"))?,
        ),
        None => None,
    };
    Ok((max_requests, session.unwrap_or_else(|| "cli".into())))
}

/// One throttle per endpoint *and credential*: roles sharing a base URL and
/// an API key share the pacing, so the provider sees one paced stream per
/// account (seen live: Mistral 429s on bursts). Roles on different providers
/// are paced independently, and so are two tenants on one provider with keys
/// of their own — a quota each means pacing each (multi-tenant plan H8). Two
/// tenants sharing a key still share the throttle, because they share the
/// quota it exists to respect.
///
/// The key is hashed rather than stored: a `DefaultHasher` is enough here
/// because this is a partitioning key and not a security boundary — it only
/// has to separate two different keys, not resist anyone.
fn throttle_for(target: &RoleTarget, key: &str) -> Arc<nsllm::client::Throttle> {
    use std::hash::{Hash, Hasher};
    type Registry =
        std::sync::Mutex<std::collections::HashMap<(String, u64), Arc<nsllm::client::Throttle>>>;
    static THROTTLES: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let mut map = THROTTLES
        .get_or_init(Registry::default)
        .lock()
        .expect("throttle registry");
    map.entry((target.base_url_or_default().to_string(), hasher.finish()))
        .or_insert_with(|| Arc::new(nsllm::client::Throttle::new(target.min_interval_ms)))
        .clone()
}

/// The one tenant a process with no tenant set is: `ns-app` on its own is
/// the company called `local` (multi-tenant plan §2).
pub(crate) const DEFAULT_TENANT: &str = "local";

/// The tenant whose wire trace this process writes, set once by `serve`
/// before the first client is built. `None` is the CLI and every one-shot
/// subcommand: one tenant on one box, which is what NS_TRACE has always
/// meant.
static TRACE_TENANT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// `serve`: name the tenant whose trace file this process writes, and refuse
/// an NS_TRACE that cannot hold one file per tenant. Called at startup so a
/// bad setting is a refusal rather than a surprise on the first request.
pub(crate) fn set_trace_tenant(tenant: &str) -> Result<(), String> {
    if let Some(raw) = env_override("NS_TRACE") {
        trace_path(&raw, Some(tenant))?;
    }
    let _ = TRACE_TENANT.set(tenant.to_string());
    Ok(())
}

/// Where NS_TRACE's value actually writes, for one tenant.
///
/// In the CLI (`tenant` is `None`) the value is the file, untouched. In
/// serve mode it must be a directory and the tenant gets its own file inside
/// it: one process serving twenty companies into one file would interleave
/// every company's prompts, a disclosure the first operator to open it would
/// cause by accident (multi-tenant plan H10).
fn trace_path(raw: &str, tenant: Option<&str>) -> Result<String, String> {
    let Some(tenant) = tenant else {
        return Ok(raw.to_string());
    };
    let dir = std::path::Path::new(raw);
    if dir.is_file() {
        return Err(format!(
            "NS_TRACE={raw} is a file; serving needs a directory to write one trace file per tenant into."
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("NS_TRACE={raw}: {e}"))?;
    Ok(dir.join(format!("{tenant}.jsonl")).to_string_lossy().into())
}

/// The wire log, opened once per path when NS_TRACE names one. A path that
/// cannot be opened is fatal: a trace the user asked for and did not get
/// would let them debug against a file that is silently never written.
fn trace_sink() -> Option<Arc<nsllm::trace::Trace>> {
    type Sinks = std::sync::Mutex<std::collections::HashMap<String, Arc<nsllm::trace::Trace>>>;
    static SINKS: std::sync::OnceLock<Sinks> = std::sync::OnceLock::new();
    let raw = env_override("NS_TRACE")?;
    let path = trace_path(&raw, TRACE_TENANT.get().map(String::as_str)).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let mut map = SINKS
        .get_or_init(Sinks::default)
        .lock()
        .expect("trace registry");
    if let Some(sink) = map.get(&path) {
        return Some(sink.clone());
    }
    match nsllm::trace::Trace::open(&path) {
        Ok(t) => {
            eprintln!("tracing every provider request to {path}");
            Some(map.entry(path).or_insert(Arc::new(t)).clone())
        }
        Err(e) => {
            eprintln!("NS_TRACE={path}: {e}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn client_for(
    target: &RoleTarget,
    transport: Arc<nsllm::transport::ReqwestTransport>,
    key: &str,
) -> nsllm::client::OpenRouterClient {
    let c = nsllm::client::OpenRouterClient::new(transport, key.to_string())
        .with_throttle(throttle_for(target, key));
    let c = match &target.base_url {
        Some(url) => c.with_base_url(url.clone()),
        None => c,
    };
    match trace_sink() {
        Some(trace) => c.with_trace(trace, target.role.as_str()),
        None => c,
    }
}

/// A non-empty env var, trimmed.
pub(crate) fn env_override(name: &str) -> Option<String> {
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
    // M12 T1.1, checked beside it: the emitter's preamble, the engine and
    // the reply path all read the same one fact about the model in play.
    // The factory resolves it again for the tenant it builds; refusing it
    // here keeps a bad `[llm]` a startup error for every subcommand.
    if let Err(e) = cfg.llm.capability() {
        eprintln!("{e}");
        std::process::exit(1);
    }
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
        std::process::exit(
            eval::run_at(
                &parsed.ledger,
                parsed.activation,
                parsed.depth,
                parsed.profile,
            )
            .await,
        );
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
        let rules = factory::load_rules(&cfg).unwrap_or_else(|e| e.exit());
        let tools = factory::build_tools(&cfg, schema_profile)
            .await
            .unwrap_or_else(|e| e.exit());
        let emitter = factory::role(&cfg, Role::Emitter).unwrap_or_else(|e| e.exit());
        let pass = factory::build_pass(&cfg, rules, &tools, &emitter, dry_run, spend);
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

    // M12 T6.1: the metered-session flags. `serve` takes its session ids
    // from its clients, so only the ceiling means anything there.
    let flags_from = if serve { 2 } else { 1 };
    let (max_requests, cli_session) = match parse_repl_args(
        &args[flags_from.min(args.len())..],
        env_override("NS_MAX_REQUESTS"),
        env_override("NS_SESSION"),
    ) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // The whole assembly, one tenant's worth (multi-tenant plan T1.1). It
    // refuses rather than exits, so the shard that will host many of these
    // survives one bad config; here, where the process is this tenant's,
    // `exit` prints and stops exactly as the inlined version did.
    if serve {
        // Serving is the multi-tenant shape even at one tenant, so the wire
        // trace is per tenant from here on (plan H10). Until Phase 3 gives
        // the process a tenant set, that tenant is the plan's `local`.
        if let Err(e) = set_trace_tenant(DEFAULT_TENANT) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    // The tenant set this working directory serves (plan T1.2). With no
    // `tenants/` directory that is one tenant called `local` built from
    // `config.toml` alone, which is this process exactly as it was; the
    // overlays are refused by name here rather than at the first turn.
    let mut set = tenant::load_set(&cfg_text, std::path::Path::new("."), DEFAULT_TENANT)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });
    if set.len() > 1 {
        let ids: Vec<&str> = set.iter().map(|t| t.id.as_str()).collect();
        eprintln!(
            "{} tenants are configured ({}) — one process serves one tenant until the \
             tenant registry lands (multi-tenant plan phase 3).",
            set.len(),
            ids.join(", ")
        );
        std::process::exit(1);
    }
    let mut tenant = set.remove(0);
    // The same swap the base config gets: NS_PROVIDER=ollama ns-app.
    tenant
        .app
        .llm
        .apply_overrides(env_override("NS_PROVIDER"), env_override("NS_MODEL"));
    let tenant = tenant;
    // One socket serves the whole process, so the bind is the caller's and
    // not the factory's (plan A1): a shard building one engine per company
    // would otherwise reach for the same address once per tenant. A missing
    // token or an address that will not bind is still refused here, before
    // the pointer has been dialled, and with the same words as before.
    let mode = if serve {
        let serve_cfg = &tenant.app.serve;
        let Some(token) = serve_cfg.token() else {
            factory::StartupError::MissingServeToken {
                env: serve_cfg.token_env.clone(),
            }
            .exit()
        };
        match nschannel_tcp::TcpChannel::bind_shared(
            &serve_cfg.listen,
            token,
            serve_cfg.max_connections,
            serve_cfg.allow_remote,
        )
        .await
        {
            Ok(channel) => factory::Mode::Serve {
                addr: channel.local_addr(),
                channel,
            },
            Err(e) => factory::StartupError::Serve(e.to_string()).exit(),
        }
    } else {
        factory::Mode::Cli {
            session: cli_session,
        }
    };
    let engine = factory::build_engine(&tenant, mode, max_requests)
        .await
        .unwrap_or_else(|e| e.exit());
    // `Engine::run` consumes the engine, and the factory hands back the only
    // handle there is, so this cannot be `None`. The `Arc` is the shape the
    // tenant registry will hold them in (plan D3); a process that runs one
    // tenant on stdin takes its engine back out.
    let engine = Arc::into_inner(engine).expect("the factory returns the only engine handle");

    match engine.run().await {
        Ok(()) => {}
        // M12 T6.1: the ceiling this run was given, reached. A stop by
        // arrangement rather than a failure, but non-zero all the same, so a
        // script driving the session can tell it ended early.
        Err(nsengine::turn::EngineError::RequestCap { spent, cap }) => {
            eprintln!("request cap {cap} reached after {spent} requests; stopping");
            std::process::exit(3);
        }
        Err(e) => eprintln!("engine stopped: {e}"),
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

    /// M12 T6.1: the two flags a metered live session is run with.
    #[test]
    fn repl_args_accept_max_requests_and_session() {
        let arg = |flags: &[&str]| {
            parse_repl_args(
                &flags.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                None,
                None,
            )
        };
        assert_eq!(arg(&[]), Ok((None, "cli".to_string())));
        assert_eq!(
            arg(&["--max-requests", "60", "--session", "m12-live"]),
            Ok((Some(60), "m12-live".to_string()))
        );
        assert_eq!(
            arg(&["--session", "m12-live"]),
            Ok((None, "m12-live".to_string()))
        );
        assert!(arg(&["--max-requests", "sixty"]).is_err());
        assert!(arg(&["--max-requests"]).is_err());
        assert!(arg(&["--wat"]).is_err());
        // The environment is the fallback; a flag beside it wins.
        assert_eq!(
            parse_repl_args(&[], Some("40".into()), Some("env-session".into())),
            Ok((Some(40), "env-session".to_string()))
        );
        assert_eq!(
            parse_repl_args(
                &["--max-requests".to_string(), "60".to_string()],
                Some("40".into()),
                None
            ),
            Ok((Some(60), "cli".to_string()))
        );
        assert!(parse_repl_args(&[], Some("lots".into()), None).is_err());
    }

    fn target(base_url: &str) -> RoleTarget {
        RoleTarget {
            role: Role::Emitter,
            model: "m".into(),
            base_url: Some(base_url.into()),
            api_key_env: "NS_TEST_KEY".into(),
            min_interval_ms: 100,
            prompt_cache: false,
            local: false,
        }
    }

    /// Multi-tenant plan H8: two companies on one provider, each with its
    /// own key, have their own quota, so one's traffic must not pace the
    /// other's.
    #[test]
    fn two_tenants_with_distinct_keys_get_distinct_throttles() {
        let t = target("http://h8-distinct.invalid");
        let acme = throttle_for(&t, "acme-key");
        let globex = throttle_for(&t, "globex-key");
        assert!(
            !Arc::ptr_eq(&acme, &globex),
            "one base URL, two keys, two quotas — the throttle must not be shared"
        );
    }

    /// The other half of H8: sharing a key means sharing a quota, so
    /// sharing the pacing is the correct answer, not a leak.
    #[test]
    fn two_tenants_sharing_a_key_share_a_throttle() {
        let t = target("http://h8-shared.invalid");
        let first = throttle_for(&t, "one-account");
        let second = throttle_for(&t, "one-account");
        assert!(
            Arc::ptr_eq(&first, &second),
            "one key is one quota — the two tenants must share the pacing"
        );
        // And a role on another endpoint under the same key is still its own.
        let elsewhere = throttle_for(&target("http://h8-elsewhere.invalid"), "one-account");
        assert!(
            !Arc::ptr_eq(&first, &elsewhere),
            "one throttle per endpoint"
        );
    }

    /// Multi-tenant plan H10: in serve mode NS_TRACE names a directory with
    /// one file per tenant. A regular file there would interleave every
    /// company's prompts, so it is refused before the first request.
    #[test]
    fn serve_mode_refuses_a_trace_path_that_is_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wire.jsonl");
        std::fs::write(&file, "").unwrap();
        let raw = file.to_string_lossy().to_string();
        let err = trace_path(&raw, Some("acme")).expect_err("a regular file is not a directory");
        assert!(err.contains("NS_TRACE"), "names the variable: {err}");
        assert!(err.contains("directory"), "says what it expects: {err}");
        assert!(err.contains(&raw), "names the path given: {err}");
        // A directory is accepted, and the file inside it is the tenant's.
        let ok = trace_path(&dir.path().to_string_lossy(), Some("acme")).unwrap();
        assert!(
            ok.ends_with("acme.jsonl"),
            "per tenant, not per process: {ok}"
        );
        let other = trace_path(&dir.path().to_string_lossy(), Some("globex")).unwrap();
        assert_ne!(ok, other, "two tenants, two files");
    }

    /// The CLI is one tenant on one box: NS_TRACE keeps naming the file it
    /// always named, byte for byte.
    #[test]
    fn cli_mode_still_accepts_a_trace_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wire.jsonl");
        std::fs::write(&file, "").unwrap();
        let raw = file.to_string_lossy().to_string();
        assert_eq!(trace_path(&raw, None).unwrap(), raw);
        // Not even a path that does not exist yet is inspected: the CLI
        // hands NS_TRACE to `Trace::open` exactly as it was given.
        let fresh = dir
            .path()
            .join("not-yet.jsonl")
            .to_string_lossy()
            .to_string();
        assert_eq!(trace_path(&fresh, None).unwrap(), fresh);
    }
}
