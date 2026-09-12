//! Which stored facts a turn shows the models, and the running summary.
//!
//! Selection, pinning and summarisation together: they are one question —
//! what does this turn get to see of everything the session ever knew —
//! answered in one place rather than inline in the loop.

use super::Engine;
use super::EngineError;
use crate::state::fold;
use crate::trace::window_range;
use nscore::{EventKind, EventLog};


impl Engine {
    /// M6 §6.5: the facts a turn shows both models — a pinned core (keys
    /// under `pinned_prefixes`, newest validated first, never cold) plus the
    /// facts lexically relevant to the current message, within
    /// `facts_in_context`. Dumping the whole store masks precision failures
    /// and irrelevant facts measurably degrade replies (findings §1).
    /// The pinned core alone (M6 §6.5): current facts under
    /// `pinned_prefixes`, newest-validated first. Shown at every tier —
    /// a `Chat` turn that has forgotten the user's name is the failure the
    /// facts block was added to fix, not a saving.
    pub(super) async fn pinned_facts(&self, scope: &str) -> Vec<nscore::Fact> {
        let live = self.parts.memory.facts(scope, "").await.unwrap_or_default();
        let mut pinned: Vec<nscore::Fact> = live
            .iter()
            .filter(|f| f.state == nscore::FactState::Current)
            .filter(|f| self.is_pinned(f))
            .cloned()
            .collect();
        pinned.sort_by(|a, b| {
            b.last_validated
                .cmp(&a.last_validated)
                .then_with(|| a.key.cmp(&b.key))
        });
        pinned.truncate(self.cfg.pinned_max);
        pinned
    }

    /// The pinned core plus the query-relevant slice.
    ///
    /// `hybrid` is the caller's decision, never this function's: M11 T1.1
    /// landed `search_facts_hybrid` in the store and left every caller on
    /// `search_facts`, and the follow-up is that the fact path takes the same
    /// gate the turn path already takes at `recall_outcome` —
    /// `cfg.recall_hybrid` **and** a tier above `Chat`. A `Chat` turn is the
    /// cheap one by construction (it is already denied the query-relevant
    /// facts on the emitter side and every registered tool), and paying a
    /// round trip to `/embed` and `/rerank` on it would spend the tier's whole
    /// saving on its reply prompt. With `hybrid` false this is byte-for-byte
    /// what it always was, and with no encoder `search_facts_hybrid` is
    /// `search_facts` anyway — the knob can only ever add.
    pub(super) async fn select_facts(&self, scope: &str, user_text: &str, hybrid: bool) -> Vec<nscore::Fact> {
        let pinned = self.pinned_facts(scope).await;
        let k = self.cfg.relevant_max + pinned.len();
        let relevant = if hybrid {
            self.parts
                .memory
                .search_facts_hybrid(scope, user_text, k)
                .await
        } else {
            self.parts.memory.search_facts(scope, user_text, k).await
        }
        .unwrap_or_default();
        let mut out = pinned;
        for f in relevant {
            if out.len() >= self.cfg.facts_in_context
                || out.iter().filter(|o| !self.is_pinned(o)).count() >= self.cfg.relevant_max
            {
                break;
            }
            if !out.iter().any(|o| o.key == f.key) {
                out.push(f);
            }
        }
        out.truncate(self.cfg.facts_in_context);
        out
    }

    pub(super) fn is_pinned(&self, f: &nscore::Fact) -> bool {
        self.cfg
            .pinned_prefixes
            .iter()
            .any(|p| f.key.starts_with(p))
    }

