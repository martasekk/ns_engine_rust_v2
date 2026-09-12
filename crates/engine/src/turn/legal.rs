//! Which actions the emitter is offered on this iteration.
//!
//! Every schema in the array is tokens spent on a choice, so the set is
//! narrowed on three separate grounds and each is a different kind of fact:
//!
//! - the **tier** decides whether registered tools ride this turn at all,
//!   and the route may have picked a subset of them;
//! - **this turn's own events** drop what was already refused, already done,
//!   or can no longer mean anything — replay reproduces all of it;
//! - two **store questions**, asked once before the loop and held fixed
//!   across it, drop `recall` and the two forgets when there is nothing to
//!   recall or nothing stored to forget.
//!
//! The order matters only in that the engine's own actions are appended last
//! and unconditionally where they are unconditional: they are how a turn
//! ends, so no narrowing may leave the emitter with no way to finish.

use super::builtins::Bookkeeping;
use super::specs::*;
use super::Engine;
use nscore::LegalActionSet;

/// What decides the legal set on one iteration.
///
/// Eight values the loop used to consult in place, which made the set's
/// shape a property of where you were standing in `run_turn`. Named
/// together, they are the question itself.
pub(super) struct Offer<'a> {
    pub book: &'a Bookkeeping,
    /// This turn's events, for the one thing that reads them: whether any
    /// result is still clipped and so worth an `inspect_result`.
    pub events: &'a [nscore::Event],
    pub turn: u32,
    pub tier: nscore::Tier,
    /// The route's tool selection; `None` means the full registry.
    pub selected_tools: Option<&'a Vec<String>>,
    /// Decided before the loop: is there anything out of sight to recall?
    pub recall_applies: bool,
    /// Decided before the loop: does the scope hold any fact to forget?
    pub scope_holds_facts: bool,
    /// Whether a staged proposal is still confirmable this turn.
    pub active_pending: bool,
}

impl Engine {
    /// The actions the emitter may choose from, this iteration.
    pub(super) fn legal_actions(&self, o: &Offer<'_>) -> LegalActionSet {
        let profile = self.cfg.schema_profile;
        if o.book.never_residual {
            // Forced clarification (spec §5.1): a NeverResidual rejection
            // occurred and nothing grounds the arg — the only way forward is
            // to ask (respond_directly stays available at schema level).
            return LegalActionSet {
                actions: vec![ask_clarification_spec(profile)],
            };
        }

        // Narrowed schema (spec §2): actions rejected this turn are removed
        // from the set the emitter sees next.
        // A `Chat` turn carries no tool schemas at all. With a desktop wired
        // in that is ten of the seventeen schemas the emitter would otherwise
        // re-send on every iteration of a turn that was never going to click
        // anything. The synthetic actions stay legal at every tier: they are
        // how a turn ends.
        //
        // M12 T2.1: unless the route selected some. A chat turn that asked
        // the time carries exactly the tools its own cue named (`[router]
        // chat_tools`) and nothing else — the selection is the whole
        // allowance there, so the filter below narrows to it the same way,
        // once per turn.
        let mut actions: Vec<_> = if o.tier.allows_tools() || o.selected_tools.is_some() {
            self.parts
                .tools
                .iter()
                .map(|t| t.spec().clone())
                .filter(|s| !o.book.denied.contains(&s.name))
                // M10 T2.1. A *turn*-level decision consulted here rather
                // than re-taken here: `selected_tools` is fixed for the loop
                // except when escalation widens it, so this filter yields the
                // same names on every iteration and the array's bytes do not
                // move.
                .filter(|s| o.selected_tools.is_none_or(|sel| sel.contains(&s.name)))
                .collect()
        } else {
            Vec::new()
        };

        actions.push(ask_clarification_spec(profile));
        if !o.book.denied.contains(REMEMBER_FACT) {
            actions.push(remember_fact_spec(profile));
        }
        if !o.book.denied.contains(RECALL) && o.recall_applies {
            actions.push(recall_spec(profile));
        }
        // Offered only while there is something to inspect. An action in the
        // schema that can only fail is a way for a small model to spend an
        // iteration discovering that.
        if !o.book.denied.contains(INSPECT_RESULT)
            && !crate::trace::clipped_results(o.events, o.turn, self.cfg.tool_result_max_chars)
                .is_empty()
        {
            actions.push(inspect_result_spec(profile));
        }
        // Forgetting is legal only while it can mean something: not after a
        // fact was written this turn (seen live: "my name is now Peter" ended
        // in forget_fact + a staged forget_all) and not after a forget
        // already ran. Both are this turn's own events, so replay reproduces
        // them; store state ("any facts at all?") must never decide legality.
        if !o.book.wrote_fact() && !o.book.forgot() && o.scope_holds_facts {
            if !o.book.denied.contains(FORGET_FACT) {
                actions.push(forget_fact_spec(profile));
            }
            if !o.book.denied.contains(FORGET_ALL) {
                actions.push(forget_all_spec(profile));
            }
        }
        if o.active_pending {
            actions.push(confirm_pending_spec(profile));
        }
        LegalActionSet { actions }
    }
}
