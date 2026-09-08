//! Deciding a turn's tier before the first model call (M7 Phase 3).
//!
//! Pure over the message and this turn's own facts, so a replayed session
//! routes identically — routing that could differ between a live run and its
//! replay would make every replay-verified candidate in the evolution pass
//! a claim about a different engine.
use nscore::Tier;

/// Everything the router is allowed to see. Deliberately small: a router
/// that read the store could route differently on a replay from a fresh
/// store, which is the rule M6 §15 records after `forget_all` was written
/// that way once.
pub struct RouteInput<'a> {
    pub user_text: &'a str,
    /// A staged action is waiting for yes or no. Whatever the words are,
    /// this turn is about doing something.
    pub pending_confirmation: bool,
    /// The previous completed turn called a tool. A desktop task is ten to
    /// fifteen actions spread over as many turns, and "now click it" carries
    /// no cue of its own.
    pub previous_turn_used_tools: bool,
    /// Registered tool names, so a message naming one routes to a tier that
    /// can actually run it.
    pub tool_names: &'a [String],
}

/// The tier and why — the cues are recorded in the manifest so a misroute
/// can be traced back to the word that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub tier: Tier,
    pub cues: Vec<String>,
}

pub trait Router: Send + Sync {
    fn route(&self, input: &RouteInput<'_>) -> Route;
}

/// Word lists, from config. Czech as well as English because the machine
/// this runs on is a Czech desktop and "co jsem" is how the user actually
/// asks what they said before — an English-only cue list is the same bug
/// `MODAL_WORDS` had in the pointer work, found only once a non-English
/// desktop was in front of it.
#[derive(Debug, Clone)]
pub struct KeywordRouter {
    pub recall_cues: Vec<String>,
    pub task_cues: Vec<String>,
}

impl Default for KeywordRouter {
    fn default() -> Self {
        Self {
            recall_cues: [
                "before",
                "earlier",
                "last time",
                "what did i",
                "previously",
                "předtím",
                "dřív",
                "minule",
                "co jsem",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            task_cues: [
                "click", "type", "open", "screen", "window", "press", "scroll", "klikni", "napiš",
                "otevři", "obrazovk", "stiskni",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

/// Lowercase for matching. Czech cues are matched as substrings on purpose:
/// `otevři` inflects, and a stem that catches `obrazovka`/`obrazovce` is
/// worth more here than a word list that misses five cases out of seven.
fn hit<'a>(text: &str, cues: &'a [String]) -> Option<&'a str> {
    cues.iter()
        .find(|c| text.contains(c.as_str()))
        .map(String::as_str)
}

impl Router for KeywordRouter {
    fn route(&self, input: &RouteInput<'_>) -> Route {
        let text = input.user_text.to_lowercase();
        let mut cues = Vec::new();

        // A pending confirmation outranks every word. "yes" carries no cue,
        // and routing it to Chat would take `confirm_pending` out of the
        // legal set and strand the staged action.
        if input.pending_confirmation {
            cues.push("pending confirmation".to_string());
            return Route {
                tier: Tier::Task,
                cues,
            };
        }
        if let Some(cue) = hit(&text, &self.recall_cues) {
            cues.push(cue.to_string());
            return Route {
                tier: Tier::Deep,
                cues,
            };
        }
        if let Some(cue) = hit(&text, &self.task_cues) {
            cues.push(cue.to_string());
            return Route {
                tier: Tier::Task,
                cues,
            };
        }
        // A tool by name is a cue whatever the word list says.
        if let Some(name) = input
            .tool_names
            .iter()
            .find(|n| text.contains(&n.to_lowercase()))
        {
            cues.push(name.clone());
            return Route {
                tier: Tier::Task,
                cues,
            };
        }
        if input.previous_turn_used_tools {
            cues.push("mid-task".to_string());
            return Route {
                tier: Tier::Task,
                cues,
            };
        }
        Route {
            tier: Tier::Chat,
            cues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<String> {
        vec!["pointer_click".to_string(), "get_time".to_string()]
    }

    fn input<'a>(text: &'a str, tool_names: &'a [String]) -> RouteInput<'a> {
        RouteInput {
            user_text: text,
            pending_confirmation: false,
            previous_turn_used_tools: false,
            tool_names,
        }
    }

    #[test]
    fn plain_conversation_routes_to_chat() {
        let t = tools();
        let r = KeywordRouter::default().route(&input("hello, how are you", &t));
        assert_eq!(r.tier, Tier::Chat);
        assert!(r.cues.is_empty());
    }

    #[test]
    fn a_recall_cue_routes_deep_in_both_languages() {
        let t = tools();
        let router = KeywordRouter::default();
        for text in [
            "what did i tell you earlier",
            "what was my name before",
            "co jsem ti říkal minule",
        ] {
            let r = router.route(&input(text, &t));
            assert_eq!(r.tier, Tier::Deep, "{text}");
            assert!(!r.cues.is_empty(), "{text} — the cue is recorded");
        }
    }

    #[test]
    fn a_task_cue_or_a_tool_name_routes_to_task() {
        let t = tools();
        let router = KeywordRouter::default();
        assert_eq!(
            router.route(&input("click the button", &t)).tier,
            Tier::Task
        );
        assert_eq!(router.route(&input("otevři firefox", &t)).tier, Tier::Task);
        // Named outright, with no cue word anywhere in the message —
        // `pointer_click` would not test this, since it contains "click".
        let r = router.route(&input("run get_time for me", &t));
        assert_eq!(r.tier, Tier::Task);
        assert_eq!(r.cues, vec!["get_time".to_string()]);
    }

    /// A desktop task is ten to fifteen actions over as many turns, and "now
    /// the other one" carries no cue at all. Without this rule the second
    /// half of every task would route to Chat and lose its tools.
    #[test]
    fn a_turn_in_the_middle_of_a_task_stays_at_task() {
        let t = tools();
        let mut i = input("now the other one", &t);
        assert_eq!(KeywordRouter::default().route(&i).tier, Tier::Chat);
        i.previous_turn_used_tools = true;
        let r = KeywordRouter::default().route(&i);
        assert_eq!(r.tier, Tier::Task);
        assert_eq!(r.cues, vec!["mid-task".to_string()]);
    }

    /// "yes" is the whole message on a confirmation turn. Routing it to Chat
    /// would drop `confirm_pending` from the legal set and strand the staged
    /// action — the user would answer the prompt and nothing would happen.
    #[test]
    fn a_pending_confirmation_outranks_every_word() {
        let t = tools();
        let mut i = input("yes", &t);
        i.pending_confirmation = true;
        let r = KeywordRouter::default().route(&i);
        assert_eq!(r.tier, Tier::Task);
        assert_eq!(r.cues, vec!["pending confirmation".to_string()]);
    }

    #[test]
    fn routing_is_pure_and_repeatable() {
        let t = tools();
        let router = KeywordRouter::default();
        let first = router.route(&input("what did i say before", &t));
        let second = router.route(&input("what did i say before", &t));
        assert_eq!(first, second, "a replay must route the same way");
    }
}
