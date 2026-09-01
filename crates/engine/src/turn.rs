use crate::state::{fold, state_summary};
use nscore::{
    ChannelError, ClassifiedProposal, EventKind, EventLog, HarnessParts, Incoming,
    LegalActionSet, RejectReason, ReplyContext, ReplyPolicy, Timestamp, ToolCtx, ToolOutcome,
    Verdict,
};

pub struct EngineConfig {
    pub max_iterations: u32,
    pub max_emit_retries: u32,
    pub persona: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { max_iterations: 5, max_emit_retries: 3, persona: String::new() }
    }
}

pub struct Engine {
    parts: HarnessParts,
    cfg: EngineConfig,
    clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
    /// Always-on guard chain, checked before plugin guards. Plugins cannot
    /// remove these (spec §5.4).
    builtin_guards: Vec<Box<dyn nscore::Guard>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("store: {0}")]
    Store(#[from] nscore::StoreError),
    #[error("channel: {0}")]
    Channel(String),
}

const FALLBACK_REPLY: &str = "Sorry, I couldn't complete that.";

/// Engine-owned synthetic action: ask the user one question (spec §5.1).
pub const ASK_CLARIFICATION: &str = "ask_clarification";

fn ask_clarification_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: ASK_CLARIFICATION.into(),
        description: "Ask the user one short question to resolve missing or ungrounded \
                      information required by the next action."
            .into(),
        args_schema: serde_json::json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        }),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

/// Engine-owned synthetic action: the user just confirmed the staged action.
pub const CONFIRM_PENDING: &str = "confirm_pending";

fn confirm_pending_spec() -> nscore::ActionSpec {
    nscore::ActionSpec {
        name: CONFIRM_PENDING.into(),
        description: "The user has just confirmed the pending action; execute it.".into(),
        args_schema: serde_json::json!({"type": "object", "properties": {}}),
        side_effect: nscore::SideEffect::Pure,
        residual_policy: Default::default(),
        dedupe_tag: None,
    }
}

impl Engine {
    pub fn new(parts: HarnessParts, cfg: EngineConfig) -> Self {
        Self::with_clock(
            parts,
            cfg,
            Box::new(|| {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Timestamp(ms)
            }),
        )
    }

