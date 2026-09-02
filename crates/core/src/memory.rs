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
