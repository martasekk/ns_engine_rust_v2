//! The turn loop.
//!
//! One pass of this loop is one model decision: project the log, build the
//! emitter context, ask for a proposal, and either settle the turn or run
//! the action and go round again. The steps are lettered in the order they
//! happen, and the phases they call into live in this module's siblings.

use super::accounting::last_proposal_ran;
use super::diagnostics::explain_error;
use super::config::RememberResidual;
use super::gate::classify;
use super::reply::{render_template, FALLBACK_REPLY};
use super::specs::*;
use super::{Engine, EngineError};
use crate::state::fold;
use crate::trace::{
    clipped_results, emitter_manifest, inspect_page, parse_result_handle, result_handle,
    result_text, result_trust, result_window, trace_for_prompt,
};
use nscore::{
    ClassifiedProposal, EventKind, EventLog, Incoming, LegalActionSet, RejectReason, ReplyPolicy,
    Timestamp, ToolCtx, ToolOutcome, Verdict,
};


impl Engine {
    pub async fn run_turn(&self, incoming: Incoming) -> Result<String, EngineError> {
        // Before anything is loaded or called: a capped run stops between
        // turns, with the message it could not afford left unanswered
        // rather than half answered.
        self.cap_reached()?;
        let sid = incoming.session.clone();
        let scope = (self.cfg.scope_for)(&sid);
        let stored = self.parts.memory.load(&sid).await?;
        let n_loaded = stored.len();
        let mut log = EventLog::from_events(sid.clone(), stored);
        // This turn's own sink for what its model calls cost (M7 T0.1). It
        // travels in every context the turn builds and is drained right
        // after each call, so a turn on another session running at the same
        // time cannot land its records on this one's `ModelCall`s
        // (multi-conversation plan Phase 1, findings §2.6).
        let usage = std::sync::Arc::new(nscore::UsageSink::new());

        let turn = fold(log.events()).turn + 1;
        let now = &self.clock;
        // One rules snapshot per turn: a driver may swap the set mid-session.
        let rules = self.cfg.learned.load_full();
        log.append(
            turn,
            now(),
            EventKind::UserSaid {
                text: incoming.text.clone(),
            },
        );

        // M7 Phase 3: what kind of turn this is, decided once and before any
        // model call, from the message and this turn's own history only.
        let routed = self.route_turn(&incoming.text, log.events(), turn);
        let mut tier = routed.tier;
        // M10 T2.1: which registered tools ride this turn, chosen once here
        // and held across every iteration. `None` is the full set.
        //
        // Once per turn rather than once per iteration is the whole rule: a
        // set recomputed each pass would change the `tools` array under a
        // provider prefix cache and buy nothing, since nothing between two
        // iterations of one turn changes what the *message* asked for
        // (decision 2, 2026-09-11). Escalation below is the one thing
        // allowed to move it, and it only ever widens.
        let mut selected_tools = routed.tools.clone();
        // `Deep` runs the recall itself rather than waiting to be asked for
        // it. That is the saving: on a fifty-request day an emitter iteration
        // spent proposing `recall` is a request that bought no progress, and
        // the query the emitter would have passed is the user's own message.
        // Recorded as a real call rather than injected as prompt text, so it
        // enters the provenance index, carries its own trust, and replays.
        if tier == nscore::Tier::Deep {
            let args = serde_json::json!({ "query": incoming.text });
            let spec = recall_spec(self.cfg.schema_profile);
            let classified = classify(log.events(), &args, &spec, turn);
            let call_id = log
                .append(
                    turn,
                    now(),
                    EventKind::ToolCalled {
                        action: RECALL.into(),
                        args: classified,
                    },
                )
                .id;
            let outcome = self
                .recall_outcome(&sid, &scope, &incoming.text, turn, tier)
                .await;
            log.append(
                turn,
                now(),
                EventKind::ToolReturned {
                    call: call_id,
                    outcome,
                },
            );

            // M10 T3.6: exemplars — the nearest earlier *conversations*,
            // by cosine over their digests' stored vectors.
            //
            // A second call rather than more lines inside the recall return,
            // because it answers a different question: recall finds the line
            // that says the thing, an exemplar is a whole conversation shaped
            // like this one. Keeping them apart is also what lets `--ablate`
            // decide the default later — a knob folded into another step's
            // output cannot be turned off and measured.
            //
            // Off at `exemplars_max = 0`, which is every deployment today,
            // and the store returns nothing without an encoder, so this is
            // two comparisons on the ordinary path.
            if self.cfg.exemplars_max > 0 {
                let args = serde_json::json!({ "query": incoming.text });
                let spec = exemplars_spec();
                let classified = classify(log.events(), &args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: EXEMPLARS.into(),
                            args: classified,
                        },
                    )
                    .id;
                let outcome = self
                    .exemplars_outcome(&sid, &scope, &incoming.text)
                    .await;
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
            }
        }

        // M10 T1.4: applicability, asked once per turn rather than per
        // iteration. A tool in the schema that cannot do anything is tokens
        // spent on a choice that can only fail — `forget_fact` and
        // `forget_all` were sent on all 21 recorded turns while the store
        // held zero facts (~216 tokens a turn), and `recall` was sent on
        // turn 1 (findings §8.1).
        //
        // This is store state deciding legality, which the narrowing below
        // deliberately avoids for *this turn's own* events. The difference
        // is that these two questions are answered before the loop and held
        // fixed across it, so an iteration cannot see the set change under
        // it; and both fail **open** — a store that errors keeps the tool.
        let scope_holds_facts = !self.cfg.prune_inapplicable
            || self
                .parts
                .memory
                .facts(&scope, "")
                .await
                .map(|f| !f.is_empty())
                .unwrap_or(true);
        // Recall is worth its schema when there is something out of sight:
        // turns older than the verbatim window, or an earlier conversation
        // in the same scope.
        let recall_applies = !self.cfg.prune_inapplicable || turn > self.cfg.window_turns as u32 || {
            self.cfg.recall_sessions > 0
                && match self
                    .parts
                    .memory
                    .session_digests(&scope, self.cfg.recall_sessions + 1)
                    .await
                {
                    Ok(digests) => digests.into_iter().any(|d| d.session != sid),
                    Err(_) => true,
                }
        };

