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
    Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    )
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
        .run_turn(Incoming {
            session: sid.clone(),
            text: "say hi".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.contains("echo: hi"),
        "trace-based reply mentions tool outcome, got: {reply}"
    );

    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            "UserSaid",
            "Proposed",
            "ToolCalled",
            "ToolReturned",
            "Proposed",
            "Settled",
            "Replied"
        ]
    );
    assert!(EventLog::from_events(sid, events).verify_chain().is_ok());
}

#[tokio::test]
async fn guard_denial_is_logged_and_turn_still_replies() {
    let store = Arc::new(InMemoryStore::new());
    let guard: Box<dyn Guard> = Box::new(DenyAction {
        action: "echo".into(),
        reason: "blocked".into(),
    });
    let mut e = engine_with(vec![echo_proposal("hi")], vec![guard], store.clone());
    let sid = SessionId("s2".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "say hi".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.contains("Rejected"),
        "reply trace shows the refusal, got: {reply}"
    );
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected {
            reason: RejectReason::GuardDenied { .. },
            ..
        }
    )));
    assert!(matches!(
        &events.last().unwrap().kind,
        EventKind::Replied { .. }
    ));
}

#[tokio::test]
async fn illegal_action_is_rejected_then_falls_through() {
    let store = Arc::new(InMemoryStore::new());
    let bad = Proposal {
        rationale: "hm".into(),
        action: "nuke".into(),
        args: serde_json::json!({}),
    };
    let mut e = engine_with(vec![bad], vec![], store.clone());
    let sid = SessionId("s3".into());
    let _ = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "x".into(),
        })
        .await
        .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected {
            reason: RejectReason::IllegalAction { .. },
            ..
        }
    )));
    assert!(matches!(
        &events.last().unwrap().kind,
        EventKind::Replied { .. }
    ));
}

#[tokio::test]
async fn second_turn_continues_same_log() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![echo_proposal("a"), echo_proposal("b")],
        vec![],
        store.clone(),
    );
    let sid = SessionId("s4".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "one".into(),
    })
    .await
    .unwrap();
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "two".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::UserSaid { .. }))
            .count(),
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
        EventKind::ReplyFailed { .. } => "ReplyFailed",
        EventKind::ReplyFlagged { .. } => "ReplyFlagged",
    }
}

/// First draft invents a count and a city; the second draft reports what it
/// was told not to state.
struct InventingReplier(std::sync::atomic::AtomicU32);
#[async_trait::async_trait]
impl Replier for InventingReplier {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            assert!(ctx.do_not_state.is_empty());
            Ok("You have 42 orders waiting in Oslo.".into())
        } else {
            Ok(format!(
                "Nothing to report. Avoided: {}",
                ctx.do_not_state.join(",")
            ))
        }
    }
}

#[tokio::test]
async fn ungrounded_reply_is_flagged_logged_and_regenerated_once() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("ground".into());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly
    b.set_replier(Box::new(InventingReplier(Default::default())));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let mut e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    );
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "anything new?".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, "Nothing to report. Avoided: 42,Oslo");
    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec!["UserSaid", "Proposed", "Settled", "ReplyFlagged", "Replied"]
    );
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::ReplyFlagged { draft, spans }
            if draft == "You have 42 orders waiting in Oslo." && spans == &["42", "Oslo"]
    )));
    // The recording replays clean: the interceptor is off under doubles and
    // ReplyFlagged is not a behavioral line.
    nsengine::replay::replay_session(sid, &events, vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn grounded_reply_is_not_flagged_and_check_can_be_disabled() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("ground2".into());
    // ScriptedReplier echoes the trace, which is material by definition.
    let mut e = engine_with(vec![echo_proposal("hi")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "say hi".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(!events
        .iter()
        .any(|ev| matches!(ev.kind, EventKind::ReplyFlagged { .. })));

    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![])));
    b.set_replier(Box::new(InventingReplier(Default::default())));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        reply_grounding_check: false,
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let reply = e
        .run_turn(Incoming {
            session: SessionId("off".into()),
            text: "anything new?".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, "You have 42 orders waiting in Oslo.");
}

/// Proposes a tool on the first call, respond_directly after — recording the
/// state_summary it was shown each time.
struct CtxProbe {
    calls: std::sync::Mutex<u32>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Emitter for CtxProbe {
    async fn propose(
        &self,
        ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        self.seen.lock().unwrap().push(ctx.trace_so_far.join("\n"));
        if *n == 1 {
            Ok(Proposal {
                rationale: "r".into(),
                action: "echo".into(),
                args: serde_json::json!({"text": "hi"}),
            })
        } else {
            Ok(Proposal {
                rationale: "done".into(),
                action: "respond_directly".into(),
                args: serde_json::json!({}),
            })
        }
    }
}

#[tokio::test]
async fn emitter_context_includes_this_turn_actions() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(CtxProbe {
        calls: std::sync::Mutex::new(0),
        seen: seen.clone(),
    }));
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
    e.run_turn(Incoming {
        session: SessionId("c1".into()),
        text: "say hi".into(),
    })
    .await
    .unwrap();

    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 2, "emitter consulted at least twice");
    assert!(
        !seen[0].contains("ToolReturned"),
        "first consult predates any tool call, got: {}",
        seen[0]
    );
    assert!(
        seen[1].contains("ToolReturned(ok: echo: hi)"),
        "second consult must show this turn's outcomes so the emitter can settle, got: {}",
        seen[1]
    );
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
    let cfg = EngineConfig {
        persona: "Tomáš the salesbot".into(),
        ..Default::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let reply = e
        .run_turn(Incoming {
            session: SessionId("p1".into()),
            text: "hi".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, "persona was: Tomáš the salesbot");
}

/// Renders what the reply model can actually see: the current user text and
/// the verbatim window of earlier turns.
struct WindowProbe;
#[async_trait::async_trait]
impl Replier for WindowProbe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        Ok(format!(
            "USER={} | WINDOW={} | FACTS={}",
            ctx.user_text,
            nscore::render_window(&ctx.window, ctx.window.len(), &ctx.caps),
            ctx.facts.len()
        ))
    }
}

