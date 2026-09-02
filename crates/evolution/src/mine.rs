//! Mining (spec M5 §3.1): scan a session log for failure signatures and
//! route each to a lane — symbolic (a deterministic patch could fix it) or
//! note (only guidance to the emitter could).
use nscore::{
    validate_args, ActionSpec, Event, EventId, EventKind, Op, Proposal, RejectReason, ReplyPolicy,
    SessionId, ToolOutcome,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum SignatureKind {
    /// A normalize candidate that makes `validate_args` pass.
    MalformedArg {
        action: String,
        arg: String,
        ops: Vec<Op>,
    },
    IllegalNearTool {
        proposed: String,
        candidate: String,
    },
    FallbackReply,
    RepeatedGuardDenial {
        guard: String,
        reason: String,
    },
    ToolErrArgs {
        action: String,
        detail: String,
    },
    Corrected {
        text: String,
    },
}

impl SignatureKind {
    pub fn lane(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } | SignatureKind::IllegalNearTool { .. } => {
                "symbolic"
            }
            _ => "note",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } => "MalformedArg",
            SignatureKind::IllegalNearTool { .. } => "IllegalNearTool",
            SignatureKind::FallbackReply => "FallbackReply",
            SignatureKind::RepeatedGuardDenial { .. } => "RepeatedGuardDenial",
            SignatureKind::ToolErrArgs { .. } => "ToolErrArgs",
            SignatureKind::Corrected { .. } => "Corrected",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub session: SessionId,
    pub turn: u32,
    pub event_id: EventId,
    pub kind: SignatureKind,
}

pub const SYNTHETIC_ACTIONS: [&str; 5] = [
    "respond_directly",
    "ask_clarification",
    "remember_fact",
    "forget_fact",
    "forget_all",
];

/// Lowercase ASCII alphanumerics only: "getTime" and "get_time" squash equal.
use nscore::squash;

/// The legal name a near-miss most plausibly meant: exact squashed match,
/// else the unique name within Damerau-Levenshtein 2. Ties yield nothing —
/// an alias must be unambiguous to be a patch.
pub fn near_tool(proposed: &str, names: &[String]) -> Option<String> {
    if names.iter().any(|n| n == proposed) {
        return None;
    }
    let p = squash(proposed);
    if let Some(exact) = names.iter().find(|n| squash(n) == p) {
        return Some(exact.clone());
    }
    let mut best: Vec<(usize, &String)> = names
        .iter()
        .map(|n| (strsim::damerau_levenshtein(&p, &squash(n)), n))
        .filter(|(d, _)| *d <= 2)
        .collect();
    best.sort_by_key(|(d, _)| *d);
    match best.as_slice() {
        [(d0, _), (d1, _), ..] if d0 == d1 => None,
        [(_, n0), ..] => Some((*n0).clone()),
        [] => None,
    }
}

/// Tried in order; the first sequence that validates wins (shortest fix).
const OP_SEQUENCES: [&[Op]; 3] = [
    &[Op::Trim],
    &[Op::Trim, Op::StripPunct],
    &[Op::Trim, Op::StripPunct, Op::Lowercase],
];

/// An (arg, ops) pair such that rewriting exactly that one string arg makes
/// the args validate. None when the args already validate or nothing helps.
pub fn normalize_candidate(
    spec: &ActionSpec,
    args: &serde_json::Value,
) -> Option<(String, Vec<Op>)> {
    let obj = args.as_object()?;
    if validate_args(&spec.args_schema, args).is_ok() {
        return None;
    }
    for (name, value) in obj {
        let Some(s) = value.as_str() else { continue };
        for ops in OP_SEQUENCES {
            let mut patched = args.clone();
            patched[name] = serde_json::Value::String(nscore::apply_ops(ops, s));
            if validate_args(&spec.args_schema, &patched).is_ok() {
                return Some((name.clone(), ops.to_vec()));
            }
        }
    }
    None
}

pub fn is_fallback(policy: &ReplyPolicy) -> bool {
    match policy {
        ReplyPolicy::Verbatim { text } => text.starts_with(nsengine::turn::FALLBACK_REPLY),
        ReplyPolicy::Template { id, .. } => id == "cant_help",
        ReplyPolicy::Generate => false,
    }
}

