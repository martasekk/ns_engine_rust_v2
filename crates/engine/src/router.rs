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
    /// M10 T2.1: which *registered* tools ride this turn, or `None` for all
    /// of them. Always `None` under [`Depth::Full`].
    ///
    /// Decided here, once, and held by the turn loop across every iteration:
    /// a set recomputed per iteration would change the `tools` array under a
    /// provider prefix cache for no gain (decision 2, 2026-09-11). The
    /// synthetic actions are not in it — their applicability is the turn
    /// loop's question, not the router's.
    pub tools: Option<Vec<String>>,
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
    /// M10 T2.1: `Full` — the default — puts every registered tool the tier
    /// allows in front of the emitter, exactly as before this existed.
    pub depth: nscore::Depth,
}

/// Cue → the tool group it asks for (M10 T2.1). One flat table rather than a
/// trait per group: the whole point is that it is *data*, so the evolution
/// pass can propose a row and the gate can price it, the same reason the
/// tier cue lists are config.
///
/// The groups are deliberately coarse. "click" without a `pointer_ui_find`
/// beside it is a click with no coordinates, and an adaptive depth that
/// dropped the tool that produces them would be measuring nothing but its
/// own escalation rate.
const UI_GROUP: &[&str] = &["pointer_ui_find", "pointer_ui_read", "pointer_click"];
const TYPE_GROUP: &[&str] = &[
    "pointer_type",
    "pointer_clipboard_read",
    "pointer_clipboard_write",
];
const SCROLL_GROUP: &[&str] = &["pointer_scroll"];
const TIME_GROUP: &[&str] = &["get_time"];

const TOOL_CUES: &[(&str, &[&str])] = &[
    ("find", UI_GROUP),
    ("click", UI_GROUP),
    ("open", UI_GROUP),
    ("button", UI_GROUP),
    ("najdi", UI_GROUP),
    ("klikni", UI_GROUP),
    ("otevři", UI_GROUP),
    ("type", TYPE_GROUP),
    ("paste", TYPE_GROUP),
    ("copy", TYPE_GROUP),
    ("clipboard", TYPE_GROUP),
    ("napiš", TYPE_GROUP),
    ("vlož", TYPE_GROUP),
    ("schránk", TYPE_GROUP),
    ("scroll", SCROLL_GROUP),
    ("down", SCROLL_GROUP),
    ("up", SCROLL_GROUP),
    ("sjeď", SCROLL_GROUP),
    ("dolů", SCROLL_GROUP),
    ("nahoru", SCROLL_GROUP),
    ("time", TIME_GROUP),
    ("clock", TIME_GROUP),
    ("čas", TIME_GROUP),
    ("hodin", TIME_GROUP),
];

/// Whether one tool cue fires on a message.
///
/// ASCII cues match a whole word; Czech cues match as a substring. That is
/// not an inconsistency but the same rule the tier cues state: `otevři` and
/// `hodin` inflect and a stem is worth more than a list that misses five
/// cases out of seven, while an English `up` matched as a substring would
/// fire on "update" and quietly pull the scroll group into every turn that
/// mentions one.
fn tool_cue_hit(text: &str, words: &[&str], cue: &str) -> bool {
    if cue.is_ascii() {
        words.contains(&cue)
    } else {
        text.contains(cue)
    }
}

/// The registered tools this message asks for, in registration order, or
/// `None` when no cue fires and the full set is the honest answer.
///
/// Pure over the text and the registered names — no store, no clock — so a
/// replayed session selects the same array it recorded (M6 §15).
pub fn select_tools(text_lower: &str, tool_names: &[String]) -> Option<Vec<String>> {
    let words: Vec<&str> = text_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut wanted: Vec<&str> = Vec::new();
    for (cue, group) in TOOL_CUES {
        if tool_cue_hit(text_lower, &words, cue) {
            for name in *group {
                if !wanted.contains(name) {
                    wanted.push(name);
                }
            }
        }
    }
    // A tool named outright is a cue whatever the table says — the same rule
    // the tier already applies one level up.
    for n in tool_names {
        let lower = n.to_lowercase();
        if text_lower.contains(&lower) && !wanted.contains(&n.as_str()) {
            wanted.push(n.as_str());
        }
    }
    if wanted.is_empty() {
        return None;
    }
    // Registration order, so the array is a *subsequence* of the full one
    // and two turns that select the same tools send the same bytes.
    let selected: Vec<String> = tool_names
        .iter()
        .filter(|n| wanted.contains(&n.as_str()))
        .cloned()
        .collect();
    // A cue whose group is not registered here selected nothing; that is not
    // a reason to send an empty toolbox. Fail open, as the applicability
    // pruning does.
    (!selected.is_empty()).then_some(selected)
}