#[tokio::test]
async fn reply_context_carries_the_user_text_and_the_verbatim_window() {
    // Seen live (turns 63–117 of the recorded session): the replier received
    // only facts, a counter and "Proposed(respond_directly)", never the
    // user's message or earlier turns, and improvised greetings.
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("win".into());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![echo_proposal("first")])));
    b.set_replier(Box::new(WindowProbe));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let mut e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    );
    let r1 = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "say first".into(),
        })
        .await
        .unwrap();
    assert!(
        r1.starts_with("USER=say first | WINDOW= |"),
        "no completed turn yet: {r1}"
    );
    let r2 = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "what did you do before?".into(),
        })
        .await
        .unwrap();
    assert!(
        r2.starts_with("USER=what did you do before? | WINDOW=[t1] user: say first"),
        "{r2}"
    );
    assert!(
        r2.contains("did:  echo -> ok: echo: first"),
        "outcomes travel with the window: {r2}"
    );
    assert!(
        r2.contains("bot:  USER=say first"),
        "the earlier reply is in the record: {r2}"
    );
}

#[tokio::test]
async fn real_classification_tags_user_input_and_residual() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: "echo".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "say hi"}), // exact substring of the user turn
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("cls1".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "please say hi now".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    let called_args = events
        .iter()
        .find_map(|ev| match &ev.kind {
            EventKind::ToolCalled { args, .. } => Some(args.clone()),
            _ => None,
        })
        .expect("a ToolCalled event");
    let (name, tv) = &called_args[0];
    assert_eq!(name, "text");
    assert!(
        matches!(tv.prov, Provenance::UserInput { .. }),
        "'say hi' comes from the user's words, got {:?}",
        tv.prov
    );
    assert_eq!(tv.trust, Trust::User);
}

struct WipeTool {
    spec: ActionSpec,
}

impl WipeTool {
    fn new() -> Self {
        Self {
            spec: ActionSpec {
                name: "wipe".into(),
                description: "wipe the database".into(),
                args_schema: serde_json::json!({"type": "object", "properties": {}}),
                side_effect: SideEffect::Irreversible,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for WipeTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: "wiped".into(),
            artifact: None,
            trust: Trust::System,
        })
    }
    async fn stage(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Option<StagedEffect> {
        Some(StagedEffect {
            description: "would delete 3 rows".into(),
        })
    }
}

fn engine_with_tools(
    proposals: Vec<Proposal>,
    tools: Vec<Arc<dyn Tool>>,
    store: Arc<InMemoryStore>,
) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    for t in tools {
        b.add_tool(t);
    }
    Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    )
}

#[tokio::test]
async fn irreversible_action_is_staged_not_executed() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with_tools(
        vec![Proposal {
            rationale: "wipe".into(),
            action: "wipe".into(),
            args: serde_json::json!({}),
        }],
        vec![Arc::new(WipeTool::new())],
        store.clone(),
    );
    let sid = SessionId("se1".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "wipe it".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.contains("irreversible"),
        "user is asked to confirm, got: {reply}"
    );
    assert!(
        reply.contains("would delete 3 rows"),
        "staged effect is shown, got: {reply}"
    );

    let events = store.load(&sid).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })),
        "the tool must NOT run before confirmation"
    );
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::PendingConfirmation { staged: Some(s), .. } if s.description == "would delete 3 rows"
    )));
    assert!(matches!(
        &events.last().unwrap().kind,
        EventKind::Replied { .. }
    ));
}

#[tokio::test]
async fn denied_action_is_removed_from_next_legal_set() {
    // Emitter proposes "echo" twice; a plugin guard denies it. The second
    // proposal must be rejected as ILLEGAL (narrowed schema), not guard-denied.
    let store = Arc::new(InMemoryStore::new());
    let guard: Box<dyn Guard> = Box::new(DenyAction {
        action: "echo".into(),
        reason: "no".into(),
    });
    let mut e = engine_with(
        vec![echo_proposal("a"), echo_proposal("b")],
        vec![guard],
        store.clone(),
    );
    let sid = SessionId("nar1".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "x".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    let reasons: Vec<&RejectReason> = events
        .iter()
        .filter_map(|ev| match &ev.kind {
            EventKind::Rejected { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert!(matches!(reasons[0], RejectReason::GuardDenied { .. }));
    assert!(
        matches!(reasons[1], RejectReason::IllegalAction { .. }),
        "second identical proposal must be illegal under the narrowed set, got {:?}",
        reasons[1]
    );
}

struct OrderTool {
    spec: ActionSpec,
}

impl OrderTool {
    fn new() -> Self {
        Self {
            spec: ActionSpec {
                name: "cancel_order".into(),
                description: "cancel an order".into(),
                args_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"order_id": {"type": "string"}},
                    "required": ["order_id"]
                }),
                side_effect: SideEffect::Reversible,
                residual_policy: [("order_id".to_string(), ResidualRule::Never)]
                    .into_iter()
                    .collect(),
                dedupe_tag: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for OrderTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: "ordered".into(),
            artifact: None,
            trust: Trust::System,
        })
    }
}

#[tokio::test]
async fn never_residual_rejection_forces_clarification() {
    // The emitter invents an order id the user never gave, then obediently
    // asks a clarification question (the only remaining legal action).
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with_tools(
        vec![
            Proposal {
                rationale: "cancel".into(),
                action: "cancel_order".into(),
                args: serde_json::json!({"order_id": "ORD-99"}), // invented
            },
            Proposal {
                rationale: "need the id".into(),
                action: "ask_clarification".into(),
                args: serde_json::json!({"question": "Which order should I cancel?"}),
            },
        ],
        vec![Arc::new(OrderTool::new())],
        store.clone(),
    );
    let sid = SessionId("clar1".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "cancel my order".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, "Which order should I cancel?");

    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::GuardDenied { reason, .. }, .. }
            if reason.contains("NeverResidual")
    )));
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })),
        "cancel_order must not run on an invented id"
    );
}

