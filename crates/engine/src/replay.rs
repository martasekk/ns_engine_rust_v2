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
    ActionSpec, Channel, ChannelError, Event, EventKind, EventLog, Guard, HarnessBuilder, Incoming,
    LearnedRules, MemoryStore, Proposal, Replier, ReplyContext, ReplyError, ReplyPolicy, SessionId,
    SideEffect, Timestamp, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, Trust,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("recorded chain broken: {0}")]
    ChainBroken(String),
    #[error("divergence at line {at}: expected `{expected}`, got `{got}`")]
    Divergence {
        at: usize,
        expected: String,
        got: String,
    },
    #[error("replay produced {got} lines, recording has {expected}")]
    LengthMismatch { expected: usize, got: usize },
    #[error("engine error during replay: {0}")]
    Engine(String),
}

/// One normalized line per behavioral event: kind + salient payload.
/// Timestamps and hashes are legitimately different on replay and excluded;
/// so are infrastructure events (`ReplyFailed`), which a replay with recorded
/// doubles cannot reproduce.
pub fn normalize(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ReplyFailed { .. }
            | EventKind::ReplyFlagged { .. }
            | EventKind::ReplyEchoed { .. }
            | EventKind::Summarized { .. }
            | EventKind::ModelCall { .. } => None,
            other => Some(normalize_kind(other)),
        })
        .collect()
}

fn normalize_kind(kind: &EventKind) -> String {
    match kind {
        EventKind::ReplyFailed { .. }
        | EventKind::ReplyFlagged { .. }
        | EventKind::ReplyEchoed { .. }
        | EventKind::Summarized { .. }
        | EventKind::ModelCall { .. } => {
            unreachable!("filtered by normalize")
        }
        EventKind::UserSaid { text } => format!("UserSaid {text}"),
        EventKind::Proposed { proposal } => format!("Proposed {}", proposal.action),
        EventKind::Rejected { reason, .. } => {
            let variant = match reason {
                nscore::RejectReason::Malformed { .. } => "Malformed",
                nscore::RejectReason::ProviderUnavailable { .. } => "ProviderUnavailable",
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
    }
}

/// What a replay runs with, beyond the recording itself (spec M5 §4.1).
pub struct ReplayOptions {
    /// Learned rules under test; default: none.
    pub learned: Arc<LearnedRules>,
    /// Plugin guards the recording ran with.
    pub extra_guards: Vec<Box<dyn Guard>>,
    /// Production tool specs: doubles carry the real spec so legality and
    /// validation match production, even for tools the recording never called.
    pub known_specs: Vec<ActionSpec>,
    /// A call with no recorded outcome returns a synthetic Ok instead of
    /// failing — used to check whether a candidate rule "flips" a failure.
    pub synthetic_ok_for_new_calls: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            learned: Arc::new(LearnedRules::default()),
            extra_guards: vec![],
            known_specs: vec![],
            synthetic_ok_for_new_calls: false,
        }
    }
}

pub struct Replayed {
    pub events: Vec<Event>,
}

/// Scripted doubles reconstructed from a recording.
pub struct Doubles {
    pub user_inputs: Vec<String>,
    pub proposals: Vec<Proposal>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub replier: Box<dyn Replier>,
}

/// Engine-synthetic actions never need tool doubles.
const SYNTHETIC: [&str; 6] = [
    "remember_fact",
    "ask_clarification",
    "confirm_pending",
    "forget_fact",
    "forget_all",
    "recall",
];

/// Replays one recorded tool: pops the recorded outcomes front-to-back.
struct ReplayTool {
    spec: ActionSpec,
    outcomes: Mutex<VecDeque<ToolOutcome>>,
    synthetic_ok: bool,
}

