//! Writing the sentence the user actually reads.
//!
//! The loop decides *that* the turn is over and under which policy; this
//! module turns that policy into text and records what the text was grounded
//! in.

use super::diagnostics::explain_error;
use super::Engine;
use crate::state::fold;
use crate::trace::{reply_manifest, trace_for_prompt};
use nscore::{EventKind, EventLog, ReplyContext};

pub const FALLBACK_REPLY: &str = "Sorry, I couldn't complete that.";

/// What the turn hands the replier.
///
/// Eight arguments before this, which is past the point where a call site
/// says what it is passing. The one thing that is *not* here is the log: the
/// replier appends to it, so it stays a separate `&mut`.
pub(super) struct Draft<'a> {
    pub scope: &'a str,
    pub user_text: &'a str,
    /// This turn's one rules snapshot.
    pub rules: &'a nscore::LearnedRules,
    pub turn: u32,
    /// Where this call's token counts are collected.
    pub usage: &'a std::sync::Arc<nscore::UsageSink>,
    /// Whether this turn's tier and config allow the hybrid fact path
    /// (M11 T1.1 follow-up). Passed in rather than re-derived: the tier is
    /// the caller's, and it may have been upgraded mid-turn.
    pub hybrid_facts: bool,
    /// M12 T4.3: the answer the emitter call already produced. `Some` skips
    /// the replier and its `ModelCall` — the turn costs one request — and
    /// then runs the identical echo, grounding, obligation and citation
    /// block on the text, because an emitted answer is a draft like any
    /// other and is not owed a lighter check.
    pub pre_draft: Option<String>,
}

