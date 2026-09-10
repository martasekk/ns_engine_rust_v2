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
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        let reply = e
            .run_turn(Incoming {
                session: sid.clone(),
                text: "say hello".into(),
            })
            .await
            .unwrap();
        assert!(reply.contains("echo: hello"));
    }

    // Reopen: log is intact, verifies, and a second turn continues it
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        let events = store.load(&sid).await.unwrap();
        assert!(!events.is_empty());
        assert!(EventLog::from_events(sid.clone(), events.clone())
            .verify_chain()
            .is_ok());
        assert_eq!(events.last().unwrap().turn, 1);

        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(43)),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "and again".into(),
        })
        .await
        .unwrap();

        let events = store.load(&sid).await.unwrap();
        assert_eq!(events.last().unwrap().turn, 2);
        assert!(EventLog::from_events(sid, events).verify_chain().is_ok());
    }
}

/// A `ScriptedEmitter` that yields to the scheduler before answering, so two
/// turns started together both load the log before either appends to it.
struct YieldingEmitter(ScriptedEmitter);
#[async_trait::async_trait]
impl Emitter for YieldingEmitter {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        tokio::task::yield_now().await;
        self.0.propose(ctx, legal).await
    }
}

/// Findings 2026-09-10 §2.2 (many conversations at once). Two turns for one
/// session that overlap both number themselves turn 1 and assign the same
/// event ids; `SqliteStore::append` keeps only ids above the stored maximum,
/// so whichever turn appends second vanishes with no error — after its reply
/// was already returned to the user. Which one that is depends on the
/// scheduler (`tokio::join!` rotates its polling order; in the first run it
/// was the *first* message that disappeared), so the assertions name no
/// loser: exactly one of the two survives, the other is nowhere, and the log
/// looks healthy. Nothing enforces "one turn at a time per session" today
/// except the serial loop in `Engine::run`. Phase 2's dispatcher inverts this
/// test: through a per-session mailbox the same two messages become turns 1
/// and 2.
#[tokio::test]
async fn two_overlapping_turns_on_one_session_lose_one_silently() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("overlap.sqlite")).unwrap());
    let sid = SessionId("one".into());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(YieldingEmitter(ScriptedEmitter::new(vec![]))));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    let e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    );

    let first = Incoming {
        session: sid.clone(),
        text: "first message".into(),
    };
    let second = Incoming {
        session: sid.clone(),
        text: "second message".into(),
    };
    let (r1, r2) = tokio::join!(e.run_turn(first), e.run_turn(second));
    // Both messages were answered ...
    assert!(!r1.unwrap().is_empty());
    assert!(!r2.unwrap().is_empty());

    // ... and only one turn exists.
    let events = store.load(&sid).await.unwrap();
    let said: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::UserSaid { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        said.len(),
        1,
        "exactly one of the two turns was recorded: {said:?}"
    );
    let lost = match said[0] {
        "first message" => "second message",
        "second message" => "first message",
        other => panic!("a message nobody sent: {other}"),
    };
    assert_eq!(events.last().unwrap().turn, 1, "the survivor is turn 1");
    let dumped = serde_json::to_string(&events).unwrap();
    assert!(!dumped.contains(lost), "no trace of the lost turn anywhere");
    assert!(
        EventLog::from_events(sid, events).verify_chain().is_ok(),
        "the surviving log looks perfectly healthy"
    );
}