impl ReplayTool {
    fn new(spec: ActionSpec, outcomes: VecDeque<ToolOutcome>, synthetic_ok: bool) -> Self {
        Self {
            spec,
            outcomes: Mutex::new(outcomes),
            synthetic_ok,
        }
    }
    fn dummy_spec(action: &str) -> ActionSpec {
        ActionSpec {
            name: action.into(),
            description: format!("replay double for {action}"),
            args_schema: serde_json::json!({"type": "object", "properties": {}}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }
}

#[async_trait]
impl Tool for ReplayTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }
    async fn call(&self, _a: &serde_json::Value, _c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        match self
            .outcomes
            .lock()
            .expect("replay outcomes lock")
            .pop_front()
        {
            Some(ToolOutcome::Ok { output }) => Ok(output),
            Some(ToolOutcome::Err { kind, detail }) => Err(ToolError::Failed { kind, detail }),
            None if self.synthetic_ok => Ok(ToolOutput {
                summary: "(verified: synthetic ok)".into(),
                artifact: None,
                trust: Trust::System,
            }),
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

/// Reconstruct scripted doubles from a recording. Every known spec gets a
/// double carrying the REAL spec (legality/validation match production);
/// recorded actions without a known spec get a permissive dummy spec.
pub fn doubles_from(recorded: &[Event], known_specs: &[ActionSpec], synthetic_ok: bool) -> Doubles {
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
                    outcomes
                        .entry(action.clone())
                        .or_default()
                        .push_back(outcome.clone());
                }
            }
            EventKind::Settled { policy } => {
                last_settled_per_turn.insert(e.turn, policy.clone());
            }
            EventKind::Replied { text } => {
                if matches!(
                    last_settled_per_turn.get(&e.turn),
                    Some(ReplyPolicy::Generate)
                ) {
                    generated_replies.push_back(text.clone());
                }
            }
            _ => {}
        }
    }
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for spec in known_specs {
        if SYNTHETIC.contains(&spec.name.as_str()) {
            continue;
        }
        let queue = outcomes.remove(&spec.name).unwrap_or_default();
        seen.insert(spec.name.clone());
        tools.push(Arc::new(ReplayTool::new(spec.clone(), queue, synthetic_ok)));
    }
    for (action, queue) in outcomes {
        if SYNTHETIC.contains(&action.as_str()) || seen.contains(&action) {
            continue;
        }
        tools.push(Arc::new(ReplayTool::new(
            ReplayTool::dummy_spec(&action),
            queue,
            synthetic_ok,
        )));
    }
    Doubles {
        user_inputs,
        proposals,
        tools,
        replier: Box::new(QueueReplier {
            texts: Mutex::new(generated_replies),
        }),
    }
}

/// Re-feed a recorded session through the engine under `opts` and return
/// the re-run events (no diff). Any behavioral change — engine, guards, or
/// learned rules — shows up in the returned log.
pub async fn replay_with(
    session: SessionId,
    recorded: &[Event],
    opts: ReplayOptions,
) -> Result<Replayed, ReplayError> {
    // 1. The recording itself must be intact.
    EventLog::from_events(session.clone(), recorded.to_vec())
        .verify_chain()
        .map_err(|e| ReplayError::ChainBroken(e.to_string()))?;

    // 2. Assemble a fresh harness over the doubles.
    let d = doubles_from(recorded, &opts.known_specs, opts.synthetic_ok_for_new_calls);
    let store = Arc::new(InMemoryStore::new());
    let mut b = HarnessBuilder::new();
    b.set_emitter(Box::new(ScriptedEmitter::new(d.proposals)));
    b.set_replier(d.replier);
    b.set_memory(store.clone());
    b.set_channel(Box::new(ReplayChannel));
    b.set_consolidator(Box::new(NoopConsolidator));
    for t in d.tools {
        b.add_tool(t);
    }
    for g in opts.extra_guards {
        b.add_guard(g);
    }
    let parts = b.build().map_err(|e| ReplayError::Engine(e.to_string()))?;
    // The interceptor is a live-only mechanism whose outcome the recording
    // already carries (the final Replied); with doubles it must stay off or
    // a re-flag would drain the reply queue.
    let cfg = EngineConfig {
        learned: Arc::new(arc_swap::ArcSwap::new(opts.learned)),
        reply_grounding_check: false,
        ..EngineConfig::default()
    };
    let engine = Engine::with_clock(parts, cfg, Box::new(|| Timestamp(0)));

    // 3. Re-feed the user inputs.
    for text in d.user_inputs {
        engine
            .run_turn(Incoming {
                session: session.clone(),
                text,
            })
            .await
            .map_err(|e| ReplayError::Engine(e.to_string()))?;
    }
    let events = store
        .load(&session)
        .await
        .map_err(|e| ReplayError::Engine(e.to_string()))?;
    Ok(Replayed { events })
}

