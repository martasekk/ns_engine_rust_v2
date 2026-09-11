//! What kind of turn this is, and therefore what it may cost (M7 Phase 3).
//!
//! MemFlow's shape: classify the message first, then compile a context and a
//! legal set sized for that class, rather than handing every turn the whole
//! store and the whole toolbox. On a frozen Qwen3-1.7B that nearly doubled
//! accuracy over full-context prompting, and the mechanism is not subtle —
//! most turns do not need most of what they are given, and a small model
//! reads irrelevant material as instruction.
//!
//! Here the classification is **symbolic**. A model in the router would put a
//! judge on the critical path, which M6 §8.7 argues against for the reply
//! check and which costs a request the free tier does not have; it would also
//! make routing non-deterministic, and a replayed session must route the same
//! way or the replay proves nothing. The cue lists are configuration, which
//! makes them exactly the kind of thing the evolution pass can propose and
//! gate later.
use serde::{Deserialize, Serialize};

/// How much of the harness a turn is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Conversation. The synthetic actions only — no tool schemas at all,
    /// which with a desktop wired in is ten of the seventeen schemas the
    /// emitter would otherwise carry on a turn that was never going to click
    /// anything. Pinned facts, half the budget.
    Chat,
    /// Something is to be done. Everything legal, the full context.
    #[default]
    Task,
    /// The answer is likely outside the window. Everything `Task` has, plus
    /// the engine runs the recall *itself* before the first proposal, which
    /// is where the saving is: on a fifty-request day, an emitter iteration
    /// spent asking for `recall` is a request that bought no progress.
    Deep,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Chat => "chat",
            Tier::Task => "task",
            Tier::Deep => "deep",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "chat" => Some(Tier::Chat),
            "task" => Some(Tier::Task),
            "deep" => Some(Tier::Deep),
            _ => None,
        }
    }

    /// The budget this tier gets out of the configured ceiling.
    ///
    /// Chat gets half. A conversational turn that needs six thousand tokens
    /// of context is not a conversational turn, and the point of a smaller
    /// ceiling is that going over it is a signal — it is how a misroute
    /// shows up in `ns-app budget` rather than only in a bad answer.
    pub fn budget(self, base: u32) -> u32 {
        match self {
            Tier::Chat => base / 2,
            Tier::Task | Tier::Deep => base,
        }
    }

    /// Whether registered tools are legal at this tier. The synthetic
    /// actions always are: `respond_directly`, `ask_clarification` and the
    /// memory actions are how a turn ends, and a tier that could not end a
    /// turn would be a trap rather than a budget.
    pub fn allows_tools(self) -> bool {
        self != Tier::Chat
    }

    /// Whether the query-relevant fact slice is worth its tokens here. The
    /// pinned core is shown at every tier — it is what stops the emitter
    /// asking for the user's name again (M6 F2).
    pub fn allows_relevant_facts(self) -> bool {
        self != Tier::Chat
    }
}

/// How many of the registered tools ride on a turn (M10 T2.1).
///
/// `Full` is every registered tool the tier allows — today's behaviour, and
/// the default. `Adaptive` lets the router map the message's own cues to
/// tool groups and send only those, which is the one lever that shrinks the
/// part of an emitter prompt no context knob touches: on the recorded
/// turn 21 the array was 731 prompt tokens (findings §8.1), and §4.1
/// measures selection accuracy falling as the array grows.
///
/// The selection is made **once per turn** and held across the turn's
/// iterations, so the `tools` array is byte-stable and a provider prefix
/// cache can survive the loop (decision 2, 2026-09-11). Escalation on an
/// `IllegalAction` is the one legitimate mid-turn change, and it only ever
/// widens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    /// Every registered tool the tier allows.
    #[default]
    Full,
    /// The cue-selected subset, widened by escalation.
    Adaptive,
}

impl Depth {
    pub fn as_str(self) -> &'static str {
        match self {
            Depth::Full => "full",
            Depth::Adaptive => "adaptive",
        }
    }

    /// Err carries the message a startup error should print — the shape
    /// `SchemaProfile::parse` already uses, so `[router] depth` and
    /// `[llm] schema_profile` fail the same way.
    pub fn parse(s: &str) -> Result<Depth, String> {
        match s {
            "full" => Ok(Depth::Full),
            "adaptive" => Ok(Depth::Adaptive),
            other => Err(format!(
                "depth {other:?} is unknown — known depths: full, adaptive"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_order_by_how_much_they_allow() {
        assert!(Tier::Chat < Tier::Task && Tier::Task < Tier::Deep);
        assert!(!Tier::Chat.allows_tools());
        assert!(Tier::Task.allows_tools() && Tier::Deep.allows_tools());
        assert!(!Tier::Chat.allows_relevant_facts());
        assert_eq!(Tier::Chat.budget(6000), 3000);
        assert_eq!(Tier::Task.budget(6000), 6000);
        assert_eq!(Tier::Deep.budget(6000), 6000);
    }

    #[test]
    fn tier_round_trips_by_name_and_through_serde() {
        for t in [Tier::Chat, Tier::Task, Tier::Deep] {
            assert_eq!(Tier::parse(t.as_str()), Some(t));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<Tier>(&json).unwrap(), t);
        }
        assert_eq!(Tier::parse("expensive"), None);
        // Ordering is load-bearing: escalation only ever moves up.
        assert_eq!(Tier::Chat.max(Tier::Deep), Tier::Deep);
    }

    #[test]
    fn depth_defaults_to_full_and_round_trips_by_name() {
        assert_eq!(Depth::default(), Depth::Full);
        for d in [Depth::Full, Depth::Adaptive] {
            assert_eq!(Depth::parse(d.as_str()), Ok(d));
            let json = serde_json::to_string(&d).unwrap();
            assert_eq!(serde_json::from_str::<Depth>(&json).unwrap(), d);
        }
        assert!(Depth::parse("shallow")
            .unwrap_err()
            .contains("full, adaptive"));
    }
}
