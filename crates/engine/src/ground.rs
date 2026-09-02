//! In-turn symbolic grounding interceptor (M6 spec §4.5): the provenance
//! idea applied to the reply model's output. A draft reply that states a
//! number, a quoted string or a proper name that appears nowhere in what
//! the model was shown is flagged, logged, and regenerated once with the
//! offending spans named. No model is involved in the check.
//!
//! The 2026 audit of tool-using agents (findings §6) cut fabricated tool
//! results by up to 24 points with a runtime interceptor of this shape.
use nscore::ReplyContext;

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
        let mut parts: Vec<String> = vec![
            ctx.persona.clone(),
            ctx.user_text.clone(),
            ctx.turn_trace.clone(),
        ];
        for f in &ctx.facts {
            parts.push(format!("{}: {}", f.key, f.value));
        }
        if let Some(s) = &ctx.summary {
            parts.push(nscore::render_summary(s));
        }
        parts.push(nscore::render_window(
            &ctx.window,
            ctx.window.len(),
            &ctx.caps,
        ));
        parts.extend(ctx.guidance.iter().cloned());
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
            turn_trace: "Proposed(respond_directly)".into(),
            guidance: vec!["Mention Praha when relevant.".into()],
            do_not_state: vec![],
        };
        let m = Material::from_context(&ctx);
        let reply = "Hi Jana, Tomáš here. Brno gets 7 Widgetron units to Karlova 12; Praha too. Not 99 to Ostrava.";
        assert_eq!(ungrounded(reply, &m), vec!["99", "Ostrava"]);
    }
}
