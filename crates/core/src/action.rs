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

/// Lifecycle state of one fact version (M6 spec §6.1–6.2). Facts are never
/// overwritten: a new value supersedes the old row, a forgotten fact keeps
/// its row with `valid_to` set, a cold fact is unused past the staleness
/// window and drops out of the pinned slice but stays searchable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactState {
    #[default]
    Current,
    Superseded,
    Cold,
    Forgotten,
}

impl FactState {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactState::Current => "current",
            FactState::Superseded => "superseded",
            FactState::Cold => "cold",
            FactState::Forgotten => "forgotten",
        }
    }
    pub fn parse(s: &str) -> Option<FactState> {
        match s {
            "current" => Some(FactState::Current),
            "superseded" => Some(FactState::Superseded),
            "cold" => Some(FactState::Cold),
            "forgotten" => Some(FactState::Forgotten),
            _ => None,
        }
    }
}

/// One version of a durable fact. Identity is `(scope, key, valid_from)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub uses: u32,
    pub last_validated: Timestamp,
    pub prov: Provenance,
    /// Per-user / per-deployment isolation (M6 §6.6); `global` by default.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Trust of the value's origin at write time (M6 §6.7).
    #[serde(default = "default_fact_trust")]
    pub trust: Trust,
    /// When this version became current.
    #[serde(default)]
    pub valid_from: Timestamp,
    /// When it stopped being current (superseded or forgotten); None = current.
    #[serde(default)]
    pub valid_to: Option<Timestamp>,
    #[serde(default)]
    pub state: FactState,
}

fn default_scope() -> String {
    "global".into()
}

fn default_fact_trust() -> Trust {
    Trust::System
}

impl Default for Fact {
    fn default() -> Self {
        Self {
            key: String::new(),
            value: serde_json::Value::Null,
            confidence: 1.0,
            uses: 0,
            last_validated: Timestamp(0),
            prov: Provenance::Residual,
            scope: default_scope(),
            trust: default_fact_trust(),
            valid_from: Timestamp(0),
            valid_to: None,
            state: FactState::Current,
        }
    }
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
    fn fact_defaults_and_state_round_trip() {
        let f = Fact {
            key: "user.name".into(),
            value: serde_json::json!("Martin"),
            ..Default::default()
        };
        assert_eq!(f.scope, "global");
        assert_eq!(f.state, FactState::Current);
        assert_eq!(f.valid_to, None);
        assert_eq!(f.trust, crate::value::Trust::System);
        // an old JSON row without the new fields still parses
        let old: Fact = serde_json::from_str(
            r#"{"key":"k","value":1,"confidence":1.0,"uses":2,"last_validated":5,"prov":{"type":"Constant"}}"#,
        )
        .unwrap();
        assert_eq!(old.scope, "global");
        assert_eq!(old.state, FactState::Current);
        for s in [
            FactState::Current,
            FactState::Superseded,
            FactState::Cold,
            FactState::Forgotten,
        ] {
            assert_eq!(FactState::parse(s.as_str()), Some(s));
        }
        assert_eq!(FactState::parse("nope"), None);
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
            EventKind::Settled {
                policy: ReplyPolicy::Generate,
            },
        ];
        for k in kinds {
            let s = serde_json::to_string(&k).unwrap();
            let back: EventKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }
}