    pub fn with_clock(
        parts: HarnessParts,
        cfg: EngineConfig,
        clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> Self {
        Self {
            parts,
            cfg,
            clock,
            builtin_guards: vec![
                Box::new(crate::guards::ResidualPolicy),
                Box::new(crate::guards::TaintPolicy),
                Box::new(crate::guards::DedupeGate),
                Box::new(crate::guards::SideEffectGate),
            ],
        }
    }

    /// Full turn: load log, run pipeline, persist NEW events, return reply text.
    pub async fn run_turn(&mut self, incoming: Incoming) -> Result<String, EngineError> {
        let sid = incoming.session.clone();
        let stored = self.parts.memory.load(&sid).await?;
        let n_loaded = stored.len();
        let mut log = EventLog::from_events(sid.clone(), stored);

        let turn = fold(log.events()).turn + 1;
        let now = &self.clock;
        log.append(turn, now(), EventKind::UserSaid { text: incoming.text.clone() });

        let mut rejections_this_turn: Vec<String> = Vec::new();
        let mut denied_this_turn: std::collections::HashSet<String> = Default::default();
        let mut never_residual_this_turn = false;
        let mut emit_failures: u32 = 0;
        let mut settled: Option<ReplyPolicy> = None;

        for _ in 0..self.cfg.max_iterations {
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
                LegalActionSet { actions: vec![ask_clarification_spec()] }
            } else {
                // Narrowed schema (spec §2): actions rejected this turn are
                // removed from the set the emitter sees next.
                let mut actions: Vec<_> = self
                    .parts
                    .tools
                    .iter()
                    .map(|t| t.spec().clone())
                    .filter(|s| !denied_this_turn.contains(&s.name))
                    .collect();
                actions.push(ask_clarification_spec());
                if active_pending.is_some() {
                    actions.push(confirm_pending_spec());
                }
                LegalActionSet { actions }
            };

            // b. emitter context
            let recent: Vec<(String, String)> = state
                .history
                .iter()
                .rev()
                .take(6)
                .rev()
                .cloned()
                .collect();
            // The emitter must see what this turn has already done — otherwise
            // it re-proposes completed actions until max_iterations exhausts.
            let mut summary = state_summary(&state);
            let trace_so_far = turn_trace(&log, turn);
            if !trace_so_far.is_empty() {
                summary.push_str("\nThis turn so far:\n");
                summary.push_str(&trace_so_far);
            }
            if active_pending.is_some() {
                summary.push_str(
                    "\nPending confirmation: awaiting the user's yes/no on the staged action.",
                );
            }
            let ctx = nscore::EmitterContext {
                state_summary: summary,
                recent_turns: recent,
                rejections_this_turn: rejections_this_turn.clone(),
            };

            // c. propose
            let mut confirmed_now = false;
            let mut proposal = match self.parts.emitter.propose(ctx, &legal).await {
                Ok(p) => p,
                Err(e) => {
                    log.append(
                        turn,
                        now(),
                        EventKind::Rejected {
                            proposal_of: nscore::EventId(0),
                            reason: RejectReason::Malformed { detail: e.to_string() },
                        },
                    );
                    rejections_this_turn.push(format!("emitter failure: {e}"));
                    emit_failures += 1;
                    if emit_failures >= self.cfg.max_emit_retries {
                        break;
                    }
                    continue;
                }
            };

            // d. record proposal
            let pid = log
                .append(turn, now(), EventKind::Proposed { proposal: proposal.clone() })
                .id;

            // e. direct reply
            if proposal.action == "respond_directly" {
                let e = log.append(turn, now(), EventKind::Settled { policy: ReplyPolicy::Generate });
                settled = Some(match &e.kind {
                    EventKind::Settled { policy } => policy.clone(),
                    _ => unreachable!(),
                });
                break;
            }

            // f. legality
            if !legal.contains(&proposal.action) {
                let reason = RejectReason::IllegalAction { action: proposal.action.clone() };
                log.append(turn, now(), EventKind::Rejected { proposal_of: pid, reason });
                rejections_this_turn.push(format!("illegal action: {}", proposal.action));
                denied_this_turn.insert(proposal.action.clone());
                continue;
            }

            // f3. confirmation: legality already guaranteed an ACTIVE pending
            // exists (confirm_pending is legal only then). Append the Confirmed
            // event and swap in the original staged proposal — it re-enters the
            // normal classify→guards→perform pipeline with the gate unlocked.
            if proposal.action == CONFIRM_PENDING {
                let pending_id = active_pending.expect("legality guaranteed an active pending");
                log.append(turn, now(), EventKind::Confirmed { pending: pending_id });
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
                let question =
                    proposal.args.get("question").and_then(|v| v.as_str()).map(String::from);
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
                let ask_spec = ask_clarification_spec();
                let index = nsprovenance::index::ValueIndex::from_events(log.events());
                let classified_args = nsprovenance::classify::classify_args(
                    &proposal.args,
                    &ask_spec,
                    &index,
                    turn,
                );
                let classified =
                    ClassifiedProposal { proposal: proposal.clone(), args: classified_args };
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
                log.append(turn, now(), EventKind::Settled { policy: policy.clone() });
                settled = Some(policy);
                break;
            }

            // g. classify args against the session's history (spec §5.4)
            let tool = self
                .parts
                .tools
                .iter()
                .find(|t| t.spec().name == proposal.action)
                .expect("legality checked above")
                .clone();
            let index = nsprovenance::index::ValueIndex::from_events(log.events());
            let classified_args =
                nsprovenance::classify::classify_args(&proposal.args, tool.spec(), &index, turn);
            let classified =
                ClassifiedProposal { proposal: proposal.clone(), args: classified_args.clone() };

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
                    let staged =
                        tool.stage(&proposal.args, &ToolCtx { session: sid.clone() }).await;
                    let mut text = prompt;
                    if let Some(s) = &staged {
                        text.push_str(&format!("\nPlanned: {}", s.description));
                    }
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation { proposal_of: pid, staged },
                    );
                    let policy = ReplyPolicy::Verbatim { text };
                    log.append(turn, now(), EventKind::Settled { policy: policy.clone() });
                    settled = Some(policy);
                    break;
                }
            }

            // i. perform
            let call_id = log
                .append(
                    turn,
                    now(),
                    EventKind::ToolCalled { action: proposal.action.clone(), args: classified_args },
                )
                .id;
            let outcome = match tool.call(&proposal.args, &ToolCtx { session: sid.clone() }).await {
                Ok(output) => ToolOutcome::Ok { output },
                Err(nscore::ToolError::Failed { kind, detail }) => {
                    ToolOutcome::Err { kind, detail }
                }
            };
            log.append(turn, now(), EventKind::ToolReturned { call: call_id, outcome });
            // loop: the emitter decides what happens next (typically respond_directly)
        }

        // 3. fallback settle
        let policy = settled.unwrap_or_else(|| {
            let p = ReplyPolicy::Verbatim { text: FALLBACK_REPLY.into() };
            log.append(turn, now(), EventKind::Settled { policy: p.clone() });
            p
        });

        // 4. reply
        let text = match policy {
            ReplyPolicy::Verbatim { text } => text,
            ReplyPolicy::Template { id, vars } => format!("[{id}] {vars}"),
            ReplyPolicy::Generate => {
                let state = fold(log.events());
                let trace = turn_trace(&log, turn);
                let ctx = ReplyContext {
                    persona: self.cfg.persona.clone(),
                    facts: vec![],
                    session_summary: state_summary(&state),
                    turn_trace: trace,
                };
                match self.parts.replier.reply(ctx).await {
                    Ok(t) => t,
                    Err(_) => FALLBACK_REPLY.into(),
                }
            }
        };

        // 5. record + persist new events only
        log.append(turn, now(), EventKind::Replied { text: text.clone() });
        self.parts.memory.append(&sid, &log.events()[n_loaded..]).await?;
        Ok(text)
    }

    /// Outer loop: recv → run_turn → send, until the channel closes.
    pub async fn run(&mut self) -> Result<(), EngineError> {
        loop {
            let incoming = match self.parts.channel.recv().await {
                Ok(i) => i,
                Err(ChannelError::Closed) => return Ok(()),
                Err(e) => return Err(EngineError::Channel(e.to_string())),
            };
            let session = incoming.session.clone();
            let text = self.run_turn(incoming).await?;
            self.parts
                .channel
                .send(&session, &text)
                .await
                .map_err(|e| EngineError::Channel(e.to_string()))?;
        }
    }
}

/// One human-readable line per this-turn event: outcomes AND refusal reasons.
fn turn_trace(log: &EventLog, turn: u32) -> String {
    log.events()
        .iter()
        .filter(|e| e.turn == turn)
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => Some(format!("Proposed({})", proposal.action)),
            EventKind::Rejected { reason, .. } => Some(match reason {
                RejectReason::Malformed { detail } => format!("Rejected(malformed: {detail})"),
                RejectReason::IllegalAction { action } => {
                    format!("Rejected(illegal action: {action})")
                }
                RejectReason::GuardDenied { guard, reason } => {
                    format!("Rejected(guard {guard}: {reason})")
                }
            }),
            EventKind::ToolReturned { outcome, .. } => Some(match outcome {
                ToolOutcome::Ok { output } => format!("ToolReturned(ok: {})", output.summary),
                ToolOutcome::Err { kind, detail } => {
                    format!("ToolReturned(err {kind}: {detail})")
                }
            }),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
