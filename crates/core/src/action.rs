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

/// Which text a component writes into the [`ActionSpec`]s it hands the
/// emitter (M10 T1.3).
///
/// Not a transform over a finished spec: the short form of a description is
/// written by whoever wrote the long one, in the same place, because a
/// mechanical first-sentence cut would silently drop the one clause a tool
/// needs (`pointer_type`'s `key` against its `text`) and nothing would
/// notice. The rule both profiles obey is the conservative one the field
/// measured (TsCG, arXiv 2605.26165): imperative, one sentence, no restating
/// of parameter names — and **every parameter that changes capability stays
/// in both profiles**, because a removed parameter is a removed capability
/// the model cannot ask for. Only descriptions shrink, and only enums the
/// recorded log never exercised collapse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchemaProfile {
    /// Today's descriptions, unchanged. The default until the live rates of
    /// M10 T1.6 say `Slim` holds.
    #[default]
    Full,
    /// One imperative sentence per tool.
    Slim,
}

impl SchemaProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaProfile::Full => "full",
            SchemaProfile::Slim => "slim",
        }
    }

    /// Config spelling → profile. `Err` carries the message a config error
    /// should print.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "full" => Ok(SchemaProfile::Full),
            "slim" => Ok(SchemaProfile::Slim),
            other => Err(format!(
                "schema_profile {other:?} is unknown — known profiles: full, slim"
            )),
        }
    }

    /// Pick between the two spellings of one piece of text.
    pub fn pick<T>(self, full: T, slim: T) -> T {
        match self {
            SchemaProfile::Full => full,
            SchemaProfile::Slim => slim,
        }
    }
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

/// The argument name that the think-then-commit rationale is injected under
/// by `nsllm::schema::build_tools`. Duplicated as a byte literal rather than
/// depended on, because `core` must not depend on `llm` — `check_arg_names`
/// below is what keeps the two from drifting, and the emitter-side test
/// asserts they still agree.
pub const RATIONALE_ARG: &str = "_rationale";

impl ActionSpec {
    /// Reject argument names that would sort ahead of the injected
    /// `_rationale` property.
    ///
    /// Strict-mode generation follows the order of the `properties` object,
    /// which `serde_json` serializes as a `BTreeMap` (byte order) because
    /// `preserve_order` is off — deliberately, since `Engine::call_key` and
    /// `event_hash` both rely on equal objects serializing identically.
    /// `_` is 0x5F, so the rationale lands first only for argument names
    /// whose first byte is above it: lowercase ASCII (0x61+).
    ///
    /// Uppercase letters (0x41-0x5A) and digits (0x30-0x39) are **below**
    /// `_` and would silently sort ahead of the rationale, reinstating the
    /// exact defect M7 T2.5 fixed by renaming `rationale` to `_rationale`.
    /// That defect was invisible for as long as it existed because the only
    /// action exercising it had a single argument (`text`) that happened to
    /// sort late. So this is checked rather than assumed.
    pub fn check_arg_names(&self) -> Result<(), String> {
        let Some(props) = self
            .args_schema
            .get("properties")
            .and_then(|p| p.as_object())
        else {
            return Ok(());
        };
        for name in props.keys() {
            if name.as_str() == RATIONALE_ARG {
                continue;
            }
            if name.as_str() >= RATIONALE_ARG {
                continue;
            }
            return Err(format!(
                "action `{}`: argument `{}` sorts before `{}`, which would put it \
                 ahead of the rationale in the compiled tool schema and defeat \
                 think-then-commit; argument names must be lowercase ASCII",
                self.name, name, RATIONALE_ARG
            ));
        }
        Ok(())
    }
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
    /// The model produced something the engine could not use.
    Malformed {
        detail: String,
    },
    /// The model endpoint refused or failed. Not the model's fault and not a
    /// proposal at all — kept distinct so it is never replayed back to the
    /// emitter as "you did something malformed", and so an audit can tell a
    /// bad session from a bad afternoon. In session `cli`, 96 of 130
    /// rejections were this, recorded as `Malformed`.
    ProviderUnavailable {
        status: u16,
        detail: String,
    },
    IllegalAction {
        action: String,
    },
    GuardDenied {
        guard: String,
        reason: String,
    },
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
    /// Last time the fact was shown to a model (decay input, M6 §6.2).
    #[serde(default)]
    pub last_used: Timestamp,
    /// M9 T4.1: model calls this fact was rendered into, derived by the
    /// evolution pass from `ModelCall.manifest.fact_keys`.
    ///
    /// Derived, never incremented: every pass recomputes both counters from
    /// the whole log and *sets* them, so a second pass is idempotent, the
    /// numbers are auditable against the events, and a pre-M9 session — whose
    /// manifests are never backfilled — contributes zero.
    #[serde(default)]
    pub exposures: u32,
    /// Of those calls, the ones whose turn went well: a good `Graded` verdict
    /// from the authoritative evaluator, or a `ReplyCited` naming this fact.
    /// The usefulness half of the fitness pair (M9 T4.1).
    #[serde(default)]
    pub credits: u32,
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
            last_used: Timestamp(0),
            exposures: 0,
            credits: 0,
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

    fn spec_with_args(args: serde_json::Value) -> ActionSpec {
        ActionSpec {
            name: "probe".into(),
            description: "probe".into(),
            args_schema: serde_json::json!({"type": "object", "properties": args}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    #[test]
    fn lowercase_argument_names_are_accepted() {
        let spec = spec_with_args(serde_json::json!({
            "button": {"type": "string"},
            "x": {"type": "number"},
            "query": {"type": "string"},
        }));
        assert!(spec.check_arg_names().is_ok());
    }

    /// The defect this guards against is a *recurrence*: the rationale was
    /// originally named `rationale` and silently sorted wherever `r` fell
    /// (M7 T2.5). Renaming it to `_rationale` fixed every argument name in
    /// the workspace at the time, but `_` is 0x5F and uppercase letters and
    /// digits sort below it — so the fix holds only for as long as nobody
    /// adds an argument named like these.
    #[test]
    fn an_uppercase_or_digit_leading_argument_is_rejected() {
        for bad in ["Button", "X", "2fa", "AAA"] {
            let spec = spec_with_args(serde_json::json!({ bad: {"type": "string"} }));
            let err = spec
                .check_arg_names()
                .expect_err("argument sorting before the rationale must be refused");
            assert!(
                err.contains(bad),
                "error names the offending argument: {err}"
            );
        }
    }

    #[test]
    fn the_rationale_argument_itself_is_not_flagged() {
        let spec = spec_with_args(serde_json::json!({
            RATIONALE_ARG: {"type": "string"},
            "text": {"type": "string"},
        }));
        assert!(spec.check_arg_names().is_ok());
    }

    #[test]
    fn a_spec_without_properties_is_vacuously_fine() {
        let spec = ActionSpec {
            name: "nullary".into(),
            description: "takes nothing".into(),
            args_schema: serde_json::json!({"type": "object"}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        };
        assert!(spec.check_arg_names().is_ok());
    }

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
