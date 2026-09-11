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

/// What the proposer is being asked for about one turn (M9 T5.1).
///
/// Two asks, one gate. The ask changes the *question* put to the proposer —
/// nothing downstream of it: the candidate that comes back is a `Note` like
/// any other and goes through [`verify_note`] unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// The turn went wrong; what would have changed the outcome.
    Failure,
    /// The turn went right and its route repeats; what would make the
    /// emitter reach that route sooner.
    Strategy { actions: String, times: u32 },
}

#[async_trait]
pub trait NoteProposer: Send + Sync {
    /// One imperative sentence that would have changed the failing turn (or,
    /// for [`Ask::Strategy`], that would reach the working one sooner), or
    /// None if nothing helps.
    async fn propose(
        &self,
        trace: &str,
        existing: &[Note],
        ask: &Ask,
    ) -> Result<Option<Note>, String>;
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

/// The [`Ask::Strategy`] half (M9 T5.1). Same reply shape, same refusal path
/// — only the question differs, because a note distilled from a success has
/// to be phrased as a route, not as a correction.
const STRATEGY_SYSTEM: &str = "You tune the action-selection prompt of a tool-using assistant. \
You see the trace of ONE turn that an evaluator graded good, the tool sequence it ran, how \
many graded-good turns ran that same sequence, and the guidance notes already in force. This \
sequence is a strategy that works. Reply with JSON only: {\"scope\": \"global\" | \
\"action:<tool name>\", \"text\": \"<one imperative sentence>\"} with one short reusable note \
that would make the assistant reach this sequence sooner on a similar request, if such a \
sentence exists and does not repeat an existing note; otherwise {\"none\": true}. Describe the \
strategy, never this one turn's specific values.";

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
    async fn propose(
        &self,
        trace: &str,
        existing: &[Note],
        ask: &Ask,
    ) -> Result<Option<Note>, String> {
        let mut user = String::from("Existing notes:\n");
        for n in existing {
            user.push_str(&format!("- [{}] {}\n", n.scope, n.text));
        }
        let system = match ask {
            Ask::Failure => {
                user.push_str("\nFailed turn:\n");
                PROPOSER_SYSTEM
            }
            Ask::Strategy { actions, times } => {
                user.push_str(&format!(
                    "\nThis tool sequence worked in {times} graded-good turns: {actions}\n\
                     \nOne of those turns:\n"
                ));
                STRATEGY_SYSTEM
            }
        };
        user.push_str(trace);
        let request = serde_json::json!({
            "model": self.model,
            "max_tokens": 300,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
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
        // M12 T3.1: stamp the note with the model that proposed it, the only
        // model id this side knows, so the engine can later tell a note
        // learned here from one learned elsewhere.
        Ok(Some(Note::new_learned_on(
            &scope,
            &text,
            0.0,
            self.model.as_str(),
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Ok,
    Fallback,
    Rejections(u32),
    /// An evaluator's recorded verdict on the turn (M8 T2.6, M9 T1.2).
    ///
    /// It outranks every heuristic here because it is the only one that
    /// looked at the *reply*. `Settled`/`ReplyFailed` see whether the machine
    /// finished; a grade sees whether the answer was any good, which is the
    /// thing a reply-scope note is trying to change.
    Graded {
        ok: bool,
    },
}

/// Ok=0 > Rejections(n)=-n > Fallback=-1000 > Graded{false}=-2000.
///
/// A graded-bad turn scores *below* a fallback deliberately. A fallback is
/// the harness admitting it failed, and that admission is already worth
/// something; a confidently wrong answer is the failure the user cannot see,
/// and a note that trades a fallback for one must not read as an improvement.
pub fn score(o: TurnOutcome) -> i64 {
    match o {
        TurnOutcome::Ok | TurnOutcome::Graded { ok: true } => 0,
        TurnOutcome::Rejections(n) => -(n as i64),
        TurnOutcome::Fallback => -1000,
        TurnOutcome::Graded { ok: false } => -2000,
    }
}

/// One outcome per turn, in turn order, believing `"symbolic"` where several
/// evaluators graded the same turn.
pub fn classify_turns(events: &[Event]) -> Vec<TurnOutcome> {
    classify_turns_by(events, "symbolic")
}

/// One outcome per turn, in turn order.
///
/// A recorded [`EventKind::Graded`] from `authoritative` replaces the
/// Settled/ReplyFailed heuristic for that turn — never averages with it. Any
/// other evaluator's grade on the same turn is ignored here; comparing two
/// scorers is κ's job (T2.7), not the gate's.
pub fn classify_turns_by(events: &[Event], authoritative: &str) -> Vec<TurnOutcome> {
    let mut per_turn: BTreeMap<u32, (bool, u32, Option<bool>)> = BTreeMap::new();
    for e in events {
        // `Graded` is appended at the end of the log, not inside the turn it
        // grades, so its own `turn` field is the one that counts — never
        // `e.turn`, even though the pass keeps the two equal.
        if let EventKind::Graded {
            turn, grade, by, ..
        } = &e.kind
        {
            if by == authoritative {
                per_turn
                    .entry(*turn)
                    .or_insert((false, 0, None))
                    .2
                    .get_or_insert(grade.ok);
            }
            continue;
        }
        let entry = per_turn.entry(e.turn).or_insert((false, 0, None));
        match &e.kind {
            EventKind::Settled { policy } if is_fallback(policy) => entry.0 = true,
            EventKind::ReplyFailed { .. } => entry.0 = true,
            EventKind::Rejected { .. } => entry.1 += 1,
            _ => {}
        }
    }
    per_turn
        .into_values()
        .map(|(fallback, rejections, graded)| match graded {
            Some(ok) => TurnOutcome::Graded { ok },
            None if fallback => TurnOutcome::Fallback,
            None if rejections > 0 => TurnOutcome::Rejections(rejections),
            None => TurnOutcome::Ok,
        })
        .collect()
}

/// Is any turn of any of these sessions graded by `authoritative`?
///
/// The question the gate asks before it trusts a reply-quality candidate.
pub fn any_graded(sessions: &[Vec<Event>], authoritative: &str) -> bool {
    sessions
        .iter()
        .flatten()
        .any(|e| matches!(&e.kind, EventKind::Graded { by, .. } if by == authoritative))
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
    async fn recv(&self) -> Result<Incoming, ChannelError> {
        Err(ChannelError::Closed)
    }
    async fn send(&self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
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
        let engine = Engine::with_clock(parts, cfg, Box::new(|| Timestamp(0)));
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
    /// The candidate was neither proved nor disproved — the budget ran out,
    /// or (M9 T1.2) nothing in the probed sessions is graded, so the probe
    /// could not have measured what the note is for. A rejection would be a
    /// claim the evidence does not support.
    pub unverified: bool,
}

/// What a candidate needs before its probe means anything (M9 T1.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteGate {
    /// The candidate is about reply *quality*, so the probe's score is only
    /// readable where a recorded grade exists. An emitter-side candidate —
    /// a malformed argument, an illegal action near a tool — is measured by
    /// `Rejections` and `Fallback`, which need no grade at all, and this
    /// stays false for it.
    pub require_graded: bool,
    /// Whose grade counts. Matches `PassConfig::authoritative_evaluator`.
    pub authoritative: String,
}

impl NoteGate {
    pub fn emitter_side() -> Self {
        Self {
            require_graded: false,
            authoritative: "symbolic".into(),
        }
    }
}

pub async fn verify_note(
    note: &Note,
    positives: &[Vec<Event>],
    negatives: &[Vec<Event>],
    base: &LearnedRules,
    probe: &dyn ProbeRunner,
    regression_budget: u32,
    budget: &mut u32,
    gate: &NoteGate,
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
        unverified: false,
    };

    // M9 T1.2. A reply-quality note is trying to change what the replier
    // says; the probe's own classification cannot see that — it runs a live
    // *emitter* and a scripted replier — so without a recorded grade in the
    // sessions being probed there is nothing for `score` to move. Spending
    // the probe budget to find that out would be worse than saying so.
    if gate.require_graded
        && !any_graded(positives, &gate.authoritative)
        && !any_graded(negatives, &gate.authoritative)
    {
        v.unverified = true;
        v.detail = format!(
            "no turn graded by `{}` in the probed sessions; reply quality is unmeasured",
            gate.authoritative
        );
        return v;
    }

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
            v.unverified = true;
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

    /// M9 T1.2. A recorded grade replaces the Settled/ReplyFailed heuristic
    /// for the turn it names, and only for the evaluator the gate believes.
    #[test]
    fn a_recorded_grade_outranks_the_heuristic_for_its_own_turn() {
        let ok = EventKind::Settled {
            policy: ReplyPolicy::Generate,
        };
        let graded = |turn: u32, ok: bool, by: &str| EventKind::Graded {
            turn,
            grade: Grade {
                ok,
                issues: if ok { vec![] } else { vec!["reask".into()] },
            },
            by: by.into(),
            revision: "n/a".into(),
        };
        let ev = turn_log(&[
            &[EventKind::UserSaid { text: "a".into() }, ok.clone()],
            &[EventKind::UserSaid { text: "b".into() }, ok.clone()],
            // Both grades are appended after the session, as the pass writes
            // them, and carry the turn they grade.
            &[
                graded(1, false, "symbolic"),
                // A second scorer's disagreement is not the gate's business.
                graded(1, true, "local"),
                graded(2, true, "symbolic"),
            ],
        ]);
        assert_eq!(
            classify_turns(&ev),
            vec![
                TurnOutcome::Graded { ok: false },
                TurnOutcome::Graded { ok: true },
                // …and no third entry: the turn the grade events were
                // appended under is not a turn of the conversation.
            ]
        );
        // Believing the other scorer flips turn 1 and leaves turn 2 to the
        // heuristic, because `local` never graded it.
        assert_eq!(
            classify_turns_by(&ev, "local")[0],
            TurnOutcome::Graded { ok: true }
        );
        assert_eq!(classify_turns_by(&ev, "local")[1], TurnOutcome::Ok);
        assert_eq!(
            score(TurnOutcome::Graded { ok: true }),
            score(TurnOutcome::Ok)
        );
        assert!(score(TurnOutcome::Graded { ok: false }) < score(TurnOutcome::Fallback));
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

    /// The same session with one turn graded by `symbolic`.
    fn graded_session(text: &str) -> Vec<Event> {
        turn_log(&[
            &[
                EventKind::UserSaid { text: text.into() },
                EventKind::Settled {
                    policy: ReplyPolicy::Generate,
                },
            ],
            &[EventKind::Graded {
                turn: 1,
                grade: Grade {
                    ok: false,
                    issues: vec!["reask".into()],
                },
                by: "symbolic".into(),
                revision: "n/a".into(),
            }],
        ])
    }

    fn reply_quality_gate() -> NoteGate {
        NoteGate {
            require_graded: true,
            authoritative: "symbolic".into(),
        }
    }

    /// M9 T1.2. The probe runs a live *emitter* against a scripted replier,
    /// so nothing it classifies looks at reply quality. Without a recorded
    /// grade there is nothing for `score` to move, and a candidate mined from
    /// `UserReask` or `UngroundedReply` is therefore unverified — never
    /// rejected, which would be a claim, and never accepted.
    #[tokio::test]
    async fn a_reply_quality_candidate_is_unverified_when_no_turn_in_the_session_is_graded() {
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
            &reply_quality_gate(),
        )
        .await;
        assert!(!v.accepted);
        assert!(v.unverified, "{}", v.detail);
        assert!(v.detail.contains("symbolic"), "{}", v.detail);
        // And it did not spend the probe budget finding that out.
        assert_eq!((v.turns_used, budget), (0, 40));

        // One graded turn is enough to make the probe readable again.
        let mut budget = 40;
        let v = verify_note(
            &note,
            &[graded_session("positive")],
            &[session("negative")],
            &LearnedRules::default(),
            &probe,
            0,
            &mut budget,
            &reply_quality_gate(),
        )
        .await;
        assert!(v.accepted && !v.unverified, "{}", v.detail);
    }

    /// The other half of the same rule: a signature mined from what the
    /// harness *did* is measured by `Rejections` and `Fallback`, which need
    /// no grade, so its path through the gate is exactly what it was.
    #[tokio::test]
    async fn an_emitter_side_candidate_still_verifies_without_grades() {
        assert!(!crate::mine::SignatureKind::MalformedArg {
            action: "echo".into(),
            arg: "text".into(),
            ops: vec![],
        }
        .from_grades());
        assert!(crate::mine::SignatureKind::UngroundedReply { spans: vec![] }.from_grades());

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
            &NoteGate::emitter_side(),
        )
        .await;
        assert!(v.accepted, "{}", v.detail);
        assert!(!v.unverified);
        assert_eq!((v.improved, v.regressed), (1, 0));
    }

    /// M9 T5.1. The success lane earns nothing on its own: a candidate
    /// proposed from a `Succeeded` signature runs the same positives, the
    /// same negatives, the same regression budget and the same
    /// `improved`/`regressed` arithmetic as one proposed from a failure.
    /// Only the question put to the proposer differs.
    #[tokio::test]
    async fn a_succeeded_candidate_takes_the_same_gate_as_a_failure_candidate() {
        use crate::mine::SignatureKind;

        let succeeded = SignatureKind::Succeeded {
            actions: "get_time -> get_weather".into(),
            times: 2,
        };
        let failure = SignatureKind::UngroundedReply { spans: vec![] };
        // The gate `pass.rs` builds is a function of `from_grades()` alone,
        // and both kinds answer it the same way — so both get this gate.
        assert_eq!(succeeded.from_grades(), failure.from_grades());
        let gate = reply_quality_gate();

        // Only the ask differs.
        assert_eq!(
            succeeded.ask(),
            Ask::Strategy {
                actions: "get_time -> get_weather".into(),
                times: 2
            }
        );
        assert_eq!(failure.ask(), Ask::Failure);

        let note = Note::new("global", &format!("{MARKER} before answering."), 0.0);
        let positives = [graded_session("positive")];
        let negatives = [session("negative")];

        // Helps a positive, hurts no negative: accepted, and the numbers are
        // the probe's, not the lane's.
        let probe = MarkerProbe {
            with_marker: vec![TurnOutcome::Ok],
            without: vec![TurnOutcome::Fallback],
            negatives_regress: false,
        };
        let mut budget = 40;
        let accepted = verify_note(
            &note,
            &positives,
            &negatives,
            &LearnedRules::default(),
            &probe,
            0,
            &mut budget,
            &gate,
        )
        .await;
        assert!(accepted.accepted, "{}", accepted.detail);
        assert_eq!((accepted.improved, accepted.regressed), (1, 0));

        // Break a negative and the same candidate is rejected under budget 0:
        // there is no success-flavoured accept path around the regression
        // rule.
        let regressing = MarkerProbe {
            with_marker: vec![TurnOutcome::Ok],
            without: vec![TurnOutcome::Fallback],
            negatives_regress: true,
        };
        let mut budget = 40;
        let rejected = verify_note(
            &note,
            &positives,
            &negatives,
            &LearnedRules::default(),
            &regressing,
            0,
            &mut budget,
            &gate,
        )
        .await;
        assert!(!rejected.accepted, "{}", rejected.detail);
        assert!(!rejected.unverified);
        assert_eq!((rejected.improved, rejected.regressed), (1, 1));

        // And byte for byte the verdict a failure candidate gets on the same
        // evidence — the gate cannot tell the two apart.
        let mut budget = 40;
        let as_failure = verify_note(
            &note,
            &positives,
            &negatives,
            &LearnedRules::default(),
            &probe,
            0,
            &mut budget,
            &NoteGate {
                require_graded: failure.from_grades(),
                authoritative: "symbolic".into(),
            },
        )
        .await;
        assert_eq!(as_failure, accepted);
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
            &NoteGate::emitter_side(),
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
            &NoteGate::emitter_side(),
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
            &NoteGate::emitter_side(),
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
            &NoteGate::emitter_side(),
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
        let n = p
            .propose("trace", &[], &Ask::Failure)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (n.scope.as_str(), n.text.as_str()),
            ("action:echo", "Call echo when asked to repeat.")
        );
        assert_eq!(p.propose("trace", &[], &Ask::Failure).await.unwrap(), None);
        assert!(p.propose("trace", &[], &Ask::Failure).await.is_err());
        let sent = mock.requests.lock().unwrap()[0].clone();
        assert!(sent["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("Failed turn:\ntrace"));
    }
}
