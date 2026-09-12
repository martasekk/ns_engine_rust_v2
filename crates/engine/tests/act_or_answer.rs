//! M12 T4.3: the chat-tier act-or-answer path.
//!
//! Its own file rather than four more tests at the end of `turn_loop.rs`:
//! every one of them needs the same chat-tier harness with a counting
//! replier and a usage-recording emitter probe, and that harness is the
//! subject here.

use nscore::*;
use nsengine::script::*;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Records what the engine pushed to the user before the turn returned
/// (M13 T4.1). A discarding channel cannot tell an interim line that was
/// delivered from one that was only logged.
struct RecordingChannel(Arc<Mutex<Vec<String>>>);
#[async_trait::async_trait]
impl Channel for RecordingChannel {
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&self, _s: &SessionId, t: &str) -> Result<(), ChannelError> {
        self.0.lock().expect("sent").push(t.to_string());
        Ok(())
    }
}

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

/// Records whether each call was offered the answer blocks, and books a
/// nominal cost so the call leaves a `ModelCall` — a scripted emitter
/// records none, and P2's `ArrayProbe` solved this the same way.
struct AnswerProbe {
    inner: ScriptedEmitter,
    answer_offered: Arc<Mutex<Vec<bool>>>,
}

#[async_trait::async_trait]
impl Emitter for AnswerProbe {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        Ok(self.propose_or_answer(ctx, legal).await?.proposal)
    }

    async fn propose_or_answer(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Emission, EmitError> {
        self.answer_offered
            .lock()
            .expect("answer_offered")
            .push(ctx.answer.is_some());
        if let Some(sink) = ctx.usage.as_deref() {
            sink.record(Usage {
                role: "emitter".into(),
                model: "probe".into(),
                prompt_tokens: 10,
                completion_tokens: 1,
                estimated: true,
                attempts: 1,
                latency_ms: 0,
                tools_tokens: 1,
                cached_tokens: 0,
            });
        }
        self.inner.propose_or_answer(ctx, legal).await
    }
}

/// Counts its calls and says something the grounding check cannot fault.
/// Books no cost, so every `ModelCall` in these logs is an emitter call.
struct CountingReplier(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Replier for CountingReplier {
    async fn reply(&self, _ctx: ReplyContext) -> Result<String, ReplyError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("the replier drafted this".into())
    }
}

struct Run {
    reply: String,
    events: Vec<Event>,
    answer_offered: Vec<bool>,
    reply_calls: usize,
    /// Lines the engine pushed mid-turn, in order.
    sent: Vec<String>,
}

impl Run {
    fn kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| kind_name(&e.kind)).collect()
    }
    fn model_calls(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ModelCall { .. }))
            .count()
    }
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
        EventKind::Said { .. } => "Said",
        EventKind::Replied { .. } => "Replied",
        EventKind::ReplyFailed { .. } => "ReplyFailed",
        EventKind::ReplyFlagged { .. } => "ReplyFlagged",
        EventKind::ReplyEchoed { .. } => "ReplyEchoed",
        EventKind::ReplyCited { .. } => "ReplyCited",
        EventKind::Summarized { .. } => "Summarized",
        EventKind::ModelCall { .. } => "ModelCall",
        EventKind::Graded { .. } => "Graded",
    }
}

fn answers(text: &str) -> Emission {
    Emission {
        proposal: Proposal {
            rationale: format!("{} {text}", nscore::ANSWERED_IN_EMITTER_PREFIX),
            action: "respond_directly".into(),
            args: serde_json::json!({}),
        },
        answer: Some(text.into()),
        say: None,
    }
}

fn acts(action: &str, args: serde_json::Value) -> Emission {
    Emission {
        proposal: Proposal {
            rationale: "because".into(),
            action: action.into(),
            args,
        },
        answer: None,
        say: None,
    }
}

