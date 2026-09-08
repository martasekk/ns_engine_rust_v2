//! What a provider call cost, and what it was shown (M6 spec §9; M7 plan
//! T0.1).
//!
//! The event log records what the *engine* decided. It has never recorded
//! what any of that cost, so three questions could not be answered from it:
//! how many tokens a turn spends, how many requests a turn spends — the
//! resource a free tier actually meters — and which facts were in front of
//! the model on the turns that went wrong. One `ModelCall` event per
//! provider call answers all three, and is infrastructure: replay ignores
//! it, the fold does not render it, no context contains it.
use serde::{Deserialize, Serialize};

/// One provider call, as the client saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Which role made the call: `emitter`, `replier`, `summarizer`.
    pub role: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// The provider returned no `usage` block, and the counts are
    /// [`estimate_tokens`] over what was sent and received. Recorded rather
    /// than hidden: the estimate is coarse and Czech text runs nearer three
    /// characters to the token, so a reader must be able to tell a measured
    /// number from a guessed one before averaging them.
    pub estimated: bool,
    /// HTTP requests actually issued, retries included. Tokens are not the
    /// scarce resource on every tier: `openrouter/free` allows fifty
    /// *requests* a day, and a call that succeeded on its third attempt
    /// spent three of them.
    pub attempts: u32,
    pub latency_ms: u32,
    /// Estimated tokens of the `tools` array alone — the action schemas,
    /// which the emitter re-sends in full on every iteration and which no
    /// context knob shrinks. Always an estimate, even when `prompt_tokens`
    /// is the provider's own number, because no provider itemizes a prompt.
    /// Zero for the replier and summarizer, which are sent no tools.
    ///
    /// Recorded to make the schema question answerable rather than
    /// arguable: `tools_tokens / prompt_tokens` per turn is the fraction,
    /// and it decides whether shortening descriptions is housekeeping or
    /// whether the legal set itself has to shrink. A guess about that
    /// fraction is not a reason to restructure the one construct in this
    /// design that is portable across all seven provider presets.
    #[serde(default)]
    pub tools_tokens: u32,
}

/// Four characters to the token.
///
/// Deliberately a constant rather than a per-model ratio. The estimate feeds
/// a budget decision, and a decision that moved with a calibration table
/// would make the same log replay differently once the table was updated.
/// Calibration is *reported* beside real usage instead, so the error is
/// visible without being load-bearing.
pub fn estimate_tokens(chars: usize) -> u32 {
    (chars / 4) as u32
}

/// What one call was shown, in keys rather than content.
///
/// Enough to reconstruct a prompt from the log together with the store, and
/// enough to ask afterwards which facts were present on the turns that were
/// graded badly — the post-hoc attribution the memory-security literature
/// found necessary (findings 2026-09-02 §5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManifest {
    /// Fact keys rendered into the prompt, in the order they were rendered.
    #[serde(default)]
    pub fact_keys: Vec<String>,
    /// `through_turn` of the rolling summary shown, if one was.
    #[serde(default)]
    pub summary_through: Option<u32>,
    /// Inclusive turn range of the verbatim window; `None` when it was empty.
    #[serde(default)]
    pub window: Option<(u32, u32)>,
    /// Lines of this turn's own trace, and the characters they took.
    #[serde(default)]
    pub trace_lines: usize,
    #[serde(default)]
    pub trace_chars: usize,
    /// Characters the per-line cap dropped before sending. The number M7
    /// phase 1 exists to move: on the recorded desktop session one
    /// `pointer_ui_read` was 14,425 characters against a median result of 24.
    #[serde(default)]
    pub clipped_chars: usize,
    /// Tool schemas sent with the call. Every one of them is prompt, and
    /// with a desktop wired in there are seventeen.
    #[serde(default)]
    pub tools: usize,
    /// Guidance notes rendered. A count, not a reconstruction key: the notes
    /// live in `learned.toml`, which the evolution pass rewrites.
    #[serde(default)]
    pub guidance: usize,
    /// What the budget did, or would have done, to this context (M7 T2.1).
    /// `None` when no budget was set. Under `report` mode this is the whole
    /// point of the field: the drops that did *not* happen, so the no-impact
    /// rate can be computed before any of them do.
    #[serde(default)]
    pub budget: Option<crate::budget::BudgetReport>,
}

/// Where a provider client leaves what a call cost, for the engine to pick
/// up immediately after it.
///
/// A side channel rather than a return value because the three model roles
/// sit behind [`crate::Emitter`], [`crate::Replier`] and
/// [`crate::Summarizer`], whose signatures say nothing about tokens and
/// which every double in the test suite implements. Widening those traits
/// would push an accounting concern into the contract the whole engine mocks
/// against, for a number only the real client can produce.
///
/// The engine drains after each of its own calls, and its three call sites
/// never overlap — the rolling summary runs while the loop waits for the
/// next message, never beside a turn — so what a drain returns belongs to
/// the call that just finished.
#[derive(Debug, Default)]
pub struct UsageSink {
    calls: std::sync::Mutex<Vec<Usage>>,
}

impl UsageSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, usage: Usage) {
        self.calls().push(usage);
    }

    /// Everything recorded since the last drain, oldest first.
    pub fn drain(&self) -> Vec<Usage> {
        std::mem::take(&mut *self.calls())
    }

    /// A poisoned lock still holds the numbers, and losing a turn over
    /// accounting would be the wrong trade.
    fn calls(&self) -> std::sync::MutexGuard<'_, Vec<Usage>> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(role: &str, prompt: u32) -> Usage {
        Usage {
            role: role.into(),
            model: "m".into(),
            prompt_tokens: prompt,
            completion_tokens: 1,
            estimated: false,
            attempts: 1,
            latency_ms: 5,
            tools_tokens: 0,
        }
    }

    #[test]
    fn sink_drains_in_order_and_empties() {
        let sink = UsageSink::new();
        assert!(sink.drain().is_empty());
        sink.record(usage("emitter", 10));
        sink.record(usage("emitter", 20));
        let drained = sink.drain();
        assert_eq!(
            drained.iter().map(|u| u.prompt_tokens).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert!(
            sink.drain().is_empty(),
            "a drained call must not be attributed to the next one too"
        );
    }

    #[test]
    fn token_estimate_is_four_characters_each() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(3), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(14_425), 3_606);
    }

    #[test]
    fn manifest_round_trips_and_old_rows_still_parse() {
        let m = ContextManifest {
            fact_keys: vec!["user.name".into()],
            summary_through: Some(4),
            window: Some((5, 10)),
            trace_lines: 3,
            trace_chars: 120,
            clipped_chars: 13_225,
            tools: 17,
            guidance: 1,
            budget: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<ContextManifest>(&json).unwrap(), m);
        // Every field is optional, so a manifest written before a field
        // existed still reads (the tier and budget of phases 2 and 3 land
        // here later).
        let sparse: ContextManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(sparse, ContextManifest::default());
    }
}