        let mut rejections_this_turn: Vec<String> = Vec::new();
        let mut denied_this_turn: std::collections::HashSet<String> = Default::default();
        let mut calls_this_turn: std::collections::HashSet<String> = Default::default();
        let mut never_residual_this_turn = false;
        let mut forget_misses: u32 = 0;
        let mut emit_failures: u32 = 0;
        let mut last_emit_error: Option<String> = None;
        // Whether the emitter ever produced a proposal this turn; decides
        // which fallback reason the user is given.
        let mut proposed_this_turn = false;
        let mut settled: Option<ReplyPolicy> = None;
        // M12 T4.3: the reply text the emitter call already produced, when
        // the turn settled on an answer rather than an action. `None` is
        // every turn before M12 and every turn with the knob off.
        let mut pre_draft: Option<String> = None;
        // M13 T3.1: an answer that arrived *beside* an action, held until the
        // action has actually run. It cannot be settled on at proposal time:
        // a guard may still refuse the call, and a reply saying "opening it
        // now" on a turn that opened nothing is worse than a second request.
        let mut answer_with_action: Option<String> = None;

        for _ in 0..self.cfg.max_iterations {
            // M13 T3.1: the held answer, collected one iteration later so the
            // log can say whether the action it was written beside happened.
            // A refused proposal leaves a `Rejected` last, not a
            // `ToolReturned`, and the answer is dropped with it.
            if let Some(text) = answer_with_action.take() {
                if last_proposal_ran(log.events(), turn) {
                    pre_draft = Some(text);
                    let e = log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: ReplyPolicy::Generate,
                        },
                    );
                    settled = Some(match &e.kind {
                        EventKind::Settled { policy } => policy.clone(),
                        _ => unreachable!(),
                    });
                    break;
                }
            }
            // a. project
            let state = fold(log.events());
            // A pending confirmation is active only on the turn immediately
            // following its creation (expiry rule, spec §9).
            let active_pending = state.pending_confirmation.filter(|_| {
                state.pending_turn == Some(turn)
                    || state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false)
            });
            let legal = if never_residual_this_turn {
                // Forced clarification (spec §5.1): a NeverResidual rejection
                // occurred and nothing grounds the arg — the only way forward
                // is to ask (respond_directly stays available at schema level).
                LegalActionSet {
                    actions: vec![ask_clarification_spec(self.cfg.schema_profile)],
                }
            } else {
                // Narrowed schema (spec §2): actions rejected this turn are
                // removed from the set the emitter sees next.
                // A `Chat` turn carries no tool schemas at all. With a desktop
                // wired in that is ten of the seventeen schemas the emitter
                // would otherwise re-send on every iteration of a turn that
                // was never going to click anything. The synthetic actions
                // stay legal at every tier: they are how a turn ends.
                //
                // M12 T2.1: unless the route selected some. A chat turn that
                // asked the time carries exactly the tools its own cue named
                // (`[router] chat_tools`) and nothing else — the selection is
                // the whole allowance there, so the filter below narrows to
                // it the same way, once per turn.
                let mut actions: Vec<_> = if tier.allows_tools() || selected_tools.is_some() {
                    self.parts
                        .tools
                        .iter()
                        .map(|t| t.spec().clone())
                        .filter(|s| !denied_this_turn.contains(&s.name))
                        // M10 T2.1. A *turn*-level decision consulted here
                        // rather than re-taken here: `selected_tools` is
                        // fixed for the loop except when escalation widens
                        // it, so this filter yields the same names on every
                        // iteration and the array's bytes do not move.
                        .filter(|s| {
                            selected_tools
                                .as_ref()
                                .map_or(true, |sel| sel.contains(&s.name))
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                actions.push(ask_clarification_spec(self.cfg.schema_profile));
                if !denied_this_turn.contains(REMEMBER_FACT) {
                    actions.push(remember_fact_spec(self.cfg.schema_profile));
                }
                if !denied_this_turn.contains(RECALL) && recall_applies {
                    actions.push(recall_spec(self.cfg.schema_profile));
                }
                // Offered only while there is something to inspect. An
                // action in the schema that can only fail is a way for a
                // small model to spend an iteration discovering that.
                if !denied_this_turn.contains(INSPECT_RESULT)
                    && !clipped_results(log.events(), turn, self.cfg.tool_result_max_chars).is_empty()
                {
                    actions.push(inspect_result_spec(self.cfg.schema_profile));
                }
                // Forgetting is legal only while it can mean something: not
                // after a fact was written this turn (seen live: "my name is
                // now Peter" ended in forget_fact + a staged forget_all) and
                // not after a forget already ran. Both are this turn's own
                // events, so replay reproduces them; store state ("any facts
                // at all?") must never decide legality.
                let wrote_fact = calls_this_turn
                    .iter()
                    .any(|k| k.starts_with(&format!("{REMEMBER_FACT}\u{0}")));
                let forgot = calls_this_turn.iter().any(|k| k.starts_with("forget_"));
                if !wrote_fact && !forgot && scope_holds_facts {
                    if !denied_this_turn.contains(FORGET_FACT) {
                        actions.push(forget_fact_spec(self.cfg.schema_profile));
                    }
                    if !denied_this_turn.contains(FORGET_ALL) {
                        actions.push(forget_all_spec(self.cfg.schema_profile));
                    }
                }
                if active_pending.is_some() {
                    actions.push(confirm_pending_spec(self.cfg.schema_profile));
                }
                LegalActionSet { actions }
            };

            // b. emitter context (M6 §4.2): the same projection of the log
            // the replier sees. The emitter must see what this turn has
            // already done — otherwise it re-proposes completed actions until
            // max_iterations exhausts — and the standing facts, or it
            // re-remembers them every turn (seen live).
            // Clipped: this is the line that is re-sent on every iteration,
            // so an uncapped tool result is paid for again at every step
            // after it.
            let (trace_so_far, clipped_chars) =
                trace_for_prompt(
                    log.events(),
                    turn,
                    self.cfg.trace_verbatim_lines,
                    self.cfg.tool_result_max_chars,
                );
            // The pinned core is shown at every tier — it is what stops the
            // emitter asking again for a name it already has (M6 F2). The
            // query-relevant slice is what a `Chat` turn does without.
            let selected = if tier.allows_relevant_facts() {
                // M11 T1.1 follow-up: the same gate `recall_outcome` takes.
                // Read here rather than bound once above because `tier` is
                // still mutable at this point — a tool-cued turn is upgraded
                // to `Task` mid-loop, and the next iteration must see it.
                self.select_facts(
                    &scope,
                    &incoming.text,
                    self.cfg.recall_hybrid && tier != nscore::Tier::Chat,
                )
                .await
            } else {
                self.pinned_facts(&scope).await
            };
            let facts = self.fact_views(&scope, &selected).await;
            let legal_names: Vec<String> = legal.actions.iter().map(|a| a.name.clone()).collect();
            // Notes and their hashes together, so the manifest can say which
            // note sat in this prompt (M9 T0.3). The texts go into the
            // context; the hashes are cut to whatever survived to be sent.
            // M12 T3.2: with the archive knob on, a note learned on another
            // emitter never reaches this prompt.
            let guidance_notes = if self.cfg.archive_foreign_notes {
                rules.guidance_notes_for_model(&legal_names, self.cfg.learning_model.as_deref())
            } else {
                rules.guidance_notes_for(&legal_names)
            };
            let mut ctx = nscore::EmitterContext {
                facts,
                summary: state.summary.clone(),
                window: state.window(self.cfg.window_turns),
                caps: self.cfg.caps,
                user_text: incoming.text.clone(),
                // M9 T2.1: a pure function of the message, recomputed each
                // iteration rather than carried, for the same reason the
                // trace is — nothing per-turn is persisted as a column.
                obligations: nscore::obligations_for(&incoming.text, self.cfg.obligations_max),
                trace_so_far,
                pending_confirmation: active_pending.is_some(),
                rejections_this_turn: rejections_this_turn.clone(),
                guidance: guidance_notes.iter().map(|(_, t)| t.clone()).collect(),
                budget_line: None,
                usage: Some(usage.clone()),
                answer: None,
            };

            // c. propose
            let mut confirmed_now = false;
            // The budget runs before the manifest, so the manifest describes
            // the context as sent rather than as composed (M7 T2.1). Under
            // the default `report` mode nothing is dropped and the two are
            // the same; the report still says what enforcing would have cost.
            let budget = nscore::fit_emitter(
                &mut ctx,
                tier.budget(self.cfg.prompt_budget_tokens),
                self.cfg.budget_mode,
                &self.cfg.pinned_prefixes,
                self.cfg.guidance_max,
            );
            if self.cfg.show_budget_line {
                let clipped: Vec<String> =
                    clipped_results(log.events(), turn, self.cfg.tool_result_max_chars)
                    .into_iter()
                    .map(result_handle)
                    .collect();
                ctx.budget_line = Some(budget.line(&clipped));
            }
            // M9 T0.4. After the fit, so the budget report above still
            // counts the block as it was composed and the ablation shows up
            // only in what was rendered and in the manifest's keys.
            match self.cfg.ablate {
                Some(nscore::Ablate::Facts) => ctx.facts.clear(),
                Some(nscore::Ablate::Summary) => ctx.summary = None,
                Some(nscore::Ablate::Guidance) => ctx.guidance.clear(),
                None => {}
            }
            // M12 T4.3: chat-tier only, and only with the knob on. Filled
            // after the fit and the ablation, so `memory_silent` is a
            // statement about the context as sent rather than as composed —
            // the same thing the replier's silence line says.
            // M13 T2.1: on every tier the offer is the same sentence — call
            // the next tool or write the reply — so the loop ends when the
            // model says it is done rather than when it names the action that
            // says so.
            let offered_answer = self.cfg.chat_act_or_answer
                && (self.cfg.act_or_answer_every_tier || tier == nscore::Tier::Chat);
            if offered_answer {
                let reply_guidance = if self.cfg.archive_foreign_notes {
                    rules.guidance_for_reply_model(self.cfg.learning_model.as_deref())
                } else {
                    rules.guidance_for_reply()
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
            // Cut to what survived: nothing drops guidance from the middle,
            // so a prefix is exact, and it keeps `note_hashes.len() ==
            // guidance` true whether the list was clamped or blanked.
            let note_hashes: Vec<String> = guidance_notes
                .iter()
                .take(ctx.guidance.len())
                .map(|(h, _)| h.clone())
                .collect();
            // The names, not just the count (M10 T0.1): `tools_tokens` says
            // what the array cost and nothing about which tool carried it,
            // and the whole of P1 is a decision about which text to cut.
            // `respond_directly` is absent because it is not in the legal
            // set — `build_tools` appends it, and a report adds it back the
            // same way.
            let tool_names: Vec<String> =
                legal.actions.iter().map(|s| s.name.clone()).collect();
            let mut manifest =
                emitter_manifest(&scope, &ctx, tool_names, clipped_chars, note_hashes);
            manifest.budget = Some(budget);
            manifest.ablated = self.cfg.ablate;
            manifest.tier = self.cfg.router.is_some().then_some(tier);
            manifest.route_cues = routed.cues.clone();
            let proposed = self.parts.emitter.propose_or_answer(ctx, &legal).await;
            self.record_model_calls(&usage, &mut log, turn, &manifest);
            // Carried as far as the `respond_directly` branch below, or to
            // the top of the next iteration when it rode beside an action.
            let mut emitted_answer: Option<String>;
            // M13 T4.1: the line to say now, before the action runs.
            let mut emitted_say: Option<String>;
            let mut proposal = match proposed {
                Ok(e) => {
                    // An answer is only ever taken from a call that was
                    // offered the choice. A double may return one anyway;
                    // with the knob off this turn must be the turn it was
                    // before M12, event for event.
                    emitted_answer = offered_answer.then_some(e.answer).flatten();
                    emitted_say = offered_answer.then_some(e.say).flatten();
                    e.proposal
                }
                Err(e) => {
                    // Four failure classes, three recoveries. A refused or
                    // failed endpoint is not a malformed proposal, and a
                    // terminal one (a model name that does not exist, an
                    // empty balance) will not become one by asking again.
                    let retryable = e.is_retryable();
                    let reason = match &e {
                        nscore::EmitError::Provider { status, detail } => {
                            RejectReason::ProviderUnavailable {
                                status: *status,
                                detail: detail.clone(),
                            }
                        }
                        other => RejectReason::Malformed {
                            detail: other.to_string(),
                        },
                    };
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: nscore::EventId(0),
                            reason,
                        },
                    );
                    rejections_this_turn.push(match &e {
                        nscore::EmitError::Provider { status, .. } => {
                            format!("provider unavailable: HTTP {status}")
                        }
                        other => format!("emitter failure: {other}"),
                    });
                    last_emit_error = Some(e.to_string());
                    emit_failures += 1;
                    if !retryable || emit_failures >= self.cfg.max_emit_retries {
                        break;
                    }
                    continue;
                }
            };

            // d. record proposal
            proposed_this_turn = true;
            let pid = log
                .append(
                    turn,
                    now(),
                    EventKind::Proposed {
                        proposal: proposal.clone(),
                    },
                )
                .id;

            // f0. learned input repairs (spec M5 §3.2). The Proposed event above
            // keeps the raw model output; ToolCalled records what actually ran.
            // Repairs only rewrite the proposal — legality, validation and
            // guards below judge the rewritten proposal exactly as raw output.
            if let Some(to) = rules.alias(&proposal.action) {
                proposal.action = to.to_string();
            }
            rules.normalize(&proposal.action, &mut proposal.args);

            // e. direct reply
            if proposal.action == "respond_directly" {
                // M12 T4.3. `Settled { Generate }` either way: what changes
                // is who drafts, not what the log says happened, so a replay
                // of this turn is the shape it always was.
                pre_draft = emitted_answer.take();
                let e = log.append(
                    turn,
                    now(),
                    EventKind::Settled {
                        policy: ReplyPolicy::Generate,
                    },
                );
                settled = Some(match &e.kind {
                    EventKind::Settled { policy } => policy.clone(),
                    _ => unreachable!(),
                });
                break;
            }

            // e2. M13 T3.1: an answer beside a real action. The model said
            // both what it is doing and that it is doing it, which is the one
            // turn shape act-or-answer could not express: "act" and "answer"
            // were alternatives, so a turn that did something always bought a
            // second call to say so.
            //
            // Held rather than settled, because the action has not run yet
            // (the check at the top of the next iteration collects it), and
            // the text is written *before* the outcome is known. That is the
            // real limit of this knob: the reply can say what is being done
            // and never what came back. An answer that needs the result is
            // one the model must write on a later iteration.
            if self.cfg.act_and_answer {
                answer_with_action = emitted_answer.take();
            }

            // e3. M13 T4.1: and the other half of that pair — a line said
            // now, with the turn carrying on. "Let me check the database for
            // that product" is not an answer and silence is not a reply, and
            // this is the only branch that can be either.
            //
            // Sent *before* the action, because being told you are about to
            // wait is the whole value of it. Nothing can hold it back, so a
            // guard refusing the action afterwards leaves the user told about
            // a check that did not happen — the loop still goes round and
            // still owes them an answer, which is the honest trade for not
            // narrating after the fact.
            if let Some(text) = emitted_say.take() {
                // The channel, not the return value: the turn's one reply is
                // still to come, and `send` is documented for exactly this.
                if let Err(e) = self.parts.channel.send(&sid, &text).await {
                    eprintln!("said: {e}");
                }
                log.append(turn, now(), EventKind::Said { text });
                // Flushed like a side effect, because it is one: it has left
                // the machine, and a crash before the next flush must not
                // lose the record of what the user was told.
                self.flush(&sid, &log, n_loaded).await;
            }

            // f. legality
            if !legal.contains(&proposal.action) {
                // A misroute is not the model's mistake. If the action exists
                // and only the tier was hiding it, widen the tier and ask
                // again rather than recording a refusal: a refusal here would
                // teach the emitter that a real action is illegal, and the
                // narrowed schema would then keep it illegal for the rest of
                // the turn. One iteration is the honest price of a wrong
                // guess about the message (MemFlow's validator-retries, with
                // no second model). The tier only ever rises, so this cannot
                // loop.
                let registered = self
                    .parts
                    .tools
                    .iter()
                    .any(|t| t.spec().name == proposal.action);
                let tiered_out = tier < nscore::Tier::Task && registered;
                // M10 T2.2: escalation is adaptive depth's discovery path,
                // and the same argument as the tier's. The cue table is a
                // guess about the message; a proposal naming a real tool is
                // the model telling us the guess was wrong, and one widening
                // is cheaper than a turn that cannot reach the action at
                // all. It widens to the *full* set for the rest of the turn
                // rather than adding one name, because a task that needed
                // `pointer_scroll` needs whatever comes after it too — and
                // it is the one legitimate mid-turn change to the `tools`
                // array, so it happens once and never again.
                //
                // A tool already refused this turn is excluded: widening
                // would not make it legal, and the loop would spin.
                let withheld = registered
                    && !denied_this_turn.contains(&proposal.action)
                    && selected_tools
                        .as_ref()
                        .is_some_and(|sel| !sel.contains(&proposal.action));
                if tiered_out || withheld {
                    if tiered_out {
                        tier = nscore::Tier::Task;
                    }
                    if withheld {
                        selected_tools = None;
                        // Recorded, because an escalation is a request
                        // already spent and T0.2's `rejections by reason` is
                        // where that is read — the rate this depth is gated
                        // on (under 5 per 100 proposals) has to come from
                        // the log rather than from a counter nothing
                        // persists.
                        //
                        // Recorded but *not* denied: `denied_this_turn`
                        // would keep the tool illegal for the rest of the
                        // turn, which is exactly what the widening just
                        // undid. It differs from the tier's escalation
                        // (which records nothing) for one reason — the tier
                        // is bounded and self-announcing, while the cue
                        // table is a guess whose error rate is the number
                        // `depth = adaptive` ships on, and a guess nobody
                        // counts is a guess nobody can retire. The line
                        // reaches the emitter through the trace, and it is
                        // true: that proposal was refused on that
                        // iteration. It is left out of
                        // `rejections_this_turn` so it is said once.
                        log.append(
                            turn,
                            now(),
                            EventKind::Rejected {
                                proposal_of: pid,
                                reason: RejectReason::IllegalAction {
                                    action: proposal.action.clone(),
                                },
                            },
                        );
                    }
                    continue;
                }
                let reason = RejectReason::IllegalAction {
                    action: proposal.action.clone(),
                };
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason,
                    },
                );
                rejections_this_turn.push(format!("illegal action: {}", proposal.action));
                denied_this_turn.insert(proposal.action.clone());
                continue;
            }

            // f1. repeat gate (engine-owned): an identical (action, args) call
            // already executed this turn yields no new information. Seen live
            // with small models that ignore "never repeat a completed action"
            // in the prompt. Recorded as a guard denial so the narrowed schema
            // drops the action for the rest of the turn.
            // `inspect_result` is the exception, and not a weakening of the
            // gate: the gate's premise is that an identical call yields no
            // new information, and for a paging action that premise is
            // simply false — the same call is how the next page is asked
            // for. It cannot run away either: the pages end, and an
            // exhausted result leaves the schema.
            if proposal.action != INSPECT_RESULT
                && calls_this_turn.contains(&Self::call_key(&proposal))
            {
                let reason = format!(
                    "identical call to '{}' already executed this turn",
                    proposal.action
                );
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason: RejectReason::GuardDenied {
                            guard: "repeat_gate".into(),
                            reason: reason.clone(),
                        },
                    },
                );
                rejections_this_turn.push(format!("guard repeat_gate: {reason}"));
                denied_this_turn.insert(proposal.action.clone());
                continue;
            }

            // f3. confirmation: legality already guaranteed an ACTIVE pending
            // exists (confirm_pending is legal only then). Append the Confirmed
            // event and swap in the original staged proposal — it re-enters the
            // normal classify→guards→perform pipeline with the gate unlocked.
            if proposal.action == CONFIRM_PENDING {
                let pending_id = active_pending.expect("legality guaranteed an active pending");
                log.append(
                    turn,
                    now(),
                    EventKind::Confirmed {
                        pending: pending_id,
                    },
                );
                let original = log
                    .events()
                    .iter()
                    .find(|e| e.id == pending_id)
                    .and_then(|e| match &e.kind {
                        EventKind::PendingConfirmation { proposal_of, .. } => Some(*proposal_of),
                        _ => None,
                    })
                    .and_then(|orig_id| log.events().iter().find(|e| e.id == orig_id))
                    .and_then(|e| match &e.kind {
                        EventKind::Proposed { proposal } => Some(proposal.clone()),
                        _ => None,
                    });
                match original {
                    Some(orig) => {
                        proposal = orig;
                        confirmed_now = true;
                        // fall through to g with the staged proposal
                    }
                    None => {
                        log.append(
                            turn,
                            now(),
                            EventKind::Rejected {
                                proposal_of: pid,
                                reason: RejectReason::Malformed {
                                    detail: "pending confirmation chain is broken".into(),
                                },
                            },
                        );
                        rejections_this_turn.push("broken confirmation chain".into());
                        continue;
                    }
                }
            }

            // f2. clarification: the question IS the reply (spec §5.1). Runs
            // through classification and guards — TaintPolicy applies to
            // questions; a gated question is re-emitted, not asked.
            if proposal.action == ASK_CLARIFICATION {
                let question = proposal
                    .args
                    .get("question")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let Some(question) = question else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "ask_clarification without question".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("ask_clarification missing question".into());
                    continue;
                };
                let ask_spec = ask_clarification_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &ask_spec, turn);
                let classified = ClassifiedProposal {
                    proposal: proposal.clone(),
                    args: classified_args,
                };
                let guard_ctx = nscore::GuardCtx {
                    spec: &ask_spec,
                    turn,
                    confirmed_this_turn: state.confirmed_this_turn_of == Some(turn),
                    fired_actions: &state.fired_tags,
                    pending_confirmation: None,
                };
                let mut denied: Option<(String, String)> = None;
                for g in self.builtin_guards.iter().chain(self.parts.guards.iter()) {
                    match g.check(&classified, &guard_ctx) {
                        Verdict::Allow => continue,
                        Verdict::Deny { reason } => {
                            denied = Some((g.name().to_string(), reason));
                            break;
                        }
                        Verdict::NeedsConfirmation { prompt } => {
                            denied = Some((g.name().to_string(), prompt));
                            break;
                        }
                    }
                }
                if let Some((guard, reason)) = denied {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: guard.clone(),
                                reason: reason.clone(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard {guard}: {reason}"));
                    denied_this_turn.insert(ASK_CLARIFICATION.to_string());
                    continue;
                }
                let policy = ReplyPolicy::Verbatim { text: question };
                log.append(
                    turn,
                    now(),
                    EventKind::Settled {
                        policy: policy.clone(),
                    },
                );
                settled = Some(policy);
                break;
            }

            // f4. remember_fact: classify (the stored provenance IS the
            // classification of the value), write the fact, log the paper
            // trail, and let the emitter decide what happens next.
            if proposal.action == REMEMBER_FACT {
                let key = proposal
                    .args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let value = proposal
                    .args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let (Some(key), Some(value)) = (key, value) else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "remember_fact needs string key and value".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("remember_fact missing key/value".into());
                    continue;
                };
                // Keys are dotted identifiers (spec of the action). Normalize
                // stray edge punctuation first (seen live: a model reliably
                // emitting ":user.name" — same spirit as the trim normalizer),
                // then reject what remains degenerate (seen live: key ", ").
                let key = key
                    .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .to_string();
                let key_ok = !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c));
                if !key_ok || value.trim().is_empty() {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!(
                                    "remember_fact key must be a dotted identifier and value \
                                     non-empty (got key {key:?})"
                                ),
                            },
                        },
                    );
                    rejections_this_turn
                        .push(format!("remember_fact rejected malformed key {key:?}"));
                    continue;
                }
                let fact_spec = remember_fact_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &fact_spec, turn);
                let prov = classified_args
                    .iter()
                    .find(|(k, _)| k == "value")
                    .map(|(_, tv)| tv.prov.clone())
                    .unwrap_or(nscore::Provenance::Residual);
                // Lifecycle merge (M6 §6.1): a restatement keeps the usage
                // count and re-validates; the same value gains confidence,
                // a new value replaces it at full confidence. Seen live: every
                // re-remember reset `uses` to 0, erasing the consolidation
                // pass's only signal.
                let value_json = serde_json::json!(value);
                let value_trust = classified_args
                    .iter()
                    .find(|(k, _)| k == "value")
                    .map(|(_, tv)| tv.trust)
                    .unwrap_or(nscore::Trust::System);
                let residual = crate::guards::contains_residual(&prov);
                // M6 §6.3: a value nothing grounds is either flagged
                // (stored at half confidence, shown as unverified) or, for
                // deployments where facts drive side effects, refused.
                if residual && self.cfg.remember_residual == RememberResidual::Never {
                    let reason = "NeverResidual: arg 'value' has no grounding in this session";
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: "residual_policy".into(),
                                reason: reason.into(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard residual_policy: {reason}"));
                    never_residual_this_turn = true;
                    denied_this_turn.insert(REMEMBER_FACT.to_string());
                    continue;
                }
                let grounded_confidence = if residual { 0.5 } else { 1.0 };
                // Key canonicalization (M6 §6.1): a spelling variant of an
                // existing key is that key (seen live: memory_reset_requested
                // next to memory.reset.requested).
                let current = self
                    .parts
                    .memory
                    .facts(&scope, "")
                    .await
                    .unwrap_or_default();
                let key = match current.iter().find(|f| f.key == key) {
                    Some(_) => key,
                    None => current
                        .iter()
                        .find(|f| nscore::squash(&f.key) == nscore::squash(&key))
                        .map(|f| f.key.clone())
                        .unwrap_or(key),
                };
                let existing = current.into_iter().find(|f| f.key == key);
                // A new version must sort after the one it supersedes even
                // under a coarse clock.
                let version_at = |prev: &nscore::Fact| {
                    let t = now();
                    if t > prev.valid_from {
                        t
                    } else {
                        Timestamp(prev.valid_from.0 + 1)
                    }
                };
                let fact = match existing {
                    Some(prev) if prev.value == value_json => nscore::Fact {
                        confidence: if residual {
                            (prev.confidence + 0.1).min(1.0)
                        } else {
                            1.0
                        },
                        last_validated: now(),
                        prov,
                        trust: value_trust,
                        // a restated cold fact is current again (M6 §6.2)
                        state: nscore::FactState::Current,
                        ..prev
                    },
                    Some(prev) => nscore::Fact {
                        key: key.clone(),
                        value: value_json,
                        confidence: grounded_confidence,
                        uses: prev.uses,
                        last_validated: now(),
                        prov,
                        scope: scope.clone(),
                        trust: value_trust,
                        valid_from: version_at(&prev),
                        valid_to: None,
                        state: nscore::FactState::Current,
                        last_used: prev.last_used,
                        // M9 T4.1: a new *value* is a new version, and it has
                        // not been shown to anything yet. The counters stay
                        // with the version whose exposures earned them —
                        // inheriting them would credit "Peter" for the calls
                        // that showed "Martin". The restatement arm above
                        // keeps them, via `..prev`, because there the version
                        // is the same one.
                        exposures: 0,
                        credits: 0,
                    },
                    None => nscore::Fact {
                        key: key.clone(),
                        value: value_json,
                        confidence: grounded_confidence,
                        uses: 0,
                        last_validated: now(),
                        prov,
                        scope: scope.clone(),
                        trust: value_trust,
                        valid_from: now(),
                        ..Default::default()
                    },
                };
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: REMEMBER_FACT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.put_fact(fact).await {
                    Ok(()) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("remembered {key}"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // f7. recall (M6 §7): progressive disclosure. Verbatim turns
            // beyond the window first, then live facts; results become
            // CopiedOutput sources with the lowest trust among them.
            if proposal.action == RECALL {
                let query = proposal
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .map(String::from);
                let Some(query) = query else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "recall needs a non-empty query".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("recall missing query".into());
                    continue;
                };
                let spec = recall_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: RECALL.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = self.recall_outcome(&sid, &scope, &query, turn, tier).await;
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                continue;
            }

            // f8. inspect_result (M7 T1.2): the other half of the cap. The
            // whole result is in the log; this pages through it without
            // running the tool again, which on a desktop is neither free nor
            // guaranteed to return the same screen.
            if proposal.action == INSPECT_RESULT {
                let raw_id = proposal.args.get("id").and_then(|v| v.as_str());
                let query = proposal
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|q| !q.is_empty());
                let handle = raw_id.and_then(parse_result_handle);
                let available =
                    clipped_results(log.events(), turn, self.cfg.tool_result_max_chars);
                let Some(id) = handle.filter(|id| available.contains(id)) else {
                    let known: Vec<String> = available.iter().map(|i| result_handle(*i)).collect();
                    let detail = format!(
                        "no clipped result named {:?} this turn; available: {}",
                        raw_id.unwrap_or(""),
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    );
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!("{INSPECT_RESULT}: {detail}"),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("{INSPECT_RESULT}: {detail}"));
                    denied_this_turn.insert(INSPECT_RESULT.to_string());
                    continue;
                };
                let spec = inspect_result_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let page = inspect_page(log.events(), turn, id);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: INSPECT_RESULT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let text = result_text(log.events(), turn, id).unwrap_or_default();
                let total = text.chars().count();
                let (window, start, end) = result_window(&text, query, page, self.cfg.tool_result_max_chars);
                let outcome = if window.is_empty() {
                    // Either the query matched nothing or the pages ran out.
                    // Both are answers, and both mean asking again is a
                    // wasted iteration — so the action leaves the schema.
                    denied_this_turn.insert(INSPECT_RESULT.to_string());
                    ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: match query {
                                Some(q) => format!("{} has no match for {q:?}", result_handle(id)),
                                None => format!("no more of {}", result_handle(id)),
                            },
                            artifact: None,
                            trust: result_trust(log.events(), turn, id),
                        },
                    }
                } else {
                    ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!(
                                "{} chars {start}-{end} of {total}: {window}",
                                result_handle(id)
                            ),
                            artifact: None,
                            trust: result_trust(log.events(), turn, id),
                        },
                    }
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                continue;
            }

            // f5. forget_fact (M6 §6.2): soft-delete one current fact. An
            // unknown key is malformed so the emitter can retry or ask.
            if proposal.action == FORGET_FACT {
                let key = proposal
                    .args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .map(|k| {
                        k.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                            .to_string()
                    })
                    .filter(|k| !k.is_empty());
                let Some(key) = key else {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: "forget_fact needs a string key".into(),
                            },
                        },
                    );
                    rejections_this_turn.push("forget_fact missing key".into());
                    continue;
                };
                let current = self
                    .parts
                    .memory
                    .facts(&scope, "")
                    .await
                    .unwrap_or_default();
                let key = current
                    .iter()
                    .find(|f| f.key == key || nscore::squash(&f.key) == nscore::squash(&key))
                    .map(|f| f.key.clone())
                    .unwrap_or(key);
                if !current.iter().any(|f| f.key == key) {
                    let detail = format!("no current fact named {key}");
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::Malformed {
                                detail: format!("forget_fact: {detail}"),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("forget_fact: {detail}"));
                    // One miss may be a fixable key; a second one is a loop
                    // (seen live: the same wrong key three times).
                    forget_misses += 1;
                    if forget_misses >= 2 {
                        denied_this_turn.insert(FORGET_FACT.to_string());
                    }
                    continue;
                }
                let spec = forget_fact_spec(self.cfg.schema_profile);
                let classified_args = classify(log.events(), &proposal.args, &spec, turn);
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: FORGET_FACT.into(),
                            args: classified_args,
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.forget_fact(&scope, &key, now()).await {
                    Ok(_) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("forgot {key}"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // f6. forget_all (M6 §6.2): irreversible, so it is staged behind
            // the same two-turn confirmation as any irreversible tool, and
            // purges the scope once confirmed.
            if proposal.action == FORGET_ALL {
                let confirmed = confirmed_now || state.confirmed_this_turn_of == Some(turn);
                if !confirmed {
                    // No count in the prompt: replay runs from a fresh store
                    // and a Verbatim reply must be reproducible from the log.
                    let description =
                        format!("This will forget every stored fact in scope {scope}.");
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation {
                            proposal_of: pid,
                            staged: Some(nscore::StagedEffect {
                                description: description.clone(),
                            }),
                        },
                    );
                    let policy = ReplyPolicy::Verbatim {
                        text: format!(
                            "'{FORGET_ALL}' is irreversible. Confirm to proceed.\nPlanned: {description}"
                        ),
                    };
                    log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: policy.clone(),
                        },
                    );
                    settled = Some(policy);
                    break;
                }
                let call_id = log
                    .append(
                        turn,
                        now(),
                        EventKind::ToolCalled {
                            action: FORGET_ALL.into(),
                            args: vec![],
                        },
                    )
                    .id;
                calls_this_turn.insert(Self::call_key(&proposal));
                let outcome = match self.parts.memory.purge_facts(&scope).await {
                    Ok(n) => ToolOutcome::Ok {
                        output: nscore::ToolOutput {
                            summary: format!("forgot {n} facts"),
                            artifact: None,
                            trust: nscore::Trust::System,
                        },
                    },
                    Err(e) => ToolOutcome::Err {
                        kind: "store".into(),
                        detail: e.to_string(),
                    },
                };
                log.append(
                    turn,
                    now(),
                    EventKind::ToolReturned {
                        call: call_id,
                        outcome,
                    },
                );
                self.flush(&sid, &log, n_loaded).await;
                continue;
            }

            // g0. find the tool (legality guaranteed it exists)
            let tool = self
                .parts
                .tools
                .iter()
                .find(|t| t.spec().name == proposal.action)
                .expect("legality checked above")
                .clone();

            // g1. schema validation (spec M5 §3.2): malformed args are
            // rejected before classification; the action stays legal so the
            // emitter can retry with repaired args.
            if let Err(detail) = nscore::validate_args(&tool.spec().args_schema, &proposal.args) {
                log.append(
                    turn,
                    now(),
                    EventKind::Rejected {
                        proposal_of: pid,
                        reason: RejectReason::Malformed {
                            detail: format!("{}: {detail}", proposal.action),
                        },
                    },
                );
                rejections_this_turn
                    .push(format!("malformed args for {}: {detail}", proposal.action));
                continue;
            }

            // g. classify args against the session's history (spec §5.4)
            let classified_args = classify(log.events(), &proposal.args, tool.spec(), turn);
            let classified = ClassifiedProposal {
                proposal: proposal.clone(),
                args: classified_args.clone(),
            };

            // h. guards
            let guard_ctx = nscore::GuardCtx {
                spec: tool.spec(),
                turn,
                confirmed_this_turn: confirmed_now || state.confirmed_this_turn_of == Some(turn),
                fired_actions: &state.fired_tags,
                pending_confirmation: active_pending,
            };
            let mut verdict = Verdict::Allow;
            let mut guard_name = String::new();
            for g in self.builtin_guards.iter().chain(self.parts.guards.iter()) {
                match g.check(&classified, &guard_ctx) {
                    Verdict::Allow => continue,
                    v => {
                        guard_name = g.name().to_string();
                        verdict = v;
                        break;
                    }
                }
            }
            match verdict {
                Verdict::Allow => {}
                Verdict::Deny { reason } => {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: pid,
                            reason: RejectReason::GuardDenied {
                                guard: guard_name.clone(),
                                reason: reason.clone(),
                            },
                        },
                    );
                    rejections_this_turn.push(format!("guard {guard_name}: {reason}"));
                    if reason.contains("NeverResidual") {
                        never_residual_this_turn = true;
                    }
                    denied_this_turn.insert(proposal.action.clone());
                    continue;
                }
                Verdict::NeedsConfirmation { prompt } => {
                    // Dry-run when the tool supports it; show the user what
                    // would happen (spec §5.4, §9).
                    let staged = tool
                        .stage(
                            &proposal.args,
                            &ToolCtx {
                                session: sid.clone(),
                                artifacts: Some(self.parts.memory.clone()),
                            },
                        )
                        .await;
                    let mut text = prompt;
                    if let Some(s) = &staged {
                        text.push_str(&format!("\nPlanned: {}", s.description));
                    }
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation {
                            proposal_of: pid,
                            staged,
                        },
                    );
                    let policy = ReplyPolicy::Verbatim { text };
                    log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: policy.clone(),
                        },
                    );
                    settled = Some(policy);
                    break;
                }
            }

            // i. perform
            let call_id = log
                .append(
                    turn,
                    now(),
                    EventKind::ToolCalled {
                        action: proposal.action.clone(),
                        args: classified_args,
                    },
                )
                .id;
            calls_this_turn.insert(Self::call_key(&proposal));
            let outcome = match tool
                .call(
                    &proposal.args,
                    &ToolCtx {
                        session: sid.clone(),
                        artifacts: Some(self.parts.memory.clone()),
                    },
                )
                .await
            {
                Ok(output) => ToolOutcome::Ok { output },
                Err(nscore::ToolError::Failed { kind, detail }) => {
                    ToolOutcome::Err { kind, detail }
                }
            };
            log.append(
                turn,
                now(),
                EventKind::ToolReturned {
                    call: call_id,
                    outcome,
                },
            );
            if tool.spec().side_effect != nscore::SideEffect::Pure {
                self.flush(&sid, &log, n_loaded).await;
            }
            // loop: the emitter decides what happens next (typically respond_directly)
        }

        // M13 T3.1: the iteration budget ran out holding an answer written
        // beside the last action, and that action ran. It is a reply to a
        // turn that did what it said; the fallback below would throw it away
        // and tell the user the loop ran out of steps.
        if settled.is_none() {
            if let Some(text) = answer_with_action.take() {
                if last_proposal_ran(log.events(), turn) {
                    pre_draft = Some(text);
                    let e = log.append(
                        turn,
                        now(),
                        EventKind::Settled {
                            policy: ReplyPolicy::Generate,
                        },
                    );
                    settled = Some(match &e.kind {
                        EventKind::Settled { policy } => policy.clone(),
                        _ => unreachable!(),
                    });
                }
            }
        }

        // 3. fallback settle (a registered cant_help template wins). The
        // reply names the cause — the user shouldn't need the event log to
        // learn it was a rate limit rather than a refusal.
        let policy = settled.unwrap_or_else(|| {
            // The test is "did the emitter ever answer", not "was the retry
            // budget spent". A terminal provider status breaks out after one
            // attempt (phase 5), and calling that "ran out of steps after 5
            // actions" would blame the loop for an endpoint that was down.
            let reason = if emit_failures > 0 && !proposed_this_turn {
                last_emit_error
                    .as_deref()
                    .map(explain_error)
                    .unwrap_or_else(|| "the model was unavailable".into())
            } else {
                let mut r = format!(
                    "ran out of steps after {} actions without reaching an answer",
                    self.cfg.max_iterations
                );
                if let Some(last) = rejections_this_turn.last() {
                    r.push_str(&format!("; last problem: {last}"));
                }
                r
            };
            let p = if self.cfg.templates.contains_key("cant_help") {
                ReplyPolicy::Template {
                    id: "cant_help".into(),
                    vars: serde_json::json!({ "reason": reason }),
                }
            } else {
                ReplyPolicy::Verbatim {
                    text: format!("{FALLBACK_REPLY} Reason: {reason}."),
                }
            };
            log.append(turn, now(), EventKind::Settled { policy: p.clone() });
            p
        });

        // 4. reply
        let text = match policy {
            ReplyPolicy::Verbatim { text } => text,
            ReplyPolicy::Template { id, vars } => match self.cfg.templates.get(&id) {
                Some(template) => render_template(template, &vars),
                None => format!("[{id}] {vars}"),
            },
            ReplyPolicy::Generate => {
                self.generate_reply(
                    &scope,
                    &incoming.text,
                    &rules,
                    &mut log,
                    turn,
                    &usage,
                    self.cfg.recall_hybrid && tier != nscore::Tier::Chat,
                    pre_draft,
                )
                .await
            }
        };

        // 5. record + persist new events only
        log.append(turn, now(), EventKind::Replied { text: text.clone() });
        self.parts
            .memory
            .append(&sid, &log.events()[n_loaded..])
            .await?;
        Ok(text)
    }
    /// Runs the engine on its channel until the channel closes: every
    /// message goes to its session's mailbox, a session runs one turn at a
    /// time, `worker_slots` sessions run at once, a quiet channel runs the
    /// consolidator (driver B, spec M5 §5), and the rolling summary (M6
    /// §5.1) runs while its session waits for the next message. The loop is
    /// `dispatch::Dispatcher`; this builds it, so the CLI and the tests keep
    /// the one call they had.
    pub async fn run(self) -> Result<(), EngineError> {
        let channel = self.parts.channel.clone();
        let slots = self.cfg.worker_slots;
        crate::dispatch::Dispatcher::new(std::sync::Arc::new(self), channel, slots)
            .run()
            .await
    }
}
