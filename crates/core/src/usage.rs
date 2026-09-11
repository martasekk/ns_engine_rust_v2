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
    /// Prompt tokens the provider served out of its own prompt cache, as it
    /// reported them: OpenRouter puts the number in
    /// `usage.prompt_tokens_details.cached_tokens`. Part of `prompt_tokens`
    /// rather than extra to it — the share of a prompt that was already
    /// sitting on the provider's side, and the only evidence that a stable
    /// prefix is being reused rather than merely intended.
    ///
    /// Zero when the provider sends no details block, and zero on an
    /// estimated call, where there is no provider number to split. A zero
    /// therefore means "not reported", which is why it is read from the
    /// body rather than derived from anything here.
    #[serde(default)]
    pub cached_tokens: u32,
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

/// One context block, blanked to measure what it was worth (M9 T0.4).
///
/// The blanking happens *after* the fit, so the budget report still counts
/// the block as it was composed and the ablation is visible only in the
/// rendered prompt and in the manifest's keys for that block being empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ablate {
    Facts,
    Summary,
    Guidance,
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
    /// The names in the legal set the call's tool array was compiled from,
    /// in the order `schema::build_tools` serialized them (M10 T0.1).
    ///
    /// The count came first and stays, for the same reason `note_hashes`
    /// left `guidance` alone: `tools` is what every recorded call carries,
    /// and events are hash-chained over their JSON, so no old manifest can
    /// be rewritten to gain a list. `tool_names.len() == tools` on every
    /// call written since — the invariant that says both were filled from
    /// one legal set — and a manifest written before M10 reads as an empty
    /// list, which `ns-app budget` prints as `n/a` rather than as a session
    /// that sent no tools.
    ///
    /// `respond_directly` is deliberately absent: it is not in the legal
    /// set, it is appended by `build_tools`, and a report that wants the
    /// whole array adds it back the same way the compiler does.
    ///
    /// `skip_serializing_if` is load-bearing and is not tidiness. `event_hash`
    /// re-serializes a whole event to recompute the chain, so a field that
    /// serialized as `"tool_names":[]` would change the bytes of every
    /// `ModelCall` event ever written and `verify_chain` would report the
    /// recorded 21-turn log as broken — which is exactly what it did, once,
    /// before this attribute. An empty list is absent, which is the state a
    /// pre-M10 event was written in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<String>,
    /// Guidance notes rendered, and the hash of each.
    ///
    /// The count came first, and stays: the notes live in `learned.toml`,
    /// which the evolution pass rewrites, so a count was all the log could
    /// hold. The hashes now ride alongside it (M9 T0.3) — a note's hash is
    /// stable across rewrites of that file, so a note can be scored per turn
    /// by joining this manifest to the turn's grade. `note_hashes.len() ==
    /// guidance` on every call written since, which is the invariant saying
    /// both were filled from one render. Manifests written before M9 carry
    /// no list and deserialize with an empty one; they are never backfilled,
    /// because events are hash-chained over their JSON.
    #[serde(default)]
    pub guidance: usize,
    #[serde(default)]
    pub note_hashes: Vec<String>,
    /// Obligation lines rendered (M9 T2.1). A count, not the text: the
    /// clauses are the user's own words, which the `UserSaid` event of the
    /// same turn already holds verbatim — `obligations_for` replays them
    /// from it. Manifests written before M9 carry a zero.
    #[serde(default)]
    pub obligations: usize,
    /// The rendered size, in characters, of the three stable blocks *as
    /// sent* — measured after the fit with the same render functions the fit
    /// measures with (M9, for T0.5's stable-prefix estimate). A sum of block
    /// sizes is not recoverable from the keys, which is why it is recorded
    /// rather than derived.
    #[serde(default)]
    pub facts_chars: usize,
    #[serde(default)]
    pub summary_chars: usize,
    #[serde(default)]
    pub window_chars: usize,
    /// Which block, if any, was blanked after fitting (M9 T0.4). Set only by
    /// the evaluation harness; `None` on every live call.
    #[serde(default)]
    pub ablated: Option<Ablate>,
    /// Which tier this call was routed to (M7 Phase 3), and the cues that
    /// decided it. `None` when no router is installed. Recorded because a
    /// misroute is invisible in an answer — a `Chat` turn that needed a tool
    /// looks like a model that would not act — and because the tools
    /// fraction only means anything per tier: schemas are most of a `Task`
    /// prompt and none of a `Chat` one.
    #[serde(default)]
    pub tier: Option<crate::router::Tier>,
    #[serde(default)]
    pub route_cues: Vec<String>,
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
/// The engine gives each call its own sink through the call's context
/// ([`crate::EmitterContext::usage`] and its siblings) and drains it right
/// after the call, so what a drain returns belongs to that call even when
/// another session's turn is in flight at the same time. A client built with
/// a sink of its own records there only for a call whose context carries
/// none — the evolution pass's probes, and anything else outside a turn.
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
            cached_tokens: 0,
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
            tool_names: vec!["pointer_click".into()],
            tier: Some(crate::router::Tier::Task),
            route_cues: vec!["click".into()],
            guidance: 1,
            note_hashes: vec!["sha256:beef".into()],
            obligations: 2,
            facts_chars: 40,
            summary_chars: 200,
            window_chars: 300,
            ablated: Some(Ablate::Summary),
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

    /// The rule the M9 plan records as a risk: events are hash-chained over
    /// their serialized JSON, so a manifest written before `note_hashes`
    /// existed can never be rewritten to carry one. It has to read as an
    /// empty list with every other field intact — a pre-M9 session simply
    /// scores no notes.
    #[test]
    fn an_old_manifest_json_without_note_hashes_still_parses() {
        let old = r#"{
            "fact_keys": ["user.name", "user.city"],
            "summary_through": 4,
            "window": [5, 10],
            "trace_lines": 3,
            "trace_chars": 120,
            "clipped_chars": 0,
            "tools": 17,
            "guidance": 2,
            "tier": "task",
            "route_cues": ["click"]
        }"#;
        let m: ContextManifest = serde_json::from_str(old).unwrap();
        assert!(m.note_hashes.is_empty(), "no list is an empty list");
        assert_eq!(m.fact_keys, vec!["user.name", "user.city"]);
        assert_eq!(m.guidance, 2, "the count it did carry is untouched");
        assert_eq!(m.summary_through, Some(4));
        assert_eq!(m.window, Some((5, 10)));
        assert_eq!(m.tools, 17);
        assert_eq!(m.tier, Some(crate::router::Tier::Task));
        // The other M9 fields read the same way.
        assert_eq!((m.facts_chars, m.summary_chars, m.window_chars), (0, 0, 0));
        assert_eq!(m.ablated, None);
    }

    /// The same rule one milestone later (M10 T0.1). The recorded 21-turn
    /// desktop log was written before `tool_names` existed and can never be
    /// rewritten to carry one, so the field has to read as an empty list
    /// with `tools` intact — and an empty list next to a non-zero `tools`
    /// is exactly the state `ns-app budget` prints as `n/a`, rather than as
    /// a call that was sent no tools.
    #[test]
    fn an_old_manifest_without_tool_names_still_parses() {
        let old = r#"{
            "fact_keys": [],
            "trace_lines": 3,
            "trace_chars": 120,
            "tools": 17,
            "guidance": 2,
            "note_hashes": ["sha256:a", "sha256:b"],
            "obligations": 1,
            "facts_chars": 40,
            "tier": "task"
        }"#;
        let m: ContextManifest = serde_json::from_str(old).unwrap();
        assert!(m.tool_names.is_empty(), "no list is an empty list");
        assert_eq!(m.tools, 17, "the count it did carry is untouched");
        assert_eq!(m.note_hashes.len(), 2, "M9's list still reads");
        assert_eq!(m.obligations, 1);
        assert_eq!(m.facts_chars, 40);
        assert_eq!(m.tier, Some(crate::router::Tier::Task));
        // And the new field is not required to round-trip a new manifest
        // either: a call that sent no tools carries an empty list, which is
        // the same bytes as a pre-M10 one. The distinction that matters —
        // "nothing was sent" vs "nothing was recorded" — is `tools`, and
        // that is why it stays.
        let none: ContextManifest = serde_json::from_str(r#"{"tools": 0}"#).unwrap();
        assert!(none.tool_names.is_empty());
        assert_eq!(none.tools, 0);
    }

    /// The rule stated as the thing it protects. `event_hash` re-serializes
    /// an event to recompute the chain, so a manifest read from an old log
    /// and written back has to be byte-identical or every `ModelCall` event
    /// in the store becomes unverifiable — the failure this test was written
    /// after seeing: `sessions: 0 (skipped broken: 1)` on the recorded
    /// 21-turn log.
    #[test]
    fn an_empty_tool_names_serializes_to_nothing_so_old_events_still_hash() {
        let old = r#"{"tools":17,"guidance":0}"#;
        let m: ContextManifest = serde_json::from_str(old).unwrap();
        let round = serde_json::to_string(&m).unwrap();
        assert!(
            !round.contains("tool_names"),
            "an absent list must stay absent: {round}"
        );
        // And a manifest that does carry names writes them, so a new event
        // records what it sent.
        let new = ContextManifest {
            tools: 1,
            tool_names: vec!["recall".into()],
            ..Default::default()
        };
        assert!(serde_json::to_string(&new)
            .unwrap()
            .contains(r#""tool_names":["recall"]"#));
    }
}