    /// M6 §5.1: fold the turns that have fallen out of the window into the
    /// rolling summary, off the user's critical path (called after the
    /// reply is sent). Returns whether a `Summarized` event was appended.
    /// A summarizer failure appends nothing; the next boundary retries with
    /// the larger range.
    pub async fn maybe_summarize(&self, sid: &nscore::SessionId) -> Result<bool, EngineError> {
        let every = self.cfg.summary_every_turns as u32;
        if every == 0 {
            return Ok(false);
        }
        let stored = self.parts.memory.load(sid).await?;
        let n_loaded = stored.len();
        let state = fold(&stored);
        let through = state.turn.saturating_sub(self.cfg.window_turns as u32);
        let last = state.summary.as_ref().map(|s| s.through_turn).unwrap_or(0);
        if through < 1 || through.saturating_sub(last) < every {
            return Ok(false);
        }
        // Drift control: every Nth summary is rebuilt from verbatim records
        // alone, so summary-of-summary chains stay short (findings §2).
        let rebuild = self.cfg.summary_rebuild_every > 0
            && (state.summaries + 1).is_multiple_of(self.cfg.summary_rebuild_every as u32);
        let from = if rebuild { 1 } else { last + 1 };
        let mut records = state.records_in(from, through);
        while records.len() > 1
            && nscore::render_window(&records, records.len(), &self.cfg.caps)
                .chars()
                .count()
                > self.cfg.summary_input_max_chars
        {
            records.remove(0);
        }
        let Some(first) = records.first() else {
            return Ok(false);
        };
        let rebuilt_from = first.turn;
        let scope = (self.cfg.scope_for)(sid);
        // Never hybrid: the query is the empty string, so there is nothing
        // for a cosine arm to be near, and the summary runs off the user's
        // critical path precisely so it costs no round trips it does not
        // need.
        let selected = self.select_facts(&scope, "", false).await;
        let facts = self.fact_views(&scope, &selected).await;
        let previous = if rebuild {
            None
        } else {
            state.summary.as_ref()
        };
        // This call's own sink (M7 T0.1; multi-conversation plan Phase 1):
        // once more than one session is live the summary can run beside a
        // turn, and a sink shared with that turn would hand this call's
        // cost to whichever of the two drained first.
        let usage = std::sync::Arc::new(nscore::UsageSink::new());
        let input = nscore::SummaryInput {
            previous,
            records: &records,
            caps: &self.cfg.caps,
            facts: &facts,
            usage: Some(usage.clone()),
        };
        // The manifest for this call: the summarizer is shown the standing
        // facts and a range of verbatim records, and no tools or trace.
        let manifest = nscore::ContextManifest {
            fact_keys: facts.iter().map(|f| f.key.clone()).collect(),
            summary_through: previous.map(|s| s.through_turn),
            window: window_range(&records),
            scope: Some(scope.clone()),
            ..Default::default()
        };
        let summarized = self.parts.summarizer.summarize(input).await;
        // The log is opened before the outcome is known, because a summarizer
        // that spent a request and produced nothing usable has still spent
        // it. Leaving that call undrained would attribute it to whichever
        // turn came next; recording it says plainly that a request bought
        // no summary.
        let mut log = EventLog::from_events(sid.clone(), stored);
        self.record_model_calls(&usage, &mut log, state.turn, &manifest);
        let draft = match summarized {
            Ok(Some(d)) => d,
            Ok(None) => return self.persist_summary_events(sid, &log, n_loaded).await,
            Err(e) => {
                eprintln!("summarizer: {e}");
                return self.persist_summary_events(sid, &log, n_loaded).await;
            }
        };
        // A summary built from external tool output stays external: the
        // summarizer is a laundering channel otherwise (findings §5).
        let trusts: Vec<nscore::Trust> = records.iter().map(|r| r.trust).collect();
        let mut summary = nscore::SessionSummary {
            through_turn: through,
            topic: draft.topic,
            established: draft.established,
            open: draft.open,
            trust: nscore::min_trust(&trusts),
            rebuilt_from,
        };
        summary.clamp(self.cfg.summary_max_chars);
        log.append(
            state.turn,
            (self.clock)(),
            EventKind::Summarized { summary },
        );
        self.parts
            .memory
            .append(sid, &log.events()[n_loaded..])
            .await?;
        Ok(true)
    }

    /// Persist whatever the summary attempt appended and report that no
    /// summary was written. On the failure paths that is the `ModelCall` of
    /// a request that bought nothing — the one thing worth keeping from a
    /// summary that did not happen.
    pub(super) async fn persist_summary_events(
        &self,
        sid: &nscore::SessionId,
        log: &EventLog,
        from: usize,
    ) -> Result<bool, EngineError> {
        if log.events().len() > from {
            self.parts.memory.append(sid, &log.events()[from..]).await?;
        }
        Ok(false)
    }

    /// Views of `facts` for the contexts; pinned keys carry the value they
    /// superseded (M6 §6.1: "what was my name before" from context alone).
    pub(super) async fn fact_views(&self, scope: &str, facts: &[nscore::Fact]) -> Vec<nscore::FactView> {
        let mut views = Vec::with_capacity(facts.len());
        for f in facts {
            let mut view: nscore::FactView = f.into();
            if self.is_pinned(f) {
                let history = self
                    .parts
                    .memory
                    .fact_history(scope, &f.key)
                    .await
                    .unwrap_or_default();
                view.previous = history
                    .iter()
                    .find(|h| h.state == nscore::FactState::Superseded && h.value != f.value)
                    .and_then(|h| h.valid_to.map(|t| (h.value.clone(), t)));
            }
            views.push(view);
        }
        views
    }
}
