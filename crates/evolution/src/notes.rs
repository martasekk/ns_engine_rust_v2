//! Notes lane (spec M5 §3.3): guidance sentences proposed by an LLM from a
//! failed turn's trace, then gated GRASP-style — a live probe re-runs held-out
//! sessions with and without the note; the note must help a positive and hurt
//! at most `regression_budget` negatives.
use crate::mine::is_fallback;
use async_trait::async_trait;
use nscore::{
    ActionSpec, ChannelError, Emitter, Event, EventKind, HarnessBuilder, Incoming, LearnedRules,
    MemoryStore, Note, SessionId, Timestamp,
};
use nsengine::replay::doubles_from;
use nsengine::script::ScriptedReplier;
use nsengine::store::{InMemoryStore, NoopConsolidator};
use nsengine::turn::{Engine, EngineConfig};
use std::collections::BTreeMap;
use std::sync::Arc;

#[async_trait]
pub trait NoteProposer: Send + Sync {
    /// One imperative sentence that would have changed the failing turn, or
    /// None if nothing helps.
    async fn propose(&self, trace: &str, existing: &[Note]) -> Result<Option<Note>, String>;
}

pub struct ClientNoteProposer {
    pub client: nsllm::client::OpenRouterClient,
    pub model: String,
}

const PROPOSER_SYSTEM: &str = "You tune the action-selection prompt of a tool-using assistant. \
You see the trace of ONE failed turn and the guidance notes already in force. Reply with JSON \
only: {\"scope\": \"global\" | \"action:<tool name>\", \"text\": \"<one imperative sentence>\"} \
if a single new sentence would have changed the outcome and does not repeat an existing note; \
otherwise {\"none\": true}.";

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

