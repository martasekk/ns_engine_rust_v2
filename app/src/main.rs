mod config;

use config::AppConfig;
use nscore::*;
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

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

    // `ns-app dump <session_id>`: print the session log as JSONL and exit.
    // Needs no API key — the log is local.
    let args: Vec<String> = std::env::args().collect();
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

    let key_env = &cfg.llm.api_key_env;
    let api_key = match std::env::var(key_env) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("{key_env} is not set — the harness needs a provider API key.");
            eprintln!("export {key_env}=... and run again (see [llm] api_key_env in config.toml).");
            std::process::exit(1);
        }
    };

    let transport = Arc::new(nsllm::transport::ReqwestTransport::new());
    let mk_client = || {
        let c = nsllm::client::OpenRouterClient::new(transport.clone(), api_key.clone());
        match &cfg.llm.base_url {
            Some(url) => c.with_base_url(url.clone()),
            None => c,
        }
    };

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(nsllm::emitter::CloudEmitter::new(
        mk_client(),
        cfg.llm.emitter.model.clone(),
    )));
    b.set_replier(Box::new(
        nsllm::replier::CloudReplier::new(mk_client(), cfg.llm.replier.model.clone())
            .with_prompt_cache(cfg.llm.prompt_cache()),
    ));
    b.set_memory(Arc::new(
        nsmemory_sqlite::SqliteStore::open(std::path::Path::new(&cfg.store.path))
            .expect("open sqlite store"),
    ));
    b.set_channel(Box::new(nschannel_cli::CliChannel::new_stdio()));
    b.set_consolidator(Box::new(NoopConsolidator));

    b.add_tool(Arc::new(nscomponents_std::time_tool::GetTimeTool::new()));
    let tool_transport = Arc::new(nscomponents_std::transport::ReqwestToolTransport::new());
    for hc in &cfg.http_components {
        b.add_tool(Arc::new(nscomponents_std::http_tool::HttpTool::new(
            hc.clone(),
            tool_transport.clone(),
        )));
    }

    let parts = b.build().expect("harness assembly");
    let engine_cfg = EngineConfig {
        max_iterations: cfg.engine.max_iterations,
        max_emit_retries: cfg.engine.max_emit_retries,
        persona: cfg.persona.text.clone(),
        templates: cfg.templates.clone(),
    };
    let mut engine = Engine::new(parts, engine_cfg);
    println!("ns-harness M2 — type text, /quit to exit");
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
        log.append(1, nscore::Timestamp(1), nscore::EventKind::UserSaid { text: "hi".into() });
        log.append(1, nscore::Timestamp(2), nscore::EventKind::Replied { text: "ho".into() });
        let out = render_dump(log.events());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["kind"]["type"], "UserSaid");
    }
}
