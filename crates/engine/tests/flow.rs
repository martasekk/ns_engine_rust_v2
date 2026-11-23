//! The engine obeys a flow (docs/flows-spec.md, "What a flow does in a turn"):
//! a flow narrows which registered tools are offered, and names the roles
//! still to collect in the guidance.

use nscore::flow::FlowDef;
use nscore::*;
use nsengine::script::*;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct NullChannel;
#[async_trait::async_trait]
impl Channel for NullChannel {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

/// Per emitter call: the legal action names, and the guidance lines shown.
type Seen = Arc<std::sync::Mutex<Vec<(Vec<String>, Vec<String>)>>>;

/// Records, for each emitter call, the legal action names and the guidance
/// lines the emitter was shown, then answers `respond_directly`.
struct FlowProbe(Seen);
#[async_trait::async_trait]
impl Emitter for FlowProbe {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        let names = legal.actions.iter().map(|a| a.name.clone()).collect();
        self.0.lock().unwrap().push((names, ctx.guidance.clone()));
        Ok(Proposal {
            rationale: "".into(),
            action: "respond_directly".into(),
            args: serde_json::json!({}),
        })
    }
}

/// A second registered tool, so a test can tell which of two a turn was offered.
struct OtherTool {
    spec: ActionSpec,
}

impl OtherTool {
    fn new() -> Self {
        Self {
            spec: ActionSpec {
                name: "other".into(),
                description: "another tool".into(),
                args_schema: serde_json::json!({"type": "object", "properties": {}}),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for OtherTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: "other".into(),
            artifact: None,
            trust: Trust::System,
        })
    }
}

/// One action, offered only once `user.city` is known.
fn city_flow() -> FlowDef {
    serde_json::from_value(serde_json::json!({
        "id": "f",
        "version": 1,
        "roles": {"city": "user.city"},
        "required_roles": ["city"],
        "actions": [{
            "name": "say",
            "component": "echo",
            "reads": [],
            "writes": [],
            "write_scope": "conversation",
            "args": {},
            "when": {"when": "roles_filled", "value": ["city"]}
        }],
        "fast_path": [],
        "reply_template": null
    }))
    .unwrap()
}

/// The legal action names and guidance of the first emitter call of one turn.
async fn first_call(flow: Option<Arc<FlowDef>>, city_known: bool) -> (Vec<String>, Vec<String>) {
    let store = Arc::new(InMemoryStore::new());
    if city_known {
        store
            .put_fact(Fact {
                key: "user.city".into(),
                value: serde_json::json!("Oslo"),
                confidence: 1.0,
                last_validated: Timestamp(1),
                prov: Provenance::Constant,
                valid_from: Timestamp(1),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let seen: Seen = Default::default();
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(FlowProbe(seen.clone())));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    b.add_tool(Arc::new(OtherTool::new()));
    let cfg = EngineConfig {
        flow,
        ..EngineConfig::default()
    };
    let e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    e.run_turn(Incoming {
        session: SessionId("flow".into()),
        text: "go".into(),
    })
    .await
    .unwrap();
    let first = seen.lock().unwrap()[0].clone();
    first
}

#[tokio::test]
async fn no_flow_offers_every_tool() {
    let (names, guidance) = first_call(None, false).await;
    assert!(names.contains(&"echo".to_string()), "{names:?}");
    assert!(names.contains(&"other".to_string()), "{names:?}");
    assert!(
        !guidance.iter().any(|g| g.starts_with("Flow ")),
        "{guidance:?}"
    );
}

#[tokio::test]
async fn a_flow_hides_tools_it_does_not_name_and_says_what_is_missing() {
    let (names, guidance) = first_call(Some(Arc::new(city_flow())), false).await;
    assert!(!names.contains(&"echo".to_string()), "{names:?}");
    assert!(!names.contains(&"other".to_string()), "{names:?}");
    assert!(
        names.contains(&"ask_clarification".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"remember_fact".to_string()), "{names:?}");
    assert_eq!(
        guidance[0],
        "Flow f: still needed: city (key user.city). Ask the user for one at a time; when they give one, store it with remember_fact under the key shown."
    );
}

#[tokio::test]
async fn a_filled_role_unlocks_its_action() {
    let (names, guidance) = first_call(Some(Arc::new(city_flow())), true).await;
    assert!(names.contains(&"echo".to_string()), "{names:?}");
    assert!(!names.contains(&"other".to_string()), "{names:?}");
    assert!(
        !guidance.iter().any(|g| g.starts_with("Flow ")),
        "{guidance:?}"
    );
}

/// What one emitter call saw: the user text of its turn, the legal names and the guidance.
type Call = (String, Vec<String>, Vec<String>);

/// The first emitter call remembers the city; every later call answers directly.
/// Records each call in order.
struct RememberThenAnswer {
    calls: Arc<std::sync::Mutex<Vec<Call>>>,
    count: AtomicUsize,
}
#[async_trait::async_trait]
impl Emitter for RememberThenAnswer {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        let names = legal.actions.iter().map(|a| a.name.clone()).collect();
        self.calls
            .lock()
            .unwrap()
            .push((ctx.user_text.clone(), names, ctx.guidance.clone()));
        let (action, args) = if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
            (
                "remember_fact",
                serde_json::json!({"key": "user.city", "value": "Prague"}),
            )
        } else {
            ("respond_directly", serde_json::json!({}))
        };
        Ok(Proposal {
            rationale: "".into(),
            action: action.into(),
            args,
        })
    }
}

#[tokio::test]
async fn remembering_the_named_key_unlocks_the_action_next_turn() {
    let store = Arc::new(InMemoryStore::new());
    let calls: Arc<std::sync::Mutex<Vec<Call>>> = Default::default();
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(RememberThenAnswer {
        calls: calls.clone(),
        count: AtomicUsize::new(0),
    }));
    b.set_memory(store.clone());
    b.set_channel(Box::new(NullChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    b.add_tool(Arc::new(OtherTool::new()));
    let cfg = EngineConfig {
        flow: Some(Arc::new(city_flow())),
        ..EngineConfig::default()
    };
    let e = Engine::with_clock(b.build().unwrap(), cfg, Box::new(|| Timestamp(42)));
    for text in ["I live in Prague", "what can you do"] {
        e.run_turn(Incoming {
            session: SessionId("flow".into()),
            text: text.into(),
        })
        .await
        .unwrap();
    }
    let calls = calls.lock().unwrap().clone();
    assert!(calls.len() >= 3, "{calls:?}");

    // Turn 1: the city is still missing, so neither tool is offered.
    let (text, names, guidance) = &calls[0];
    assert_eq!(text, "I live in Prague");
    assert!(!names.contains(&"echo".to_string()), "{names:?}");
    assert!(!names.contains(&"other".to_string()), "{names:?}");
    assert!(
        guidance[0].starts_with("Flow f: still needed: city"),
        "{guidance:?}"
    );

    // The remembered fact is stored under the key the flow names.
    let facts = store.facts("global", "user.city").await.unwrap();
    assert!(
        facts
            .iter()
            .any(|f| f.key == "user.city" && f.value == serde_json::json!("Prague")),
        "{facts:?}"
    );

    // Turn 2: the first call of that turn offers the unlocked action.
    let (_, names, guidance) = calls
        .iter()
        .find(|(text, _, _)| text == "what can you do")
        .expect("turn 2 made an emitter call");
    assert!(names.contains(&"echo".to_string()), "{names:?}");
    assert!(!names.contains(&"other".to_string()), "{names:?}");
    assert!(
        !guidance.iter().any(|g| g.starts_with("Flow ")),
        "{guidance:?}"
    );
}
