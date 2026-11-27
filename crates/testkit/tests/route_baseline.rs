//! The keyword router's score on the labelled route set (plan
//! `docs/plans/2026-11-26-classifier-router.md`, JR-1).
//!
//! `fixtures/routes.toml` holds 100 customer messages, each with the class a
//! person gave it. This test routes every message with
//! `KeywordRouter::default()` and prints the confusion matrix at tier level:
//! the number JR-2 (the hosted classifier) and JR-8 (end to end) are compared
//! with. It asserts the fixture's shape and nothing about accuracy, so a
//! change to the word lists moves the printed numbers and never fails here.
//!
//! The router sees what it sees on a first turn: the message alone, nothing
//! pending, no tool used the turn before, no registered tool names. Its words
//! are all it routes on, which is what the classifier replaces. The turn
//! before (`prev_user`, `prev_reply`) is in the fixture for the classifier;
//! `KeywordRouter` has no input for it.
//!
//! Read the numbers with `cargo test -p ns-testkit --test route_baseline --
//! --nocapture`.

use std::collections::BTreeMap;

use nscore::Tier;
use nsengine::router::{KeywordRouter, RouteInput, Router};

const FIXTURE: &str = include_str!("../fixtures/routes.toml");

const TIERS: [Tier; 3] = [Tier::Chat, Tier::Task, Tier::Deep];
const CLASSES: [&str; 4] = ["chat", "lookup", "recall", "act"];

struct Labelled {
    id: String,
    lang: String,
    class: String,
    text: String,
    follows_a_turn: bool,
}

fn tier_named(name: &str) -> Tier {
    TIERS
        .into_iter()
        .find(|t| t.as_str() == name)
        .unwrap_or_else(|| panic!("unknown tier {name:?} in routes.toml"))
}

fn str_field<'a>(table: &'a toml::Table, key: &str, what: &str) -> &'a str {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("{what}: `{key}` is missing or not a string"))
}

/// The class table and the messages, with every field the fixture's header
/// promises checked to be there.
fn load() -> (BTreeMap<String, Tier>, Vec<Labelled>) {
    let root: toml::Table = toml::from_str(FIXTURE).expect("routes.toml is valid TOML");
    assert_eq!(
        root.get("version").and_then(toml::Value::as_integer),
        Some(1),
        "routes.toml: version"
    );

    let mut classes = BTreeMap::new();
    for class in root
        .get("class")
        .and_then(toml::Value::as_array)
        .expect("routes.toml: [[class]]")
    {
        let class = class.as_table().expect("[[class]] is a table");
        let key = str_field(class, "key", "[[class]]");
        str_field(class, "description", key);
        let tier = tier_named(str_field(class, "tier", key));
        assert!(
            classes.insert(key.to_string(), tier).is_none(),
            "class {key} twice"
        );
    }
    assert_eq!(
        classes.keys().map(String::as_str).collect::<Vec<_>>(),
        {
            let mut sorted = CLASSES.to_vec();
            sorted.sort_unstable();
            sorted
        },
        "the plan's four default classes"
    );

    let mut routes = Vec::new();
    for route in root
        .get("route")
        .and_then(toml::Value::as_array)
        .expect("routes.toml: [[route]]")
    {
        let route = route.as_table().expect("[[route]] is a table");
        let id = str_field(route, "id", "[[route]]").to_string();
        let class = str_field(route, "class", &id).to_string();
        assert!(classes.contains_key(&class), "{id}: unknown class {class}");
        let lang = str_field(route, "lang", &id).to_string();
        assert!(lang == "cs" || lang == "en", "{id}: lang {lang}");
        let text = str_field(route, "text", &id).to_string();
        assert!(!text.trim().is_empty(), "{id}: empty text");
        let prev_user = route.get("prev_user").is_some();
        let prev_reply = route.get("prev_reply").is_some();
        assert_eq!(
            prev_user, prev_reply,
            "{id}: prev_user and prev_reply come together"
        );
        if prev_user {
            str_field(route, "prev_user", &id);
            str_field(route, "prev_reply", &id);
        }
        routes.push(Labelled {
            id,
            lang,
            class,
            text,
            follows_a_turn: prev_user,
        });
    }
    (classes, routes)
}