/// Diff normalized lines: first divergence wins, then length.
pub fn diff(recorded: &[Event], replayed: &[Event]) -> Result<(), ReplayError> {
    let expected = normalize(recorded);
    let got = normalize(replayed);
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
        return Err(ReplayError::LengthMismatch {
            expected: expected.len(),
            got: got.len(),
        });
    }
    Ok(())
}

/// Faithful replay: re-run under the recording's guards and no learned rules,
/// then diff against the recording. This is the M4 regression check.
pub async fn replay_session(
    session: SessionId,
    recorded: &[Event],
    extra_guards: Vec<Box<dyn Guard>>,
) -> Result<(), ReplayError> {
    let opts = ReplayOptions {
        extra_guards,
        ..Default::default()
    };
    let r = replay_with(session, recorded, opts).await?;
    diff(recorded, &r.events)
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
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(42)),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "please replay me".into(),
        })
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
        let guard: Box<dyn Guard> = Box::new(DenyAction {
            action: "echo".into(),
            reason: "no".into(),
        });
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

    #[tokio::test]
    async fn replay_with_returns_events_and_replay_session_is_its_wrapper() {
        let (sid, events) = record_session(vec![]).await;
        let r = replay_with(sid.clone(), &events, ReplayOptions::default())
            .await
            .unwrap();
        assert_eq!(normalize(&r.events), normalize(&events));
        assert!(diff(&events, &r.events).is_ok());
        replay_session(sid, &events, vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn learned_alias_flips_a_recorded_illegal_action_into_a_call() {
        // Record a session where the emitter proposed "eko" (illegal) then gave up.
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("flip".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(vec![Proposal {
            rationale: "typo".into(),
            action: "eko".into(),
            args: serde_json::json!({"text": "hi"}),
        }])));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        let e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(1)),
        );
        e.run_turn(Incoming {
            session: sid.clone(),
            text: "say hi".into(),
        })
        .await
        .unwrap();
        let recorded = store.load(&sid).await.unwrap();
        let rejected_at = normalize(&recorded)
            .iter()
            .position(|l| l == "Rejected IllegalAction")
            .unwrap();

        // echo was never called in the recording, so the double only exists
        // because known_specs carries it; the call has no recorded outcome.
        let opts = ReplayOptions {
            learned: Arc::new(nscore::LearnedRules {
                alias_action: vec![nscore::AliasAction {
                    from: "eko".into(),
                    to: "echo".into(),
                }],
                ..Default::default()
            }),
            known_specs: vec![EchoTool::new().spec().clone()],
            synthetic_ok_for_new_calls: true,
            ..Default::default()
        };
        let r = replay_with(sid, &recorded, opts).await.unwrap();
        assert_eq!(normalize(&r.events)[rejected_at], "ToolCalled echo");
        assert_eq!(normalize(&r.events)[rejected_at + 1], "ToolReturned Ok");
    }

    #[tokio::test]
    async fn without_synthetic_ok_a_new_call_errors() {
        let (_sid, events) = record_session(vec![]).await;
        // Strip the recorded echo outcome so the double's queue is empty
        // (the chain is broken now, so build doubles directly, no replay).
        let stripped: Vec<Event> = events
            .iter()
            .filter(|e| !matches!(e.kind, EventKind::ToolReturned { .. }))
            .cloned()
            .collect();
        let ctx = || ToolCtx {
            session: SessionId("s".into()),
            artifacts: None,
        };
        let d = doubles_from(&stripped, &[EchoTool::new().spec().clone()], false);
        let echo = d.tools.iter().find(|t| t.spec().name == "echo").unwrap();
        let out = echo.call(&serde_json::json!({"text": "x"}), &ctx()).await;
        assert!(
            matches!(out, Err(ToolError::Failed { ref detail, .. }) if detail.contains("exhausted"))
        );
        let d = doubles_from(&stripped, &[EchoTool::new().spec().clone()], true);
        let echo = d.tools.iter().find(|t| t.spec().name == "echo").unwrap();
        let out = echo
            .call(&serde_json::json!({"text": "x"}), &ctx())
            .await
            .unwrap();
        assert!(out.summary.contains("synthetic ok"));
    }
}
