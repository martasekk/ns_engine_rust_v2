//! Working and episodic memory types that cross the trait boundary (M6 spec
//! §4–§5): one verbatim record per completed turn, the bounded window both
//! models read, and the rolling summary of the turns that fell out of it.
use crate::value::Trust;
use serde::{Deserialize, Serialize};

/// One completed turn as both models see it: what the user said, what the
/// engine did (with outcomes and refusals), what was replied. A pure
/// projection of the log — never persisted, always rebuilt by the fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn: u32,
    /// Verbatim user text.
    pub user: String,
    /// One line per action, in order, e.g.
    /// `get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday)`,
    /// `remember_fact user.age=17 -> ok`, `wipe -> denied (taint_policy: …)`,
    /// `asked: Could you clarify …`, `staged: 'wipe' awaits confirmation`.
    pub did: Vec<String>,
    /// Verbatim reply text.
    pub reply: String,
    /// Minimum trust over the turn's tool outputs; `User` when no tool ran.
    pub trust: Trust,
}

/// Rendering caps for the window (config `[memory]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    /// Whole rendered record, including the user text.
    pub record_max_chars: usize,
    /// Each `did` line and the reply.
    pub line_max_chars: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            record_max_chars: 300,
            line_max_chars: 120,
        }
    }
}

/// Truncate to `max` characters (not bytes), marking the cut with `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Render one record. Lines are capped individually; the whole record is
/// capped last so a long user text cannot crowd out the outcome lines.
pub fn render_record(r: &TurnRecord, caps: &Caps) -> String {
    let mut s = format!("[t{}] user: {}", r.turn, r.user.replace('\n', " "));
    if !r.did.is_empty() {
        let did: Vec<String> = r
            .did
            .iter()
            .map(|d| truncate_chars(&d.replace('\n', " "), caps.line_max_chars))
            .collect();
        s.push_str(&format!("\n      did:  {}", did.join(" | ")));
    }
    if !r.reply.is_empty() {
        s.push_str(&format!(
            "\n      bot:  {}",
            truncate_chars(&r.reply.replace('\n', " "), caps.line_max_chars)
        ));
    }
    truncate_chars(&s, caps.record_max_chars)
}

/// The last `k` records, oldest first, one blank line between records.
/// Empty string when there is nothing to show.
pub fn render_window(records: &[TurnRecord], k: usize, caps: &Caps) -> String {
    let start = records.len().saturating_sub(k);
    records[start..]
        .iter()
        .map(|r| render_record(r, caps))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rolling summary of the turns that have fallen out of the window (M6
/// §5.1). Written by a cheap model off the hot path, stored as a
/// `Summarized` event, rebuilt from verbatim records to bound drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Last turn the summary covers.
    pub through_turn: u32,
    /// What the user is trying to do, one sentence.
    pub topic: String,
    /// Decisions and answers given that are not already facts.
    #[serde(default)]
    pub established: Vec<String>,
    /// Pending questions and unconfirmed requests.
    #[serde(default)]
    pub open: Vec<String>,
    /// Minimum trust over the summarized records (a summary built from
    /// external tool output stays external: findings §5, laundering).
    pub trust: Trust,
    /// First turn of the verbatim range this summary was built from
    /// (`1` after a full rebuild).
    #[serde(default = "one")]
    pub rebuilt_from: u32,
}

fn one() -> u32 {
    1
}

/// Lowercase ASCII alphanumerics only: `memory_reset_requested` and
/// `Memory.Reset.Requested` squash equal. Used for key canonicalization at
/// write time (M6 §6.1) and for near-miss action names (M5).
pub fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Query tokens worth matching: lowercase alphanumeric runs of 3+ chars.
pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_string)
        .collect()
}

/// Lexical relevance of current facts to a query (M6 §6.5 "query-relevant"):
/// number of query tokens found in the key (dots and underscores read as
/// spaces) or the value; zero-score facts are dropped; ties go to the most
/// recently validated. Shared by both stores until FTS5 lands (Phase 4).
pub fn lexical_rank(
    facts: &[crate::action::Fact],
    query: &str,
    k: usize,
) -> Vec<crate::action::Fact> {
    let tokens = query_tokens(query);
    if tokens.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &crate::action::Fact)> = facts
        .iter()
        .map(|f| {
            let hay = format!(
                "{} {}",
                f.key.replace(['.', '_', '-'], " ").to_lowercase(),
                match &f.value {
                    serde_json::Value::String(s) => s.to_lowercase(),
                    other => other.to_string().to_lowercase(),
                }
            );
            (
                tokens.iter().filter(|t| hay.contains(t.as_str())).count(),
                f,
            )
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.last_validated.cmp(&a.1.last_validated))
            .then_with(|| a.1.key.cmp(&b.1.key))
    });
    scored.into_iter().take(k).map(|(_, f)| f.clone()).collect()
}