#[tokio::test]
async fn confirmation_flow_executes_on_next_turn_yes() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("cf1".into());
    // Turn 1: propose wipe -> staged, asks for confirmation.
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "wipe".into(),
                action: "wipe".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "wipe it".into(),
        })
        .await
        .unwrap();
    }
    // Turn 2: user says yes; emitter proposes confirm_pending.
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "user confirmed".into(),
                action: "confirm_pending".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        let reply = e
            .run_turn(Incoming {
                session: sid.clone(),
                text: "yes".into(),
            })
            .await
            .unwrap();
        assert!(
            reply.contains("wiped"),
            "trace reply reports execution, got: {reply}"
        );
    }
    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert!(kinds.contains(&"Confirmed"));
    assert!(
        kinds.contains(&"ToolCalled"),
        "the staged action ran after Confirmed"
    );
    // paper trail order: PendingConfirmation before Confirmed before ToolCalled
    let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
    assert!(pos("PendingConfirmation") < pos("Confirmed"));
    assert!(pos("Confirmed") < pos("ToolCalled"));
}

#[tokio::test]
async fn pending_confirmation_expires_after_one_turn() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("cf2".into());
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "wipe".into(),
                action: "wipe".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "wipe it".into(),
        })
        .await
        .unwrap();
    }
    // Turn 2: user changes the subject; scripted emitter falls through to respond_directly.
    {
        let mut e = engine_with_tools(vec![], vec![Arc::new(WipeTool::new())], store.clone());
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "actually, what time is it?".into(),
        })
        .await
        .unwrap();
    }
    // Turn 3: a late confirm_pending must be rejected as illegal and nothing runs.
    {
        let mut e = engine_with_tools(
            vec![Proposal {
                rationale: "late yes".into(),
                action: "confirm_pending".into(),
                args: serde_json::json!({}),
            }],
            vec![Arc::new(WipeTool::new())],
            store.clone(),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "yes do it".into(),
        })
        .await
        .unwrap();
    }
    let events = store.load(&sid).await.unwrap();
    assert!(!events
        .iter()
        .any(|ev| matches!(ev.kind, EventKind::Confirmed { .. })));
    assert!(!events
        .iter()
        .any(|ev| matches!(ev.kind, EventKind::ToolCalled { .. })));
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::IllegalAction { action }, .. }
            if action == "confirm_pending"
    )));
}

struct FactsProbe;
#[async_trait::async_trait]
impl Replier for FactsProbe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        let lines: Vec<String> = ctx
            .facts
            .iter()
            .map(|f| format!("{}={} uses={}", f.key, f.value, f.uses))
            .collect();
        Ok(format!("FACTS[{}]", lines.join(";")))
    }
}

#[tokio::test]
async fn remember_fact_stores_classified_fact_and_recall_bumps_uses() {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId("facts1".into());
    // Turn 1: remember the user's name (value is a span of the user's words).
    {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "durable".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": "user.name", "value": "Martin"}),
        }])));
        b.set_replier(Box::new(FactsProbe));
        b.set_memory(store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        let reply = e
            .run_turn(Incoming {
                session: sid.clone(),
                text: "my name is Martin".into(),
            })
            .await
            .unwrap();
        // Generate path recalls the just-stored fact (uses already bumped to 1)
        assert!(
            reply.contains("user.name=\"Martin\" uses=1"),
            "got: {reply}"
        );
    }
    let stored = store.facts("global", "user").await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        matches!(stored[0].prov, Provenance::UserInput { .. }),
        "fact provenance comes from classification, got {:?}",
        stored[0].prov
    );
    assert_eq!(stored[0].uses, 1, "recall bumped lifecycle metadata");

    // The log carries the paper trail.
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::ToolCalled { action, .. } if action == "remember_fact"
    )));
}