impl Engine {
    /// Draft the user-facing reply from the trace of what happened.
    ///
    /// Was the tail of a match arm inside `run_turn`, which left the most
    /// expensive phase of the turn as the one with no name. It is a phase:
    /// it selects facts, fits a budget, calls a model, and may call it a
    /// second time when the grounding check fires — the only place besides
    /// the emitter loop that spends a request.
    pub(super) async fn generate_reply(&self, d: Draft<'_>, log: &mut EventLog) -> String {
        let Draft {
            scope,
            user_text,
            rules,
            turn,
            usage,
            hybrid_facts,
            pre_draft,
        } = d;
        let now = &self.clock;
        let state = fold(log.events());
        // Clipped for the same reason, though this one is built once
        // per turn rather than once per iteration. `turn_trace` itself
        // stays uncapped: `render_echo` measures the reply against the
        // full material, and capping there would change what that
        // number means.
        let (trace_lines, reply_clipped_chars) = trace_for_prompt(
            log.events(),
            turn,
            self.cfg.trace_verbatim_lines,
            self.cfg.tool_result_max_chars,
        );
        let trace = trace_lines.join("\n");
        // Implicit recall (spec §5): standing facts enter the reply
        // context; each recall bumps `uses` (lifecycle metadata for
        // the future consolidation pass).
        let mut selected = self.select_facts(scope, user_text, hybrid_facts).await;
        for f in selected.iter_mut() {
            f.uses += 1;
            f.last_used = now();
            let _ = self.parts.memory.put_fact(f.clone()).await;
        }
        let facts = self.fact_views(scope, &selected).await;
        // M6 §4.3: the reply model gets the user's message, the
        // verbatim window and the summary — not a counter string.
        let window = state.window(self.cfg.window_turns);
        // M12 T3.2: the reply path archives by the same rule as the emitter.
        let guidance_notes = if self.cfg.archive_foreign_notes {
            rules.guidance_notes_for_reply_model(self.cfg.learning_model.as_deref())
        } else {
            rules.guidance_notes_for_reply()
        };
        let guidance: Vec<String> = guidance_notes.iter().map(|(_, t)| t.clone()).collect();
        // M9 T2.1: the same pure function the emitter path calls, on the
        // same message.
        let obligations = nscore::obligations_for(user_text, self.cfg.obligations_max);
        // `extra_guidance` is how the obligation interceptor speaks to the
        // second draft: one added note, the shape `do_not_state` already has
        // on the grounding path.
        let make_ctx =
            |do_not_state: Vec<String>, do_not_repeat: Vec<String>, extra_guidance: Vec<String>| {
                let mut notes = guidance.clone();
                notes.extend(extra_guidance);
                ReplyContext {
                    persona: self.cfg.persona.clone(),
                    facts: facts.clone(),
                    summary: state.summary.clone(),
                    window: window.clone(),
                    caps: self.cfg.caps,
                    user_text: user_text.to_string(),
                    obligations: obligations.clone(),
                    turn_trace: trace.clone(),
                    guidance: notes,
                    do_not_state,
                    do_not_repeat,
                    usage: Some(usage.clone()),
                }
            };
        // The reply context is fitted too, and reported on the same
        // way. Its `turn_trace` is exempt: it is the material the
        // reply narrates from, and the grounding interceptor flags a
        // reply for stating anything absent from it — trimming it
        // would manufacture the fabrications the interceptor catches.
        let mut budgeted = make_ctx(vec![], vec![], vec![]);
        let budget = nscore::fit_reply(
            &mut budgeted,
            self.cfg.prompt_budget_tokens,
            self.cfg.budget_mode,
            &self.cfg.pinned_prefixes,
            self.cfg.guidance_max,
        );
        // M9 T0.4, as on the emitter path: after the fit, so only the
        // rendered prompt and the manifest's keys change.
        match self.cfg.ablate {
            Some(nscore::Ablate::Facts) => budgeted.facts.clear(),
            Some(nscore::Ablate::Summary) => budgeted.summary = None,
            Some(nscore::Ablate::Guidance) => budgeted.guidance.clear(),
            None => {}
        }
        let note_hashes: Vec<String> = guidance_notes
            .iter()
            .take(budgeted.guidance.len())
            .map(|(h, _)| h.clone())
            .collect();
        let mut manifest = reply_manifest(scope, &budgeted, reply_clipped_chars, note_hashes);
        manifest.budget = Some(budget);
        manifest.ablated = self.cfg.ablate;
        // M12 T4.3. An emitted answer is already drafted and already paid
        // for; everything below it is the same.
        let drafted = match pre_draft {
            Some(text) => Ok(text),
            None => {
                let d = self.parts.replier.reply(budgeted).await;
                self.record_model_calls(usage, log, turn, &manifest);
                d
            }
        };
        match drafted {
            Ok(draft) if self.cfg.reply_grounding_check => {
                // M6 §4.5. Two checks, one of which acts.
                //
                // `ungrounded` gates: a claim nothing above supports
                // is named and the reply regenerated once, and the
                // second draft stands whatever it says.
                //
                // `echoed` only observes. The 2026-09-04 ablation
                // (plan §7–§8) scored it over four control arms: 21
                // firings, zero true positives. `echo_ratio` is
                // reference-free, so it cannot tell a copied engine
                // artifact from the same short correct answer given
                // twice — the two have identical verbatim overlap,
                // and the historical parrots (0.80–1.00) and the
                // false positives (0.60–1.00) overlap completely, so
                // no threshold separates them either. Both loop
                // detectors this borrows from are monitors, at far
                // more conservative thresholds. So it is logged, and
                // nothing is regenerated on it: the observability is
                // what found all of this, and it is free.
                let ctx = make_ctx(vec![], vec![], vec![]);
                let echo_material = crate::ground::echo_material(&ctx);
                if let Some(span) =
                    crate::echo::echoed(&draft, &echo_material, self.cfg.max_echo_ratio)
                {
                    log.append(
                        turn,
                        now(),
                        EventKind::ReplyEchoed {
                            draft: draft.clone(),
                            span,
                            ratio: crate::echo::echo_ratio(&draft, &echo_material),
                        },
                    );
                }
                let material = crate::ground::Material::from_context(&ctx);
                let spans = crate::ground::ungrounded(&draft, &material);
                if !spans.is_empty() {
                    log.append(
                        turn,
                        now(),
                        EventKind::ReplyFlagged {
                            draft: draft.clone(),
                            spans: spans.clone(),
                        },
                    );
                    // M12 T1.2: the flag above is free and always written;
                    // the call below is billed and exists to talk a weak
                    // model out of its fabrication. Where the model is not
                    // weak, the draft stands and falls through to the same
                    // obligations and citation path any draft takes.
                    if self.cfg.reply_regenerate {
                        let regenerated = self
                            .parts
                            .replier
                            .reply(make_ctx(spans, vec![], vec![]))
                            .await;
                        // The regeneration is a second billed call, and
                        // the point of counting it is to know what the
                        // grounding check costs.
                        self.record_model_calls(usage, log, turn, &manifest);
                        let final_reply = regenerated.unwrap_or(draft);
                        Self::record_cited(log, turn, now(), &ctx, &final_reply);
                        return final_reply;
                    }
                }
                // M9 T2.1. The obligation interceptor, behind its own
                // knob and *after* grounding: a draft that already had
                // to be regenerated has spent this turn's one spare
                // call. No new event kind — the mechanics are the
                // `ReplyFlagged` path's, with a distinct guidance
                // line, because the extraction is measured weak and a
                // signature written from it would be counted as if it
                // were not.
                match self
                    .cfg
                    .obligation_check
                    .then(|| crate::ground::unaddressed(&obligations, &draft))
                    .flatten()
                {
                    Some(clause) => {
                        let regenerated = self
                            .parts
                            .replier
                            .reply(make_ctx(
                                vec![],
                                vec![],
                                vec![format!("Not yet addressed: {clause}")],
                            ))
                            .await;
                        self.record_model_calls(usage, log, turn, &manifest);
                        let final_reply = regenerated.unwrap_or(draft);
                        Self::record_cited(log, turn, now(), &ctx, &final_reply);
                        final_reply
                    }
                    None => {
                        Self::record_cited(log, turn, now(), &ctx, &draft);
                        draft
                    }
                }
            }
            Ok(draft) => draft,
            Err(e) => {
                // F7: a replier failure is an event, not just a
                // fallback text — mining and audits must see it.
                log.append(
                    turn,
                    now(),
                    EventKind::ReplyFailed {
                        detail: e.to_string(),
                    },
                );
                format!(
                    "{FALLBACK_REPLY} Reason: the reply could not be generated — {}.",
                    explain_error(&e.to_string())
                )
            }
        }
    }

