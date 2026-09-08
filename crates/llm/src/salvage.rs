//! Tolerant repair of a tool call's `arguments` string, for the branch that
//! runs *after* `serde_json::from_str` has already failed.
//!
//! Plan §T2.5 keeps this off the primary path: the emitter's illegality
//! guarantee comes from the provider constraining generation to the legal
//! set, and no parse recovered here is worth widening it. So the action name
//! is never salvaged, only `function.arguments`, and every repair either
//! deletes syntax or closes what the model left open. Nothing here can put a
//! value into an object the model did not write — which is what leaves
//! `validate_args` able to reject a call whose required argument is missing.
//!
//! What it buys: today a shim that answers `{'text': 'hi',}` costs one of
//! `max_emit_retries`, and on `openrouter/free` that is one of fifty requests
//! a day.

use serde_json::Value;

/// Best-effort repair of a tool-call argument string a provider returned
/// malformed. Returns the parsed object and a short name for the repair
/// that worked, or None when nothing plausible can be recovered.
pub fn salvage_arguments(raw: &str) -> Option<(Value, &'static str)> {
    // Cumulative, because the failures arrive together: the shims that fence
    // their JSON are the same ones that single-quote it. Each repair runs on
    // the last one's output, one that changes nothing is skipped, and the
    // name reported is the repair that got the text over the line.
    let mut text = raw.trim().to_string();
    for &(name, repair) in REPAIRS {
        let next = repair(&text);
        if next == text {
            continue;
        }
        text = next;
        if let Some(value) = parse_object(&text) {
            return Some((value, name));
        }
    }
    None
}

/// The name reported to stderr, and the rewrite it applies.
type Repair = (&'static str, fn(&str) -> String);

/// In the order a weak OpenAI shim produces them, shallowest first.
/// `truncated object` is last because it is the only repair that drops
/// content the model did write.
const REPAIRS: &[Repair] = &[
    ("fence", unfence),
    ("prose around the object", outermost_object),
    ("trailing comma", drop_trailing_commas),
    ("single quotes", double_quote_strings),
    ("unquoted keys", quote_bare_keys),
    ("python literals", python_literals),
    ("truncated object", close_truncated),
];

fn parse_object(s: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(s).ok()?;
    let object = value.as_object()?;
    // Empty is never a salvage. `{}` parses on the primary path, so text that
    // reaches this module had content that did not, and answering with `{}`
    // would turn "the arguments were unreadable" into "the model called this
    // action with no arguments" — a different claim, and one the engine would
    // act on.
    if object.is_empty() {
        return None;
    }
    Some(value)
}

/// The same shape as `summarizer::strip_fence`: a shim that fences one JSON
/// reply fences them all, so the two have to accept the same fences.
fn unfence(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

/// Tracks whether the scan sits inside a string literal. Both quote
/// characters open one: a shim that emits single-quoted JSON emits it
/// throughout, and a brace inside `'Okno {Nastavení}'` is no more structural
/// than one inside `"Okno {Nastavení}"`. Whichever quote opened the literal
/// is the only one that closes it, so an apostrophe in `"don't"` is content.
#[derive(Default)]
struct StringScan {
    delim: Option<char>,
    escaped: bool,
}

impl StringScan {
    /// True when `c` is a quote or string content, i.e. never structure.
    fn quoted(&mut self, c: char) -> bool {
        if let Some(delim) = self.delim {
            if self.escaped {
                self.escaped = false;
            } else if c == '\\' {
                self.escaped = true;
            } else if c == delim {
                self.delim = None;
            }
            return true;
        }
        if c == '"' || c == '\'' {
            self.delim = Some(c);
            return true;
        }
        false
    }
}

/// The outermost balanced `{…}` span, so "Sure, here you go:" in front of the
/// object and "Hope that helps!" behind it both go. With no matching brace —
/// the truncation case — everything from the first `{` is kept, which still
/// strips the prose in front of it.
fn outermost_object(s: &str) -> String {
    let Some(start) = s.find('{') else {
        return s.to_string();
    };
    let mut scan = StringScan::default();
    let mut depth = 0i32;
    for (i, c) in s[start..].char_indices() {
        if scan.quoted(c) {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return s[start..start + i + c.len_utf8()].to_string();
                }
            }
            _ => {}
        }
    }
    s[start..].to_string()
}

fn drop_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut scan = StringScan::default();
    for (i, &c) in chars.iter().enumerate() {
        if scan.quoted(c) {
            out.push(c);
            continue;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace()).copied();
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Single-quoted strings become double-quoted ones. A raw `"` inside such a
/// string has to be escaped on the way out, and `\'` has to lose its
/// backslash, which JSON does not allow.
fn double_quote_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_double {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_double = false;
            }
            continue;
        }
        if in_single {
            if escaped {
                escaped = false;
                if c == '\'' {
                    out.pop();
                }
                out.push(c);
            } else if c == '\\' {
                escaped = true;
                out.push(c);
            } else if c == '\'' {
                in_single = false;
                out.push('"');
            } else if c == '"' {
                out.push_str("\\\"");
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_double = true;
                out.push(c);
            }
            '\'' => {
                in_single = true;
                out.push('"');
            }
            _ => out.push(c),
        }
    }
    out
}