#[tokio::test]
async fn remember_fact_restatement_keeps_uses_and_raises_confidence() {
    // Seen live: user.name was re-remembered ten times and every write reset
    // `uses` to 0. A restatement must keep the count, re-validate, and (for
    // an unverified fact) raise confidence; a new value replaces the old one
    // at full confidence without losing the count.
    let store = Arc::new(InMemoryStore::new());
    store
        .put_fact(Fact {
            key: "user.name".into(),
            value: serde_json::json!("Martin"),
            confidence: 0.5,
            uses: 3,
            last_validated: Timestamp(1),
            prov: Provenance::Residual,
            valid_from: Timestamp(1),
            ..Default::default()
        })
        .await
        .unwrap();
    let remember = |v: &str| Proposal {
        rationale: "remember".into(),
        action: "remember_fact".into(),
        args: serde_json::json!({"key": "user.name", "value": v}),
    };
    let sid = SessionId("facts5".into());

    // A grounded restatement promotes an unverified fact to full confidence.
    let mut e = engine_with(vec![remember("Martin")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "my name is Martin".into(),
    })
    .await
    .unwrap();
    let f = store.facts("global", "user.name").await.unwrap().remove(0);
    assert_eq!(f.uses, 4, "3 kept, then one recall bump in the reply path");
    assert!((f.confidence - 1.0).abs() < 1e-6, "got {}", f.confidence);
    assert_eq!(f.last_validated, Timestamp(42));
    assert_eq!(f.valid_from, Timestamp(1), "same version, updated in place");
    assert!(
        matches!(f.prov, Provenance::UserInput { .. }),
        "a restatement re-grounds the value, got {:?}",
        f.prov
    );
    assert_eq!(f.trust, Trust::User);

    // A new value supersedes: the old version stays in history with valid_to.
    let mut e = engine_with(vec![remember("Peter")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "call me Peter".into(),
    })
    .await
    .unwrap();
    let f = store.facts("global", "user.name").await.unwrap().remove(0);
    assert_eq!(f.value, serde_json::json!("Peter"));
    assert_eq!(f.uses, 5);
    assert!((f.confidence - 1.0).abs() < 1e-6);
    assert_eq!(f.valid_from, Timestamp(42));
    let hist = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[1].state, FactState::Superseded);
    assert_eq!(hist[1].valid_to, Some(Timestamp(42)));

    // An ungrounded value is flagged under the default policy: half
    // confidence, and a residual restatement only creeps up.
    let mut e = engine_with(vec![remember("Zed")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "whatever".into(),
    })
    .await
    .unwrap();
    let f = store.facts("global", "user.name").await.unwrap().remove(0);
    assert_eq!(f.value, serde_json::json!("Zed"));
    assert!((f.confidence - 0.5).abs() < 1e-6, "got {}", f.confidence);
    assert!(matches!(f.prov, Provenance::Residual));
    assert_eq!(
        f.valid_from,
        Timestamp(43),
        "coarse clock: still sorts after the previous version"
    );
    let mut e = engine_with(vec![remember("Zed")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "ok".into(),
    })
    .await
    .unwrap();
    let f = store.facts("global", "user.name").await.unwrap().remove(0);
    assert!((f.confidence - 0.6).abs() < 1e-6, "got {}", f.confidence);
    assert_eq!(
        store
            .fact_history("global", "user.name")
            .await
            .unwrap()
            .len(),
        3
    );
}

/// Renders the fact views exactly as the cloud replier would.
struct FactLinesProbe;
#[async_trait::async_trait]
impl Replier for FactLinesProbe {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        Ok(ctx
            .facts
            .iter()
            .map(nscore::render_fact)
            .collect::<Vec<_>>()
            .join(" | "))
    }
}

#[tokio::test]
async fn facts_in_context_are_pinned_plus_relevant_with_previous_values() {
    // Seen live: 20 facts by alphabet, and "what was my name before" was
    // unanswerable because the old value had been overwritten.
    let store = Arc::new(InMemoryStore::new());
    let put = |key: String, value: &'static str, at: u64| {
        let store = store.clone();
        async move {
            store
                .put_fact(Fact {
                    key,
                    value: serde_json::json!(value),
                    last_validated: Timestamp(at),
                    valid_from: Timestamp(at),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
    };
    put("user.name".into(), "Martin", 10).await;
    put("user.name".into(), "Peter", 20).await; // supersedes
    put("user.age".into(), "17", 15).await;
    put("order.42.status".into(), "shipped", 5).await;
    for i in 0..12 {
        put(format!("misc.{i:02}"), "noise", 1).await;
    }
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly
    b.set_replier(Box::new(FactLinesProbe));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let mut e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    );
    let reply = e
        .run_turn(Incoming {
            session: SessionId("sel".into()),
            text: "is my order shipped?".into(),
        })
        .await
        .unwrap();
    // pinned user.* first (newest validated first), the superseded value
    // shown, then the fact relevant to the question; the noise stays out.
    assert!(
        reply.starts_with(
            "user.name: \"Peter\" (was \"Martin\" until 00:00 UTC) | user.age: \"17\""
        ),
        "{reply}"
    );
    assert!(reply.contains("order.42.status: \"shipped\""), "{reply}");
    assert!(
        !reply.contains("misc."),
        "irrelevant facts are not shown: {reply}"
    );
    assert!(
        reply.matches(" | ").count() <= 9,
        "at most facts_in_context: {reply}"
    );
    // the shown facts were used; the noise was not
    let name = store.facts("global", "user.name").await.unwrap().remove(0);
    assert_eq!((name.uses, name.last_used), (1, Timestamp(42)));
    let noise = store.facts("global", "misc.00").await.unwrap().remove(0);
    assert_eq!(noise.uses, 0);
}

#[tokio::test]
async fn remember_fact_never_residual_policy_denies_ungrounded_values() {
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
        rationale: "".into(),
        action: "remember_fact".into(),
        args: serde_json::json!({"key": "memory_reset_requested", "value": "true"}),
    }])));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        remember_residual: nsengine::turn::RememberResidual::Never,
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let sid = SessionId("never".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "can you reset the memory".into(),
    })
    .await
    .unwrap();
    // Seen live: "true" was never said by the user and got stored anyway.
    assert!(store.facts("global", "").await.unwrap().is_empty());
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::GuardDenied { guard, reason }, .. }
            if guard == "residual_policy" && reason.contains("NeverResidual")
    )));
}

