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
    }
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
        self.seen.lock().unwrap().push(ctx.state_summary.clone());
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
    let stored = store.facts("user").await.unwrap();
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
    assert!(store.facts("").await.unwrap().is_empty());
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
    let stored = store.facts("user.name").await.unwrap();
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
        store.facts("").await.unwrap().is_empty(),
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
async fn generate_fallback_explains_replier_error() {
    let mut e = engine_from(
        Box::new(ScriptedEmitter::new(vec![])), // respond_directly immediately
        Box::new(ReplyFailsWith(
            "status 402: {\"error\":{\"message\":\"This request requires more credits\"}}",
        )),
        EngineConfig::default(),
    );
    let reply = e
        .run_turn(Incoming {
            session: SessionId("why3".into()),
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
