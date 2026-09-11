use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// Unix milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    UserSaid {
        text: String,
    },
    Proposed {
        proposal: crate::action::Proposal,
    },
    Rejected {
        proposal_of: EventId,
        reason: crate::action::RejectReason,
    },
    ToolCalled {
        action: String,
        args: Vec<(String, crate::value::TaggedValue)>,
    },
    ToolReturned {
        call: EventId,
        outcome: crate::action::ToolOutcome,
    },
    PendingConfirmation {
        proposal_of: EventId,
        staged: Option<crate::action::StagedEffect>,
    },
    Confirmed {
        pending: EventId,
    },
    Corrected {
        target: Option<EventId>,
        text: String,
    },
    Settled {
        policy: crate::action::ReplyPolicy,
    },
    Replied {
        text: String,
    },
    /// The reply model failed after `Settled { Generate }`; the fallback
    /// `Replied` follows. Infrastructure, not behavior: replay ignores it
    /// (the recorded reply is replayed as if it had succeeded).
    ReplyFailed {
        detail: String,
    },
    /// The grounding interceptor (M6 §4.5) found `spans` in a first draft
    /// that nothing in the reply context supports; the reply was generated
    /// once more with them named. Infrastructure: replay ignores it.
    ReplyFlagged {
        draft: String,
        spans: Vec<String>,
    },
    /// The echo interceptor found that a first draft was `ratio` lifted out
    /// of its own prompt — `span` is the longest copied run — and the reply
    /// was generated once more with it named. The mirror of `ReplyFlagged`:
    /// that one catches invention, this one catches copying, and only the
    /// pair of them bounds a reply on both sides. Infrastructure: replay
    /// ignores it.
    ReplyEchoed {
        draft: String,
        span: String,
        ratio: f32,
    },
    /// Which reference parts the final reply drew on (M9 T4.2): `persona`,
    /// `trace`, `fact:<key>`, `summary`, `window:<turn>`, `guidance:<hash>`,
    /// `obligations`. Written only when the list is non-empty.
    ///
    /// The backward arrow's second input. A grade says a turn went well; this
    /// says *what was in the prompt when it did*, at the granularity the
    /// forgetting decision needs — `ContextManifest` records what was shown,
    /// and only the pair of them can tell a fact that earned its place from
    /// one that merely occupied it. Infrastructure exactly like
    /// [`EventKind::Graded`]: replay ignores it, the fold does not turn it
    /// into a `did:` line, and no context ever contains it.
    ReplyCited {
        sources: Vec<String>,
    },
    /// Rolling summary of the turns outside the window (M6 §5.1), written
    /// off the user's critical path. In the log because it is what the
    /// models were shown; excluded from replay diffs (derived prose whose
    /// cadence is configuration).
    Summarized {
        summary: crate::memory::SessionSummary,
    },
    /// What one provider call cost and what it was shown (M6 §9, M7 T0.1).
    /// Infrastructure: replay ignores it, the fold does not turn it into a
    /// `did:` line, and no context ever contains it — measuring a turn must
    /// not change it.
    ModelCall {
        usage: crate::usage::Usage,
        manifest: crate::usage::ContextManifest,
    },
    /// What an evaluator made of one turn (M8 T2.3a, M9 T1.1).
    ///
    /// A grade is a *recorded value*, the way Temporal records a
    /// `SideEffect`: the pass computes it once, writes it here, and every
    /// later reader — a second pass, the notes gate, a replay — reads the
    /// record instead of asking a scorer again. Without that, replaying a
    /// Rust session would need a Python service running, and a weight change
    /// would silently rewrite history.
    ///
    /// `by` is the evaluator's id and `revision` its model or cut string, so
    /// drift shows up as a diff instead of disappearing into a number — the
    /// `MutableSideEffect` half. Infrastructure, exactly like
    /// [`EventKind::ModelCall`]: replay ignores it, the fold does not turn it
    /// into a `did:` line, and no context ever contains it — grading a turn
    /// must not change it.
    Graded {
        turn: u32,
        grade: Grade,
        by: String,
        revision: String,
    },
}

/// One evaluator's verdict on one turn, in the only vocabulary the log keeps.
///
/// The issue kinds travel as strings on purpose: `nscore` sits underneath
/// `nsevolution`, and a log format that named `nsevolution::Issue` would make
/// the event schema a hostage of the lane's enum. A reader that wants the
/// enum back parses the string; a reader that only wants "did this turn go
/// wrong" reads `ok`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Grade {
    pub ok: bool,
    #[serde(default)]
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub parent: Option<EventId>,
    #[serde(with = "hash_serde")]
    pub prev_hash: [u8; 32],
    pub turn: u32,
    pub at: Timestamp,
    pub kind: EventKind,
}