#[tokio::test]
async fn remember_fact_canonicalizes_key_spelling_variants() {
    let store = Arc::new(InMemoryStore::new());
    let remember = |key: &str| Proposal {
        rationale: "".into(),
        action: "remember_fact".into(),
        args: serde_json::json!({"key": key, "value": "Brno"}),
    };
    let sid = SessionId("canon".into());
    let mut e = engine_with(vec![remember("user.city")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "I live in Brno".into(),
    })
    .await
    .unwrap();
    let mut e = engine_with(vec![remember("User_City")], vec![], store.clone());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "Brno, as I said".into(),
    })
    .await
    .unwrap();
    let facts = store.facts("global", "").await.unwrap();
    assert_eq!(facts.len(), 1, "one key, not two spellings");
    assert_eq!(facts[0].key, "user.city");
    assert_eq!(facts[0].uses, 2, "restatement kept the count");
}

#[tokio::test]
async fn forget_fact_soft_deletes_and_unknown_key_is_malformed() {
    let store = Arc::new(InMemoryStore::new());
    store
        .put_fact(Fact {
            key: "user.name".into(),
            value: serde_json::json!("Martin"),
            valid_from: Timestamp(1),
            ..Default::default()
        })
        .await
        .unwrap();
    let forget = |key: &str| Proposal {
        rationale: "".into(),
        action: "forget_fact".into(),
        args: serde_json::json!({"key": key}),
    };
    let sid = SessionId("forget".into());
    let mut e = engine_with(
        vec![forget("user.nope"), forget("user.name")],
        vec![],
        store.clone(),
    );
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "forget my name".into(),
        })
        .await
        .unwrap();
    assert!(reply.contains("forgot user.name"), "{reply}");
    assert!(store.facts("global", "").await.unwrap().is_empty());
    let hist = store.fact_history("global", "user.name").await.unwrap();
    assert_eq!(hist[0].state, FactState::Forgotten);
    assert_eq!(hist[0].valid_to, Some(Timestamp(42)));
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected { reason: RejectReason::Malformed { detail }, .. }
            if detail.contains("no current fact named user.nope")
    )));
    assert_eq!(tool_calls(&events, "forget_fact"), 1);
}

#[tokio::test]
async fn forget_all_is_staged_then_purges_on_confirmation() {
    let store = Arc::new(InMemoryStore::new());
    for (k, v) in [("user.name", "Martin"), ("user.age", "17")] {
        store
            .put_fact(Fact {
                key: k.into(),
                value: serde_json::json!(v),
                valid_from: Timestamp(1),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let sid = SessionId("reset".into());
    let forget_all = || Proposal {
        rationale: "".into(),
        action: "forget_all".into(),
        args: serde_json::json!({}),
    };
    // Turn 1: staged, not executed (seen live: "reset the memory" stored a
    // junk fact instead).
    let mut e = engine_with(vec![forget_all()], vec![], store.clone());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "can you reset the memory".into(),
        })
        .await
        .unwrap();
    assert!(reply.contains("irreversible"), "{reply}");
    assert!(
        reply.contains("This will forget every stored fact in scope global."),
        "{reply}"
    );
    assert_eq!(store.facts("global", "").await.unwrap().len(), 2);
    // Turn 2: the user confirms; the purge runs and history is gone too.
    let mut e = engine_with(
        vec![Proposal {
            rationale: "".into(),
            action: "confirm_pending".into(),
            args: serde_json::json!({}),
        }],
        vec![],
        store.clone(),
    );
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "yes".into(),
        })
        .await
        .unwrap();
    assert!(reply.contains("forgot 2 facts"), "{reply}");
    assert!(store.facts("global", "").await.unwrap().is_empty());
    assert!(store
        .fact_history("global", "user.name")
        .await
        .unwrap()
        .is_empty());
    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    let pos = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
    assert!(pos("PendingConfirmation") < pos("Confirmed"));
    assert!(pos("Confirmed") < pos("ToolCalled"));
    // The recording replays clean with the synthetic actions.
    nsengine::replay::replay_session(sid, &events, vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn remember_fact_without_value_is_malformed() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: "bad".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": "user.name"}),
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("facts2".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "hi".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected {
            reason: RejectReason::Malformed { .. },
            ..
        }
    )));
    assert!(store.facts("global", "").await.unwrap().is_empty());
}

#[tokio::test]
async fn remember_fact_trims_stray_punctuation_from_key() {
    // Seen live: a model that reliably emits ":user.name". Edge punctuation
    // is normalized away (like the trim normalizer); the identifier survives.
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: "remember".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": ":user.name", "value": "Martin"}),
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("facts4".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "my name is Martin".into(),
    })
    .await
    .unwrap();
    let stored = store.facts("global", "user.name").await.unwrap();
    assert_eq!(
        stored.len(),
        1,
        "colon-prefixed key stored under the clean identifier"
    );
    assert_eq!(stored[0].key, "user.name");
}

