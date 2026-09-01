use nscore::*;
use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
use nsengine::store::NoopConsolidator;
use nsengine::turn::{Engine, EngineConfig};
use nsmemory_sqlite::SqliteStore;
use std::sync::Arc;

struct NullChannel;
#[async_trait::async_trait]
impl Channel for NullChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

#[tokio::test]
async fn full_turn_persists_to_sqlite_and_chain_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e2e.sqlite");
    let sid = SessionId("cli".into());

    // Turn 1 against a fresh store
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "echo it".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "hello"}),
        }])));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store);
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        let reply = e
            .run_turn(Incoming { session: sid.clone(), text: "say hello".into() })
            .await
            .unwrap();
        assert!(reply.contains("echo: hello"));
    }

    // Reopen: log is intact, verifies, and a second turn continues it
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        let events = store.load(&sid).await.unwrap();
        assert!(!events.is_empty());
        assert!(EventLog::from_events(sid.clone(), events.clone()).verify_chain().is_ok());
        assert_eq!(events.last().unwrap().turn, 1);

        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(43)),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "and again".into() }).await.unwrap();

        let events = store.load(&sid).await.unwrap();
        assert_eq!(events.last().unwrap().turn, 2);
        assert!(EventLog::from_events(sid, events).verify_chain().is_ok());
    }
}