/// A router pinned to one tier, so a test says which tier it is about
/// instead of arranging cues that happen to land there.
struct FixedRouter(Tier);
impl nsengine::router::Router for FixedRouter {
    fn route(&self, _input: &nsengine::router::RouteInput<'_>) -> nsengine::router::Route {
        nsengine::router::Route {
            tier: self.0,
            cues: vec!["fixed".into()],
            tools: None,
        }
    }
}

async fn run(name: &str, emissions: Vec<Emission>, cfg: EngineConfig) -> Run {
    run_at(Tier::Chat, name, emissions, cfg).await
}

async fn run_at(tier: Tier, name: &str, emissions: Vec<Emission>, cfg: EngineConfig) -> Run {
    let store = Arc::new(InMemoryStore::new());
    let sid = SessionId(name.into());
    let answer_offered = Arc::new(Mutex::new(Vec::new()));
    let reply_calls = Arc::new(AtomicUsize::new(0));
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(AnswerProbe {
        inner: ScriptedEmitter::answering(emissions),
        answer_offered: Arc::clone(&answer_offered),
    }));
    b.set_replier(Box::new(CountingReplier(Arc::clone(&reply_calls))));
    b.set_memory(store.clone());
    let sent = Arc::new(Mutex::new(Vec::new()));
    b.set_channel(Box::new(RecordingChannel(Arc::clone(&sent))));
    b.set_consolidator(Box::new(NoopConsolidator));
    b.add_tool(Arc::new(EchoTool::new()));
    let e = Engine::with_clock(
        b.build().unwrap(),
        EngineConfig {
            router: Some(Arc::new(FixedRouter(tier))),
            ..cfg
        },
        Box::new(|| Timestamp(42)),
    );
    let reply = e
        .run_turn(Incoming {
            session: sid.clone(),
            text: "what is the time?".into(),
        })
        .await
        .unwrap();
    let taken = answer_offered.lock().expect("answer_offered").clone();
    let delivered = sent.lock().expect("sent").clone();
    Run {
        reply,
        events: store.load(&sid).await.unwrap(),
        answer_offered: taken,
        reply_calls: reply_calls.load(Ordering::SeqCst),
        sent: delivered,
    }
}

fn on() -> EngineConfig {
    EngineConfig {
        chat_act_or_answer: true,
        max_echo_ratio: 1.1,
        reply_grounding_check: false,
        ..EngineConfig::default()
    }
}

/// The whole point of the phase: a chat turn for one request. And the log
/// has to be the log a replier turn would have written, or every downstream
/// reader — replay, the graders, `ns-app budget` — reads it as a different
/// kind of turn.
#[tokio::test]
async fn an_emitted_answer_is_the_reply_and_costs_one_request() {
    let r = run("aoa-one", vec![answers("It is 10:41.")], on()).await;
    assert_eq!(r.reply, "It is 10:41.");
    assert_eq!(r.reply_calls, 0, "the replier was called anyway");
    assert_eq!(r.model_calls(), 1, "{:?}", r.kinds());
    assert_eq!(r.answer_offered, vec![true]);
    // The same shape as a replier turn: Settled { Generate }, then Replied.
    assert_eq!(
        r.kinds(),
        vec!["UserSaid", "ModelCall", "Proposed", "Settled", "Replied"]
    );
    assert!(r.events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Settled {
            policy: ReplyPolicy::Generate
        }
    )));
    assert!(r
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::Replied { text } if text == "It is 10:41.")));
    // And the rationale carries the prefix the counter reads.
    assert!(r.events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Proposed { proposal }
            if proposal.rationale.starts_with(nscore::ANSWERED_IN_EMITTER_PREFIX)
    )));
}

/// M13 T3.1: one call that does both. The action is the proposal, the text
/// rides beside it.
fn acts_and_answers(action: &str, args: serde_json::Value, text: &str) -> Emission {
    Emission {
        proposal: Proposal {
            rationale: format!("{} {text}", nscore::ANSWERED_IN_EMITTER_PREFIX),
            action: action.into(),
            args,
        },
        answer: Some(text.into()),
        say: None,
    }
}

