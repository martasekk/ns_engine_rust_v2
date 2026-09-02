mod config;

use config::AppConfig;
use nscore::{HarnessBuilder, SessionId, Tool};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

type RulesHandle = Arc<nsengine::arc_swap::ArcSwap<nscore::LearnedRules>>;

fn build_tools(cfg: &AppConfig) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> =
        vec![Arc::new(nscomponents_std::time_tool::GetTimeTool::new())];
    let tool_transport = Arc::new(nscomponents_std::transport::ReqwestToolTransport::new());
    for hc in &cfg.http_components {
        tools.push(Arc::new(nscomponents_std::http_tool::HttpTool::new(
            hc.clone(),
            tool_transport.clone(),
        )));
    }
    tools
}

/// `ns-app evolve [--dry-run]` → Ok(dry_run)
fn parse_evolve_args(args: &[String]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--dry-run" => Ok(true),
        other => Err(format!("usage: ns-app evolve [--dry-run] (got {other:?})")),
    }
}

/// The provider key, if the configured env var is set and non-empty.
fn api_key(cfg: &AppConfig) -> Option<String> {
    std::env::var(&cfg.llm.api_key_env)
        .ok()
        .filter(|k| !k.is_empty())
}

fn make_client(
    cfg: &AppConfig,
    transport: Arc<nsllm::transport::ReqwestTransport>,
    key: &str,
) -> nsllm::client::OpenRouterClient {
    let c = nsllm::client::OpenRouterClient::new(transport, key.to_string());
    match &cfg.llm.base_url {
        Some(url) => c.with_base_url(url.clone()),
        None => c,
    }
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
    key: Option<&str>,
    dry_run: bool,
) -> nsevolution::pass::EvolutionPass {
    let specs: Vec<nscore::ActionSpec> = tools.iter().map(|t| t.spec().clone()).collect();
    let pass = nsevolution::pass::EvolutionPass::new(
        rules,
        specs.clone(),
        std::path::PathBuf::from(&cfg.evolution.learned_path),
        std::path::PathBuf::from(&cfg.evolution.ledger_path),
        cfg.evolution.pass_config(dry_run),
    );
    match key {
        None => {
            eprintln!(
                "{} is not set — notes lane skipped (symbolic lane needs no key).",
                cfg.llm.api_key_env
            );
            pass
        }
        Some(key) => {
            let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
            let model = cfg.llm.emitter.model.clone();
            let factory_cfg = (cfg.llm.base_url.clone(), key.to_string(), model.clone());
            let factory_transport = transport.clone();
            let emitter: nsevolution::notes::EmitterFactory = Arc::new(move || {
                let (base_url, key, model) = factory_cfg.clone();
                let c = nsllm::client::OpenRouterClient::new(factory_transport.clone(), key);
                let c = match base_url {
                    Some(u) => c.with_base_url(u),
                    None => c,
                };
                Box::new(nsllm::emitter::CloudEmitter::new(c, model)) as Box<dyn nscore::Emitter>
            });
            let probe = nsevolution::notes::LiveProbe {
                emitter,
                known_specs: specs,
                persona: cfg.persona.text.clone(),
            };
            let proposer = nsevolution::notes::ClientNoteProposer {
                client: make_client(cfg, transport, key),
                model,
            };
            pass.with_notes(Box::new(probe), Box::new(proposer))
        }
    }
}

#[tokio::main]
async fn main() {
    let cfg_text = std::fs::read_to_string("config.toml").unwrap_or_default();
    let cfg = match AppConfig::parse(&cfg_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().collect();

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

    // `ns-app evolve [--dry-run]`: driver A (spec M5 §5). The symbolic lane
    // needs no key; without one the notes lane is skipped with a warning.
    if args.get(1).map(String::as_str) == Some("evolve") {
        let dry_run = match parse_evolve_args(&args[2..]) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        let rules = load_rules_or_exit(&cfg);
        let tools = build_tools(&cfg);
        let key = api_key(&cfg);
        let pass = build_pass(&cfg, rules, &tools, key.as_deref(), dry_run);
        let store = nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store");
        match pass.run_report(&store).await {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("evolve: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Some(key) = api_key(&cfg) else {
        let key_env = &cfg.llm.api_key_env;
        eprintln!("{key_env} is not set — the harness needs a provider API key.");
        eprintln!("export {key_env}=... and run again (see [llm] api_key_env in config.toml).");
        std::process::exit(1);
    };

    let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
    let rules = load_rules_or_exit(&cfg);
    let tools = build_tools(&cfg);

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(nsllm::emitter::CloudEmitter::new(
        make_client(&cfg, transport.clone(), &key),
        cfg.llm.emitter.model.clone(),
    )));
    b.set_replier(Box::new(
        nsllm::replier::CloudReplier::new(
            make_client(&cfg, transport.clone(), &key),
            cfg.llm.replier.model.clone(),
        )
        .with_prompt_cache(cfg.llm.prompt_cache()),
    ));
    b.set_memory(Arc::new(
        nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store"),
    ));
    b.set_channel(Box::new(nschannel_cli::CliChannel::new_stdio()));
    if cfg.evolution.enabled {
        // Driver B: the idle timer runs this pass during quiet periods.
        b.set_consolidator(Box::new(build_pass(
            &cfg,
            rules.clone(),
            &tools,
            Some(&key),
            false,
        )));
    } else {
        b.set_consolidator(Box::new(NoopConsolidator));
    }
    for t in &tools {
        b.add_tool(t.clone());
    }

    let parts = b.build().expect("harness assembly");
    let remember_residual = match cfg.memory.remember_residual() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("config.toml: {e}");
            std::process::exit(1);
        }
    };
    let engine_cfg = EngineConfig {
        max_iterations: cfg.engine.max_iterations,
        max_emit_retries: cfg.engine.max_emit_retries,
        persona: cfg.persona.text.clone(),
        templates: cfg.templates.clone(),
        learned: rules,
        idle_after: cfg.evolution.idle_after(),
        window_turns: cfg.memory.window_turns,
        caps: cfg.memory.caps(),
        facts_in_context: cfg.memory.facts_in_context,
        reply_grounding_check: cfg.memory.reply_grounding_check,
        // The CLI is single-user: every session shares the global scope.
        scope_for: Arc::new(|_| "global".to_string()),
        remember_residual,
    };
    let mut engine = Engine::new(parts, engine_cfg);
    println!("ns-harness M5 — type text, /quit to exit");
    if let Err(e) = engine.run().await {
        eprintln!("engine stopped: {e}");
    }
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
    fn evolve_args_accept_only_dry_run() {
        assert_eq!(parse_evolve_args(&[]), Ok(false));
        assert_eq!(parse_evolve_args(&["--dry-run".to_string()]), Ok(true));
        assert!(parse_evolve_args(&["--wat".to_string()]).is_err());
    }
}
