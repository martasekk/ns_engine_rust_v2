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
    /// Rolling summary of the turns outside the window (M6 §5.1), written
    /// off the user's critical path. In the log because it is what the
    /// models were shown; excluded from replay diffs (derived prose whose
    /// cadence is configuration).
    Summarized {
        summary: crate::memory::SessionSummary,
    },
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
}