fn rejection_reasons(events: &[Event]) -> Vec<&RejectReason> {
    events
        .iter()
        .filter_map(|ev| match &ev.kind {
            EventKind::Rejected { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
}

fn tool_calls(events: &[Event], action: &str) -> usize {
    events
        .iter()
        .filter(|ev| matches!(&ev.kind, EventKind::ToolCalled { action: a, .. } if a == action))
        .count()
}

#[tokio::test]
async fn identical_call_repeated_in_one_turn_is_denied_then_narrowed() {
    // Seen live with qwen2.5:3b: the emitter re-proposes the exact completed
    // call until max_iterations exhausts. An identical (action, args) pair
    // yields no new information — the engine must refuse it, and the
    // narrowed schema must then remove the action entirely.
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![
            echo_proposal("hi"),
            echo_proposal("hi"),
            echo_proposal("hi"),
        ],
        vec![],
        store.clone(),
    );
    let sid = SessionId("rep1".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "say hi".into(),
        })
        .await
        .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(tool_calls(&events, "echo"), 1, "the tool runs exactly once");
    let reasons = rejection_reasons(&events);
    assert!(
        matches!(reasons[0], RejectReason::GuardDenied { guard, .. } if guard == "repeat_gate"),
        "first repeat is denied by the repeat gate, got {:?}",
        reasons[0]
    );
    assert!(
        matches!(reasons[1], RejectReason::IllegalAction { .. }),
        "second repeat is illegal under the narrowed set, got {:?}",
        reasons[1]
    );
    assert!(
        reply.contains("echo: hi"),
        "turn still ends in a trace-based reply: {reply}"
    );
    assert!(matches!(
        &events.last().unwrap().kind,
        EventKind::Replied { .. }
    ));
}

#[tokio::test]
async fn same_action_with_different_args_is_not_a_repeat() {
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![echo_proposal("a"), echo_proposal("b")],
        vec![],
        store.clone(),
    );
    let sid = SessionId("rep2".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "x".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(tool_calls(&events, "echo"), 2);
    assert!(rejection_reasons(&events).is_empty());
}

#[tokio::test]
async fn remember_fact_repeated_in_one_turn_is_denied_then_narrowed() {
    let fact = || Proposal {
        rationale: "remember".into(),
        action: "remember_fact".into(),
        args: serde_json::json!({"key": "user.name", "value": "Martin"}),
    };
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(vec![fact(), fact(), fact()], vec![], store.clone());
    let sid = SessionId("rep3".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "my name is Martin".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(tool_calls(&events, "remember_fact"), 1);
    let reasons = rejection_reasons(&events);
    assert!(
        matches!(reasons[0], RejectReason::GuardDenied { guard, .. } if guard == "repeat_gate")
    );
    assert!(matches!(reasons[1], RejectReason::IllegalAction { .. }));
    assert!(matches!(
        &events.last().unwrap().kind,
        EventKind::Replied { .. }
    ));
}

#[tokio::test]
async fn remember_fact_with_junk_key_is_malformed() {
    // Degenerate model outputs (seen live: key ", ") must not become facts.
    let store = Arc::new(InMemoryStore::new());
    let mut e = engine_with(
        vec![Proposal {
            rationale: ", ".into(),
            action: "remember_fact".into(),
            args: serde_json::json!({"key": ", ", "value": ","}),
        }],
        vec![],
        store.clone(),
    );
    let sid = SessionId("facts3".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "my name is Martin".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::Rejected {
            reason: RejectReason::Malformed { .. },
            ..
        }
    )));
    assert!(
        store.facts("global", "").await.unwrap().is_empty(),
        "junk key must not be stored"
    );
}

struct FailingEmitter;
#[async_trait::async_trait]
impl Emitter for FailingEmitter {
    async fn propose(
        &self,
        _ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        Err(EmitError::Transport("down".into()))
    }
}

/// Emitter that always fails with the given transport detail.
struct EmitFailsWith(&'static str);

#[async_trait::async_trait]
impl Emitter for EmitFailsWith {
    async fn propose(
        &self,
        _ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        Err(EmitError::Transport(self.0.to_string()))
    }
}

/// Replier that always fails with the given transport detail.
struct ReplyFailsWith(&'static str);

#[async_trait::async_trait]
impl Replier for ReplyFailsWith {
    async fn reply(&self, _ctx: ReplyContext) -> Result<String, ReplyError> {
        Err(ReplyError::Transport(self.0.to_string()))
    }
}

fn engine_from(emitter: Box<dyn Emitter>, replier: Box<dyn Replier>, cfg: EngineConfig) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(emitter);
    b.set_replier(replier);
    b.set_memory(Arc::new(InMemoryStore::new()));
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)))
}

const RATE_LIMITED: &str =
    "status 429: {\"error\":{\"code\":429,\"message\":\"Rate limit exceeded: free-models-per-day\"}}";