/// M13 T4.1: an action with a line said now, the turn carrying on.
fn says_and_acts(action: &str, args: serde_json::Value, say: &str) -> Emission {
    Emission {
        proposal: Proposal {
            rationale: "checking first".into(),
            action: action.into(),
            args,
        },
        answer: None,
        say: Some(say.into()),
    }
}

fn both() -> EngineConfig {
    EngineConfig {
        act_or_answer_every_tier: true,
        act_and_answer: true,
        ..on()
    }
}

/// M13 T3.1. "Do this, and tell me you did" is the commonest task turn there
/// is, and act-or-answer could not express it: one call for the action, a
/// second to say what the first did. One call now buys both.
#[tokio::test]
async fn an_action_and_an_answer_in_one_call_cost_one_request() {
    let r = run_at(
        Tier::Task,
        "aoa-both",
        vec![acts_and_answers(
            "echo",
            serde_json::json!({"text": "hi"}),
            "Sending that now.",
        )],
        both(),
    )
    .await;
    assert_eq!(r.reply, "Sending that now.");
    assert_eq!(r.reply_calls, 0, "the replier was called anyway");
    assert_eq!(r.model_calls(), 1, "{:?}", r.kinds());
    // The action really ran, and the turn settled after it rather than on
    // the strength of the text alone.
    assert!(r
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ToolReturned { .. })));
    let order = r.kinds();
    let returned = order.iter().position(|k| *k == "ToolReturned").unwrap();
    let settled = order.iter().position(|k| *k == "Settled").unwrap();
    assert!(returned < settled, "{order:?}");
}

/// M13 T4.1. "Wait, let me check the database for that product" — said now,
/// the turn carrying on, the answer written afterwards from what came back.
/// The case neither an answer nor a bare action could express.
#[tokio::test]
async fn a_said_line_reaches_the_user_and_the_turn_keeps_looping() {
    let r = run_at(
        Tier::Task,
        "aoa-say",
        vec![
            says_and_acts(
                "echo",
                serde_json::json!({"text": "10:41"}),
                "Hold on, let me check that.",
            ),
            answers("It is 10:41."),
        ],
        both(),
    )
    .await;
    // The user heard the interim line first and the answer second.
    assert_eq!(r.sent, vec!["Hold on, let me check that."]);
    assert_eq!(r.reply, "It is 10:41.");
    // Said, then the action, then the answer: the loop went round.
    assert_eq!(
        r.kinds(),
        vec![
            "UserSaid",
            "ModelCall",
            "Proposed",
            "Said",
            "ToolCalled",
            "ToolReturned",
            "ModelCall",
            "Proposed",
            "Settled",
            "Replied",
        ]
    );
    // It is not a second reply. One turn, one answer, whatever else was said.
    assert_eq!(
        r.events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::Replied { .. }))
            .count(),
        1
    );
    assert_eq!(r.reply_calls, 0);
    assert_eq!(r.model_calls(), 2);
}

/// And the next iteration can see it was said, so the model does not
/// announce the same check twice.
#[tokio::test]
async fn a_said_line_enters_the_turns_own_trace() {
    let r = run_at(
        Tier::Task,
        "aoa-say-trace",
        vec![
            says_and_acts(
                "echo",
                serde_json::json!({"text": "10:41"}),
                "Checking now.",
            ),
            answers("Done."),
        ],
        both(),
    )
    .await;
    let folded = nsengine::state::fold(&r.events);
    let did: Vec<&String> = folded
        .records
        .iter()
        .flat_map(|rec| rec.did.iter())
        .collect();
    assert!(
        did.iter()
            .any(|l| l.contains("said to the user: Checking now.")),
        "{did:?}"
    );
}

/// And the guard rail on it: the text was written before the action ran, so
/// a refused action must take its sentence with it. Otherwise the user is
/// told something happened that did not.
#[tokio::test]
async fn an_answer_beside_a_refused_action_is_dropped() {
    let r = run_at(
        Tier::Task,
        "aoa-both-refused",
        vec![
            acts_and_answers("no_such_tool", serde_json::json!({}), "Done, all sorted."),
            answers("I could not do that."),
        ],
        both(),
    )
    .await;
    assert_eq!(r.reply, "I could not do that.");
    assert_ne!(r.reply, "Done, all sorted.");
    assert_eq!(r.model_calls(), 2, "{:?}", r.kinds());
    assert!(r
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::Rejected { .. })));
}