fn count(routes: &[Labelled], keep: impl Fn(&Labelled) -> bool) -> usize {
    routes.iter().filter(|r| keep(r)).count()
}

#[test]
fn the_keyword_router_on_the_labelled_route_set() {
    let (classes, routes) = load();

    // The composition the plan asks for (JR-1).
    assert_eq!(routes.len(), 100, "100 messages");
    let mut ids: Vec<&str> = routes.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 100, "ids are unique");
    assert_eq!(count(&routes, |r| r.lang == "cs"), 80, "80 Czech");
    assert_eq!(count(&routes, |r| r.lang == "en"), 20, "20 English");
    assert_eq!(count(&routes, |r| r.follows_a_turn), 20, "20 follow a turn");
    for class in CLASSES {
        assert_eq!(count(&routes, |r| r.class == class), 25, "25 {class}");
    }

    let router = KeywordRouter::default();
    let tool_names: Vec<String> = Vec::new();
    // [expected tier][routed tier], and [class][routed tier].
    let mut by_tier = [[0usize; 3]; 3];
    let mut by_class = [[0usize; 3]; 4];
    let mut misses = Vec::new();
    let mut missed_by_lang: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &routes {
        let routed = router
            .route(&RouteInput {
                user_text: &r.text,
                pending_confirmation: false,
                previous_turn_used_tools: false,
                tool_names: &tool_names,
            })
            .tier;
        let expected = classes[&r.class];
        let col = TIERS.iter().position(|t| *t == routed).expect("a tier");
        let row = TIERS.iter().position(|t| *t == expected).expect("a tier");
        let class_row = CLASSES.iter().position(|c| *c == r.class).expect("a class");
        by_tier[row][col] += 1;
        by_class[class_row][col] += 1;
        if routed != expected {
            *missed_by_lang.entry(r.lang.as_str()).or_insert(0usize) += 1;
            misses.push(format!(
                "  {:<10} {:<6} -> {:<4} {}",
                r.id,
                r.class,
                routed.as_str(),
                r.text
            ));
        }
    }

    let right: usize = (0..3).map(|i| by_tier[i][i]).sum();
    let mut out = String::new();
    out.push_str("JR-1 keyword baseline, KeywordRouter::default(), 100 messages\n");
    out.push_str("expected tier \\ routed   chat  task  deep\n");
    for (i, tier) in TIERS.iter().enumerate() {
        out.push_str(&format!(
            "  {:<22} {:>4}  {:>4}  {:>4}\n",
            tier.as_str(),
            by_tier[i][0],
            by_tier[i][1],
            by_tier[i][2]
        ));
    }
    out.push_str("class (tier) \\ routed    chat  task  deep\n");
    for (i, class) in CLASSES.iter().enumerate() {
        out.push_str(&format!(
            "  {:<22} {:>4}  {:>4}  {:>4}\n",
            format!("{class} ({})", classes[*class].as_str()),
            by_class[i][0],
            by_class[i][1],
            by_class[i][2]
        ));
    }
    let costly = by_class[1][0] + by_class[3][0];
    out.push_str(&format!("tier accuracy: {right}/100\n"));
    out.push_str(&format!(
        "lookup or act routed to chat (the costly error): {costly}\n"
    ));
    for (lang, n) in [("cs", 80), ("en", 20)] {
        let ok = n - missed_by_lang.get(lang).copied().unwrap_or(0);
        out.push_str(&format!("  {lang}: {ok}/{n}\n"));
    }
    out.push_str(&format!("misrouted ({}):\n", misses.len()));
    for m in &misses {
        out.push_str(m);
        out.push('\n');
    }
    println!("{out}");

    assert_eq!(
        by_tier.iter().flatten().sum::<usize>(),
        100,
        "every message routed once"
    );
}
