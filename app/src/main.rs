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

    let api_key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("OPENROUTER_API_KEY is not set — the M2 harness needs real models.");
            eprintln!("export OPENROUTER_API_KEY=... and run again.");
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
    b.set_replier(Box::new(nsllm::replier::CloudReplier::new(
        mk_client(),
        cfg.llm.replier.model.clone(),
    )));
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
    };
    let mut engine = Engine::new(parts, engine_cfg);
    println!("ns-harness M2 — type text, /quit to exit");
    if let Err(e) = engine.run().await {
        eprintln!("engine stopped: {e}");
    }
}