    /// M9 T4.2: record which reference parts the *final* reply drew on.
    ///
    /// The final draft, not the first: a flagged draft was regenerated, and
    /// what the discarded one quoted is not what the user was told. Written
    /// only when something was cited — an empty list is not a fact about the
    /// turn, and the join downstream reads absence as "nothing cited".
    pub(super) fn record_cited(
        log: &mut EventLog,
        turn: u32,
        at: nscore::Timestamp,
        ctx: &ReplyContext,
        reply: &str,
    ) {
        let sources = crate::ground::cited(ctx, reply);
        if !sources.is_empty() {
            log.append(turn, at, EventKind::ReplyCited { sources });
        }
    }
}

/// A fact value as prose: a JSON string without its quotes, anything else as
/// it serializes. Model-visible text carries no engine syntax — no `k = v`,
/// no JSON envelope — because whatever the reply model is shown it may
/// reproduce verbatim (plan §3, phase 1).
pub(super) fn value_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace "{name}" with vars["name"] (strings unquoted); unknown
/// placeholders are left verbatim. Deterministic fill-in, no escaping (M4).
pub(super) fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut out = template.to_string();
    if let Some(map) = vars.as_object() {
        for (k, v) in map {
            let replacement = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{k}}}"), &replacement);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub(super) fn render_substitutes_known_placeholders_only() {
        let vars = serde_json::json!({"name": "Martin", "n": 3});
        assert_eq!(
            render_template("Hi {name}, {n} items, {missing} stays", &vars),
            "Hi Martin, 3 items, {missing} stays"
        );
    }
}
