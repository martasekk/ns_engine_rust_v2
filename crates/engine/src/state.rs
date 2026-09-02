use nscore::{
    min_trust, Event, EventId, EventKind, RejectReason, ReplyPolicy, SessionSummary, ToolOutcome,
    Trust, TurnRecord,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    /// Highest turn seen.
    pub turn: u32,
    /// ("user" | "assistant", text)
    pub history: Vec<(String, String)>,
    /// One verbatim record per COMPLETED turn (M6 §4.1); the turn in
    /// progress is not a record until its `Replied` lands.
    pub records: Vec<TurnRecord>,
    /// Latest rolling summary, if any (M6 §5.1).
    pub summary: Option<SessionSummary>,
    /// Number of `Summarized` events so far (rebuild cadence substrate).
    pub summaries: u32,
    /// Last unconfirmed PendingConfirmation.
    pub pending_confirmation: Option<EventId>,
    /// Turn on which the pending confirmation was created (expiry substrate).
    pub pending_turn: Option<u32>,
    /// Turn of the most recent Confirmed event.
    pub confirmed_this_turn_of: Option<u32>,
    /// DedupeGate substrate: action names that have ToolCalled this session.
    pub fired_tags: HashSet<String>,
}

/// `remember_fact user.age=17` from the call's classified args; `action` for
/// everything else.
fn call_label(action: &str, args: &[(String, nscore::TaggedValue)]) -> String {
    if action != crate::turn::REMEMBER_FACT {
        return action.to_string();
    }
    let get = |name: &str| {
        args.iter()
            .find(|(k, _)| k == name)
            .map(|(_, tv)| match &tv.value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
    };
    match (get("key"), get("value")) {
        (Some(k), Some(v)) => format!("{action} {k}={v}"),
        _ => action.to_string(),
    }
}

pub fn fold(events: &[Event]) -> SessionState {
    let mut s = SessionState::default();
    // Record under construction for the turn in progress, plus the lookups
    // that turn event references into readable `did` lines.
    let mut current: Option<TurnRecord> = None;
    let mut trusts: Vec<Trust> = Vec::new();
    let mut proposal_action: HashMap<u64, String> = HashMap::new();
    let mut call_label_of: HashMap<u64, String> = HashMap::new();
    let mut last_proposal: Option<String> = None;
    let mut staged_this_turn = false;
    let push_line = |current: &mut Option<TurnRecord>, line: String| {
        if let Some(r) = current.as_mut() {
            r.did.push(line);
        }
    };
    for e in events {
        if e.turn > s.turn {
            s.turn = e.turn;
        }
        match &e.kind {
            EventKind::UserSaid { text } => {
                s.history.push(("user".into(), text.clone()));
                if let Some(mut r) = current.take() {
                    r.trust = min_trust(&trusts);
                    s.records.push(r);
                }
                trusts.clear();
                staged_this_turn = false;
                last_proposal = None;
                current = Some(TurnRecord {
                    turn: e.turn,
                    user: text.clone(),
                    did: Vec::new(),
                    reply: String::new(),
                    trust: Trust::User,
                });
            }
            EventKind::Proposed { proposal } => {
                proposal_action.insert(e.id.0, proposal.action.clone());
                last_proposal = Some(proposal.action.clone());
            }
            EventKind::Rejected {
                proposal_of,
                reason,
            } => {
                let action = proposal_action
                    .get(&proposal_of.0)
                    .cloned()
                    .unwrap_or_else(|| "?".into());
                let line = match reason {
                    RejectReason::Malformed { detail } if proposal_of.0 == 0 => {
                        format!("model failed: {detail}")
                    }
                    RejectReason::Malformed { detail } => {
                        format!("{action} -> malformed: {detail}")
                    }
                    RejectReason::IllegalAction { action } => format!("{action} -> illegal"),
                    RejectReason::GuardDenied { guard, reason } => {
                        format!("{action} -> denied ({guard}: {reason})")
                    }
                };
                push_line(&mut current, line);
            }
            EventKind::ToolCalled { action, args } => {
                s.fired_tags.insert(action.clone());
                call_label_of.insert(e.id.0, call_label(action, args));
            }
            EventKind::ToolReturned { call, outcome } => {
                let label = call_label_of
                    .get(&call.0)
                    .cloned()
                    .unwrap_or_else(|| "?".into());
                let line = match outcome {
                    ToolOutcome::Ok { output } => {
                        trusts.push(output.trust);
                        if label.starts_with(crate::turn::REMEMBER_FACT) {
                            format!("{label} -> ok")
                        } else {
                            format!("{label} -> ok: {}", output.summary)
                        }
                    }
                    ToolOutcome::Err { kind, detail } => format!("{label} -> err {kind}: {detail}"),
                };
                push_line(&mut current, line);
            }
            EventKind::PendingConfirmation {
                proposal_of,
                staged,
            } => {
                s.pending_confirmation = Some(e.id);
                s.pending_turn = Some(e.turn);
                staged_this_turn = true;
                let action = proposal_action
                    .get(&proposal_of.0)
                    .cloned()
                    .unwrap_or_else(|| "?".into());
                let mut line = format!("staged: '{action}' awaits confirmation");
                if let Some(st) = staged {
                    line.push_str(&format!(": {}", st.description));
                }
                push_line(&mut current, line);
            }
            EventKind::Confirmed { pending } => {
                if s.pending_confirmation == Some(*pending) {
                    s.pending_confirmation = None;
                    s.pending_turn = None;
                    s.confirmed_this_turn_of = Some(e.turn);
                }
                push_line(&mut current, "confirmed".into());
            }
            EventKind::Corrected { text, .. } => {
                push_line(&mut current, format!("corrected: {text}"));
            }
            EventKind::Settled { policy } => {
                let line = match policy {
                    ReplyPolicy::Verbatim { .. }
                        if last_proposal.as_deref() == Some(crate::turn::ASK_CLARIFICATION) =>
                    {
                        Some("asked a clarification".to_string())
                    }
                    ReplyPolicy::Verbatim { text }
                        if text.starts_with(crate::turn::FALLBACK_REPLY) =>
                    {
                        Some(format!(
                            "fallback:{}",
                            text.trim_start_matches(crate::turn::FALLBACK_REPLY)
                        ))
                    }
                    ReplyPolicy::Verbatim { .. } if staged_this_turn => None,
                    ReplyPolicy::Template { id, .. } => Some(format!("fallback template {id}")),
                    _ => None,
                };
                if let Some(l) = line {
                    push_line(&mut current, l);
                }
            }
            EventKind::ReplyFailed { detail } => {
                push_line(&mut current, format!("reply failed: {detail}"));
            }
            // A flagged first draft is audit material, not something the
            // models need to see again: the final Replied is the record.
            EventKind::ReplyFlagged { .. } => {}
            EventKind::Summarized { summary } => {
                s.summary = Some(summary.clone());
                s.summaries += 1;
            }
            EventKind::Replied { text } => {
                s.history.push(("assistant".into(), text.clone()));
                if let Some(mut r) = current.take() {
                    r.reply = text.clone();
                    r.trust = min_trust(&trusts);
                    s.records.push(r);
                }
                trusts.clear();
            }
        }
    }
    s
}

impl SessionState {
    /// The last `k` completed turns, oldest first (M6 §4.1).
    pub fn window(&self, k: usize) -> Vec<TurnRecord> {
        let start = self.records.len().saturating_sub(k);
        self.records[start..].to_vec()
    }

    /// Completed turns with `from <= turn <= to`, oldest first.
    pub fn records_in(&self, from: u32, to: u32) -> Vec<TurnRecord> {
        self.records
            .iter()
            .filter(|r| r.turn >= from && r.turn <= to)
            .cloned()
            .collect()
    }
}

pub fn state_summary(s: &SessionState) -> String {
    format!("turn {}, {} messages", s.turn, s.history.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    #[test]
    fn fold_builds_history_and_tracks_confirmation() {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        log.append(
            1,
            Timestamp(2),
            EventKind::Replied {
                text: "hello".into(),
            },
        );
        log.append(
            2,
            Timestamp(3),
            EventKind::UserSaid {
                text: "delete it".into(),
            },
        );
        let pending_id = log
            .append(
                2,
                Timestamp(4),
                EventKind::PendingConfirmation {
                    proposal_of: EventId(3),
                    staged: None,
                },
            )
            .id;
        let s = fold(log.events());
        assert_eq!(s.turn, 2);
        assert_eq!(s.history.len(), 3);
        assert_eq!(s.pending_confirmation, Some(pending_id));

        let mut log2 = EventLog::from_events(SessionId("s".into()), log.events().to_vec());
        log2.append(
            3,
            Timestamp(5),
            EventKind::Confirmed {
                pending: pending_id,
            },
        );
        let s2 = fold(log2.events());
        assert_eq!(s2.pending_confirmation, None);
    }

    #[test]
    fn fold_tracks_fired_actions_pending_turn_and_confirmation_turn() {
        let mut log = EventLog::new(SessionId("s".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "go".into() });
        log.append(
            1,
            Timestamp(2),
            EventKind::ToolCalled {
                action: "echo".into(),
                args: vec![],
            },
        );
        let pending = log
            .append(
                1,
                Timestamp(3),
                EventKind::PendingConfirmation {
                    proposal_of: EventId(1),
                    staged: None,
                },
            )
            .id;
        let s = fold(log.events());
        assert!(s.fired_tags.contains("echo"));
        assert_eq!(s.pending_confirmation, Some(pending));
        assert_eq!(s.pending_turn, Some(1));
        assert_eq!(s.confirmed_this_turn_of, None);

        log.append(2, Timestamp(4), EventKind::UserSaid { text: "yes".into() });
        log.append(2, Timestamp(5), EventKind::Confirmed { pending });
        let s2 = fold(log.events());
        assert_eq!(s2.pending_confirmation, None);
        assert_eq!(s2.pending_turn, None);
        assert_eq!(s2.confirmed_this_turn_of, Some(2));
    }

    fn tv(v: serde_json::Value) -> TaggedValue {
        TaggedValue {
            value: v,
            prov: Provenance::Residual,
            trust: Trust::System,
        }
    }

    fn ok(summary: &str, trust: Trust) -> ToolOutcome {
        ToolOutcome::Ok {
            output: ToolOutput {
                summary: summary.into(),
                artifact: None,
                trust,
            },
        }
    }

    #[test]
    fn fold_builds_one_verbatim_record_per_completed_turn() {
        let sid = SessionId("r".into());
        let mut log = EventLog::new(sid);
        let t = Timestamp(1);
        // turn 1: a tool call with an external result, a guard denial, a fact
        log.append(
            1,
            t,
            EventKind::UserSaid {
                text: "check stock\nplease".into(),
            },
        );
        let p = log
            .append(
                1,
                t,
                EventKind::Proposed {
                    proposal: Proposal {
                        rationale: "".into(),
                        action: "check_stock".into(),
                        args: serde_json::json!({}),
                    },
                },
            )
            .id;
        let c = log
            .append(
                1,
                t,
                EventKind::ToolCalled {
                    action: "check_stock".into(),
                    args: vec![],
                },
            )
            .id;
        log.append(
            1,
            t,
            EventKind::ToolReturned {
                call: c,
                outcome: ok("12 in stock", Trust::External),
            },
        );
        let p2 = log
            .append(
                1,
                t,
                EventKind::Proposed {
                    proposal: Proposal {
                        rationale: "".into(),
                        action: "wipe".into(),
                        args: serde_json::json!({}),
                    },
                },
            )
            .id;
        log.append(
            1,
            t,
            EventKind::Rejected {
                proposal_of: p2,
                reason: RejectReason::GuardDenied {
                    guard: "taint_policy".into(),
                    reason: "external".into(),
                },
            },
        );
        let _ = p;
        let c2 = log
            .append(
                1,
                t,
                EventKind::ToolCalled {
                    action: "remember_fact".into(),
                    args: vec![
                        ("key".into(), tv(serde_json::json!("user.age"))),
                        ("value".into(), tv(serde_json::json!("17"))),
                    ],
                },
            )
            .id;
        log.append(
            1,
            t,
            EventKind::ToolReturned {
                call: c2,
                outcome: ok("remembered user.age", Trust::System),
            },
        );
        log.append(
            1,
            t,
            EventKind::Settled {
                policy: ReplyPolicy::Generate,
            },
        );
        log.append(
            1,
            t,
            EventKind::Replied {
                text: "Twelve.".into(),
            },
        );
        // turn 2: clarification
        log.append(
            2,
            t,
            EventKind::UserSaid {
                text: "text".into(),
            },
        );
        log.append(
            2,
            t,
            EventKind::Proposed {
                proposal: Proposal {
                    rationale: "".into(),
                    action: "ask_clarification".into(),
                    args: serde_json::json!({"question": "To whom?"}),
                },
            },
        );
        log.append(
            2,
            t,
            EventKind::Settled {
                policy: ReplyPolicy::Verbatim {
                    text: "To whom?".into(),
                },
            },
        );
        log.append(
            2,
            t,
            EventKind::Replied {
                text: "To whom?".into(),
            },
        );
        // turn 3: staged, then turn 4 confirmed and run; turn 5 in progress
        log.append(
            3,
            t,
            EventKind::UserSaid {
                text: "wipe it".into(),
            },
        );
        let p3 = log
            .append(
                3,
                t,
                EventKind::Proposed {
                    proposal: Proposal {
                        rationale: "".into(),
                        action: "wipe".into(),
                        args: serde_json::json!({}),
                    },
                },
            )
            .id;
        let pend = log
            .append(
                3,
                t,
                EventKind::PendingConfirmation {
                    proposal_of: p3,
                    staged: Some(StagedEffect {
                        description: "would delete 3 rows".into(),
                    }),
                },
            )
            .id;
        log.append(
            3,
            t,
            EventKind::Settled {
                policy: ReplyPolicy::Verbatim {
                    text: "'wipe' is irreversible. Confirm to proceed.".into(),
                },
            },
        );
        log.append(
            3,
            t,
            EventKind::Replied {
                text: "'wipe' is irreversible. Confirm to proceed.".into(),
            },
        );
        log.append(4, t, EventKind::UserSaid { text: "yes".into() });
        log.append(4, t, EventKind::Confirmed { pending: pend });
        let c3 = log
            .append(
                4,
                t,
                EventKind::ToolCalled {
                    action: "wipe".into(),
                    args: vec![],
                },
            )
            .id;
        log.append(
            4,
            t,
            EventKind::ToolReturned {
                call: c3,
                outcome: ok("wiped", Trust::System),
            },
        );
        log.append(
            4,
            t,
            EventKind::ReplyFailed {
                detail: "status 402".into(),
            },
        );
        log.append(
            4,
            t,
            EventKind::Replied {
                text: "Sorry, I couldn't complete that.".into(),
            },
        );
        log.append(
            5,
            t,
            EventKind::UserSaid {
                text: "in progress".into(),
            },
        );

        let s = fold(log.events());
        assert_eq!(s.records.len(), 4, "the turn in progress is not a record");
        let r1 = &s.records[0];
        assert_eq!(r1.turn, 1);
        assert_eq!(
            r1.user, "check stock\nplease",
            "verbatim, newline kept in the record"
        );
        assert_eq!(
            r1.did,
            vec![
                "check_stock -> ok: 12 in stock",
                "wipe -> denied (taint_policy: external)",
                "remember_fact user.age=17 -> ok",
            ]
        );
        assert_eq!(r1.reply, "Twelve.");
        assert_eq!(
            r1.trust,
            Trust::External,
            "min trust over the turn's outputs"
        );
        assert_eq!(s.records[1].did, vec!["asked a clarification"]);
        assert_eq!(s.records[1].trust, Trust::User, "no tool ran");
        assert_eq!(
            s.records[2].did,
            vec!["staged: 'wipe' awaits confirmation: would delete 3 rows"]
        );
        assert_eq!(
            s.records[3].did,
            vec!["confirmed", "wipe -> ok: wiped", "reply failed: status 402"]
        );
        assert_eq!(s.turn, 5);
    }

    #[test]
    fn fold_keeps_the_latest_summary_and_counts_them() {
        let mut log = EventLog::new(SessionId("sum".into()));
        let t = Timestamp(1);
        let sum = |through: u32| SessionSummary {
            through_turn: through,
            topic: format!("through {through}"),
            established: vec![],
            open: vec![],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        for turn in 1..=6 {
            log.append(turn, t, EventKind::UserSaid { text: "x".into() });
            log.append(turn, t, EventKind::Replied { text: "y".into() });
            if turn == 4 {
                log.append(turn, t, EventKind::Summarized { summary: sum(2) });
            }
            if turn == 6 {
                log.append(turn, t, EventKind::Summarized { summary: sum(4) });
            }
        }
        let s = fold(log.events());
        assert_eq!(s.summary.as_ref().map(|x| x.through_turn), Some(4));
        assert_eq!(s.summaries, 2);
        assert_eq!(s.records.len(), 6, "summaries are not turn records");
        let r = s.records_in(3, 4);
        assert_eq!(r.iter().map(|r| r.turn).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn fold_labels_emitter_failures_and_fallbacks() {
        let mut log = EventLog::new(SessionId("f".into()));
        let t = Timestamp(1);
        log.append(1, t, EventKind::UserSaid { text: "hi".into() });
        log.append(
            1,
            t,
            EventKind::Rejected {
                proposal_of: EventId(0),
                reason: RejectReason::Malformed {
                    detail: "transport: status 429".into(),
                },
            },
        );
        log.append(
            1,
            t,
            EventKind::Settled {
                policy: ReplyPolicy::Verbatim {
                    text: format!("{} Reason: HTTP 429.", crate::turn::FALLBACK_REPLY),
                },
            },
        );
        log.append(
            1,
            t,
            EventKind::Replied {
                text: format!("{} Reason: HTTP 429.", crate::turn::FALLBACK_REPLY),
            },
        );
        let s = fold(log.events());
        assert_eq!(
            s.records[0].did,
            vec![
                "model failed: transport: status 429",
                "fallback: Reason: HTTP 429."
            ]
        );
    }
}
