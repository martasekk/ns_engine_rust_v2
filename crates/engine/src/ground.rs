//! In-turn symbolic grounding interceptor (M6 spec §4.5): the provenance
//! idea applied to the reply model's output. A draft reply that states a
//! number, a quoted string or a proper name that appears nowhere in what
//! the model was shown is flagged, logged, and regenerated once with the
//! offending spans named. No model is involved in the check.
//!
//! The 2026 audit of tool-using agents (findings §6) cut fabricated tool
//! results by up to 24 points with a runtime interceptor of this shape.
use nscore::ReplyContext;

/// Every block `CloudReplier` renders *except* the user's own message, in the
/// replier's own rendering — a superseded value shown as "(was …)" is
/// legitimate material (seen live: "Your name was Martin." flagged against a
/// bare `key: value`). One function so the two interceptors can never drift
/// from each other or from the prompt.
/// Each part carries the id a `ReplyCited` event would name it by (M9 T4.2):
/// `persona`, `trace`, `fact:<key>`, `summary`, `window:<turn>`,
/// `guidance:<hash>`, `obligations`. The ids are new; the *texts*, in this
/// order, are exactly what they were, which is what keeps `echo_material` and
/// `Material::from_context` byte-identical to the pre-M9 material.
pub fn reference_parts(ctx: &ReplyContext) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = vec![
        ("persona".to_string(), ctx.persona.clone()),
        ("trace".to_string(), ctx.turn_trace.clone()),
    ];
    for f in &ctx.facts {
        parts.push((format!("fact:{}", f.key), nscore::render_fact(f)));
    }
    if let Some(s) = &ctx.summary {
        parts.push(("summary".to_string(), nscore::render_summary(s)));
    }
    // One part per record rather than one rendered block: `render_window`
    // with `k = len` is exactly these renders joined by a newline, so the
    // flattened text is unchanged, and a citation can now name the turn it
    // came from. An empty window keeps its one empty part for the same
    // reason — it is what `render_window` returned.
    if ctx.window.is_empty() {
        parts.push(("window".to_string(), String::new()));
    } else {
        for r in &ctx.window {
            parts.push((
                format!("window:{}", r.turn),
                nscore::render_record(r, &ctx.caps),
            ));
        }
    }
    // Reply guidance is the `reply`-scoped notes, so the hash `learned.toml`
    // and the manifest know a note by is recoverable from its text alone —
    // `ReplyContext` carries texts, and adding a hash to it would put a
    // `learned.toml` detail into every reply context in the engine.
    for text in &ctx.guidance {
        parts.push((
            format!("guidance:{}", nscore::Note::hash_of("reply", text)),
            text.clone(),
        ));
    }
    // M9 T2.1: the obligations block is rendered, so it is material. A reply
    // that names something only the obligation line named would otherwise be
    // flagged for stating what it was shown.
    for o in &ctx.obligations {
        parts.push(("obligations".to_string(), o.clone()));
    }
    parts
}

