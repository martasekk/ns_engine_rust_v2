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
        let mut emit_failures: u32 = 0;
        let mut settled: Option<ReplyPolicy> = None;

        for _ in 0..self.cfg.max_iterations {
            // a. project
            let state = fold(log.events());
            let legal = LegalActionSet {
                actions: self.parts.tools.iter().map(|t| t.spec().clone()).collect(),
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
            let ctx = nscore::EmitterContext {
                state_summary: summary,
                recent_turns: recent,
                rejections_this_turn: rejections_this_turn.clone(),
            };

            // c. propose
            let proposal = match self.parts.emitter.propose(ctx, &legal).await {
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
                continue;
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
            let active_pending = state.pending_confirmation.filter(|_| {
                state.pending_turn == Some(turn)
                    || state.pending_turn.map(|pt| pt + 1 == turn).unwrap_or(false)
            });
            let guard_ctx = nscore::GuardCtx {
                spec: tool.spec(),
                turn,
                confirmed_this_turn: state.confirmed_this_turn_of == Some(turn),
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
                    continue;
                }
                Verdict::NeedsConfirmation { prompt } => {
                    log.append(
                        turn,
                        now(),
                        EventKind::PendingConfirmation { proposal_of: pid, staged: None },
                    );
                    let policy = ReplyPolicy::Verbatim { text: prompt };
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
