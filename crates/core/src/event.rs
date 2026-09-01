use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

/// Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    UserSaid { text: String },
    Proposed { proposal: crate::action::Proposal },
    Rejected { proposal_of: EventId, reason: crate::action::RejectReason },
    ToolCalled { action: String, args: Vec<(String, crate::value::TaggedValue)> },
    ToolReturned { call: EventId, outcome: crate::action::ToolOutcome },
    PendingConfirmation { proposal_of: EventId, staged: Option<crate::action::StagedEffect> },
    Confirmed { pending: EventId },
    Corrected { target: Option<EventId>, text: String },
    Settled { policy: crate::action::ReplyPolicy },
    Replied { text: String },
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
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serde_round_trip() {
        let e = Event {
            id: EventId(1),
            parent: None,
            prev_hash: [0u8; 32],
            turn: 0,
            at: Timestamp(1_756_700_000_000),
            kind: EventKind::UserSaid { text: "ahoj".into() },
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }
}
