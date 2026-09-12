//! The actions the engine runs itself.
//!
//! Six handlers that used to sit inline in the turn loop, one `if
//! proposal.action ==` after another, six hundred lines between the
//! iteration's first statement and its last. They are here because each is a
//! complete answer to "what happens when the model asks for this", and the
//! loop's business with them is only which one to call.
//!
//! Adding a seventh is now three edits in this crate rather than five
//! scattered through one function: its schema in [`super::specs`], its entry
//! in [`Engine::run_builtin`], and the handler itself.

use super::config::RememberResidual;
use super::gate::classify;
use super::specs::*;
use super::Engine;
use crate::state::SessionState;
use crate::trace::{
    clipped_results, inspect_page, parse_result_handle, result_handle, result_text, result_trust,
    result_window,
};
use nscore::{
    ClassifiedProposal, EventKind, EventLog, RejectReason, ReplyPolicy, Timestamp, ToolOutcome,
    Verdict,
};

/// What the turn loop does next, once a handler has had its say.
///
/// The handlers used to say this with `continue` and `break`, which only
/// reads as an answer while the code is physically inside the loop. Naming
/// the two outcomes is what let them move out of it.
pub(super) enum Step {
    /// The action ran, or was refused and the refusal logged. Either way the
    /// emitter gets another iteration to decide what follows.
    Again,
    /// The turn is over and this is the reply policy it ended on.
    Settled(ReplyPolicy),
}

/// The bookkeeping one turn accumulates as it goes.
///
/// These were five separate locals in `run_turn`, and every handler took all
/// five. Gathering them means a handler's signature says what it may change,
/// and the three-statement "log the refusal, tell the emitter, narrow the
/// schema" dance below is written once instead of sixteen times.
#[derive(Default)]
pub(super) struct Bookkeeping {
    /// Why this turn's earlier proposals did not run, as the emitter will be
    /// shown them on the next iteration.
    pub rejections: Vec<String>,
    /// Actions dropped from the legal set for the rest of the turn.
    pub denied: std::collections::HashSet<String>,
    /// `(action, args)` pairs that actually executed, for the repeat gate.
    pub calls: std::collections::HashSet<String>,
    /// A `NeverResidual` refusal happened and nothing grounds the argument,
    /// so the only legal move left is to ask (spec §5.1).
    pub never_residual: bool,
    /// `forget_fact` calls that named no stored key. One may be a fixable
    /// spelling; a second is a loop.
    pub forget_misses: u32,
}

impl Bookkeeping {
    /// Record a refusal: in the log for the replay, in the rejection lines
    /// for the emitter's next prompt.
    pub fn reject(
        &mut self,
        log: &mut EventLog,
        turn: u32,
        at: Timestamp,
        pid: nscore::EventId,
        reason: RejectReason,
        line: impl Into<String>,
    ) {
        log.append(
            turn,
            at,
            EventKind::Rejected {
                proposal_of: pid,
                reason,
            },
        );
        self.rejections.push(line.into());
    }

    /// Did this turn already write a fact? Forgetting after remembering is
    /// how "my name is now Peter" ended in a staged `forget_all`.
    pub fn wrote_fact(&self) -> bool {
        let prefix = format!("{REMEMBER_FACT}\u{0}");
        self.calls.iter().any(|k| k.starts_with(&prefix))
    }

    /// Did this turn already forget something? Twice is never meaningful.
    pub fn forgot(&self) -> bool {
        self.calls.iter().any(|k| k.starts_with("forget_"))
    }
}

/// Everything a handler may read or change, and nothing else.
///
/// Passed as one object rather than eleven arguments, which is also the
/// honest description of the situation: these are the turn, and a handler is
/// one thing that may happen during it.
pub(super) struct Ctx<'a> {
    pub log: &'a mut EventLog,
    pub book: &'a mut Bookkeeping,
    /// The projection the loop took at the top of this iteration.
    pub state: &'a SessionState,
    pub proposal: &'a nscore::Proposal,
    /// The `Proposed` event this handler is answering, for `proposal_of`.
    pub pid: nscore::EventId,
    pub turn: u32,
    pub sid: &'a nscore::SessionId,
    pub scope: &'a str,
    pub tier: nscore::Tier,
    /// Where the store's copy of the log ended when the turn began, so a
    /// side effect can be flushed without re-appending what is already there.
    pub n_loaded: usize,
    /// Whether `confirm_pending` unlocked the gate on this iteration.
    pub confirmed_now: bool,
    pub at: Timestamp,
}

