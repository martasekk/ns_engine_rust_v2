//! Composing what the emitter is shown, and the manifest that records it.
//!
//! The context and its manifest are built together on purpose. The manifest
//! has to describe the prompt **as sent** — after the budget has dropped
//! whatever did not fit and after `--ablate` has blanked whatever the
//! experiment is holding out — so anything that reads one and not the other
//! is reading a prompt that was never sent.
//!
//! Hence the order here, which is the whole file: compose, fit, ablate, then
//! describe. Nothing between the fit and the manifest may add to the context.

use super::Engine;
use crate::state::SessionState;
use crate::trace::{clipped_results, emitter_manifest, result_handle, trace_for_prompt};
use nscore::{ContextManifest, EmitterContext, LegalActionSet};

/// What one iteration needs in order to write the emitter's prompt.
///
/// The turn's own facts, gathered rather than reached for: the loop used to
/// read a dozen of its locals across a hundred and forty lines here, which
/// made "what is in the prompt" a question you could only answer by reading
/// the loop.
pub(super) struct Compose<'a> {
    pub events: &'a [nscore::Event],
    /// The projection taken at the top of this iteration.
    pub state: &'a SessionState,
    /// The actions the emitter may choose from, already narrowed.
    pub legal: &'a LegalActionSet,
    /// This turn's one rules snapshot.
    pub rules: &'a nscore::LearnedRules,
    /// Where this call's token counts are collected.
    pub usage: &'a std::sync::Arc<nscore::UsageSink>,
    /// Why this turn's earlier proposals did not run.
    pub rejections: &'a [String],
    /// The cues the router matched, for the manifest.
    pub route_cues: &'a [String],
    pub user_text: &'a str,
    pub scope: &'a str,
    pub turn: u32,
    pub tier: nscore::Tier,
    pub active_pending: bool,
}

/// A composed prompt: what is sent, what describes it, and whether the call
/// was offered the choice of answering instead of acting.
pub(super) struct Prompt {
    pub ctx: EmitterContext,
    pub manifest: ContextManifest,
    /// M13 T2.1. Carried out of here because an answer is only ever taken
    /// from a call that was offered one — a recorded double may return one
    /// anyway, and with the knob off this turn must be the turn it was before
    /// M12, event for event.
    pub offered_answer: bool,
}

