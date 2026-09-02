use nscore::{
    ClassifiedProposal, Guard, GuardCtx, Provenance, ResidualRule, SideEffect, Trust, Verdict,
};

pub(crate) fn contains_residual(p: &Provenance) -> bool {
    match p {
        Provenance::Residual => true,
        Provenance::Transform { inputs, .. } => inputs.iter().any(contains_residual),
        _ => false,
    }
}

/// Denies proposals whose args are Residual where the spec forbids it.
/// The Deny reason carries the "NeverResidual" marker — the turn loop keys
/// forced clarification off it.
pub struct ResidualPolicy;

impl Guard for ResidualPolicy {
    fn name(&self) -> &str {
        "residual_policy"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        for (arg, tv) in &p.args {
            let forbidden = ctx.spec.residual_policy.get(arg) == Some(&ResidualRule::Never);
            if forbidden && contains_residual(&tv.prov) {
                return Verdict::Deny {
                    reason: format!("NeverResidual: arg '{arg}' has no grounding in this session"),
                };
            }
        }
        Verdict::Allow
    }
}

/// Side-effectful actions and clarification questions may not be driven by
/// External-trust values without confirmation (spec §5.4).
pub struct TaintPolicy;

impl Guard for TaintPolicy {
    fn name(&self) -> &str {
        "taint_policy"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        let gated =
            ctx.spec.side_effect != SideEffect::Pure || p.proposal.action == "ask_clarification";
        if !gated || ctx.confirmed_this_turn {
            return Verdict::Allow;
        }
        let tainted: Vec<&str> = p
            .args
            .iter()
            .filter(|(_, tv)| tv.trust == Trust::External)
            .map(|(k, _)| k.as_str())
            .collect();
        if tainted.is_empty() {
            Verdict::Allow
        } else {
            Verdict::NeedsConfirmation {
                prompt: format!(
                    "This uses data from an external source ({}). Proceed?",
                    tainted.join(", ")
                ),
            }
        }
    }
}

/// (session, dedupe_tag) fires once.
pub struct DedupeGate;

impl Guard for DedupeGate {
    fn name(&self) -> &str {
        "dedupe_gate"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        if ctx.spec.dedupe_tag.is_some() && ctx.fired_actions.contains(&p.proposal.action) {
            Verdict::Deny {
                reason: format!("'{}' already done this session", p.proposal.action),
            }
        } else {
            Verdict::Allow
        }
    }
}

/// Irreversible actions require a Confirmed event this turn; otherwise the
/// proposal is staged and the user is asked (spec §5.4, §9).
pub struct SideEffectGate;