impl Ctx<'_> {
    /// This iteration's timestamp. One reading per iteration, so every event
    /// a handler appends carries the same instant the proposal did.
    pub fn now(&self) -> Timestamp {
        self.at
    }

    /// Refuse the proposal and go round again.
    fn refuse(&mut self, reason: RejectReason, line: impl Into<String>) -> Step {
        let at = self.at;
        self.book
            .reject(self.log, self.turn, at, self.pid, reason, line);
        Step::Again
    }

    /// Refuse it as malformed, which is the emitter's cue to fix the
    /// arguments or ask the user rather than to stop offering the action.
    fn malformed(&mut self, detail: impl Into<String>, line: impl Into<String>) -> Step {
        self.refuse(
            RejectReason::Malformed {
                detail: detail.into(),
            },
            line,
        )
    }

    /// Settle the turn on a policy, recording it the way the loop does.
    fn settle(&mut self, policy: ReplyPolicy) -> Step {
        let at = self.at;
        self.log.append(
            self.turn,
            at,
            EventKind::Settled {
                policy: policy.clone(),
            },
        );
        Step::Settled(policy)
    }

    /// Record that an action really ran, and hand back the call's id so its
    /// outcome can be logged against it.
    fn called(
        &mut self,
        action: &str,
        args: Vec<(String, nscore::TaggedValue)>,
    ) -> nscore::EventId {
        let at = self.at;
        let id = self
            .log
            .append(
                self.turn,
                at,
                EventKind::ToolCalled {
                    action: action.to_string(),
                    args,
                },
            )
            .id;
        self.book.calls.insert(Engine::call_key(self.proposal));
        id
    }

    /// Record what the call returned.
    fn returned(&mut self, call: nscore::EventId, outcome: ToolOutcome) {
        let at = self.at;
        self.log
            .append(self.turn, at, EventKind::ToolReturned { call, outcome });
    }
}

impl Engine {
    /// Dispatch to the handler for an engine-owned action, or `None` when the
    /// proposal names a registered tool and the loop should run it itself.
    ///
    /// The one place that knows which actions the engine answers. A seventh
    /// builtin is a line here and a function below.
    pub(super) async fn run_builtin(&self, cx: &mut Ctx<'_>) -> Option<Step> {
        let step = match cx.proposal.action.as_str() {
            ASK_CLARIFICATION => self.ask_clarification(cx).await,
            REMEMBER_FACT => self.remember_fact(cx).await,
            RECALL => self.recall(cx).await,
            INSPECT_RESULT => self.inspect_result(cx).await,
            FORGET_FACT => self.forget_fact(cx).await,
            FORGET_ALL => self.forget_all(cx).await,
            _ => return None,
        };
        Some(step)
    }

    // f2. clarification: the question IS the reply (spec §5.1). Runs
    // through classification and guards — TaintPolicy applies to
    // questions; a gated question is re-emitted, not asked.
    async fn ask_clarification(&self, cx: &mut Ctx<'_>) -> Step {
        let question = cx
            .proposal
            .args
            .get("question")
            .and_then(|v| v.as_str())
            .map(String::from);
        let Some(question) = question else {
            return cx.malformed(
                "ask_clarification without question",
                "ask_clarification missing question",
            );
        };
        let ask_spec = ask_clarification_spec(self.cfg.schema_profile);
        let classified_args = classify(cx.log.events(), &cx.proposal.args, &ask_spec, cx.turn);
        let classified = ClassifiedProposal {
            proposal: cx.proposal.clone(),
            args: classified_args,
        };
        let guard_ctx = nscore::GuardCtx {
            spec: &ask_spec,
            turn: cx.turn,
            confirmed_this_turn: cx.state.confirmed_this_turn_of == Some(cx.turn),
            fired_actions: &cx.state.fired_tags,
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
            cx.log.append(
                cx.turn,
                cx.now(),
                EventKind::Rejected {
                    proposal_of: cx.pid,
                    reason: RejectReason::GuardDenied {
                        guard: guard.clone(),
                        reason: reason.clone(),
                    },
                },
            );
            cx.book.rejections.push(format!("guard {guard}: {reason}"));
            cx.book.denied.insert(ASK_CLARIFICATION.to_string());
            return Step::Again;
        }
        let policy = ReplyPolicy::Verbatim { text: question };
        return cx.settle(policy);
    }

