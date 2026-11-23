//! A flow is the old flow engine's script format, loaded unchanged. In v2 it narrows which
//! registered tools the emitter is offered and names the details still to collect. Fields v2
//! does not act on yet (fast path, templates, follow-up, action writes) are kept, so a round
//! trip through this module loses nothing.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "when", content = "value", rename_all = "snake_case")]
pub enum Guard {
    Always,
    Never,
    RolesFilled(Vec<String>),
    RoleMissing(String),
    IntentIs(String),
}

/// Where an action's writes are kept. Stored as written; v2 does not act on it yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteScope {
    Turn,
    Flow,
    Conversation,
    User,
}

/// One action as written in configuration. `reads` and `args` values are raw
/// ref strings: either "$out:<action>.<field>" or a ROLE name.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ActionDecl {
    pub name: String,
    pub component: String,
    pub reads: Vec<String>,
    pub writes: Vec<String>,
    pub write_scope: WriteScope,
    pub args: BTreeMap<String, String>,
    pub when: Guard,
}

/// A rule that answers a structured event without the LLM. `match_on` is
/// compared against `UniversalTurnInput::event_id`; `pattern` (when present)
/// is a plain substring test on the text, kept deliberately dumber than a
/// regex so the fast path cannot become a second rule engine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FastPathRule {
    pub match_on: Option<String>,
    pub pattern: Option<String>,
    pub sets: BTreeMap<String, serde_json::Value>,
    pub reply_template: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FlowDef {
    pub id: String,
    pub version: u32,
    /// The only place slot names exist.
    pub roles: BTreeMap<String, String>,
    pub required_roles: Vec<String>,
    pub actions: Vec<ActionDecl>,
    pub fast_path: Vec<FastPathRule>,
    pub reply_template: Option<String>,
    /// Reply templates by id. A template reply is rendered here with no model
    /// call, which is what makes the fast path free.
    #[serde(default)]
    pub templates: BTreeMap<String, String>,
    /// Wake this conversation this many milliseconds after a turn settles.
    /// None means no follow-up.
    #[serde(default)]
    pub follow_up_ms: Option<i64>,
}

/// What a flow allows on one turn: the registered tools it offers, and the required roles still unfilled.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FlowOffer {
    pub tools: BTreeSet<String>,
    pub missing: Vec<String>,
}
