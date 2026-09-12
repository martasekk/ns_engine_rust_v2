//! Running an action the deployment registered.
//!
//! The other half of [`super::builtins`]: those are the actions the engine
//! answers itself, this is the one path every wired-in tool takes. Validate
//! the arguments against the tool's own schema, establish where they came
//! from, let the guards object, and only then call it.
//!
//! The order is the point. Nothing reaches a tool that the schema rejected,
//! and no guard is asked about an argument whose provenance is unknown —
//! which is why classification sits between them rather than beside either.

use super::builtins::{Ctx, Step};
use super::gate::classify;
use super::Engine;
use nscore::{
    ClassifiedProposal, EventKind, RejectReason, ReplyPolicy, ToolCtx, ToolOutcome, Verdict,
};

impl Engine {
    /// Run the registered tool this proposal names.
    ///
    /// Legality was checked before the call, so the tool exists. Everything
    /// after that is the gate: a malformed call is refused but keeps the
    /// action legal (the emitter can repair the arguments), a denied one
    /// leaves the schema for the rest of the turn, and an irreversible one
    /// stages itself and ends the turn on the question.
    pub(super) async fn run_tool(&self, cx: &mut Ctx<'_>) -> Step {
        // g0. find the tool (legality guaranteed it exists)
        let tool = self
            .parts
            .tools
            .iter()
            .find(|t| t.spec().name == cx.proposal.action)
            .expect("legality checked above")
            .clone();

        // g1. schema validation (spec M5 §3.2): malformed args are rejected
        // before classification; the action stays legal so the emitter can
        // retry with repaired args.
        if let Err(detail) = nscore::validate_args(&tool.spec().args_schema, &cx.proposal.args) {
            let action = cx.proposal.action.clone();
            return cx.malformed(
                format!("{action}: {detail}"),
                format!("malformed args for {action}: {detail}"),
            );
        }

        // g. classify args against the session's history (spec §5.4)
        let classified_args = classify(cx.log.events(), &cx.proposal.args, tool.spec(), cx.turn);
        let classified = ClassifiedProposal {
            proposal: cx.proposal.clone(),
            args: classified_args.clone(),
        };

        // h. guards
        let guard_ctx = nscore::GuardCtx {
            spec: tool.spec(),
            turn: cx.turn,
            confirmed_this_turn: cx.confirmed_now
                || cx.state.confirmed_this_turn_of == Some(cx.turn),
            fired_actions: &cx.state.fired_tags,
            pending_confirmation: cx.active_pending,
        };
        match self.first_objection(&classified, &guard_ctx) {
            None => {}
            Some((guard, Verdict::Deny { reason })) => {
                // A `NeverResidual` denial is the one that changes what the
                // next iteration may even propose: nothing in the session
                // grounds the argument, so asking is the only way forward.
                if reason.contains("NeverResidual") {
                    cx.book.never_residual = true;
                }
                cx.book.denied.insert(cx.proposal.action.clone());
                let line = format!("guard {guard}: {reason}");
                return cx.refuse(RejectReason::GuardDenied { guard, reason }, line);
            }
            Some((_, Verdict::NeedsConfirmation { prompt })) => {
                // Dry-run when the tool supports it; show the user what would
                // happen (spec §5.4, §9).
                let staged = tool.stage(&cx.proposal.args, &self.tool_ctx(cx)).await;
                let mut text = prompt;
                if let Some(s) = &staged {
                    text.push_str(&format!("\nPlanned: {}", s.description));
                }
                let at = cx.now();
                cx.log.append(
                    cx.turn,
                    at,
                    EventKind::PendingConfirmation {
                        proposal_of: cx.pid,
                        staged,
                    },
                );
                return cx.settle(ReplyPolicy::Verbatim { text });
            }
            // `first_objection` never hands back an allow.
            Some((_, Verdict::Allow)) => unreachable!("an objection is not an allow"),
        }

        // i. perform
        let action = cx.proposal.action.clone();
        let call_id = cx.called(&action, classified_args);
        let outcome = match tool.call(&cx.proposal.args, &self.tool_ctx(cx)).await {
            Ok(output) => ToolOutcome::Ok { output },
            Err(nscore::ToolError::Failed { kind, detail }) => ToolOutcome::Err { kind, detail },
        };
        cx.returned(call_id, outcome);
        // A side effect that really happened is flushed before the turn's
        // remaining iterations get a chance to fail (see `accounting::flush`).
        if tool.spec().side_effect != nscore::SideEffect::Pure {
            self.flush(cx.sid, cx.log, cx.n_loaded).await;
        }
        // The emitter decides what happens next, typically respond_directly.
        Step::Again
    }

    /// What a tool is given besides its arguments: the session it is acting
    /// for, and the store it may put artifacts in.
    fn tool_ctx(&self, cx: &Ctx<'_>) -> ToolCtx {
        ToolCtx {
            session: cx.sid.clone(),
            artifacts: Some(self.parts.memory.clone()),
        }
    }
}