/// A fact as the models see it (M6 §6.5): the current version plus, for
/// pinned keys, the value it superseded — "what was my name before" is
/// answerable from the context alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactView {
    pub key: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub uses: u32,
    pub state: crate::action::FactState,
    /// The superseded value and when it stopped being current.
    #[serde(default)]
    pub previous: Option<(serde_json::Value, crate::event::Timestamp)>,
}

impl From<&crate::action::Fact> for FactView {
    fn from(f: &crate::action::Fact) -> Self {
        Self {
            key: f.key.clone(),
            value: f.value.clone(),
            confidence: f.confidence,
            uses: f.uses,
            state: f.state,
            previous: None,
        }
    }
}

impl From<crate::action::Fact> for FactView {
    fn from(f: crate::action::Fact) -> Self {
        (&f).into()
    }
}

/// `user.name: "Peter" (was "Martin" until 17:35 UTC)`, plus `(unverified)`
/// below confidence 0.75 and `(stale)` for a cold fact (M6 §6.1, §6.3).
pub fn render_fact(f: &FactView) -> String {
    let mut s = format!("{}: {}", f.key, f.value);
    if f.confidence < 0.75 {
        s.push_str(" (unverified)");
    }
    if f.state == crate::action::FactState::Cold {
        s.push_str(" (stale)");
    }
    if let Some((prev, until)) = &f.previous {
        s.push_str(&format!(
            " (was {prev} until {})",
            crate::time::format_utc_short(until.0)
        ));
    }
    s
}

impl SessionSummary {
    /// Deterministic size cap (M6 §5.1): drop `open` items from the end,
    /// then `established` items, then cut the topic. The model's output
    /// length is never trusted.
    pub fn clamp(&mut self, max_chars: usize) {
        while render_summary(self).chars().count() > max_chars {
            if self.open.pop().is_some() {
                continue;
            }
            if self.established.pop().is_some() {
                continue;
            }
            let keep = self.topic.chars().count().saturating_sub(20);
            if keep == 0 {
                break;
            }
            self.topic = truncate_chars(&self.topic, keep);
        }
    }
}

