//! Replay harness (spec §11): a recorded session log is the fixture. The
//! recorded proposals drive a scripted emitter, recorded tool outcomes drive
//! replay tools, recorded generated replies drive a queue replier — then the
//! re-run log is normalized and diffed against the recording. This is the
//! hard dependency of all future self-improvement (verified-before-write).

use crate::script::ScriptedEmitter;
use crate::store::{InMemoryStore, NoopConsolidator};
use crate::turn::{Engine, EngineConfig};
use async_trait::async_trait;
use nscore::{
    ActionSpec, Channel, ChannelError, Event, EventKind, EventLog, Guard, HarnessBuilder,
    Incoming, MemoryStore, Proposal, Replier, ReplyContext, ReplyError, ReplyPolicy, SessionId,
    SideEffect, Timestamp, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("recorded chain broken: {0}")]
    ChainBroken(String),
    #[error("divergence at line {at}: expected `{expected}`, got `{got}`")]
    Divergence { at: usize, expected: String, got: String },
    #[error("replay produced {got} lines, recording has {expected}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("engine error during replay: {0}")]
    Engine(String),
}

/// One normalized line per event: kind + salient payload. Timestamps and
/// hashes are legitimately different on replay and excluded.
pub fn normalize(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.kind {
            EventKind::UserSaid { text } => format!("UserSaid {text}"),
            EventKind::Proposed { proposal } => format!("Proposed {}", proposal.action),
            EventKind::Rejected { reason, .. } => {
                let variant = match reason {
                    nscore::RejectReason::Malformed { .. } => "Malformed",
                    nscore::RejectReason::IllegalAction { .. } => "IllegalAction",
                    nscore::RejectReason::GuardDenied { .. } => "GuardDenied",
                };
                format!("Rejected {variant}")
            }
            EventKind::ToolCalled { action, .. } => format!("ToolCalled {action}"),
            EventKind::ToolReturned { outcome, .. } => match outcome {
                ToolOutcome::Ok { .. } => "ToolReturned Ok".into(),
                ToolOutcome::Err { .. } => "ToolReturned Err".into(),
            },
            EventKind::PendingConfirmation { .. } => "PendingConfirmation".into(),
            EventKind::Confirmed { .. } => "Confirmed".into(),
            EventKind::Corrected { .. } => "Corrected".into(),
            EventKind::Settled { policy } => {
                let p = match policy {
                    ReplyPolicy::Verbatim { .. } => "Verbatim",
                    ReplyPolicy::Template { .. } => "Template",
                    ReplyPolicy::Generate => "Generate",
                };
                format!("Settled {p}")
            }
            EventKind::Replied { text } => format!("Replied {text}"),
        })
        .collect()
}

/// Replays one recorded tool: pops the recorded outcomes front-to-back.
struct ReplayTool {
    spec: ActionSpec,
    outcomes: Mutex<VecDeque<ToolOutcome>>,
}

impl ReplayTool {
    fn new(action: &str, outcomes: VecDeque<ToolOutcome>) -> Self {
        Self {
            spec: ActionSpec {
                name: action.into(),
                description: format!("replay double for {action}"),
                args_schema: serde_json::json!({"type": "object", "properties": {}}),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
            outcomes: Mutex::new(outcomes),
        }
    }
}

#[async_trait]
impl Tool for ReplayTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        match self.outcomes.lock().expect("replay outcomes lock").pop_front() {
            Some(ToolOutcome::Ok { output }) => Ok(output),
            Some(ToolOutcome::Err { kind, detail }) => Err(ToolError::Failed { kind, detail }),
            None => Err(ToolError::Failed {
                kind: "replay".into(),
                detail: "outcome queue exhausted".into(),
            }),
        }
    }
}

/// Replays the recorded GENERATED replies front-to-back (Verbatim/Template
/// replies are reproduced by the engine itself and never reach the replier).
struct QueueReplier {
    texts: Mutex<VecDeque<String>>,
}

#[async_trait]
impl Replier for QueueReplier {
    async fn reply(&self, _ctx: ReplyContext) -> Result<String, ReplyError> {
        self.texts
            .lock()
            .expect("replay replier lock")
            .pop_front()
            .ok_or_else(|| ReplyError::Transport("replay reply queue exhausted".into()))
    }
}

struct ReplayChannel;

#[async_trait]
impl Channel for ReplayChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