#[tokio::test]
async fn fallback_reply_explains_provider_error() {
    // Seen live: a 429/402 from the provider surfaced as a bare "Sorry" and
    // the cause was only visible in the event log.
    let mut e = engine_from(
        Box::new(EmitFailsWith(RATE_LIMITED)),
        Box::new(ScriptedReplier),
        EngineConfig::default(),
    );
    let reply = e
        .run_turn(Incoming {
            session: SessionId("why1".into()),
            text: "hi".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.starts_with("Sorry, I couldn't complete that."),
        "{reply}"
    );
    assert!(reply.contains("429"), "status code is named: {reply}");
    assert!(
        reply.contains("Rate limit exceeded"),
        "provider message is quoted: {reply}"
    );
}

#[tokio::test]
async fn fallback_reply_explains_step_exhaustion() {
    let proposals = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|t| echo_proposal(t))
        .collect();
    let mut e = engine_from(
        Box::new(ScriptedEmitter::new(proposals)),
        Box::new(ScriptedReplier),
        EngineConfig::default(), // max_iterations = 5
    );
    let reply = e
        .run_turn(Incoming {
            session: SessionId("why2".into()),
            text: "go".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.starts_with("Sorry, I couldn't complete that."),
        "{reply}"
    );
    assert!(reply.contains("ran out of steps"), "{reply}");
}

#[tokio::test]
async fn generate_fallback_explains_replier_error_and_logs_reply_failed() {
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![]))); // respond_directly immediately
    b.set_replier(Box::new(ReplyFailsWith(
        "status 402: {\"error\":{\"message\":\"This request requires more credits\"}}",
    )));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let mut e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig::default(),
        Box::new(|| Timestamp(42)),
    );
    let sid = SessionId("why3".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "hi".into(),
        })
        .await
        .unwrap();
    assert!(
        reply.starts_with("Sorry, I couldn't complete that."),
        "{reply}"
    );
    assert!(
        reply.contains("402") && reply.contains("more credits"),
        "{reply}"
    );
    // F7: the failure is an event, placed between Settled and the fallback Replied.
    let events = store.load(&sid).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(&e.kind)).collect();
    assert_eq!(
        kinds,
        vec!["UserSaid", "Proposed", "Settled", "ReplyFailed", "Replied"]
    );
    assert!(events.iter().any(|ev| matches!(
        &ev.kind,
        EventKind::ReplyFailed { detail } if detail.contains("402")
    )));
    // Replay ignores infrastructure events: the recording still replays clean.
    nsengine::replay::replay_session(sid, &events, vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn cant_help_template_receives_reason_var() {
    let cfg = EngineConfig {
        templates: [("cant_help".to_string(), "Nezvládnu: {reason}".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let mut e = engine_from(
        Box::new(EmitFailsWith(RATE_LIMITED)),
        Box::new(ScriptedReplier),
        cfg,
    );
    let reply = e
        .run_turn(Incoming {
            session: SessionId("why4".into()),
            text: "hi".into(),
        })
        .await
        .unwrap();
    assert!(reply.starts_with("Nezvládnu: "), "{reply}");
    assert!(
        reply.contains("429") && reply.contains("Rate limit exceeded"),
        "{reply}"
    );
}

#[tokio::test]
async fn registered_cant_help_template_replaces_fallback() {
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(FailingEmitter));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        templates: [("cant_help".to_string(), "Promiň, to nezvládnu.".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let reply = e
        .run_turn(Incoming {
            session: SessionId("tpl1".into()),
            text: "x".into(),
        })
        .await
        .unwrap();
    assert_eq!(reply, "Promiň, to nezvládnu.");
}

#[tokio::test]
async fn tool_args_failing_schema_are_rejected_as_malformed_not_called() {
    let store = Arc::new(InMemoryStore::new());
    // echo requires a string "text"; an integer must be rejected before the tool runs.
    let bad = Proposal {
        rationale: "r".into(),
        action: "echo".into(),
        args: serde_json::json!({"text": 42}),
    };
    let mut e = engine_with(vec![bad, echo_proposal("ok")], vec![], store.clone());
    let sid = SessionId("val".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "go".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    let reasons = rejection_reasons(&events);
    assert_eq!(reasons.len(), 1);
    assert!(
        matches!(reasons[0], RejectReason::Malformed { detail } if detail.contains("must be string")),
        "got {:?}",
        reasons[0]
    );
    // The action stays legal: the repaired second proposal runs.
    assert_eq!(tool_calls(&events, "echo"), 1);
}

fn rules_handle(rules: LearnedRules) -> Arc<nsengine::arc_swap::ArcSwap<LearnedRules>> {
    Arc::new(nsengine::arc_swap::ArcSwap::from_pointee(rules))
}

fn engine_with_rules(
    proposals: Vec<Proposal>,
    store: Arc<InMemoryStore>,
    rules: LearnedRules,
) -> Engine {
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store);
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        learned: rules_handle(rules),
        ..EngineConfig::default()
    };
    Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)))
}

#[tokio::test]
async fn alias_action_rewrites_a_near_miss_name_but_proposed_event_keeps_the_raw_name() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        alias_action: vec![AliasAction {
            from: "eko".into(),
            to: "echo".into(),
        }],
        ..Default::default()
    };
    let p = Proposal {
        rationale: "r".into(),
        action: "eko".into(),
        args: serde_json::json!({"text": "hi"}),
    };
    let mut e = engine_with_rules(vec![p], store.clone(), rules);
    let sid = SessionId("alias".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "go".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::Proposed { proposal } if proposal.action == "eko")));
    assert_eq!(tool_calls(&events, "echo"), 1);
    assert!(rejection_reasons(&events).is_empty());
}

#[tokio::test]
async fn normalize_arg_repairs_args_before_validation() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        normalize_arg: vec![NormalizeArg {
            action: "echo".into(),
            arg: "text".into(),
            ops: vec![Op::Trim, Op::StripPunct],
        }],
        ..Default::default()
    };
    let p = Proposal {
        rationale: "r".into(),
        action: "echo".into(),
        args: serde_json::json!({"text": " \"hi\" "}),
    };
    let mut e = engine_with_rules(vec![p], store.clone(), rules);
    let sid = SessionId("norm".into());
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "go".into(),
        })
        .await
        .unwrap();
    assert!(reply.contains("echo: hi"), "{reply}");
}

