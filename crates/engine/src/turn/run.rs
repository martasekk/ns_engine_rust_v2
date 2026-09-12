//! The turn loop.
//!
//! One pass of this loop is one model decision: project the log, build the
//! emitter context, ask for a proposal, and either settle the turn or run
//! the action and go round again. The steps are lettered in the order they
//! happen, and the phases they call into live in this module's siblings.

use super::accounting::last_proposal_ran;
use super::builtins::{Bookkeeping, Ctx, Step};
use super::diagnostics::explain_error;
use super::gate::classify;
use super::reply::{render_template, FALLBACK_REPLY};
use super::specs::*;
use super::{Engine, EngineError};
use crate::state::fold;
use crate::trace::{
    clipped_results, emitter_manifest, result_handle, trace_for_prompt,
};
use nscore::{
    EventKind, EventLog, Incoming, LegalActionSet, RejectReason, ReplyPolicy,
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

        // What this turn accumulates as it goes: the refusals the emitter is
        // shown, the actions it may no longer propose, the calls that really
        // ran (see `builtins::Bookkeeping`).
        let mut book = Bookkeeping::default();
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
            let legal = if book.never_residual {
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
                        .filter(|s| !book.denied.contains(&s.name))
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
                if !book.denied.contains(REMEMBER_FACT) {
                    actions.push(remember_fact_spec(self.cfg.schema_profile));
                }
                if !book.denied.contains(RECALL) && recall_applies {
                    actions.push(recall_spec(self.cfg.schema_profile));
                }
                // Offered only while there is something to inspect. An
                // action in the schema that can only fail is a way for a
                // small model to spend an iteration discovering that.
                if !book.denied.contains(INSPECT_RESULT)
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
                if !book.wrote_fact() && !book.forgot() && scope_holds_facts {
                    if !book.denied.contains(FORGET_FACT) {
                        actions.push(forget_fact_spec(self.cfg.schema_profile));
                    }
                    if !book.denied.contains(FORGET_ALL) {
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
                rejections_this_turn: book.rejections.clone(),
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
                    book.rejections.push(match &e {
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
                    && !book.denied.contains(&proposal.action)
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
                        // Recorded but *not* denied: the denied set
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
                        // the rejection lines so it is said once.
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
                book.rejections.push(format!("illegal action: {}", proposal.action));
                book.denied.insert(proposal.action.clone());
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
                && book.calls.contains(&Self::call_key(&proposal))
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
                book.rejections.push(format!("guard repeat_gate: {reason}"));
                book.denied.insert(proposal.action.clone());
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
                        book.rejections.push("broken confirmation chain".into());
                        continue;
                    }
                }
            }

            // f2-i. run it.
            //
            // Two kinds of action and one shape: the engine's own
            // (`builtins.rs`) and the deployment's (`tools.rs`). Either
            // settles the turn or leaves the emitter another iteration to
            // decide what follows, which is the only thing the loop needs to
            // know about what just happened.
            let mut cx = Ctx {
                log: &mut log,
                book: &mut book,
                state: &state,
                proposal: &proposal,
                pid,
                turn,
                sid: &sid,
                scope: &scope,
                tier,
                n_loaded,
                confirmed_now,
                active_pending,
                at: now(),
            };
            let step = match self.run_builtin(&mut cx).await {
                Some(step) => step,
                None => self.run_tool(&mut cx).await,
            };
            match step {
                Step::Again => continue,
                Step::Settled(policy) => {
                    settled = Some(policy);
                    break;
                }
            }
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
                if let Some(last) = book.rejections.last() {
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