/// M13 T2.1. The loop's exit becomes the model's own call: act, see the
/// result, then decide the turn is over and write the reply — two requests
/// for a task turn that used to cost three, and the replier never runs.
#[tokio::test]
async fn a_task_turn_can_act_then_answer_in_the_loop() {
    let cfg = EngineConfig {
        act_or_answer_every_tier: true,
        ..on()
    };
    let r = run_at(
        Tier::Task,
        "aoa-task",
        vec![
            acts("echo", serde_json::json!({"text": "10:41"})),
            answers("It is 10:41."),
        ],
        cfg,
    )
    .await;
    assert_eq!(r.reply, "It is 10:41.");
    assert_eq!(r.reply_calls, 0, "the replier was called anyway");
    assert_eq!(r.model_calls(), 2, "{:?}", r.kinds());
    // Both iterations were offered the choice, including the one that acted.
    assert_eq!(r.answer_offered, vec![true, true]);
    assert!(r.events.iter().any(|e| matches!(
        &e.kind,
        EventKind::Settled {
            policy: ReplyPolicy::Generate
        }
    )));
    assert!(r
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ToolReturned { .. })));
}

/// And the boundary M12 drew is still the default: a task turn is not
/// offered the answer unless this deployment asked for it, so the shape of
/// every task turn that ships today is unchanged.
#[tokio::test]
async fn a_task_turn_is_not_offered_the_answer_by_default() {
    let r = run_at(
        Tier::Task,
        "aoa-task-off",
        vec![
            acts("echo", serde_json::json!({"text": "10:41"})),
            answers("It is 10:41."),
        ],
        on(),
    )
    .await;
    assert_eq!(r.answer_offered, vec![false, false]);
    assert_eq!(r.reply_calls, 1, "the replier still narrates the task turn");
}

/// An emitted answer is a draft like any other and is owed the same check:
/// the cheaper path must not also be the unwatched one.
#[tokio::test]
async fn an_emitted_answer_that_is_ungrounded_is_still_flagged() {
    let cfg = || EngineConfig {
        reply_grounding_check: true,
        ..on()
    };
    let r = run(
        "aoa-flagged",
        vec![answers("You have 42 orders waiting in Oslo.")],
        cfg(),
    )
    .await;
    assert!(r.kinds().contains(&"ReplyFlagged"), "{:?}", r.kinds());
    assert!(r.events.iter().any(|e| matches!(
        &e.kind,
        EventKind::ReplyFlagged { draft, spans }
            if draft == "You have 42 orders waiting in Oslo." && spans == &["42", "Oslo"]
    )));
    // reply_regenerate defaults on: the replier regenerates exactly once,
    // and its draft is what the user is told — the same rule a replier's own
    // flagged draft gets.
    assert_eq!(r.reply_calls, 1);
    assert_eq!(r.reply, "the replier drafted this");

    // With regeneration stood down (M12 T1.2) the flag stays and the
    // emitted answer stands as written.
    let strong = run(
        "aoa-flagged-strong",
        vec![answers("You have 42 orders waiting in Oslo.")],
        EngineConfig {
            reply_regenerate: false,
            ..cfg()
        },
    )
    .await;
    assert!(strong.kinds().contains(&"ReplyFlagged"));
    assert_eq!(strong.reply_calls, 0);
    assert_eq!(strong.reply, "You have 42 orders waiting in Oslo.");
}