/// Rejected proposals bucketed by the guard or parse step that fired, and
/// the proposals they are rated against (M10 T0.2).
///
/// Every rejection is a request already spent — the emitter was called, the
/// provider answered, and the answer was thrown away — so the count that
/// matters is not "how many rejections" but "how many per hundred things the
/// emitter proposed". On the recorded 21-turn desktop log that rate is 11.1,
/// and six of the nine are `repeat_gate`: the model re-proposing a call the
/// trace already shows as done. Which bucket is largest decides what to fix,
/// and the buckets are different fixes — a `repeat_gate` loop is prompt-side,
/// a `Malformed` is the schema, an `IllegalAction` is the legal set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RejectionTally {
    /// `Proposed` events, the denominator. Every one of them cost a request.
    pub proposals: usize,
    /// Reason label to count, ordered by label so a report is stable; the
    /// rendered line reorders by size.
    pub by_reason: std::collections::BTreeMap<String, usize>,
}

impl RejectionTally {
    pub fn rejections(&self) -> usize {
        self.by_reason.values().sum()
    }

    /// Rejections per hundred proposals, or `None` when nothing was
    /// proposed — a rate over an empty denominator is not zero, it is
    /// absent, and printing `0.0` would read as a session that proposed
    /// cleanly.
    pub fn per_hundred(&self) -> Option<f64> {
        (self.proposals > 0).then(|| 100.0 * self.rejections() as f64 / self.proposals as f64)
    }

    /// Largest bucket first, ties by label, then the rate. One line, because
    /// both readers of it (`ns-app budget`'s footer and the evolution pass's
    /// report) print it among other one-line numbers.
    pub fn line(&self) -> String {
        if self.proposals == 0 {
            return "no proposals in this log".to_string();
        }
        let rate = self.per_hundred().unwrap_or_default();
        if self.by_reason.is_empty() {
            return format!("none — 0.0 per 100 proposals ({} proposed)", self.proposals);
        }
        let mut buckets: Vec<(&String, &usize)> = self.by_reason.iter().collect();
        buckets.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let list: Vec<String> = buckets.iter().map(|(k, n)| format!("{k} {n}")).collect();
        format!(
            "{} \u{2014} {rate:.1} per 100 proposals ({} proposed)",
            list.join(", "),
            self.proposals
        )
    }
}

/// The bucket one rejection falls in. A `GuardDenied` is labelled by the
/// guard that fired, because "GuardDenied 6" names no fix; the other three
/// are labelled by the variant, because the variant *is* the step that
/// failed.
pub fn reject_bucket(reason: &crate::action::RejectReason) -> String {
    match reason {
        crate::action::RejectReason::GuardDenied { guard, .. } => guard.clone(),
        crate::action::RejectReason::IllegalAction { .. } => "IllegalAction".to_string(),
        crate::action::RejectReason::Malformed { .. } => "Malformed".to_string(),
        crate::action::RejectReason::ProviderUnavailable { .. } => {
            "ProviderUnavailable".to_string()
        }
    }
}

/// Count `Proposed` and bucket `Rejected` over one session's events.
///
/// Pure over the slice so both callers share one definition of the number:
/// a rate that meant one thing in `ns-app budget` and another in the
/// evolution pass would be worse than no rate at all.
pub fn tally_rejections(events: &[Event]) -> RejectionTally {
    let mut tally = RejectionTally::default();
    for e in events {
        match &e.kind {
            EventKind::Proposed { .. } => tally.proposals += 1,
            EventKind::Rejected { reason, .. } => {
                *tally.by_reason.entry(reject_bucket(reason)).or_default() += 1;
            }
            _ => {}
        }
    }
    tally
}

/// Serialize [u8; 32] as lowercase hex.
pub(crate) mod hash_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(h: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 64 {
            return Err(serde::de::Error::custom("hash must be 64 hex chars"));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(serde::de::Error::custom)?;
        }
        Ok(out)
    }
}

fn event_hash(e: &Event) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(e).expect("event serialization is infallible");
    let mut h = Sha256::new();
    h.update(&json);
    h.finalize().into()
}