/// Re-feed a recorded session through the engine. Any behavioral change in
/// the engine (guards, narrowing, settling) surfaces as a Divergence.
/// `extra_guards` reproduces the plugin guards the recording ran with.
pub async fn replay_session(
    session: SessionId,
    recorded: &[Event],
    extra_guards: Vec<Box<dyn Guard>>,
) -> Result<(), ReplayError> {
    // 1. The recording itself must be intact.
    EventLog::from_events(session.clone(), recorded.to_vec())
        .verify_chain()
        .map_err(|e| ReplayError::ChainBroken(e.to_string()))?;

    // 2. Reconstruct the scripted doubles from the recording.
    let mut user_inputs: Vec<String> = Vec::new();
    let mut proposals: Vec<Proposal> = Vec::new();
    let mut call_actions: HashMap<u64, String> = HashMap::new(); // call event id -> action
    let mut outcomes: HashMap<String, VecDeque<ToolOutcome>> = HashMap::new();
    let mut last_settled_per_turn: HashMap<u32, ReplyPolicy> = HashMap::new();
    let mut generated_replies: VecDeque<String> = VecDeque::new();
    for e in recorded {
        match &e.kind {
            EventKind::UserSaid { text } => user_inputs.push(text.clone()),
            EventKind::Proposed { proposal } => proposals.push(proposal.clone()),
            EventKind::ToolCalled { action, .. } => {
                call_actions.insert(e.id.0, action.clone());
            }
            EventKind::ToolReturned { call, outcome } => {
                if let Some(action) = call_actions.get(&call.0) {
                    outcomes.entry(action.clone()).or_default().push_back(outcome.clone());
                }
            }
            EventKind::Settled { policy } => {
                last_settled_per_turn.insert(e.turn, policy.clone());
            }
            EventKind::Replied { text } => {
                if matches!(last_settled_per_turn.get(&e.turn), Some(ReplyPolicy::Generate)) {
                    generated_replies.push_back(text.clone());
                }
            }
            _ => {}
        }
    }

    // 3. Assemble a fresh harness over the doubles.
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(proposals)));
    b.set_replier(Box::new(QueueReplier { texts: Mutex::new(generated_replies) }));
    b.set_memory(store.clone());
    b.set_channel(Box::new(ReplayChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    // remember_fact/ask_clarification/confirm_pending are engine-synthetic;
    // only real tool calls need doubles.
    let synthetic = ["remember_fact", "ask_clarification", "confirm_pending"];
    for (action, queue) in outcomes {
        if !synthetic.contains(&action.as_str()) {
            b.add_tool(Arc::new(ReplayTool::new(&action, queue)));
        }
    }
    for g in extra_guards {
        b.add_guard(g);
    }
    let parts = b.build().map_err(|e| ReplayError::Engine(e.to_string()))?;
    let mut engine =
        Engine::with_clock(parts, EngineConfig::default(), Box::new(|| Timestamp(0)));

    // 4. Re-feed the user inputs.
    for text in user_inputs {
        engine
            .run_turn(Incoming { session: session.clone(), text })
            .await
            .map_err(|e| ReplayError::Engine(e.to_string()))?;
    }

    // 5. Diff normalized lines.
    let replayed = store.load(&session).await.map_err(|e| ReplayError::Engine(e.to_string()))?;
    let expected = normalize(recorded);
    let got = normalize(&replayed);
    for (at, (want, have)) in expected.iter().zip(got.iter()).enumerate() {
        if want != have {
            return Err(ReplayError::Divergence {
                at,
                expected: want.clone(),
                got: have.clone(),
            });
        }
    }
    if expected.len() != got.len() {
        return Err(ReplayError::LengthMismatch { expected: expected.len(), got: got.len() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{DenyAction, EchoTool, ScriptedReplier};

    struct ClosedChannel;
    #[async_trait::async_trait]
    impl Channel for ClosedChannel {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    async fn record_session(guards: Vec<Box<dyn Guard>>) -> (SessionId, Vec<Event>) {
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("rec".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "echo".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "replay me"}),
        }])));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        for g in guards {
            b.add_guard(g);
        }
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        e.run_turn(Incoming { session: sid.clone(), text: "please replay me".into() })
            .await
            .unwrap();
        let events = store.load(&sid).await.unwrap();
        (sid, events)
    }

    #[tokio::test]
    async fn faithful_replay_of_a_recorded_session_passes() {
        let (sid, events) = record_session(vec![]).await;
        replay_session(sid, &events, vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn replay_detects_behavioral_divergence() {
        // Recorded WITH a guard that denied echo; replayed WITHOUT it,
        // the engine now executes echo instead of rejecting -> divergence.
        let guard: Box<dyn Guard> =
            Box::new(DenyAction { action: "echo".into(), reason: "no".into() });
        let (sid, events) = record_session(vec![guard]).await;
        let err = replay_session(sid, &events, vec![]).await.unwrap_err();
        assert!(matches!(
            err,
            ReplayError::Divergence { .. } | ReplayError::LengthMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn replay_refuses_a_tampered_recording() {
        let (sid, mut events) = record_session(vec![]).await;
        if let EventKind::UserSaid { text } = &mut events[0].kind {
            *text = "TAMPERED".into();
        }
        let err = replay_session(sid, &events, vec![]).await.unwrap_err();
        assert!(matches!(err, ReplayError::ChainBroken(_)));
    }
}