/// `{text: "hi"}` → `{"text": "hi"}`. Only a bare word that sits where a key
/// sits — after `{` or `,`, before `:` — is quoted, so a bare word in value
/// position is left for `python_literals` or left to fail.
fn quote_bare_keys(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut scan = StringScan::default();
    let mut prev = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if scan.quoted(c) {
            out.push(c);
            prev = Some(c);
            i += 1;
            continue;
        }
        if is_word_start(c) && matches!(prev, Some('{' | ',')) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let after = chars[i..].iter().find(|n| !n.is_whitespace()).copied();
            if after == Some(':') {
                out.push('"');
                out.push_str(&word);
                out.push('"');
            } else {
                out.push_str(&word);
            }
            prev = word.chars().last();
            continue;
        }
        out.push(c);
        if !c.is_whitespace() {
            prev = Some(c);
        }
        i += 1;
    }
    out
}

/// Python's literals for a model that has written more Python than JSON.
/// Whole words only, and outside strings only: `"None of that"` is text.
fn python_literals(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut scan = StringScan::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if scan.quoted(c) || !is_word_start(c) {
            out.push(c);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        out.push_str(match word.as_str() {
            "True" => "true",
            "False" => "false",
            "None" => "null",
            other => other,
        });
    }
    out
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A `max_tokens` cut leaves the object open: close the string it stopped
/// inside and every brace still on the stack. When the cut landed on a key or
/// between a key and its value, closing alone cannot help — `{"a": 1, "b` has
/// no value to close over, and inventing one is the thing this module must
/// never do — so the incomplete trailing member is dropped instead, member by
/// member, until what remains closes into an object. Parsing here rather than
/// in the caller is what tells the loop when to stop.
fn close_truncated(s: &str) -> String {
    let closed = close_open(s);
    if closed == s || parse_object(&closed).is_some() {
        return closed;
    }
    let mut head = s;
    while let Some(shorter) = drop_last_member(head) {
        let candidate = close_open(shorter);
        if parse_object(&candidate).is_some() {
            return candidate;
        }
        head = shorter;
    }
    closed
}

fn close_open(s: &str) -> String {
    let mut scan = StringScan::default();
    let mut stack = Vec::new();
    for c in s.chars() {
        if scan.quoted(c) {
            continue;
        }
        match c {
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    if scan.delim.is_none() && stack.is_empty() {
        return s.to_string();
    }
    // Trailing whitespace inside an unterminated string is part of the value;
    // outside one it is nothing.
    let mut out = match scan.delim {
        Some(_) => s.to_string(),
        None => s.trim_end().to_string(),
    };
    if let Some(delim) = scan.delim {
        // A cut after a backslash would otherwise escape the closing quote.
        if scan.escaped {
            out.pop();
        }
        out.push(delim);
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    out
}

/// Everything up to the last comma that is structure, i.e. the text without
/// its final member. Nesting needs no special case: the last such comma in
/// `{"a": {"b": 1, "c` is the nested one, and dropping from there leaves the
/// outer object still open for `close_open`.
fn drop_last_member(s: &str) -> Option<&str> {
    let mut scan = StringScan::default();
    let mut last = None;
    for (i, c) in s.char_indices() {
        if !scan.quoted(c) && c == ',' {
            last = Some(i);
        }
    }
    last.map(|i| &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salvaged(raw: &str) -> (Value, &'static str) {
        salvage_arguments(raw).unwrap_or_else(|| panic!("not salvaged: {raw}"))
    }

    /// Weak shims wrap tool arguments in the fence they were trained to put
    /// around chat JSON; the summarizer already strips the same one.
    #[test]
    fn a_markdown_fence_is_stripped() {
        let (v, repair) = salvaged("```json\n{\"text\": \"hi\"}\n```");
        assert_eq!(v, serde_json::json!({"text": "hi"}));
        assert_eq!(repair, "fence");
        assert_eq!(salvaged("```\n{\"text\": \"hi\"}\n```").1, "fence");
    }

    /// The model narrates the call around it: "Sure, here you go: {…}. Hope
    /// that helps!" is one tool call and two sentences of prose.
    #[test]
    fn prose_before_and_after_the_object_is_dropped() {
        let (v, repair) = salvaged("Sure, here you go: {\"text\": \"hi\"}. Hope that helps!");
        assert_eq!(v, serde_json::json!({"text": "hi"}));
        assert_eq!(repair, "prose around the object");
    }

    /// The span is counted over structure, not over bytes: a Czech window
    /// title with a brace in it used to end the object early and leave the
    /// rest of the arguments as prose.
    #[test]
    fn a_brace_inside_a_string_does_not_end_the_span() {
        let (v, _) = salvaged("Zde: {\"title\": \"Okno {Nastavení\", \"text\": \"hi\"} hotovo");
        assert_eq!(
            v,
            serde_json::json!({"title": "Okno {Nastavení", "text": "hi"})
        );
    }

    /// The single most common shim defect, and the cheapest to repair.
    #[test]
    fn a_trailing_comma_before_a_brace_or_bracket() {
        let (v, repair) = salvaged("{\"text\": \"hi\", \"tags\": [\"a\", \"b\",],}");
        assert_eq!(v, serde_json::json!({"text": "hi", "tags": ["a", "b"]}));
        assert_eq!(repair, "trailing comma");
    }

    /// Python-flavoured quoting. The apostrophe in a double-quoted value is
    /// not a delimiter and must survive verbatim.
    #[test]
    fn single_quoted_strings_become_double_quoted() {
        let (v, repair) = salvaged("{'text': 'hi', 'note': \"don't\"}");
        assert_eq!(v, serde_json::json!({"text": "hi", "note": "don't"}));
        assert_eq!(repair, "single quotes");
        let (v, _) = salvaged("{'text': 'say \"hi\"'}");
        assert_eq!(v, serde_json::json!({"text": "say \"hi\""}));
    }

    #[test]
    fn unquoted_object_keys_are_quoted() {
        let (v, repair) = salvaged("{text: \"hi\", count: 2}");
        assert_eq!(v, serde_json::json!({"text": "hi", "count": 2}));
        assert_eq!(repair, "unquoted keys");
    }

    /// A bare word in *value* position is not a key and must not be quoted:
    /// quoting it would turn an unusable call into a plausible one carrying a
    /// value the model never wrote.
    #[test]
    fn a_bare_word_in_value_position_is_not_quoted() {
        assert_eq!(quote_bare_keys("{\"text\": hi}"), "{\"text\": hi}");
        assert!(salvage_arguments("{\"text\": hi}").is_none());
    }

    /// Models that write more Python than JSON. `None` inside a string is a
    /// word, not a literal.
    #[test]
    fn python_literals_become_json_literals() {
        let (v, repair) = salvaged("{\"ok\": True, \"bad\": False, \"seen\": None}");
        assert_eq!(
            v,
            serde_json::json!({"ok": true, "bad": false, "seen": null})
        );
        assert_eq!(repair, "python literals");
        let (v, _) = salvaged("{\"text\": \"None of that\", \"ok\": True}");
        assert_eq!(v, serde_json::json!({"text": "None of that", "ok": true}));
    }

    /// `max_tokens` cuts mid-value: the open string and the open brace close,
    /// and the partial value is the model's own text, not an invention.
    #[test]
    fn a_value_cut_in_half_is_closed() {
        let (v, repair) = salvaged("{\"_rationale\": \"asked\", \"text\": \"the quick bro");
        assert_eq!(
            v,
            serde_json::json!({"_rationale": "asked", "text": "the quick bro"})
        );
        assert_eq!(repair, "truncated object");
        let (v, _) = salvaged("{\"a\": {\"b\": [1, 2");
        assert_eq!(v, serde_json::json!({"a": {"b": [1, 2]}}));
    }

    /// The cut lands on a key, or between a key and its value. Closing cannot
    /// invent the value, so the whole incomplete member goes — and the
    /// argument it was carrying stays missing, for `validate_args` to reject.
    #[test]
    fn a_member_cut_before_its_value_is_dropped_not_filled_in() {
        for raw in [
            "{\"_rationale\": \"asked\", \"te",
            "{\"_rationale\": \"asked\", \"text\":",
            "{\"_rationale\": \"asked\", \"text\": 1.",
        ] {
            let (v, repair) = salvaged(raw);
            assert_eq!(v, serde_json::json!({"_rationale": "asked"}), "{raw}");
            assert_eq!(repair, "truncated object");
            assert!(v.get("text").is_none(), "nothing was invented: {raw}");
        }
    }

    /// Every defect at once, which is how they actually arrive.
    #[test]
    fn the_repairs_compose() {
        let (v, _) = salvaged("Here:\n```json\n{ok: True, 'text': 'hi',}\n```");
        assert_eq!(v, serde_json::json!({"ok": true, "text": "hi"}));
    }

    /// A refusal in prose has no object in it. Returning `{}` would report it
    /// as a call with no arguments, which the engine would then execute.
    #[test]
    fn prose_with_no_object_is_not_salvaged() {
        assert!(salvage_arguments("I'm sorry, I can't call that tool.").is_none());
        assert!(salvage_arguments("").is_none());
    }

    /// Truncation that leaves nothing behind: `{"te` closes to `{}`, and an
    /// empty object is a claim about the call, not a repair of it.
    #[test]
    fn a_truncation_that_recovers_nothing_is_not_an_empty_object() {
        assert!(salvage_arguments("{\"te").is_none());
        assert!(salvage_arguments("{").is_none());
        assert!(salvage_arguments("{not json").is_none());
    }

    /// Arguments are an object. An array parses and is still not a call.
    #[test]
    fn a_non_object_is_not_salvaged() {
        assert!(salvage_arguments("[\"hi\", \"there\",]").is_none());
        assert!(salvage_arguments("'hi'").is_none());
    }
}