    // f4. remember_fact: classify (the stored provenance IS the
    // classification of the value), write the fact, log the paper
    // trail, and let the emitter decide what happens next.
    async fn remember_fact(&self, cx: &mut Ctx<'_>) -> Step {
        let key = cx
            .proposal
            .args
            .get("key")
            .and_then(|v| v.as_str())
            .map(String::from);
        let value = cx
            .proposal
            .args
            .get("value")
            .and_then(|v| v.as_str())
            .map(String::from);
        let (Some(key), Some(value)) = (key, value) else {
            return cx.malformed(
                "remember_fact needs string key and value",
                "remember_fact missing key/value",
            );
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
            return cx.malformed(
                format!(
                    "remember_fact key must be a dotted identifier and value \
                                 non-empty (got key {key:?})"
                ),
                format!("remember_fact rejected malformed key {key:?}"),
            );
        }
        let fact_spec = remember_fact_spec(self.cfg.schema_profile);
        let classified_args = classify(cx.log.events(), &cx.proposal.args, &fact_spec, cx.turn);
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
            cx.log.append(
                cx.turn,
                cx.now(),
                EventKind::Rejected {
                    proposal_of: cx.pid,
                    reason: RejectReason::GuardDenied {
                        guard: "residual_policy".into(),
                        reason: reason.into(),
                    },
                },
            );
            cx.book
                .rejections
                .push(format!("guard residual_policy: {reason}"));
            cx.book.never_residual = true;
            cx.book.denied.insert(REMEMBER_FACT.to_string());
            return Step::Again;
        }
        let grounded_confidence = if residual { 0.5 } else { 1.0 };
        // Key canonicalization (M6 §6.1): a spelling variant of an
        // existing key is that key (seen live: memory_reset_requested
        // next to memory.reset.requested).
        let current = self
            .parts
            .memory
            .facts(cx.scope, "")
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
            let t = cx.now();
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
                last_validated: cx.now(),
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
                last_validated: cx.now(),
                prov,
                scope: cx.scope.to_string(),
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
                last_validated: cx.now(),
                prov,
                scope: cx.scope.to_string(),
                trust: value_trust,
                valid_from: cx.now(),
                ..Default::default()
            },
        };
        let call_id = cx.called(REMEMBER_FACT, classified_args);
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
        cx.returned(call_id, outcome);
        self.flush(cx.sid, cx.log, cx.n_loaded).await;
        return Step::Again;
    }

    // f7. recall (M6 §7): progressive disclosure. Verbatim turns
    // beyond the window first, then live facts; results become
    // CopiedOutput sources with the lowest trust among them.
    async fn recall(&self, cx: &mut Ctx<'_>) -> Step {
        let query = cx
            .proposal
            .args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(String::from);
        let Some(query) = query else {
            return cx.malformed("recall needs a non-empty query", "recall missing query");
        };
        let spec = recall_spec(self.cfg.schema_profile);
        let classified_args = classify(cx.log.events(), &cx.proposal.args, &spec, cx.turn);
        let call_id = cx.called(RECALL, classified_args);
        let outcome = self
            .recall_outcome(cx.sid, cx.scope, &query, cx.turn, cx.tier)
            .await;
        cx.returned(call_id, outcome);
        return Step::Again;
    }

    // f8. inspect_result (M7 T1.2): the other half of the cap. The
    // whole result is in the log; this pages through it without
    // running the tool again, which on a desktop is neither free nor
    // guaranteed to return the same screen.
    async fn inspect_result(&self, cx: &mut Ctx<'_>) -> Step {
        let raw_id = cx.proposal.args.get("id").and_then(|v| v.as_str());
        let query = cx
            .proposal
            .args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let handle = raw_id.and_then(parse_result_handle);
        let available = clipped_results(cx.log.events(), cx.turn, self.cfg.tool_result_max_chars);
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
            cx.book.denied.insert(INSPECT_RESULT.to_string());
            return cx.malformed(
                format!("{INSPECT_RESULT}: {detail}"),
                format!("{INSPECT_RESULT}: {detail}"),
            );
        };
        let spec = inspect_result_spec(self.cfg.schema_profile);
        let classified_args = classify(cx.log.events(), &cx.proposal.args, &spec, cx.turn);
        let page = inspect_page(cx.log.events(), cx.turn, id);
        let call_id = cx.called(INSPECT_RESULT, classified_args);
        let text = result_text(cx.log.events(), cx.turn, id).unwrap_or_default();
        let total = text.chars().count();
        let (window, start, end) =
            result_window(&text, query, page, self.cfg.tool_result_max_chars);
        let outcome = if window.is_empty() {
            // Either the query matched nothing or the pages ran out.
            // Both are answers, and both mean asking again is a
            // wasted iteration — so the action leaves the schema.
            cx.book.denied.insert(INSPECT_RESULT.to_string());
            ToolOutcome::Ok {
                output: nscore::ToolOutput {
                    summary: match query {
                        Some(q) => format!("{} has no match for {q:?}", result_handle(id)),
                        None => format!("no more of {}", result_handle(id)),
                    },
                    artifact: None,
                    trust: result_trust(cx.log.events(), cx.turn, id),
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
                    trust: result_trust(cx.log.events(), cx.turn, id),
                },
            }
        };
        cx.returned(call_id, outcome);
        return Step::Again;
    }

    // f5. forget_fact (M6 §6.2): soft-delete one current fact. An
    // unknown key is malformed so the emitter can retry or ask.
    async fn forget_fact(&self, cx: &mut Ctx<'_>) -> Step {
        let key = cx
            .proposal
            .args
            .get("key")
            .and_then(|v| v.as_str())
            .map(|k| {
                k.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .to_string()
            })
            .filter(|k| !k.is_empty());
        let Some(key) = key else {
            return cx.malformed("forget_fact needs a string key", "forget_fact missing key");
        };
        let current = self
            .parts
            .memory
            .facts(cx.scope, "")
            .await
            .unwrap_or_default();
        let key = current
            .iter()
            .find(|f| f.key == key || nscore::squash(&f.key) == nscore::squash(&key))
            .map(|f| f.key.clone())
            .unwrap_or(key);
        if !current.iter().any(|f| f.key == key) {
            let detail = format!("no current fact named {key}");
            cx.log.append(
                cx.turn,
                cx.now(),
                EventKind::Rejected {
                    proposal_of: cx.pid,
                    reason: RejectReason::Malformed {
                        detail: format!("forget_fact: {detail}"),
                    },
                },
            );
            cx.book.rejections.push(format!("forget_fact: {detail}"));
            // One miss may be a fixable key; a second one is a loop
            // (seen live: the same wrong key three times).
            cx.book.forget_misses += 1;
            if cx.book.forget_misses >= 2 {
                cx.book.denied.insert(FORGET_FACT.to_string());
            }
            return Step::Again;
        }
        let spec = forget_fact_spec(self.cfg.schema_profile);
        let classified_args = classify(cx.log.events(), &cx.proposal.args, &spec, cx.turn);
        let call_id = cx.called(FORGET_FACT, classified_args);
        let outcome = match self
            .parts
            .memory
            .forget_fact(cx.scope, &key, cx.now())
            .await
        {
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
        cx.returned(call_id, outcome);
        self.flush(cx.sid, cx.log, cx.n_loaded).await;
        return Step::Again;
    }

    // f6. forget_all (M6 §6.2): irreversible, so it is staged behind
    // the same two-turn confirmation as any irreversible tool, and
    // purges the scope once confirmed.
    async fn forget_all(&self, cx: &mut Ctx<'_>) -> Step {
        let confirmed = cx.confirmed_now || cx.state.confirmed_this_turn_of == Some(cx.turn);
        if !confirmed {
            // No count in the prompt: replay runs from a fresh store
            // and a Verbatim reply must be reproducible from the log.
            let description = format!("This will forget every stored fact in scope {}.", cx.scope);
            cx.log.append(
                cx.turn,
                cx.now(),
                EventKind::PendingConfirmation {
                    proposal_of: cx.pid,
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
            return cx.settle(policy);
        }
        let call_id = cx.called(FORGET_ALL, vec![]);
        let outcome = match self.parts.memory.purge_facts(cx.scope).await {
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
        cx.returned(call_id, outcome);
        self.flush(cx.sid, cx.log, cx.n_loaded).await;
        return Step::Again;
    }
}
