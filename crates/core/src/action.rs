use crate::event::{SessionId, Timestamp};
use crate::value::{ArtifactId, Provenance, TaggedValue, Trust};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffect {
    Pure,
    Reversible,
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResidualRule {
    Allowed,
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub name: String,
    pub description: String,
    pub args_schema: serde_json::Value,
    pub side_effect: SideEffect,
    pub residual_policy: HashMap<String, ResidualRule>,
    /// When set, this action fires at most once per session (DedupeGate).
    #[serde(default)]
    pub dedupe_tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegalActionSet {
    pub actions: Vec<ActionSpec>,
}

impl LegalActionSet {
    pub fn contains(&self, name: &str) -> bool {
        self.actions.iter().any(|a| a.name == name)
    }

    pub fn get(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|a| a.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub rationale: String,
    pub action: String,
    pub args: serde_json::Value,
}

/// Proposal after provenance classification — what guards actually see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifiedProposal {
    pub proposal: Proposal,
    pub args: Vec<(String, TaggedValue)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Verdict {
    Allow,
    Deny { reason: String },
    NeedsConfirmation { prompt: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RejectReason {
    Malformed { detail: String },
    IllegalAction { action: String },
    GuardDenied { guard: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReplyPolicy {
    Verbatim { text: String },
    Template { id: String, vars: serde_json::Value },
    Generate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub summary: String,
    pub artifact: Option<ArtifactId>,
    pub trust: Trust,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolOutcome {
    Ok { output: ToolOutput },
    Err { kind: String, detail: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedEffect {
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub uses: u32,
    pub last_validated: Timestamp,
    pub prov: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incoming {
    pub session: SessionId,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventId, EventKind};

    #[test]
    fn legal_set_lookup() {
        let set = LegalActionSet {
            actions: vec![ActionSpec {
                name: "echo".into(),
                description: "echo text back".into(),
                args_schema: serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            }],
        };
        assert!(set.contains("echo"));
        assert!(!set.contains("nuke"));
        assert_eq!(set.get("echo").unwrap().side_effect, SideEffect::Pure);
    }

    #[test]
    fn full_event_kind_serde() {
        let kinds = vec![
            EventKind::Proposed {
                proposal: Proposal {
                    rationale: "user wants an echo".into(),
                    action: "echo".into(),
                    args: serde_json::json!({"text":"hi"}),
                },
            },
            EventKind::Rejected {
                proposal_of: EventId(3),
                reason: RejectReason::GuardDenied {
                    guard: "residual".into(),
                    reason: "OrderId residual".into(),
                },
            },
            EventKind::Settled { policy: ReplyPolicy::Generate },
        ];
        for k in kinds {
            let s = serde_json::to_string(&k).unwrap();
            let back: EventKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }
}