impl Default for KeywordRouter {
    fn default() -> Self {
        Self {
            depth: nscore::Depth::Full,
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
        let mut route = self.tier_of(&text, input);
        // Selected once, here, for the whole turn. Only where tools are
        // legal at all: a `Chat` route carries none either way, and
        // recording a selection on it would make the manifest claim a
        // narrowing that never happened.
        if self.depth == nscore::Depth::Adaptive && route.tier.allows_tools() {
            route.tools = select_tools(&text, input.tool_names);
        }
        route
    }
}

impl KeywordRouter {
    fn tier_of(&self, text: &str, input: &RouteInput<'_>) -> Route {
        let mut cues = Vec::new();

        // A pending confirmation outranks every word. "yes" carries no cue,
        // and routing it to Chat would take `confirm_pending` out of the
        // legal set and strand the staged action.
        if input.pending_confirmation {
            cues.push("pending confirmation".to_string());
            return Route {
                tier: Tier::Task,
                cues,
                tools: None,
            };
        }
        if let Some(cue) = hit(text, &self.recall_cues) {
            cues.push(cue.to_string());
            return Route {
                tier: Tier::Deep,
                cues,
                tools: None,
            };
        }
        if let Some(cue) = hit(text, &self.task_cues) {
            cues.push(cue.to_string());
            return Route {
                tier: Tier::Task,
                cues,
                tools: None,
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
                tools: None,
            };
        }
        if input.previous_turn_used_tools {
            cues.push("mid-task".to_string());
            return Route {
                tier: Tier::Task,
                cues,
                tools: None,
            };
        }
        Route {
            tier: Tier::Chat,
            cues,
            tools: None,
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

    /// The desktop, as `ns-run` registers it.
    fn desktop() -> Vec<String> {
        [
            "pointer_ui_read",
            "pointer_ui_find",
            "pointer_click",
            "pointer_type",
            "pointer_scroll",
            "pointer_clipboard_read",
            "pointer_clipboard_write",
            "get_time",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn adaptive() -> KeywordRouter {
        KeywordRouter {
            depth: nscore::Depth::Adaptive,
            ..KeywordRouter::default()
        }
    }

    /// M10 T2.1. Three of eight on an easy cue, and the three that belong
    /// together: a click with no `pointer_ui_find` beside it is a click with
    /// no coordinates.
    #[test]
    fn an_open_and_click_cue_selects_the_ui_and_click_tools_only() {
        let t = desktop();
        let r = adaptive().route(&input("open the panel and click Save", &t));
        assert_eq!(r.tier, Tier::Task);
        assert_eq!(
            r.tools,
            Some(vec![
                "pointer_ui_read".to_string(),
                "pointer_ui_find".to_string(),
                "pointer_click".to_string(),
            ]),
            "the ui group, in registration order"
        );
        // Nothing from the other groups rode along.
        let sent = r.tools.unwrap();
        for absent in ["pointer_type", "pointer_scroll", "get_time"] {
            assert!(!sent.contains(&absent.to_string()), "{absent} was withheld");
        }

        // A hard cue reaches two groups and still stays under the full set.
        let hard = adaptive().route(&input("sjeď dolů v panelu a klikni na Zmrazit", &t));
        let hard = hard.tools.expect("a cue fired");
        assert!(hard.contains(&"pointer_scroll".to_string()));
        assert!(hard.contains(&"pointer_click".to_string()));
        assert!(hard.len() < t.len(), "still a narrowing: {hard:?}");
    }

    /// No cue is not "no tools". A message the table does not recognise gets
    /// the whole toolbox, which is what makes adaptive depth a narrowing of
    /// the recognised cases rather than a guess about every message.
    #[test]
    fn no_cue_selects_the_full_set() {
        let t = desktop();
        let router = adaptive();
        let mut i = input("now the other one", &t);
        i.previous_turn_used_tools = true;
        let r = router.route(&i);
        assert_eq!(r.tier, Tier::Task);
        assert_eq!(r.tools, None, "None is the full set");
        // `update` must not fire the scroll group's `up`: ASCII cues match a
        // whole word, which is the bug a substring match would have.
        assert_eq!(router.route(&input("run the update", &t)).tools, None);
        // And `full` changes nothing at all.
        assert_eq!(
            KeywordRouter::default()
                .route(&input("open the panel and click Save", &t))
                .tools,
            None
        );
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