#[async_trait]
impl NoteProposer for ClientNoteProposer {
    async fn propose(&self, trace: &str, existing: &[Note]) -> Result<Option<Note>, String> {
        let mut user = String::from("Existing notes:\n");
        for n in existing {
            user.push_str(&format!("- [{}] {}\n", n.scope, n.text));
        }
        user.push_str("\nFailed turn:\n");
        user.push_str(trace);
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": 300,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": PROPOSER_SYSTEM},
                {"role": "user", "content": user},
            ],
        });
        let body = self.client.chat(request).await.map_err(|e| e.to_string())?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let v: serde_json::Value = serde_json::from_str(strip_fence(&content))
            .map_err(|e| format!("proposer returned non-JSON: {e}: {content}"))?;
        if v.get("none").and_then(|b| b.as_bool()) == Some(true) {
            return Ok(None);
        }
        let scope = v["scope"].as_str().unwrap_or("").to_string();
        let text = v["text"].as_str().unwrap_or("").trim().to_string();
        let scope_ok = scope == "global"
            || scope
                .strip_prefix("action:")
                .map(|n| !n.is_empty())
                .unwrap_or(false);
        if !scope_ok || text.is_empty() || text.chars().count() > 200 {
            return Err(format!("proposer returned an invalid note: {v}"));
        }
        Ok(Some(Note::new(&scope, &text, 0.0)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Ok,
    Fallback,
    Rejections(u32),
}

/// Ok=0 > Rejections(n)=-n > Fallback=-1000.
pub fn score(o: TurnOutcome) -> i64 {
    match o {
        TurnOutcome::Ok => 0,
        TurnOutcome::Rejections(n) => -(n as i64),
        TurnOutcome::Fallback => -1000,
    }
}

/// One outcome per turn, in turn order.
pub fn classify_turns(events: &[Event]) -> Vec<TurnOutcome> {
    let mut per_turn: BTreeMap<u32, (bool, u32)> = BTreeMap::new();
    for e in events {
        let entry = per_turn.entry(e.turn).or_insert((false, 0));
        match &e.kind {
            EventKind::Settled { policy } if is_fallback(policy) => entry.0 = true,
            EventKind::ReplyFailed { .. } => entry.0 = true,
            EventKind::Rejected { .. } => entry.1 += 1,
            _ => {}
        }
    }
    per_turn
        .into_values()
        .map(|(fallback, rejections)| {
            if fallback {
                TurnOutcome::Fallback
            } else if rejections > 0 {
                TurnOutcome::Rejections(rejections)
            } else {
                TurnOutcome::Ok
            }
        })
        .collect()
}

#[async_trait]
pub trait ProbeRunner: Send + Sync {
    /// Re-run a recorded session's user inputs through a LIVE emitter with
    /// recorded tool outcomes replayed.
    async fn run(
        &self,
        recorded: &[Event],
        rules: Arc<LearnedRules>,
    ) -> Result<Vec<TurnOutcome>, String>;
}

pub type EmitterFactory = Arc<dyn Fn() -> Box<dyn Emitter> + Send + Sync>;

pub struct LiveProbe {
    pub emitter: EmitterFactory,
    pub known_specs: Vec<ActionSpec>,
    pub persona: String,
}

struct ClosedChannel;
#[async_trait]
impl nscore::Channel for ClosedChannel {
    async fn recv(&mut self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
        Ok(())
    }
}

#[async_trait]
impl ProbeRunner for LiveProbe {
    async fn run(
        &self,
        recorded: &[Event],
        rules: Arc<LearnedRules>,
    ) -> Result<Vec<TurnOutcome>, String> {
        let d = doubles_from(recorded, &self.known_specs, true);
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId("probe".into());
        let mut b = HarnessBuilder::new();
        b.set_emitter((self.emitter)());
        // The reply text is irrelevant to classification.
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(ClosedChannel));
        b.set_consolidator(Box::new(NoopConsolidator));
        for t in d.tools {
            b.add_tool(t);
        }
        let parts = b.build().map_err(|e| e.to_string())?;
        let cfg = EngineConfig {
            persona: self.persona.clone(),
            learned: Arc::new(arc_swap::ArcSwap::new(rules)),
            reply_grounding_check: false,
            ..EngineConfig::default()
        };
        let mut engine = Engine::with_clock(parts, cfg, Box::new(|| Timestamp(0)));
        for text in d.user_inputs {
            engine
                .run_turn(Incoming {
                    session: sid.clone(),
                    text,
                })
                .await
                .map_err(|e| e.to_string())?;
        }
        let events = store.load(&sid).await.map_err(|e| e.to_string())?;
        Ok(classify_turns(&events))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteVerdict {
    pub accepted: bool,
    pub improved: usize,
    pub regressed: usize,
    pub lift: f64,
    pub turns_used: u32,
    pub detail: String,
}

pub async fn verify_note(
    note: &Note,
    positives: &[Vec<Event>],
    negatives: &[Vec<Event>],
    base: &LearnedRules,
    probe: &dyn ProbeRunner,
    regression_budget: u32,
    budget: &mut u32,
) -> NoteVerdict {
    let without = Arc::new(base.clone());
    let mut with_rules = base.clone();
    with_rules.notes.push(note.clone());
    let with = Arc::new(with_rules);
    let mut v = NoteVerdict {
        accepted: false,
        improved: 0,
        regressed: 0,
        lift: 0.0,
        turns_used: 0,
        detail: String::new(),
    };
    let mut positives_probed = 0usize;

    // A session costs 2× its turns: one run without the note, one with.
    let mut probe_pair = |events: &Vec<Event>| -> Option<u32> {
        let cost = 2 * turns_in(events);
        if cost == 0 || *budget < cost {
            return None;
        }
        *budget -= cost;
        Some(cost)
    };

    let queue = positives
        .iter()
        .map(|e| (true, e))
        .chain(negatives.iter().map(|e| (false, e)));
    for (is_positive, events) in queue {
        let Some(cost) = probe_pair(events) else {
            v.detail.push_str("budget exhausted; ");
            break;
        };
        v.turns_used += cost;
        let a = probe.run(events, without.clone()).await;
        let b = probe.run(events, with.clone()).await;
        match (a, b) {
            (Ok(a), Ok(b)) => {
                let sa: i64 = a.iter().copied().map(score).sum();
                let sb: i64 = b.iter().copied().map(score).sum();
                if is_positive {
                    positives_probed += 1;
                    if sb > sa {
                        v.improved += 1;
                    }
                } else if sb < sa {
                    v.regressed += 1;
                }
            }
            (Err(e), _) | (_, Err(e)) => v.detail.push_str(&format!("probe error: {e}; ")),
        }
    }
    v.accepted = v.improved >= 1 && v.regressed <= regression_budget as usize;
    v.lift = if positives_probed == 0 {
        0.0
    } else {
        (v.improved as f64 - v.regressed as f64) / positives_probed as f64
    };
    v
}

fn turns_in(events: &[Event]) -> u32 {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::UserSaid { .. }))
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn turn_log(turns: &[&[EventKind]]) -> Vec<Event> {
        let mut l = EventLog::new(SessionId("p".into()));
        for (i, kinds) in turns.iter().enumerate() {
            for k in kinds.iter() {
                l.append(i as u32 + 1, Timestamp(1), k.clone());
            }
        }
        l.events().to_vec()
    }

    #[test]
    fn classify_turns_orders_fallback_below_rejections_below_ok() {
        let fb = EventKind::Settled {
            policy: ReplyPolicy::Verbatim {
                text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY),
            },
        };
        let rej = EventKind::Rejected {
            proposal_of: EventId(1),
            reason: RejectReason::IllegalAction { action: "x".into() },
        };
        let ok = EventKind::Settled {
            policy: ReplyPolicy::Generate,
        };
        let ev = turn_log(&[
            &[EventKind::UserSaid { text: "a".into() }, ok.clone()],
            &[
                EventKind::UserSaid { text: "b".into() },
                rej.clone(),
                rej.clone(),
                ok.clone(),
            ],
            &[EventKind::UserSaid { text: "c".into() }, rej, fb],
        ]);
        assert_eq!(
            classify_turns(&ev),
            vec![
                TurnOutcome::Ok,
                TurnOutcome::Rejections(2),
                TurnOutcome::Fallback
            ]
        );
        assert!(score(TurnOutcome::Ok) > score(TurnOutcome::Rejections(1)));
        assert!(score(TurnOutcome::Rejections(1)) > score(TurnOutcome::Rejections(3)));
        assert!(score(TurnOutcome::Rejections(3)) > score(TurnOutcome::Fallback));
    }

    /// Probe double: outcome depends on whether the rules carry a note containing MARKER.
    struct MarkerProbe {
        with_marker: Vec<TurnOutcome>,
        without: Vec<TurnOutcome>,
        negatives_regress: bool,
    }
    const MARKER: &str = "CALL THE TOOL";
    #[async_trait::async_trait]
    impl ProbeRunner for MarkerProbe {
        async fn run(
            &self,
            recorded: &[Event],
            rules: Arc<LearnedRules>,
        ) -> Result<Vec<TurnOutcome>, String> {
            let has = rules.notes.iter().any(|n| n.text.contains(MARKER));
            let is_negative = recorded
                .iter()
                .any(|e| matches!(&e.kind, EventKind::UserSaid { text } if text == "negative"));
            Ok(if is_negative {
                if has && self.negatives_regress {
                    vec![TurnOutcome::Fallback]
                } else {
                    vec![TurnOutcome::Ok]
                }
            } else if has {
                self.with_marker.clone()
            } else {
                self.without.clone()
            })
        }
    }

    fn session(text: &str) -> Vec<Event> {
        turn_log(&[&[
            EventKind::UserSaid { text: text.into() },
            EventKind::Settled {
                policy: ReplyPolicy::Generate,
            },
        ]])
    }

    #[tokio::test]
    async fn note_that_helps_a_positive_and_hurts_no_negative_is_accepted_with_lift() {
        let probe = MarkerProbe {
            with_marker: vec![TurnOutcome::Ok],
            without: vec![TurnOutcome::Fallback],
            negatives_regress: false,
        };
        let note = Note::new("global", &format!("{MARKER} before answering."), 0.0);
        let mut budget = 40;
        let v = verify_note(
            &note,
            &[session("positive")],
            &[session("negative")],
            &LearnedRules::default(),
            &probe,
            0,
            &mut budget,
        )
        .await;
        assert!(v.accepted, "{}", v.detail);
        assert_eq!((v.improved, v.regressed), (1, 0));
        assert!((v.lift - 1.0).abs() < 1e-9);
        assert_eq!(v.turns_used, 4);
        assert_eq!(budget, 36);
    }

    #[tokio::test]
    async fn note_that_breaks_a_negative_is_rejected_under_budget_zero_and_accepted_under_one() {
        let probe = MarkerProbe {
            with_marker: vec![TurnOutcome::Ok],
            without: vec![TurnOutcome::Fallback],
            negatives_regress: true,
        };
        let note = Note::new("global", &format!("{MARKER} always."), 0.0);
        let mut b = 40;
        let v0 = verify_note(
            &note,
            &[session("positive")],
            &[session("negative")],
            &LearnedRules::default(),
            &probe,
            0,
            &mut b,
        )
        .await;
        assert!(!v0.accepted && v0.regressed == 1);
        let mut b = 40;
        let v1 = verify_note(
            &note,
            &[session("positive")],
            &[session("negative")],
            &LearnedRules::default(),
            &probe,
            1,
            &mut b,
        )
        .await;
        assert!(v1.accepted);
        assert!((v1.lift - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn probe_budget_stops_probing_and_leaves_the_note_unaccepted() {
        let probe = MarkerProbe {
            with_marker: vec![TurnOutcome::Ok],
            without: vec![TurnOutcome::Fallback],
            negatives_regress: false,
        };
        let note = Note::new("global", &format!("{MARKER}."), 0.0);
        let mut budget = 1;
        let v = verify_note(
            &note,
            &[session("positive")],
            &[],
            &LearnedRules::default(),
            &probe,
            0,
            &mut budget,
        )
        .await;
        assert!(!v.accepted);
        assert!(v.detail.contains("budget exhausted"));
        assert_eq!(budget, 1);
    }

    /// Emitter double for LiveProbe: with MARKER in guidance it calls echo once
    /// then responds directly; without it, it proposes an illegal action every
    /// time (→ max_iterations → fallback). Fresh per factory call, so the
    /// "called once" flag is per probe run.
    struct GuidedEmitter(std::sync::atomic::AtomicBool);
    #[async_trait::async_trait]
    impl Emitter for GuidedEmitter {
        async fn propose(
            &self,
            ctx: EmitterContext,
            _l: &LegalActionSet,
        ) -> Result<Proposal, EmitError> {
            let guided = ctx.guidance.iter().any(|g| g.contains(MARKER));
            let action = if !guided {
                "nope"
            } else if !self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                "echo"
            } else {
                "respond_directly"
            };
            Ok(Proposal {
                rationale: "".into(),
                action: action.into(),
                args: serde_json::json!({"text": "hi"}),
            })
        }
    }

    #[tokio::test]
    async fn live_probe_runs_user_inputs_through_the_factory_emitter_with_rules() {
        let recorded = turn_log(&[&[
            EventKind::UserSaid {
                text: "say hi".into(),
            },
            EventKind::Settled {
                policy: ReplyPolicy::Generate,
            },
        ]]);
        let probe = LiveProbe {
            emitter: Arc::new(|| Box::new(GuidedEmitter(Default::default())) as Box<dyn Emitter>),
            known_specs: vec![nsengine::script::EchoTool::new().spec().clone()],
            persona: String::new(),
        };
        let without = probe
            .run(&recorded, Arc::new(LearnedRules::default()))
            .await
            .unwrap();
        let with = probe
            .run(
                &recorded,
                Arc::new(LearnedRules {
                    notes: vec![Note::new("global", MARKER, 0.0)],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(without[0], TurnOutcome::Fallback);
        assert_eq!(with[0], TurnOutcome::Ok);
    }

    #[tokio::test]
    async fn client_proposer_parses_json_and_none() {
        use nsllm::transport::{HttpResponse, MockTransport};
        let reply = |content: &str| HttpResponse {
            status: 200,
            body: serde_json::json!({"choices": [{"message": {"content": content}}]}),
        };
        let mock = MockTransport::new(vec![
            Ok(reply(
                "```json\n{\"scope\": \"action:echo\", \"text\": \"Call echo when asked to repeat.\"}\n```",
            )),
            Ok(reply("{\"none\": true}")),
            Ok(reply("{\"scope\": \"bogus\", \"text\": \"x\"}")),
        ]);
        let p = ClientNoteProposer {
            client: nsllm::client::OpenRouterClient::new(mock.clone(), "k".into()),
            model: "m".into(),
        };
        let n = p.propose("trace", &[]).await.unwrap().unwrap();
        assert_eq!(
            (n.scope.as_str(), n.text.as_str()),
            ("action:echo", "Call echo when asked to repeat.")
        );
        assert_eq!(p.propose("trace", &[]).await.unwrap(), None);
        assert!(p.propose("trace", &[]).await.is_err());
        let sent = mock.requests.lock().unwrap()[0].clone();
        assert!(sent["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Failed turn:\ntrace"));
    }
}