/// Which reference parts this reply drew on (M9 T4.2).
///
/// Two rules, because the parts are two kinds of thing. A **fact** is a
/// key and a value, and the value is short, so the test is direct: the
/// rendered value appears in the reply. A **summary** or a **window record**
/// is prose, and no single span of it is the thing being used, so the test
/// runs the other way — a claim the grounding check already extracted from
/// the reply appears in that part. Guidance is prose too, but prose the model
/// was told to *follow*, not to repeat, so a note counts as cited only when a
/// distinctive run of it — three or more consecutive words — is echoed
/// verbatim; that is deliberately strict, and a note that shaped a reply
/// without being quoted scores nothing.
///
/// Everything is lowercased, and a fact value under three characters is
/// skipped: `"17"` matches too much English to mean anything.
///
/// The result is evidence, not proof. A reply that says "Brno" because the
/// user just said "Brno" credits the fact too. The number it feeds is a
/// fitness signal read beside grades, and it is offline, so a wrong credit
/// costs a rank position rather than a wrong answer.
pub fn cited(ctx: &ReplyContext, reply: &str) -> Vec<String> {
    let lower = reply.to_lowercase();
    let claims = extract_claims(reply);
    let mut out: Vec<String> = Vec::new();
    for (id, text) in reference_parts(ctx) {
        let hit = if id.starts_with("fact:") {
            let value = ctx
                .facts
                .iter()
                .find(|f| id == format!("fact:{}", f.key))
                .map(|f| value_string(&f.value))
                .unwrap_or_default()
                .to_lowercase();
            value.chars().count() >= 3 && lower.contains(&value)
        } else if id == "summary" || id.starts_with("window:") {
            let part = text.to_lowercase();
            claims
                .iter()
                .any(|c| !c.is_empty() && part.contains(&c.to_lowercase()))
        } else if id.starts_with("guidance:") {
            let part = text.to_lowercase();
            distinctive_spans(&part)
                .into_iter()
                .any(|span| lower.contains(&span))
        } else {
            false
        };
        if hit && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// A fact value as the reply would have to say it: a JSON string without its
/// quotes, anything else as it serializes. The same rule `turn.rs` renders
/// values with, so what is searched for is what was shown.
fn value_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Every run of three consecutive words in `text`.
fn distinctive_spans(text: &str) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    (0..words.len().saturating_sub(2))
        .map(|i| words[i..i + 3].join(" "))
        .collect()
}

/// The first `answer:` obligation the draft shows no sign of having
/// addressed, if any (M9 T2.1).
///
/// Only `answer:` lines: whether a `do:` obligation was met is a question
/// about the turn's actions, which the trace already answers structurally,
/// and lexical overlap would be the wrong instrument for it. The predicate
/// is [`nscore::addresses`] — the same one the offline `IgnoredQuestion`
/// signature uses, and measured weak there, which is why the interceptor it
/// gates is off by default and regenerates at most once.
pub fn unaddressed(obligations: &[String], draft: &str) -> Option<String> {
    obligations
        .iter()
        .filter_map(|o| o.strip_prefix("answer: "))
        .find(|clause| !nscore::addresses(clause, draft))
        .map(str::to_string)
}

/// What a reply may draw on but must not reproduce, for `echo::echoed`. The
/// user's own message is deliberately out: echoing the user back is a
/// different failure with a different fix (plan §4), and counting it here
/// would flag every reply that quotes the question it answers.
pub fn echo_material(ctx: &ReplyContext) -> String {
    reference_parts(ctx)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything the reply model was shown, lowercased, for substring checks.
pub struct Material {
    text: String,
}

impl Material {
    pub fn from_parts(parts: &[&str]) -> Self {
        let text = parts
            .iter()
            .map(|p| p.to_lowercase())
            .collect::<Vec<_>>()
            .join("\n");
        Self { text }
    }

    /// Exactly the blocks `CloudReplier` renders, plus the persona.
    pub fn from_context(ctx: &ReplyContext) -> Self {
        let mut parts: Vec<String> = reference_parts(ctx)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        parts.push(ctx.user_text.clone());
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        Self::from_parts(&refs)
    }

    pub fn contains(&self, claim: &str) -> bool {
        let needle = claim.to_lowercase();
        !needle.is_empty() && self.text.contains(&needle)
    }
}

/// Capitalized words that start clauses or are conversational filler, not
/// claims about the world. Deliberately small: a false positive costs one
/// extra reply call, a false negative lets an invented name through.
const STOPLIST: &[&str] = &[
    "i",
    "i'm",
    "i'll",
    "i've",
    "i'd",
    "ok",
    "okay",
    "hello",
    "hi",
    "hey",
    "sure",
    "yes",
    "no",
    "thanks",
    "thank",
    "sorry",
    "please",
    "great",
    "good",
    "fine",
    "right",
    "well",
    "also",
    "and",
    "but",
    "or",
    "if",
    "so",
    "then",
    "now",
    "here",
    "there",
    "what",
    "which",
    "who",
    "when",
    "where",
    "why",
    "how",
    "let",
    "let's",
    "you",
    "your",
    "yours",
    "the",
    "a",
    "an",
    "it",
    "it's",
    "this",
    "that",
    "these",
    "those",
    "we",
    "our",
    "me",
    "my",
    "mine",
    "he",
    "she",
    "they",
    "them",
    "do",
    "does",
    "did",
    "can",
    "could",
    "would",
    "should",
    "will",
    "is",
    "are",
    "was",
    "were",
    "not",
    "nothing",
    "none",
    "just",
    "only",
    "today",
    "tomorrow",
    "yesterday",
    "am",
    "pm",
    "utc",
];

fn strip_edges(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

fn digits(s: &str) -> usize {
    s.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Claims worth checking, in order of appearance, deduplicated:
/// numbers with at least two digits (times, dates, counts, ids), quoted
/// strings, and capitalized words that are not sentence-initial, not
/// all-caps and not in the stoplist.
pub fn extract_claims(reply: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: String| {
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    };
    // quoted strings (ASCII and typographic quotes)
    let quote_chars = [
        '"', '\u{201C}', '\u{201D}', '\u{201E}', '\u{2018}', '\u{2019}',
    ];
    let mut inside: Option<String> = None;
    for ch in reply.chars() {
        if quote_chars.contains(&ch) {
            match inside.take() {
                Some(q) => {
                    let q = q.trim().to_string();
                    if q.chars().count() >= 2 && q.chars().any(|c| c.is_alphanumeric()) {
                        push(q);
                    }
                }
                None => inside = Some(String::new()),
            }
        } else if let Some(q) = inside.as_mut() {
            q.push(ch);
        }
    }
    // tokens
    let mut sentence_start = true;
    for raw in reply.split_whitespace() {
        let token = strip_edges(raw);
        let ends_sentence = raw.ends_with(['.', '!', '?', ':']);
        if token.is_empty() {
            sentence_start = sentence_start || ends_sentence;
            continue;
        }
        if digits(token) >= 2 {
            push(token.to_string());
        } else {
            let first_upper = token
                .chars()
                .next()
                .map(char::is_uppercase)
                .unwrap_or(false);
            let all_caps = token.chars().all(|c| !c.is_lowercase());
            let base = token.strip_suffix("'s").unwrap_or(token);
            if first_upper
                && !sentence_start
                && !all_caps
                && base.chars().count() >= 3
                && !STOPLIST.contains(&base.to_lowercase().as_str())
            {
                push(base.to_string());
            }
        }
        sentence_start = ends_sentence;
    }
    out
}

/// Claims in `reply` that the material does not support.
pub fn ungrounded(reply: &str, material: &Material) -> Vec<String> {
    extract_claims(reply)
        .into_iter()
        .filter(|c| !material.contains(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_numbers_quotes_and_mid_sentence_names_only() {
        let claims = extract_claims(
            "Hello Martin! You have 42 orders in Oslo, and \"blue widgets\" at 10:41 UTC, and Peter's order is due Wednesday.",
        );
        assert_eq!(
            claims,
            vec![
                "blue widgets",
                "Martin",
                "42",
                "Oslo",
                "10:41",
                "Peter",
                "Wednesday"
            ]
        );
        // sentence-initial words, all-caps labels, stoplist and short tokens are not claims
        assert!(extract_claims("Sure. I can help. OK? WINDOW= The time. Thanks!").is_empty());
        assert!(
            extract_claims("It is 5 now.").is_empty(),
            "single digits are not claims"
        );
    }

    #[test]
    fn material_check_is_case_insensitive_and_exact_substring() {
        let m = Material::from_parts(&[
            "user.name: \"Martin\"",
            "[t74] user: tell me the time\n      did:  get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday)",
        ]);
        assert_eq!(
            ungrounded("Hello Martin! It is 10:41 UTC on Wednesday.", &m),
            Vec::<String>::new()
        );
        assert_eq!(
            ungrounded("Martin, the session has reached 185 messages in Oslo.", &m),
            vec!["185", "Oslo"]
        );
        assert_eq!(
            ungrounded("You said \"tell me the time\".", &m),
            Vec::<String>::new()
        );
        assert_eq!(
            ungrounded("You said \"tell me a joke\".", &m),
            vec!["tell me a joke"]
        );
    }

    #[test]
    fn context_material_covers_every_rendered_block_and_the_persona() {
        let ctx = ReplyContext {
            usage: None,
            persona: "You are Tomáš, a sales assistant.".into(),
            facts: vec![nscore::Fact {
                key: "user.city".into(),
                value: serde_json::json!("Brno"),
                confidence: 1.0,
                uses: 0,
                last_validated: nscore::Timestamp(1),
                prov: nscore::Provenance::Constant,
                ..Default::default()
            }
            .into()],
            summary: Some(nscore::SessionSummary {
                through_turn: 3,
                topic: "ordering Widgetron units".into(),
                established: vec![],
                open: vec![],
                trust: nscore::Trust::User,
                rebuilt_from: 1,
            }),
            window: vec![nscore::TurnRecord {
                turn: 4,
                user: "ship to Karlova 12".into(),
                did: vec!["check_stock -> ok: 7 left".into()],
                reply: "Seven left.".into(),
                trust: nscore::Trust::External,
            }],
            caps: Default::default(),
            user_text: "and my colleague Jana?".into(),
            obligations: vec![],
            turn_trace: "Proposed(respond_directly)".into(),
            guidance: vec!["Mention Praha when relevant.".into()],
            do_not_state: vec![],
            do_not_repeat: vec![],
        };
        let m = Material::from_context(&ctx);
        let reply = "Hi Jana, Tomáš here. Brno gets 7 Widgetron units to Karlova 12; Praha too. Not 99 to Ostrava.";
        assert_eq!(ungrounded(reply, &m), vec!["99", "Ostrava"]);
    }

    #[test]
    fn superseded_fact_values_are_material() {
        let mut view: nscore::FactView = nscore::Fact {
            key: "user.name".into(),
            value: serde_json::json!("Peter"),
            ..Default::default()
        }
        .into();
        view.previous = Some((serde_json::json!("Martin"), nscore::Timestamp(1)));
        let ctx = ReplyContext {
            usage: None,
            persona: String::new(),
            facts: vec![view],
            summary: None,
            window: vec![],
            caps: Default::default(),
            user_text: "what was my name before?".into(),
            obligations: vec![],
            turn_trace: String::new(),
            guidance: vec![],
            do_not_state: vec![],
            do_not_repeat: vec![],
        };
        let m = Material::from_context(&ctx);
        assert_eq!(
            ungrounded("Your name was Martin.", &m),
            Vec::<String>::new()
        );
    }
}
