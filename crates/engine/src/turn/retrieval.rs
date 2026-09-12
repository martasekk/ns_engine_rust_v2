//! The two searches the engine runs for itself.
//!
//! `recall` when the emitter asks for it and `exemplars` when the tier does.
//! Both produce a tool outcome the loop records like any other, so what the
//! engine looked up is in the log beside what the model chose.

use super::reply::value_text;
use super::Engine;
use nscore::ToolOutcome;


impl Engine {
    /// The `recall` search itself (M6 §7): verbatim turns beyond the window
    /// first, then live facts, with the lowest trust among them.
    ///
    /// One function because two callers need it — the `recall` action, and
    /// the `Deep` tier running it pre-emptively (M7 Phase 3). Two copies
    /// would drift, and the one that drifted would be the one a model reached
    /// for after the other had already failed it.
    ///
    /// M8 T3.3: `tier` is here and not inferred because the hybrid arm is
    /// tier-gated, and both callers know their tier. `Chat` recall stays
    /// lexical — a conversational turn never dials a model — and the
    /// `recall` action is reachable from `Chat`, so the gate cannot live at
    /// the pre-emptive call site alone.
    pub(super) async fn recall_outcome(
        &self,
        sid: &nscore::SessionId,
        scope: &str,
        query: &str,
        turn: u32,
        tier: nscore::Tier,
    ) -> ToolOutcome {
        let k = self.cfg.recall_top_k;
        let hybrid = self.cfg.recall_hybrid && tier != nscore::Tier::Chat;
        // Turns already visible in the window (and this one) add nothing.
        let visible_from = turn.saturating_sub(self.cfg.window_turns as u32);
        let mut lines: Vec<String> = Vec::new();
        let mut trusts: Vec<nscore::Trust> = Vec::new();
        let mut failure: Option<String> = None;
        let this_session = std::slice::from_ref(sid);
        let within = if hybrid {
            self.parts
                .memory
                .search_turns_hybrid(this_session, query, k * 3)
                .await
        } else {
            self.parts.memory.search_turns(sid, query, k * 3).await
        };
        match within {
            Ok(hits) => {
                for h in hits.into_iter().filter(|h| h.turn < visible_from).take(k) {
                    trusts.push(if h.speaker == "user" {
                        nscore::Trust::User
                    } else {
                        nscore::Trust::System
                    });
                    lines.push(format!("t{} {}: {}", h.turn, h.speaker, h.text));
                }
            }
            Err(e) => failure = Some(e.to_string()),
        }
        // Then the sessions before this one (M7 Phase 4). Only digested
        // sessions are searched, which is exactly what "an earlier
        // conversation" means here: the consolidator writes a digest once a
        // session has a summary, so one still in progress is not among them.
        if self.cfg.recall_sessions > 0 {
            match self
                .parts
                .memory
                .session_digests(scope, self.cfg.recall_sessions + 1)
                .await
            {
                Ok(digests) => {
                    let earlier: Vec<nscore::SessionId> = digests
                        .into_iter()
                        .map(|d| d.session)
                        .filter(|s| s != sid)
                        .take(self.cfg.recall_sessions)
                        .collect();
                    if !earlier.is_empty() {
                        let across = if hybrid {
                            self.parts
                                .memory
                                .search_turns_hybrid(&earlier, query, k)
                                .await
                        } else {
                            self.parts.memory.search_turns_in(&earlier, query, k).await
                        };
                        match across {
                            Ok(hits) => {
                                for h in hits.into_iter().take(k) {
                                    trusts.push(if h.speaker == "user" {
                                        nscore::Trust::User
                                    } else {
                                        nscore::Trust::System
                                    });
                                    lines.push(format!(
                                        "in an earlier conversation, t{} {}: {}",
                                        h.turn, h.speaker, h.text
                                    ));
                                }
                            }
                            Err(e) => failure = Some(e.to_string()),
                        }
                    }
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        match self.parts.memory.search_facts(scope, query, k).await {
            Ok(facts) => {
                for f in facts {
                    trusts.push(f.trust);
                    lines.push(format!(
                        "from memory, {} is {}",
                        f.key,
                        value_text(&f.value)
                    ));
                }
            }
            Err(e) => failure = Some(e.to_string()),
        }
        // Digests last. They are summaries of summaries, and the controlled
        // ablation this design rests on puts extracted artifacts 16–22 points
        // below verbatim text (findings 2026-09-02 §1). They earn their place
        // by answering what a whole earlier conversation was about, which no
        // single verbatim line does.
        if self.cfg.recall_sessions > 0 {
            match self.parts.memory.search_digests(scope, query, k).await {
                Ok(digests) => {
                    for d in digests.into_iter().filter(|d| &d.session != sid) {
                        trusts.push(d.summary.trust);
                        lines.push(format!(
                            "an earlier conversation (through t{}) was about: {}",
                            d.last_turn, d.summary.topic
                        ));
                    }
                }
                Err(e) => failure = Some(e.to_string()),
            }
        }
        match failure {
            Some(detail) => ToolOutcome::Err {
                kind: "store".into(),
                detail,
            },
            None if lines.is_empty() => ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: "no matches".into(),
                    artifact: None,
                    trust: nscore::Trust::System,
                },
            },
            // Joined, not JSON: brackets and escaped quotes are pure
            // copy-bait for the reply model and buy nothing, since nothing
            // parses this back (plan §3, phase 1). One line, because
            // `turn_trace` is line-per-event.
            None => ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: lines.join("; "),
                    artifact: None,
                    trust: nscore::min_trust(&trusts),
                },
            },
        }
    }

    /// The exemplars step (M9 T5.3, M10 T3.6): at most `exemplars_max`
    /// digests of this scope nearest the message by cosine, as **one**
    /// `ToolReturned` carrying their lowest trust.
    ///
    /// One return and not one per digest: they are a single answer to a
    /// single question, and N returns would be N entries in the trace
    /// competing with the turn's real tool results for the verbatim lines
    /// `trace_verbatim_lines` allows.
    ///
    /// Lowest trust, not each digest's own: they arrive folded into one
    /// text, a reader cannot tell which sentence came from which
    /// conversation, and trust that cannot be attributed has to be the
    /// weakest of what it is made of (M6 §5.1).
    ///
    /// The current session is excluded — a conversation is not an exemplar
    /// of itself — and so is a store with no digest vectors, which returns
    /// an empty list and therefore "no similar conversations".
    pub(super) async fn exemplars_outcome(
        &self,
        sid: &nscore::SessionId,
        scope: &str,
        query: &str,
    ) -> ToolOutcome {
        let digests = match self
            .parts
            .memory
            .nearest_digests(scope, query, self.cfg.exemplars_max + 1)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                return ToolOutcome::Err {
                    kind: "store".into(),
                    detail: e.to_string(),
                }
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let mut trusts: Vec<nscore::Trust> = Vec::new();
        for d in digests
            .into_iter()
            .filter(|d| &d.session != sid)
            .take(self.cfg.exemplars_max)
        {
            trusts.push(d.summary.trust);
            lines.push(format!(
                "a similar earlier conversation (through t{}) was about: {}",
                d.last_turn, d.summary.topic
            ));
        }
        if lines.is_empty() {
            return ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: "no similar conversations".into(),
                    artifact: None,
                    trust: nscore::Trust::System,
                },
            };
        }
        ToolOutcome::Ok {
            output: nscore::ToolOutput {
                summary: lines.join("; "),
                artifact: None,
                trust: nscore::min_trust(&trusts),
            },
        }
    }
}
