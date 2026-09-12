//! What kind of turn this is, decided once and before any model call.

use super::Engine;
use crate::state::fold;
use nscore::EventKind;

impl Engine {
    /// Route this turn, or hand back `Task` when no router is installed —
    /// which is what the engine did before there was one, so an engine
    /// without a router is unchanged rather than differently behaved.
    pub(super) fn route_turn(
        &self,
        user_text: &str,
        events: &[nscore::Event],
        turn: u32,
    ) -> crate::router::Route {
        let Some(router) = &self.cfg.router else {
            return crate::router::Route {
                tier: nscore::Tier::Task,
                cues: Vec::new(),
                // No router, no narrowing: the full set, which is what every
                // scripted double and every replay has always been sent.
                tools: None,
            };
        };
        let state = fold(events);
        // The same expiry rule the loop applies: a pending confirmation is
        // live only on the turn after the one that staged it.
        let pending_confirmation = state
            .pending_confirmation
            .filter(|_| state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false))
            .is_some();
        // Precise, and cheaper than reading it back out of a rendered record:
        // did the turn immediately before this one actually call a tool.
        let previous_turn_used_tools = events
            .iter()
            .any(|e| e.turn + 1 == turn && matches!(e.kind, EventKind::ToolCalled { .. }));
        let tool_names: Vec<String> = self
            .parts
            .tools
            .iter()
            .map(|t| t.spec().name.clone())
            .collect();
        router.route(&crate::router::RouteInput {
            user_text,
            pending_confirmation,
            previous_turn_used_tools,
            tool_names: &tool_names,
        })
    }
}