impl Guard for SideEffectGate {
    fn name(&self) -> &str {
        "side_effect_gate"
    }
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
        if ctx.spec.side_effect == SideEffect::Irreversible && !ctx.confirmed_this_turn {
            Verdict::NeedsConfirmation {
                prompt: format!(
                    "'{}' is irreversible. Confirm to proceed.",
                    p.proposal.action
                ),
            }
        } else {
            Verdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;
    use std::collections::HashSet;

    fn spec(
        effect: SideEffect,
        residual: &[(&str, ResidualRule)],
        dedupe: Option<&str>,
    ) -> ActionSpec {
        ActionSpec {
            name: "act".into(),
            description: "d".into(),
            args_schema: serde_json::json!({"type": "object", "properties": {}}),
            side_effect: effect,
            residual_policy: residual.iter().map(|(k, r)| (k.to_string(), *r)).collect(),
            dedupe_tag: dedupe.map(String::from),
        }
    }

    fn proposal(action: &str, args: Vec<(&str, Provenance, Trust)>) -> ClassifiedProposal {
        ClassifiedProposal {
            proposal: Proposal {
                rationale: "".into(),
                action: action.into(),
                args: serde_json::json!({}),
            },
            args: args
                .into_iter()
                .map(|(k, prov, trust)| {
                    (
                        k.to_string(),
                        TaggedValue {
                            value: serde_json::json!("v"),
                            prov,
                            trust,
                        },
                    )
                })
                .collect(),
        }
    }

    fn ctx<'a>(spec: &'a ActionSpec, fired: &'a HashSet<String>, confirmed: bool) -> GuardCtx<'a> {
        GuardCtx {
            spec,
            turn: 1,
            confirmed_this_turn: confirmed,
            fired_actions: fired,
            pending_confirmation: None,
        }
    }

    #[test]
    fn residual_policy_denies_never_residual_args() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let p = proposal(
            "act",
            vec![("order_id", Provenance::Residual, Trust::System)],
        );
        match ResidualPolicy.check(&p, &ctx(&s, &fired, false)) {
            Verdict::Deny { reason } => {
                assert!(reason.contains("NeverResidual"));
                assert!(reason.contains("order_id"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn residual_policy_allows_grounded_args_and_allowed_residuals() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let grounded = proposal(
            "act",
            vec![(
                "order_id",
                Provenance::UserInput {
                    turn: 1,
                    start: 0,
                    end: 2,
                },
                Trust::User,
            )],
        );
        assert!(matches!(
            ResidualPolicy.check(&grounded, &ctx(&s, &fired, false)),
            Verdict::Allow
        ));
        let free_arg = proposal("act", vec![("note", Provenance::Residual, Trust::System)]);
        assert!(matches!(
            ResidualPolicy.check(&free_arg, &ctx(&s, &fired, false)),
            Verdict::Allow
        ));
    }

    #[test]
    fn residual_policy_sees_residual_inside_transforms() {
        let s = spec(SideEffect::Pure, &[("order_id", ResidualRule::Never)], None);
        let fired = HashSet::new();
        let p = proposal(
            "act",
            vec![(
                "order_id",
                Provenance::Transform {
                    func: "trim".into(),
                    inputs: vec![Provenance::Residual],
                },
                Trust::System,
            )],
        );
        assert!(matches!(
            ResidualPolicy.check(&p, &ctx(&s, &fired, false)),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn taint_policy_gates_external_trust_on_side_effects() {
        let s = spec(SideEffect::Reversible, &[], None);
        let fired = HashSet::new();
        let p = proposal(
            "act",
            vec![(
                "target",
                Provenance::CopiedOutput {
                    call: EventId(3),
                    path: "$.x".into(),
                },
                Trust::External,
            )],
        );
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
        // confirmed this turn -> allowed
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&s, &fired, true)),
            Verdict::Allow
        ));
        // pure action -> not gated
        let pure = spec(SideEffect::Pure, &[], None);
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&pure, &fired, false)),
            Verdict::Allow
        ));
    }

    #[test]
    fn taint_policy_gates_clarification_questions_too() {
        let s = spec(SideEffect::Pure, &[], None);
        let fired = HashSet::new();
        let p = proposal(
            "ask_clarification",
            vec![(
                "question",
                Provenance::CopiedOutput {
                    call: EventId(3),
                    path: "$".into(),
                },
                Trust::External,
            )],
        );
        assert!(matches!(
            TaintPolicy.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
    }

    #[test]
    fn side_effect_gate_blocks_unconfirmed_irreversible() {
        let s = spec(SideEffect::Irreversible, &[], None);
        let fired = HashSet::new();
        let p = proposal("wipe", vec![]);
        assert!(matches!(
            SideEffectGate.check(&p, &ctx(&s, &fired, false)),
            Verdict::NeedsConfirmation { .. }
        ));
        assert!(matches!(
            SideEffectGate.check(&p, &ctx(&s, &fired, true)),
            Verdict::Allow
        ));
        let reversible = spec(SideEffect::Reversible, &[], None);
        assert!(matches!(
            SideEffectGate.check(&p, &ctx(&reversible, &fired, false)),
            Verdict::Allow
        ));
    }

    #[test]
    fn dedupe_gate_fires_once_per_tagged_action() {
        let s = spec(SideEffect::Pure, &[], Some("greeting"));
        let mut fired = HashSet::new();
        let p = proposal("act", vec![]);
        assert!(matches!(
            DedupeGate.check(&p, &ctx(&s, &fired, false)),
            Verdict::Allow
        ));
        fired.insert("act".into());
        assert!(matches!(
            DedupeGate.check(&p, &ctx(&s, &fired, false)),
            Verdict::Deny { .. }
        ));
        // untagged spec never gated
        let untagged = spec(SideEffect::Pure, &[], None);
        assert!(matches!(
            DedupeGate.check(&p, &ctx(&untagged, &fired, false)),
            Verdict::Allow
        ));
    }
}
