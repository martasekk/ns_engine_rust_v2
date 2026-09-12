//! What a proposal has to get past before it runs.
//!
//! Provenance for its arguments, then the guard chain the engine always
//! runs. Both are here so "what checks a call" is one file rather than a
//! detail of the loop.

use super::Engine;
use nscore::{ClassifiedProposal, GuardCtx, Verdict};

impl Engine {
    /// The first guard to object to a proposal, and what it said.
    ///
    /// The chain is always the engine's own guards followed by the harness's,
    /// and it always stops at the first objection — a proposal refused twice
    /// is still refused once. Written out twice in the loop before this, once
    /// for registered tools and once for `ask_clarification`, which differ
    /// only in whether they distinguish a denial from a confirmation prompt.
    pub(super) fn first_objection(
        &self,
        classified: &ClassifiedProposal,
        ctx: &GuardCtx,
    ) -> Option<(String, Verdict)> {
        self.builtin_guards
            .iter()
            .chain(self.parts.guards.iter())
            .find_map(|g| match g.check(classified, ctx) {
                Verdict::Allow => None,
                objection => Some((g.name().to_string(), objection)),
            })
    }
}

/// Establish where each of a proposal's arguments came from.
///
/// Seven places in the turn loop built this pair by hand — rebuild the value
/// index from the session's events, then classify against it — and provenance
/// is the mechanism that decides what a guard is allowed to conclude about an
/// argument. Seven copies of it is seven places a change has to be made and
/// six places it can be forgotten, which is the shape of an invariant that
/// eventually holds in most of the codebase.
///
/// The index is rebuilt per call rather than cached because it is derived
/// from the log, and the log grows within a turn: a value copied out of a
/// tool result three steps ago must be groundable now.
pub(super) fn classify(
    events: &[nscore::Event],
    args: &serde_json::Value,
    spec: &nscore::ActionSpec,
    turn: u32,
) -> Vec<(String, nscore::TaggedValue)> {
    let index = nsprovenance::index::ValueIndex::from_events(events);
    nsprovenance::classify::classify_args(args, spec, &index, turn)
}

/// The guards every engine runs, before the harness's own.
///
/// `SideEffectGate` is left out rather than neutered when confirmation is off,
/// so a trace shows no side-effect gate at all instead of one that silently
/// allows everything. An unattended session has nobody to answer the prompt,
/// and a staged proposal there is not a safeguard, it is a stall.
pub(super) fn builtin_guards(confirm_irreversible: bool) -> Vec<Box<dyn nscore::Guard>> {
    let mut guards: Vec<Box<dyn nscore::Guard>> = vec![
        Box::new(crate::guards::ResidualPolicy),
        Box::new(crate::guards::TaintPolicy),
        Box::new(crate::guards::DedupeGate),
    ];
    if confirm_irreversible {
        guards.push(Box::new(crate::guards::SideEffectGate));
    }
    guards
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::EngineConfig;

    /// The default must stay the safe one: a config that says nothing about
    /// confirmation gets the gate.
    #[test]
    fn irreversible_actions_are_confirmed_unless_asked_otherwise() {
        assert!(EngineConfig::default().confirm_irreversible);
    }

    /// Switching it off removes the gate rather than leaving one that always
    /// allows, so a trace shows honestly that nothing was gating.
    #[test]
    fn the_side_effect_gate_is_absent_when_confirmation_is_off() {
        fn names(confirm: bool) -> Vec<String> {
            builtin_guards(confirm)
                .iter()
                .map(|g| g.name().to_string())
                .collect()
        }
        assert!(names(true).contains(&"side_effect_gate".to_string()));
        assert!(!names(false).contains(&"side_effect_gate".to_string()));
        assert_eq!(
            names(false).len() + 1,
            names(true).len(),
            "only the one guard differs"
        );
    }
}