/// One block: "Conversation so far (turns 1–8): …\nEstablished: …\nOpen: …".
pub fn render_summary(s: &SessionSummary) -> String {
    let mut out = format!(
        "Conversation so far (turns {}–{}): {}",
        s.rebuilt_from, s.through_turn, s.topic
    );
    if !s.established.is_empty() {
        out.push_str(&format!("\nEstablished: {}", s.established.join("; ")));
    }
    if !s.open.is_empty() {
        out.push_str(&format!("\nOpen: {}", s.open.join("; ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(turn: u32, user: &str, did: &[&str], reply: &str) -> TurnRecord {
        TurnRecord {
            turn,
            user: user.into(),
            did: did.iter().map(|s| s.to_string()).collect(),
            reply: reply.into(),
            trust: Trust::User,
        }
    }

    #[test]
    fn record_renders_user_did_and_bot_lines() {
        let r = rec(
            74,
            "tell me the time",
            &[
                "get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday)",
                "get_time -> denied (repeat_gate)",
            ],
            "It is 10:41 UTC.",
        );
        assert_eq!(
            render_record(&r, &Caps::default()),
            "[t74] user: tell me the time\n      did:  get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday) | get_time -> denied (repeat_gate)\n      bot:  It is 10:41 UTC."
        );
        let plain = rec(1, "hi", &[], "hello");
        assert_eq!(
            render_record(&plain, &Caps::default()),
            "[t1] user: hi\n      bot:  hello"
        );
    }

    #[test]
    fn caps_truncate_lines_and_whole_record_with_ellipsis() {
        let long = "x".repeat(500);
        let r = rec(2, &long, &[&long], &long);
        let caps = Caps {
            record_max_chars: 100,
            line_max_chars: 20,
        };
        let out = render_record(&r, &caps);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
        // a short user text with a long did line: only the line is cut
        let r = rec(3, "short", &[&long], "ok");
        let out = render_record(&r, &caps);
        assert!(out.contains(&format!("did:  {}…", "x".repeat(19))));
        assert!(out.ends_with("bot:  ok"));
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "ab…");
        // multi-byte safe
        assert_eq!(truncate_chars("žluťoučký", 4), "žlu…");
    }

    #[test]
    fn window_keeps_the_last_k_records_oldest_first() {
        let records: Vec<TurnRecord> = (1..=10)
            .map(|i| rec(i, &format!("u{i}"), &[], &format!("b{i}")))
            .collect();
        let w = render_window(&records, 3, &Caps::default());
        assert!(w.starts_with("[t8] user: u8"));
        assert!(w.contains("[t9] user: u9"));
        assert!(w.ends_with("[t10] user: u10\n      bot:  b10"));
        assert!(!w.contains("[t7]"));
        assert_eq!(render_window(&records, 0, &Caps::default()), "");
        assert_eq!(render_window(&[], 6, &Caps::default()), "");
        // fewer records than k: all of them
        assert_eq!(
            render_window(&records[..2], 6, &Caps::default())
                .matches("[t")
                .count(),
            2
        );
    }

    #[test]
    fn newlines_inside_fields_are_flattened() {
        let r = rec(4, "line1\nline2", &["a\nb"], "x\ny");
        let out = render_record(&r, &Caps::default());
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("user: line1 line2"));
    }

    #[test]
    fn squash_and_lexical_rank() {
        assert_eq!(squash("Memory.Reset-Requested"), "memoryresetrequested");
        assert_eq!(squash("memory_reset_requested"), "memoryresetrequested");
        let fact = |key: &str, value: &str, at: u64| crate::action::Fact {
            key: key.into(),
            value: serde_json::json!(value),
            last_validated: crate::event::Timestamp(at),
            ..Default::default()
        };
        let facts = vec![
            fact("user.name", "Martin", 1),
            fact("user.city", "Brno", 2),
            fact("user.previous_name", "Tomas", 3),
            fact("order.42.status", "shipped", 4),
        ];
        let hits = lexical_rank(&facts, "what was my previous name?", 5);
        let keys: Vec<&str> = hits.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["user.previous_name", "user.name"]);
        assert!(lexical_rank(&facts, "hi", 5).is_empty(), "no 3-char tokens");
        assert_eq!(lexical_rank(&facts, "is my order shipped", 1).len(), 1);
        assert_eq!(
            lexical_rank(&facts, "brno", 5)[0].key,
            "user.city",
            "values match too"
        );
    }

    #[test]
    fn fact_view_renders_markers() {
        let mut v: FactView = crate::action::Fact {
            key: "user.name".into(),
            value: serde_json::json!("Peter"),
            ..Default::default()
        }
        .into();
        assert_eq!(render_fact(&v), "user.name: \"Peter\"");
        v.previous = Some((
            serde_json::json!("Martin"),
            crate::event::Timestamp(1_788_370_524_628),
        ));
        assert_eq!(
            render_fact(&v),
            "user.name: \"Peter\" (was \"Martin\" until 17:35 UTC)"
        );
        v.confidence = 0.5;
        v.state = crate::action::FactState::Cold;
        assert_eq!(
            render_fact(&v),
            "user.name: \"Peter\" (unverified) (stale) (was \"Martin\" until 17:35 UTC)"
        );
    }

    #[test]
    fn summary_clamp_drops_lists_before_cutting_the_topic() {
        let mut s = SessionSummary {
            through_turn: 8,
            topic: "The user is testing memory and asking about times.".into(),
            established: vec!["a".repeat(40), "b".repeat(40)],
            open: vec!["c".repeat(40), "d".repeat(40)],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        s.clamp(200);
        assert!(render_summary(&s).chars().count() <= 200);
        assert_eq!(s.open.len(), 0, "open items go first");
        assert_eq!(s.established.len(), 2, "established survives while it fits");
        assert!(s.topic.starts_with("The user is testing"));
        s.clamp(120);
        assert!(s.established.is_empty(), "then established items");
        assert!(
            s.topic.starts_with("The user is testing"),
            "topic untouched so far"
        );
        s.clamp(50);
        assert!(
            render_summary(&s).chars().count() <= 50,
            "topic is cut last"
        );
        assert!(s.topic.ends_with('…'));
    }

    #[test]
    fn summary_renders_and_round_trips() {
        let s = SessionSummary {
            through_turn: 8,
            topic: "The user is testing memory.".into(),
            established: vec!["name is Martin".into(), "wants 24h times".into()],
            open: vec!["confirm forget_all".into()],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        assert_eq!(
            render_summary(&s),
            "Conversation so far (turns 1–8): The user is testing memory.\nEstablished: name is Martin; wants 24h times\nOpen: confirm forget_all"
        );
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SessionSummary>(&json).unwrap(), s);
        // old rows without the optional fields still parse
        let minimal: SessionSummary =
            serde_json::from_str(r#"{"through_turn":2,"topic":"t","trust":"User"}"#).unwrap();
        assert_eq!(minimal.rebuilt_from, 1);
        assert!(minimal.established.is_empty());
        assert_eq!(
            render_summary(&minimal),
            "Conversation so far (turns 1–2): t"
        );
    }
}