#[tokio::test]
async fn aliased_action_still_goes_through_guards() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        alias_action: vec![AliasAction {
            from: "eko".into(),
            to: "echo".into(),
        }],
        ..Default::default()
    };
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
        rationale: "r".into(),
        action: "eko".into(),
        args: serde_json::json!({"text": "hi"}),
    }])));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    b.add_guard(Box::new(DenyAction {
        action: "echo".into(),
        reason: "no".into(),
    }));
    let cfg = EngineConfig {
        learned: rules_handle(rules),
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    let sid = SessionId("alias-guard".into());
    e.run_turn(Incoming {
        session: sid.clone(),
        text: "go".into(),
    })
    .await
    .unwrap();
    let events = store.load(&sid).await.unwrap();
    assert_eq!(tool_calls(&events, "echo"), 0);
    assert!(matches!(
        rejection_reasons(&events)[0],
        RejectReason::GuardDenied { .. }
    ));
}

#[tokio::test]
async fn guidance_reaches_the_emitter_scoped_to_legal_actions() {
    let store = Arc::new(InMemoryStore::new());
    let rules = LearnedRules {
        notes: vec![
            Note::new("global", "G", 0.0),
            Note::new("action:echo", "E", 0.0),
            Note::new("action:wipe", "W", 0.0),
        ],
        ..Default::default()
    };
    let seen: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
    let probe = GuidanceProbe(seen.clone());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(probe));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        learned: rules_handle(rules),
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    e.run_turn(Incoming {
        session: SessionId("g".into()),
        text: "go".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        seen.lock().unwrap()[0],
        vec!["G".to_string(), "E".to_string()]
    );
}

struct GuidanceProbe(Arc<std::sync::Mutex<Vec<Vec<String>>>>);
#[async_trait::async_trait]
impl Emitter for GuidanceProbe {
    async fn propose(
        &self,
        ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        self.0.lock().unwrap().push(ctx.guidance.clone());
        Ok(Proposal {
            rationale: "".into(),
            action: "respond_directly".into(),
            args: serde_json::json!({}),
        })
    }
}

enum Step {
    Say(&'static str),
    Idle,
}

/// Channel double: pops one step per recv. `Idle` sleeps long enough for the
/// engine's idle timeout to cancel the recv future (timeouts drop it).
struct ScriptedChannel(std::sync::Mutex<std::collections::VecDeque<Step>>);
#[async_trait::async_trait]
impl Channel for ScriptedChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        let next = self.0.lock().unwrap().pop_front();
        match next {
            Some(Step::Say(t)) => Ok(Incoming {
                session: SessionId("idle".into()),
                text: t.into(),
            }),
            Some(Step::Idle) => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Err(ChannelError::Closed)
            }
            None => Err(ChannelError::Closed),
        }
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

/// Consolidator double: counts runs and swaps an alias into the shared handle.
struct SwapIn {
    rules: Arc<nsengine::arc_swap::ArcSwap<LearnedRules>>,
    runs: Arc<std::sync::atomic::AtomicU32>,
}
#[async_trait::async_trait]
impl Consolidator for SwapIn {
    async fn run(&self, _store: &dyn MemoryStore) -> Result<(), StoreError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.rules.store(Arc::new(LearnedRules {
            alias_action: vec![AliasAction {
                from: "eko".into(),
                to: "echo".into(),
            }],
            ..Default::default()
        }));
        Ok(())
    }
}

#[tokio::test]
async fn idle_timer_runs_the_consolidator_once_per_quiet_period_with_new_turns() {
    let store = Arc::new(InMemoryStore::new());
    let rules = rules_handle(LearnedRules::default());
    let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut b = HarnessBuilder::new();
    // turn 1: respond directly; turn 2: propose the typo, which only works
    // once the alias is swapped in.
    b.set_emitter(Box::new(ScriptedEmitter::new(vec![
        Proposal {
            rationale: "".into(),
            action: "respond_directly".into(),
            args: serde_json::json!({}),
        },
        Proposal {
            rationale: "".into(),
            action: "eko".into(),
            args: serde_json::json!({"text": "hi"}),
        },
    ])));
    b.set_replier(Box::new(ScriptedReplier));
    b.set_memory(store.clone());
    b.set_channel(Box::new(ScriptedChannel(std::sync::Mutex::new(
        [
            Step::Say("one"),
            Step::Idle,
            Step::Say("two"),
            Step::Idle,
            Step::Idle,
        ]
        .into(),
    ))));
    b.set_consolidator(Box::new(SwapIn {
        rules: rules.clone(),
        runs: runs.clone(),
    }));
    b.add_tool(Arc::new(EchoTool::new()));
    let cfg = EngineConfig {
        learned: rules,
        idle_after: Some(std::time::Duration::from_millis(20)),
        ..EngineConfig::default()
    };
    let mut e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(1)));
    e.run().await.unwrap();
    // pass after "one", pass after "two", and NOT a third time (no turn in between).
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    let events = store.load(&SessionId("idle".into())).await.unwrap();
    assert_eq!(
        tool_calls(&events, "echo"),
        1,
        "turn two saw the swapped-in alias"
    );
}
