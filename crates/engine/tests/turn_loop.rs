use nscore::*;
use nsengine::script::*;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
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

fn engine_with(
    proposals: Vec<Proposal>,
    guards: Vec<Box<dyn Guard>>,
    store: Arc<InMemoryStore>,
) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    for g in guards {
        b.add_guard(g);
    }
    Engine::with_clock(b.build().unwrap(), EngineConfig::default(), Box::new(|| Timestamp(42)))
}

fn echo_proposal(text: &str) -> Proposal {
    Proposal {
        rationale: "echo it".into(),
        action: "echo".into(),
        args: serde_json::json!({"text": text}),
    }
}

#[tokio::test]
async fn happy_path_tool_then_reply() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(vec![echo_proposal("hi")], vec![], store.clone());
    let sid = SessionId("s1".into());
    let reply = e
        .run_turn(Incoming { session: sid.clone(), text: "say hi".into() })
        .await
        .unwrap();
    assert!(reply.contains("echo: hi"), "trace-based reply mentions tool outcome, got: {reply}");

    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec!["UserSaid", "Proposed", "ToolCalled", "ToolReturned", "Proposed", "Settled", "Replied"]
    );
    assert!(EventLog::from_events(sid, events).verify_chain().is_ok());
}

#[tokio::test]
async fn guard_denial_is_logged_and_turn_still_replies() {
    let store = Arc::new(InMemoryStore::new());
    let guard: Box<dyn Guard> =
        Box::new(DenyAction { action: "echo".into(), reason: "blocked".into() });
    let mut e = engine_with(vec![echo_proposal("hi")], vec![guard], store.clone());
    let sid = SessionId("s2".into());
    let reply = e
        .run_turn(Incoming { session: sid.clone(), text: "say hi".into() })
        .await
        .unwrap();
    assert!(reply.contains("Rejected"), "reply trace shows the refusal, got: {reply}");
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::GuardDenied { .. }, .. }
    )));
    assert!(matches!(&events.last().unwrap().kind, EventKind::Replied { .. }));
}

#[tokio::test]
async fn illegal_action_is_rejected_then_falls_through() {
    let store = Arc::new(InMemoryStore::new());
    let bad = Proposal { rationale: "hm".into(), action: "nuke".into(), args: serde_json::json!({}) };
    let mut e = engine_with(vec![bad], vec![], store.clone());
    let sid = SessionId("s3".into());
    let _ = e
        .run_turn(Incoming { session: sid.clone(), text: "x".into() })
        .await
        .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::IllegalAction { .. }, .. }
    )));
    assert!(matches!(&events.last().unwrap().kind, EventKind::Replied { .. }));
}

#[tokio::test]
async fn second_turn_continues_same_log() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(vec![echo_proposal("a"), echo_proposal("b")], vec![], store.clone());
    let sid = SessionId("s4".into());
    e.run_turn(Incoming { session: sid.clone(), text: "one".into() }).await.unwrap();
    e.run_turn(Incoming { session: sid.clone(), text: "two".into() }).await.unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(
        events.iter().filter(|ev| matches!(ev.kind, EventKind::UserSaid { .. })).count(),
        2
    );
    assert_eq!(events.last().unwrap().turn, 2);
    assert!(EventLog::from_events(sid, events).verify_chain().is_ok());
}

fn kind_name(k: &EventKind) -> &'static str {
    match k {
        EventKind::UserSaid { .. } => "UserSaid",
        EventKind::Proposed { .. } => "Proposed",
        EventKind::Rejected { .. } => "Rejected",
        EventKind::ToolCalled { .. } => "ToolCalled",
        EventKind::ToolReturned { .. } => "ToolReturned",
        EventKind::PendingConfirmation { .. } => "PendingConfirmation",
        EventKind::Confirmed { .. } => "Confirmed",
        EventKind::Corrected { .. } => "Corrected",
        EventKind::Settled { .. } => "Settled",
        EventKind::Replied { .. } => "Replied",
    }
}

struct PersonaProbe;
#[async_trait::async_trait]
impl Replier for PersonaProbe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        Ok(format!("persona was: {}", ctx.persona))
    }
}

#[tokio::test]
async fn persona_flows_from_config_to_reply_context() {
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly immediately
    b.set_replier(Box::new(PersonaProbe));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig { persona: "Tomáš the salesbot".into(), ..Default::default() };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let reply = e
        .run_turn(Incoming { session: SessionId("p1".into()), text: "hi".into() })
        .await
        .unwrap();
    assert_eq!(reply, "persona was: Tomáš the salesbot");
}