/// The saving is only available to a turn that had nothing to do. A chat
/// turn that reaches for a tool pays the emitter twice and the replier
/// once, exactly as it did before M12.
#[tokio::test]
async fn a_chat_turn_that_proposes_a_tool_still_costs_two_calls() {
    let r = run(
        "aoa-tool",
        vec![
            acts(
                "remember_fact",
                serde_json::json!({"key": "user.name", "value": "Martin"}),
            ),
            Emission {
                proposal: Proposal {
                    rationale: "done".into(),
                    action: "respond_directly".into(),
                    args: serde_json::json!({}),
                },
                answer: None,
                say: None,
            },
        ],
        on(),
    )
    .await;
    // Two emitter calls; the replier books no cost, so the count is theirs.
    assert_eq!(r.model_calls(), 2, "{:?}", r.kinds());
    assert_eq!(r.reply_calls, 1, "the replier still drafts");
    assert_eq!(r.reply, "the replier drafted this");
    assert_eq!(r.answer_offered, vec![true, true]);
    assert!(r.kinds().contains(&"ToolCalled"), "{:?}", r.kinds());
}

/// Off is off: not "the emitter may answer and the engine ignores it" but
/// "the emitter is never offered the choice", because the offer is what
/// changes the request bytes.
#[tokio::test]
async fn with_the_knob_off_an_answering_emitter_is_never_asked() {
    let r = run(
        "aoa-off",
        vec![answers("It is 10:41.")],
        EngineConfig {
            max_echo_ratio: 1.1,
            reply_grounding_check: false,
            ..EngineConfig::default()
        },
    )
    .await;
    assert_eq!(r.answer_offered, vec![false]);
    // The scripted answer still comes back — and is not used, because a
    // turn that was never offered the choice did not make one.
    assert_eq!(r.reply_calls, 1);
    assert_eq!(r.reply, "the replier drafted this");
}

/// Books a nominal cost so a replier call lands as a `ModelCall` too: the
/// cap counts every role's requests, not the emitter's alone.
struct BillingReplier;
#[async_trait::async_trait]
impl Replier for BillingReplier {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        if let Some(sink) = ctx.usage.as_deref() {
            sink.record(Usage {
                role: "replier".into(),
                model: "probe".into(),
                prompt_tokens: 10,
                completion_tokens: 1,
                estimated: true,
                attempts: 1,
                latency_ms: 0,
                tools_tokens: 0,
                cached_tokens: 0,
            });
        }
        Ok("the replier drafted this".into())
    }
}

/// M12 T6.1: a metered session stops itself. Two turns at two requests each
/// against a cap of two: the first turn runs to the end, the second is
/// refused before it calls anything.
#[tokio::test]
async fn the_engine_stops_at_the_request_cap() {
    async fn two_turns(max_requests: Option<u32>) -> (Result<String, String>, usize) {
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("cap".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(AnswerProbe {
            inner: ScriptedEmitter::answering(vec![
                acts("respond_directly", serde_json::json!({})),
                acts("respond_directly", serde_json::json!({})),
            ]),
            answer_offered: Arc::new(Mutex::new(Vec::new())),
        }));
        b.set_replier(Box::new(BillingReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig {
                max_requests,
                max_echo_ratio: 1.1,
                reply_grounding_check: false,
                summary_every_turns: 0,
                ..EngineConfig::default()
            },
            Box::new(|| Timestamp(42)),
        );
        let say = |text: &str| Incoming {
            session: sid.clone(),
            text: text.into(),
        };
        let first = e.run_turn(say("first")).await;
        assert!(first.is_ok(), "the first turn must complete: {first:?}");
        let second = e.run_turn(say("second")).await.map_err(|e| e.to_string());
        let calls = store
            .load(&sid)
            .await
            .unwrap()
            .iter()
            .filter(|ev| matches!(ev.kind, EventKind::ModelCall { .. }))
            .count();
        (second, calls)
    }

    let (second, calls) = two_turns(Some(2)).await;
    let err = second.expect_err("the second turn must be refused");
    assert!(
        err.contains("request cap 2") && err.contains("2 requests"),
        "{err}"
    );
    assert_eq!(calls, 2, "the refused turn must not call anything");

    // Uncapped, the same session runs both turns.
    let (second, calls) = two_turns(None).await;
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(calls, 4);
}
