use nscore::*;
use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // M1 demo: the first user line triggers one echo-tool proposal; the scripted
    // emitter then responds directly on later turns. Real emitter lands in M2;
    // config file lands in M2.
    let script = vec![Proposal {
        rationale: "demo: echo the user".into(),
        action: "echo".into(),
        args: serde_json::json!({"text": "you said something"}),
    }];

    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(script)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(Arc::new(InMemoryStore::new()));
    b.set_channel(Box::new(nschannel_cli::CliChannel::new_stdio()));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));

    let parts = b.build().expect("harness assembly");
    let mut engine = Engine::new(parts, EngineConfig::default());
    println!("ns-harness M1 — type text, /quit to exit");
    if let Err(e) = engine.run().await {
        eprintln!("engine stopped: {e}");
    }
}