#[derive(Debug, Clone)]
pub struct EventLog {
    session: SessionId,
    events: Vec<Event>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ChainError {
    #[error("hash chain broken at event {id:?}")]
    BrokenAt { id: EventId },
}

impl EventLog {
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            events: Vec::new(),
        }
    }

    pub fn from_events(session: SessionId, events: Vec<Event>) -> Self {
        Self { session, events }
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn next_id(&self) -> EventId {
        EventId(self.events.last().map(|e| e.id.0 + 1).unwrap_or(1))
    }

    pub fn append(&mut self, turn: u32, at: Timestamp, kind: EventKind) -> &Event {
        let prev_hash = self.events.last().map(event_hash).unwrap_or([0u8; 32]);
        let e = Event {
            id: self.next_id(),
            parent: None,
            prev_hash,
            turn,
            at,
            kind,
        };
        self.events.push(e);
        self.events.last().unwrap()
    }

    pub fn verify_chain(&self) -> Result<(), ChainError> {
        let mut prev: Option<&Event> = None;
        for e in &self.events {
            let expected = prev.map(event_hash).unwrap_or([0u8; 32]);
            if e.prev_hash != expected {
                return Err(ChainError::BrokenAt { id: e.id });
            }
            prev = Some(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_chains_hashes_and_verifies() {
        let mut log = EventLog::new(SessionId("s1".into()));
        log.append(0, Timestamp(1), EventKind::UserSaid { text: "a".into() });
        log.append(0, Timestamp(2), EventKind::Replied { text: "b".into() });
        log.append(1, Timestamp(3), EventKind::UserSaid { text: "c".into() });
        assert_eq!(log.events().len(), 3);
        assert_eq!(log.events()[0].prev_hash, [0u8; 32]);
        assert_ne!(log.events()[1].prev_hash, [0u8; 32]);
        assert!(log.verify_chain().is_ok());
    }

    #[test]
    fn tampering_breaks_the_chain() {
        let mut log = EventLog::new(SessionId("s1".into()));
        log.append(0, Timestamp(1), EventKind::UserSaid { text: "a".into() });
        log.append(0, Timestamp(2), EventKind::Replied { text: "b".into() });
        let mut events = log.events().to_vec();
        events[0].kind = EventKind::UserSaid {
            text: "TAMPERED".into(),
        };
        let tampered = EventLog::from_events(SessionId("s1".into()), events);
        assert!(matches!(
            tampered.verify_chain(),
            Err(ChainError::BrokenAt { id: EventId(2) })
        ));
    }

    #[test]
    fn event_serde_round_trip() {
        let e = Event {
            id: EventId(1),
            parent: None,
            prev_hash: [0u8; 32],
            turn: 0,
            at: Timestamp(1_756_700_000_000),
            kind: EventKind::UserSaid {
                text: "ahoj".into(),
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    /// M9 T1.1. Events are hash-chained over their serialized JSON, so the
    /// only safe way to add a grade is a new event appended after the ones
    /// that exist. A log written before `Graded` did must therefore still
    /// parse, unchanged, byte for byte.
    #[test]
    fn an_old_log_without_graded_events_still_parses() {
        let json = r#"{"id":1,"parent":null,"prev_hash":"0000000000000000000000000000000000000000000000000000000000000000","turn":3,"at":1756700000000,"kind":{"type":"UserSaid","text":"ahoj"}}"#;
        let e: Event = serde_json::from_str(json).unwrap();
        assert_eq!(e.turn, 3);
        assert!(matches!(e.kind, EventKind::UserSaid { .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), json);
    }

    #[test]
    fn a_graded_event_round_trips() {
        let e = Event {
            id: EventId(7),
            parent: None,
            prev_hash: [0u8; 32],
            turn: 4,
            at: Timestamp(1_756_700_000_000),
            kind: EventKind::Graded {
                turn: 4,
                grade: Grade {
                    ok: false,
                    issues: vec!["ungrounded".into()],
                },
                by: "symbolic".into(),
                revision: "n/a".into(),
            },
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains(r#""type":"Graded""#), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
        // Appending it leaves every earlier event's hash alone.
        let mut log = EventLog::new(SessionId("s1".into()));
        log.append(1, Timestamp(1), EventKind::UserSaid { text: "a".into() });
        let before = log.events()[0].clone();
        log.append(1, Timestamp(2), e.kind.clone());
        assert_eq!(log.events()[0], before);
        assert!(log.verify_chain().is_ok());
    }

    /// A grade written by an evaluator that recorded no issue list at all.
    #[test]
    fn a_grade_without_issues_defaults_to_empty() {
        let g: Grade = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(g.ok && g.issues.is_empty());
    }
}
