use crate::event::EventId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Declaration order defines Ord: External is LEAST trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Trust {
    External,
    System,
    User,
}

/// Minimum trust across a chain; empty chain is User (nothing untrusted involved).
pub fn min_trust(chain: &[Trust]) -> Trust {
    chain.iter().copied().min().unwrap_or(Trust::User)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Provenance {
    Constant,
    UserInput { turn: u32, start: u32, end: u32 },
    CopiedOutput { call: EventId, path: String },
    Transform { func: String, inputs: Vec<Provenance> },
    Residual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedValue {
    pub value: serde_json::Value,
    pub prov: Provenance,
    pub trust: Trust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(#[serde(with = "crate::event::hash_serde")] pub [u8; 32]);

impl ArtifactId {
    pub fn for_content(bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(bytes);
        ArtifactId(h.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_ordering_least_first() {
        assert!(Trust::External < Trust::System);
        assert!(Trust::System < Trust::User);
        assert_eq!(
            min_trust(&[Trust::User, Trust::External, Trust::System]),
            Trust::External
        );
        assert_eq!(min_trust(&[]), Trust::User);
    }

    #[test]
    fn artifact_id_is_content_hash() {
        let a = ArtifactId::for_content(b"hello");
        let b = ArtifactId::for_content(b"hello");
        let c = ArtifactId::for_content(b"world");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn provenance_serde_round_trip() {
        let tv = TaggedValue {
            value: serde_json::json!({"order": 42}),
            prov: Provenance::CopiedOutput {
                call: crate::event::EventId(7),
                path: "$.result.id".into(),
            },
            trust: Trust::System,
        };
        let json = serde_json::to_string(&tv).unwrap();
        let back: TaggedValue = serde_json::from_str(&json).unwrap();
        assert_eq!(tv, back);
    }
}
