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

/// A problem in a flow definition, found by `FlowDef::validate`.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FlowError {
    /// A name used by a guard, read, write or required role is not a role of the flow.
    #[error("flow does not define role {0}")]
    UnknownRole(String),
    /// A role maps to a slot whose name starts with '$', which is reserved.
    #[error("role {role} maps to slot {slot}, but slot names may not begin with '$'")]
    ReservedSlotPrefix { role: String, slot: String },
    /// A slot remember_fact could never write, so the role could never be filled.
    #[error("role {role} maps to slot {slot:?}, which is not a fact key: use letters, digits, '.', '-' and '_', starting and ending with a letter, digit or '_'")]
    UnwritableSlot { role: String, slot: String },
    /// A `$out:` reference is not of the shape `$out:action.field`.
    #[error("action {decl} has a malformed reference {raw}: the shape is $out:action.field")]
    BadRef { decl: String, raw: String },
    /// An action passes an arg whose value is not one of its reads.
    #[error("action {decl} has arg {arg} that is not declared in reads")]
    ArgNotDeclaredAsRead { decl: String, arg: String },
    /// Two actions that can both be enabled write the same role.
    #[error("actions {first} and {second} can both be enabled and both write role {role}")]
    ConflictingWriters {
        role: String,
        first: String,
        second: String,
    },
}

/// True when remember_fact can store a fact under `s`. The rule mirrors the key check in
/// `crates/engine/src/turn/builtins.rs` (remember_fact), which trims edge characters first.
fn writable_key(s: &str) -> bool {
    let edge_ok = |c: char| c.is_ascii_alphanumeric() || c == '_';
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
        && s.chars().next().is_some_and(edge_ok)
        && s.chars().last().is_some_and(edge_ok)
}

impl FlowDef {
    /// True when `role` maps to a slot and that slot is in `filled`.
    pub fn role_filled(&self, role: &str, filled: &BTreeSet<String>) -> bool {
        self.roles
            .get(role)
            .is_some_and(|slot| filled.contains(slot))
    }

    /// Whether a guard holds, given the filled fact keys and the intent (None when unknown).
    pub fn guard_holds(
        &self,
        guard: &Guard,
        filled: &BTreeSet<String>,
        intent: Option<&str>,
    ) -> bool {
        match guard {
            Guard::Always => true,
            Guard::Never => false,
            Guard::RolesFilled(roles) => roles.iter().all(|r| self.role_filled(r, filled)),
            Guard::RoleMissing(role) => !self.role_filled(role, filled),
            Guard::IntentIs(want) => intent == Some(want.as_str()),
        }
    }

    /// Every problem in the definition, in a fixed order, with exact duplicates removed.
    pub fn validate(&self) -> Vec<FlowError> {
        let mut found = Vec::new();
        for (role, slot) in &self.roles {
            if slot.starts_with('$') {
                found.push(FlowError::ReservedSlotPrefix {
                    role: role.clone(),
                    slot: slot.clone(),
                });
            } else if !writable_key(slot) {
                found.push(FlowError::UnwritableSlot {
                    role: role.clone(),
                    slot: slot.clone(),
                });
            }
        }
        for action in &self.actions {
            if let Guard::RolesFilled(roles) = &action.when {
                for role in roles {
                    self.check_role(role, &mut found);
                }
            }
            if let Guard::RoleMissing(role) = &action.when {
                self.check_role(role, &mut found);
            }
        }
        for role in &self.required_roles {
            self.check_role(role, &mut found);
        }
        for action in &self.actions {
            for raw in action.reads.iter().chain(action.args.values()) {
                if let Some(rest) = raw.strip_prefix("$out:") {
                    match rest.split_once('.') {
                        Some((act, field)) if !act.is_empty() && !field.is_empty() => {}
                        _ => found.push(FlowError::BadRef {
                            decl: action.name.clone(),
                            raw: raw.clone(),
                        }),
                    }
                } else {
                    self.check_role(raw, &mut found);
                }
            }
            for role in &action.writes {
                self.check_role(role, &mut found);
            }
            for (arg, value) in &action.args {
                if !action.reads.contains(value) {
                    found.push(FlowError::ArgNotDeclaredAsRead {
                        decl: action.name.clone(),
                        arg: arg.clone(),
                    });
                }
            }
        }
        found.extend(self.conflicting_writers());
        let mut unique: Vec<FlowError> = Vec::new();
        for error in found {
            if !unique.contains(&error) {
                unique.push(error);
            }
        }
        unique
    }

    /// Pushes `UnknownRole` when `role` is not a role of the flow.
    fn check_role(&self, role: &str, found: &mut Vec<FlowError>) {
        if !self.roles.contains_key(role) {
            found.push(FlowError::UnknownRole(role.to_string()));
        }
    }

    /// Pairs of actions that can both be enabled in one state and write the same role.
    /// States are: nothing filled or everything filled, crossed with each intent.
    fn conflicting_writers(&self) -> Vec<FlowError> {
        // shortcut: only the two extreme fill states are checked, not every subset of slots,
        // so a conflict that appears only in a partial fill is missed; upgrade when guards get richer.
        let all: BTreeSet<String> = self.roles.values().cloned().collect();
        let empty = BTreeSet::new();
        let mut intents: Vec<Option<&str>> = vec![None];
        for action in &self.actions {
            if let Guard::IntentIs(want) = &action.when {
                if !intents.contains(&Some(want.as_str())) {
                    intents.push(Some(want.as_str()));
                }
            }
        }
        let mut found = Vec::new();
        for filled in [&empty, &all] {
            for intent in &intents {
                let enabled: Vec<&ActionDecl> = self
                    .actions
                    .iter()
                    .filter(|a| self.guard_holds(&a.when, filled, *intent))
                    .collect();
                for (i, first) in enabled.iter().enumerate() {
                    for second in &enabled[i + 1..] {
                        for role in &first.writes {
                            if second.writes.contains(role) {
                                found.push(FlowError::ConflictingWriters {
                                    role: role.clone(),
                                    first: first.name.clone(),
                                    second: second.name.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
        found
    }

    /// The tools the flow offers with no intent, and the required roles still unfilled.
    pub fn offer(&self, filled: &BTreeSet<String>) -> FlowOffer {
        let tools = self
            .actions
            .iter()
            .filter(|a| self.guard_holds(&a.when, filled, None))
            .map(|a| a.component.clone())
            .collect();
        let missing = self
            .required_roles
            .iter()
            .filter(|r| !self.role_filled(r, filled))
            .cloned()
            .collect();
        FlowOffer { tools, missing }
    }

    /// The guidance line naming each missing role and its fact key; None when nothing is missing.
    pub fn note(&self, offer: &FlowOffer) -> Option<String> {
        if offer.missing.is_empty() {
            return None;
        }
        let list = offer
            .missing
            .iter()
            .map(|role| {
                let slot = self.roles.get(role).map_or(role.as_str(), String::as_str);
                format!("{role} (key {slot})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "Flow {}: still needed: {list}. Ask the user for one at a time; when they give one, store it with remember_fact under the key shown.",
            self.id
        ))
    }
}