/// One line per event of `turn`: the trace text a note proposer reads.
pub fn render_turn(events: &[Event], turn: u32) -> String {
    events
        .iter()
        .filter(|e| e.turn == turn)
        .map(|e| match &e.kind {
            EventKind::UserSaid { text } => format!("UserSaid: {text}"),
            EventKind::Proposed { proposal } => {
                format!("Proposed: {} {}", proposal.action, proposal.args)
            }
            EventKind::Rejected { reason, .. } => match reason {
                RejectReason::Malformed { detail } => format!("Rejected: malformed — {detail}"),
                RejectReason::IllegalAction { action } => {
                    format!("Rejected: illegal action {action}")
                }
                RejectReason::GuardDenied { guard, reason } => {
                    format!("Rejected: guard {guard} — {reason}")
                }
            },
            EventKind::ToolCalled { action, .. } => format!("ToolCalled: {action}"),
            EventKind::ToolReturned { outcome, .. } => match outcome {
                ToolOutcome::Ok { output } => format!("ToolReturned: ok — {}", output.summary),
                ToolOutcome::Err { kind, detail } => format!("ToolReturned: err {kind} — {detail}"),
            },
            EventKind::PendingConfirmation { .. } => "PendingConfirmation".into(),
            EventKind::Confirmed { .. } => "Confirmed".into(),
            EventKind::Corrected { text, .. } => format!("Corrected: {text}"),
            EventKind::Settled { policy } => match policy {
                ReplyPolicy::Verbatim { text } => format!("Settled: verbatim — {text}"),
                ReplyPolicy::Template { id, .. } => format!("Settled: template {id}"),
                ReplyPolicy::Generate => "Settled: generate".into(),
            },
            EventKind::Replied { text } => format!("Replied: {text}"),
            EventKind::ReplyFailed { detail } => format!("ReplyFailed: {detail}"),
            EventKind::ReplyFlagged { spans, .. } => format!("ReplyFlagged: {}", spans.join(", ")),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn mine(session: &SessionId, events: &[Event], known_specs: &[ActionSpec]) -> Vec<Signature> {
    let specs: HashMap<&str, &ActionSpec> =
        known_specs.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut legal_names: Vec<String> = known_specs.iter().map(|s| s.name.clone()).collect();
    legal_names.extend(SYNTHETIC_ACTIONS.iter().map(|s| s.to_string()));
    let proposals: HashMap<u64, &Proposal> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => Some((e.id.0, proposal)),
            _ => None,
        })
        .collect();
    let calls: HashMap<u64, &str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCalled { action, .. } => Some((e.id.0, action.as_str())),
            _ => None,
        })
        .collect();
    let mut seen_denials: HashSet<(u32, String, String)> = HashSet::new();
    let mut counted_denials: HashSet<(u32, String, String)> = HashSet::new();
    let mut out = Vec::new();
    let sig = |e: &Event, kind: SignatureKind| Signature {
        session: session.clone(),
        turn: e.turn,
        event_id: e.id,
        kind,
    };
    for e in events {
        match &e.kind {
            EventKind::Rejected {
                proposal_of,
                reason,
            } => match reason {
                RejectReason::Malformed { .. } => {
                    if let Some(p) = proposals.get(&proposal_of.0) {
                        if let Some(spec) = specs.get(p.action.as_str()) {
                            if let Some((arg, ops)) = normalize_candidate(spec, &p.args) {
                                let kind = SignatureKind::MalformedArg {
                                    action: p.action.clone(),
                                    arg,
                                    ops,
                                };
                                out.push(sig(e, kind));
                            }
                        }
                    }
                }
                RejectReason::IllegalAction { action } => {
                    if let Some(candidate) = near_tool(action, &legal_names) {
                        let kind = SignatureKind::IllegalNearTool {
                            proposed: action.clone(),
                            candidate,
                        };
                        out.push(sig(e, kind));
                    }
                }
                RejectReason::GuardDenied { guard, reason } => {
                    let key = (e.turn, guard.clone(), reason.clone());
                    if !seen_denials.insert(key.clone()) && counted_denials.insert(key) {
                        let kind = SignatureKind::RepeatedGuardDenial {
                            guard: guard.clone(),
                            reason: reason.clone(),
                        };
                        out.push(sig(e, kind));
                    }
                }
            },
            EventKind::Settled { policy } if is_fallback(policy) => {
                out.push(sig(e, SignatureKind::FallbackReply))
            }
            // A failed reply is a fallback the user saw (F7); same lane.
            EventKind::ReplyFailed { .. } => out.push(sig(e, SignatureKind::FallbackReply)),
            EventKind::ToolReturned {
                call,
                outcome: ToolOutcome::Err { kind, detail },
            } if kind == "bad_args" => {
                let action = calls.get(&call.0).unwrap_or(&"?").to_string();
                out.push(sig(
                    e,
                    SignatureKind::ToolErrArgs {
                        action,
                        detail: detail.clone(),
                    },
                ));
            }
            EventKind::Corrected { text, .. } => {
                out.push(sig(e, SignatureKind::Corrected { text: text.clone() }))
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn spec(name: &str) -> ActionSpec {
        ActionSpec {
            name: name.into(),
            description: "".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {"unit": {"type": "string", "enum": ["c", "f"]}, "city": {"type": "string"}},
                "required": ["city"]
            }),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    fn log() -> EventLog {
        EventLog::new(SessionId("m".into()))
    }

    fn proposal(action: &str, args: serde_json::Value) -> Proposal {
        Proposal {
            rationale: "".into(),
            action: action.into(),
            args,
        }
    }

    #[test]
    fn near_tool_prefers_exact_normalized_match_then_unique_edit_distance() {
        let names: Vec<String> = ["get_time", "get_weather", "wipe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(near_tool("getTime", &names), Some("get_time".into()));
        assert_eq!(near_tool("get_tme", &names), Some("get_time".into()));
        assert_eq!(
            near_tool("get_time", &names),
            None,
            "already legal is not a near miss"
        );
        assert_eq!(near_tool("zzzzzz", &names), None);
        let ambiguous: Vec<String> = ["ab", "ac"].iter().map(|s| s.to_string()).collect();
        assert_eq!(near_tool("ad", &ambiguous), None, "ties yield nothing");
    }

    #[test]
    fn normalize_candidate_finds_the_shortest_op_sequence_that_validates() {
        let s = spec("get_weather");
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": " \"C\" "})),
            Some(("unit".into(), vec![Op::Trim, Op::StripPunct, Op::Lowercase]))
        );
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": "kelvin"})),
            None
        );
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"unit": "c"})),
            None,
            "missing required cannot be normalized"
        );
    }

    #[test]
    fn mines_malformed_illegal_fallback_repeat_toolerr_and_corrected() {
        let mut l = log();
        // turn 1: malformed unit, then illegal near-miss, then repeated guard denial, fallback.
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "weather".into(),
            },
        );
        let p1 = l
            .append(
                1,
                Timestamp(2),
                EventKind::Proposed {
                    proposal: proposal(
                        "get_weather",
                        serde_json::json!({"city": "Brno", "unit": " \"C\" "}),
                    ),
                },
            )
            .id;
        l.append(
            1,
            Timestamp(3),
            EventKind::Rejected {
                proposal_of: p1,
                reason: RejectReason::Malformed { detail: "x".into() },
            },
        );
        let p2 = l
            .append(
                1,
                Timestamp(4),
                EventKind::Proposed {
                    proposal: proposal("getWeather", serde_json::json!({})),
                },
            )
            .id;
        l.append(
            1,
            Timestamp(5),
            EventKind::Rejected {
                proposal_of: p2,
                reason: RejectReason::IllegalAction {
                    action: "getWeather".into(),
                },
            },
        );
        for _ in 0..2 {
            let p = l
                .append(
                    1,
                    Timestamp(6),
                    EventKind::Proposed {
                        proposal: proposal("wipe", serde_json::json!({})),
                    },
                )
                .id;
            l.append(
                1,
                Timestamp(7),
                EventKind::Rejected {
                    proposal_of: p,
                    reason: RejectReason::GuardDenied {
                        guard: "taint".into(),
                        reason: "external".into(),
                    },
                },
            );
        }
        l.append(
            1,
            Timestamp(8),
            EventKind::Settled {
                policy: ReplyPolicy::Verbatim {
                    text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY),
                },
            },
        );
        l.append(1, Timestamp(9), EventKind::Replied { text: "…".into() });
        // turn 2: tool error attributable to args, then a correction.
        l.append(
            2,
            Timestamp(10),
            EventKind::UserSaid {
                text: "again".into(),
            },
        );
        let c = l
            .append(
                2,
                Timestamp(11),
                EventKind::ToolCalled {
                    action: "get_weather".into(),
                    args: vec![],
                },
            )
            .id;
        l.append(
            2,
            Timestamp(12),
            EventKind::ToolReturned {
                call: c,
                outcome: ToolOutcome::Err {
                    kind: "bad_args".into(),
                    detail: "city unknown".into(),
                },
            },
        );
        l.append(
            2,
            Timestamp(13),
            EventKind::Corrected {
                target: None,
                text: "user.city = Brno".into(),
            },
        );

        let sigs = mine(
            &SessionId("m".into()),
            l.events(),
            &[spec("get_weather"), spec("wipe")],
        );
        let names: Vec<&str> = sigs.iter().map(|s| s.kind.name()).collect();
        assert_eq!(
            names,
            vec![
                "MalformedArg",
                "IllegalNearTool",
                "RepeatedGuardDenial",
                "FallbackReply",
                "ToolErrArgs",
                "Corrected"
            ]
        );
        assert!(matches!(
            &sigs[0].kind,
            SignatureKind::MalformedArg { action, arg, ops }
                if action == "get_weather" && arg == "unit"
                    && ops == &vec![Op::Trim, Op::StripPunct, Op::Lowercase]
        ));
        assert!(matches!(
            &sigs[1].kind,
            SignatureKind::IllegalNearTool { proposed, candidate }
                if proposed == "getWeather" && candidate == "get_weather"
        ));
        assert_eq!(sigs[0].turn, 1);
        assert_eq!(sigs[4].turn, 2);
        assert!(sigs.iter().all(|s| s.session.0 == "m"));
        assert_eq!(sigs[0].kind.lane(), "symbolic");
        assert_eq!(sigs[3].kind.lane(), "note");
    }

    #[test]
    fn render_turn_is_one_line_per_event_of_that_turn() {
        let mut l = log();
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(
            2,
            Timestamp(2),
            EventKind::UserSaid {
                text: "again".into(),
            },
        );
        l.append(2, Timestamp(3), EventKind::Replied { text: "ok".into() });
        let t = render_turn(l.events(), 2);
        assert_eq!(t, "UserSaid: again\nReplied: ok");
    }
}