impl Engine {
    /// Build the emitter's context for this iteration, and the manifest that
    /// says what it cost.
    pub(super) async fn compose_prompt(&self, c: &Compose<'_>) -> Prompt {
        // The same projection of the log the replier sees (M6 §4.2). The
        // emitter must see what this turn has already done — otherwise it
        // re-proposes completed actions until max_iterations exhausts — and
        // the standing facts, or it re-remembers them every turn (seen live).
        // Clipped: this is the line that is re-sent on every iteration, so an
        // uncapped tool result is paid for again at every step after it.
        let (trace_so_far, clipped_chars) = trace_for_prompt(
            c.events,
            c.turn,
            self.cfg.trace_verbatim_lines,
            self.cfg.tool_result_max_chars,
        );
        // The pinned core is shown at every tier — it is what stops the
        // emitter asking again for a name it already has (M6 F2). The
        // query-relevant slice is what a `Chat` turn does without.
        let selected = if c.tier.allows_relevant_facts() {
            // M11 T1.1 follow-up: the same gate `recall_outcome` takes. Read
            // per iteration rather than once per turn because the tier still
            // moves — a tool-cued turn is upgraded to `Task` mid-loop, and
            // the next iteration must see it.
            self.select_facts(
                c.scope,
                c.user_text,
                self.cfg.recall_hybrid && c.tier != nscore::Tier::Chat,
            )
            .await
        } else {
            self.pinned_facts(c.scope).await
        };
        let facts = self.fact_views(c.scope, &selected).await;
        let legal_names: Vec<String> = c.legal.actions.iter().map(|a| a.name.clone()).collect();
        // Notes and their hashes together, so the manifest can say which note
        // sat in this prompt (M9 T0.3). The texts go into the context; the
        // hashes are cut to whatever survived to be sent.
        // M12 T3.2: with the archive knob on, a note learned on another
        // emitter never reaches this prompt.
        let guidance_notes = if self.cfg.archive_foreign_notes {
            c.rules
                .guidance_notes_for_model(&legal_names, self.cfg.learning_model.as_deref())
        } else {
            c.rules.guidance_notes_for(&legal_names)
        };
        let mut ctx = EmitterContext {
            facts,
            summary: c.state.summary.clone(),
            window: c.state.window(self.cfg.window_turns),
            caps: self.cfg.caps,
            user_text: c.user_text.to_string(),
            // M9 T2.1: a pure function of the message, recomputed each
            // iteration rather than carried, for the same reason the trace is
            // — nothing per-turn is persisted as a column.
            obligations: nscore::obligations_for(c.user_text, self.cfg.obligations_max),
            trace_so_far,
            pending_confirmation: c.active_pending,
            rejections_this_turn: c.rejections.to_vec(),
            guidance: guidance_notes.iter().map(|(_, t)| t.clone()).collect(),
            budget_line: None,
            usage: Some(c.usage.clone()),
            answer: None,
        };

        // The budget runs before the manifest, so the manifest describes the
        // context as sent rather than as composed (M7 T2.1). Under the default
        // `report` mode nothing is dropped and the two are the same; the
        // report still says what enforcing would have cost.
        let budget = nscore::fit_emitter(
            &mut ctx,
            c.tier.budget(self.cfg.prompt_budget_tokens),
            self.cfg.budget_mode,
            &self.cfg.pinned_prefixes,
            self.cfg.guidance_max,
        );
        if self.cfg.show_budget_line {
            let clipped: Vec<String> =
                clipped_results(c.events, c.turn, self.cfg.tool_result_max_chars)
                    .into_iter()
                    .map(result_handle)
                    .collect();
            ctx.budget_line = Some(budget.line(&clipped));
        }
        // M9 T0.4. After the fit, so the budget report above still counts the
        // block as it was composed and the ablation shows up only in what was
        // rendered and in the manifest's keys.
        match self.cfg.ablate {
            Some(nscore::Ablate::Facts) => ctx.facts.clear(),
            Some(nscore::Ablate::Summary) => ctx.summary = None,
            Some(nscore::Ablate::Guidance) => ctx.guidance.clear(),
            None => {}
        }
        // M12 T4.3: chat-tier only, and only with the knob on. Filled after
        // the fit and the ablation, so `memory_silent` is a statement about
        // the context as sent rather than as composed — the same thing the
        // replier's silence line says.
        // M13 T2.1: on every tier the offer is the same sentence — call the
        // next tool or write the reply — so the loop ends when the model says
        // it is done rather than when it names the action that says so.
        let offered_answer = self.cfg.chat_act_or_answer
            && (self.cfg.act_or_answer_every_tier || c.tier == nscore::Tier::Chat);
        if offered_answer {
            let reply_guidance = if self.cfg.archive_foreign_notes {
                c.rules
                    .guidance_for_reply_model(self.cfg.learning_model.as_deref())
            } else {
                c.rules.guidance_for_reply()
            };
            ctx.answer = Some(nscore::AnswerBlocks {
                persona: self.cfg.persona.clone(),
                reply_guidance,
                memory_silent: ctx.facts.is_empty()
                    && ctx.summary.is_none()
                    && !ctx.trace_so_far.iter().any(|l| l.contains("recall")),
                with_action: self.cfg.act_and_answer,
            });
        }
        // Cut to what survived: nothing drops guidance from the middle, so a
        // prefix is exact, and it keeps `note_hashes.len() == guidance` true
        // whether the list was clamped or blanked.
        let note_hashes: Vec<String> = guidance_notes
            .iter()
            .take(ctx.guidance.len())
            .map(|(h, _)| h.clone())
            .collect();
        // The names, not just the count (M10 T0.1): `tools_tokens` says what
        // the array cost and nothing about which tool carried it, and the
        // whole of P1 is a decision about which text to cut.
        // `respond_directly` is absent because it is not in the legal set —
        // `build_tools` appends it, and a report adds it back the same way.
        let tool_names: Vec<String> = c.legal.actions.iter().map(|s| s.name.clone()).collect();
        let mut manifest = emitter_manifest(c.scope, &ctx, tool_names, clipped_chars, note_hashes);
        manifest.budget = Some(budget);
        manifest.ablated = self.cfg.ablate;
        manifest.tier = self.cfg.router.is_some().then_some(c.tier);
        manifest.route_cues = c.route_cues.to_vec();

        Prompt {
            ctx,
            manifest,
            offered_answer,
        }
    }
}
