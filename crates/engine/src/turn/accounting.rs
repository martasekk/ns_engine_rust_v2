//! Counting what a turn spent, and persisting what it did.
//!
//! Every model call is booked in one place, which is what makes
//! `max_requests` a cap rather than an estimate.

use super::Engine;
use super::EngineError;
use nscore::{EventKind, EventLog};

/// M13 T3.1: did the proposal this turn most recently decided on actually
/// run? Read backwards over this turn's own events and stop at the first
/// thing that answers it: a `ToolReturned` means it ran, a `Rejected` means a
/// guard refused it. Neither means nothing was decided yet.
///
/// Asked of the log rather than tracked in a local, because "the action ran"
/// is true at six different places in the loop — one per builtin plus the
/// registered-tool path — and a flag set at six sites is a flag that is
/// eventually set at five.
pub(super) fn last_proposal_ran(events: &[nscore::Event], turn: u32) -> bool {
    events
        .iter()
        .rev()
        .take_while(|e| e.turn == turn)
        .find_map(|e| match &e.kind {
            EventKind::ToolReturned { .. } => Some(true),
            EventKind::Rejected { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}


impl Engine {
    /// Append one `ModelCall` for every provider call recorded in `sink`
    /// since its last drain (M7 T0.1).
    ///
    /// Called immediately after each of the engine's own model calls, with
    /// the sink that call's context carried — one per turn, one per summary
    /// — so what the drain returns is that call's and the manifest describes
    /// what it was shown, even while another session's turn is in flight
    /// (multi-conversation plan Phase 1, findings §2.6). Nothing here reads
    /// a process-wide sink: a client whose context carries none records into
    /// its own, and that one belongs to the calls made outside a turn.
    ///
    /// Retries inside one call do not appear as separate events — they are
    /// counted in `Usage::attempts`, because the thing a reader wants to
    /// know is what one decision cost, requests included.
    pub(super) fn record_model_calls(
        &self,
        sink: &nscore::UsageSink,
        log: &mut EventLog,
        turn: u32,
        manifest: &nscore::ContextManifest,
    ) {
        let mut calls = 0u32;
        for usage in sink.drain() {
            log.append(
                turn,
                (self.clock)(),
                EventKind::ModelCall {
                    usage,
                    manifest: manifest.clone(),
                },
            );
            calls += 1;
        }
        // M12 T6.1: what the request cap is measured against. Here because
        // this is the one place the engine's own calls are already counted.
        self.spent
            .fetch_add(calls, std::sync::atomic::Ordering::SeqCst);
    }

    /// M12 T6.1: whether this engine may start another turn.
    ///
    /// Checked at the start of a turn and again once it has ended — an
    /// engine over its ceiling refuses the *next* turn, rather than cutting
    /// the one in flight, whose tool calls have already happened and whose
    /// reply is owed to whoever is reading. The check after the turn is the
    /// same check: it is the one the next `run_turn` makes.
    pub(super) fn cap_reached(&self) -> Result<(), EngineError> {
        let Some(cap) = self.cfg.max_requests else {
            return Ok(());
        };
        let spent = self.spent.load(std::sync::atomic::Ordering::SeqCst);
        if spent >= cap {
            return Err(EngineError::RequestCap { spent, cap });
        }
        Ok(())
    }

    /// Write everything appended so far to the store, without waiting for the
    /// end of the turn.
    ///
    /// `run_turn` otherwise appends once, after `Replied`. Between an action
    /// that really happened — a click on a desktop, a purged fact table — and
    /// that append sit the turn's remaining iterations and the reply model, a
    /// network call that can hang or fail. A crash anywhere in there leaves
    /// the world changed and nothing in the log saying so, which is the one
    /// inconsistency this engine's design does not otherwise permit: every
    /// context is a projection of the log, so what the log missed did not
    /// happen. Pure results are recomputable and do not pay for this.
    ///
    /// `MemoryStore::append` skips ids it already holds, so this costs one
    /// statement and leaves the end-of-turn append writing exactly the
    /// remainder. A failure here is reported and not fatal: the same append
    /// runs again at the end of the turn, and *that* one propagates. Ending
    /// the turn early on a store error would abandon it after the side effect
    /// rather than before.
    pub(super) async fn flush(&self, sid: &nscore::SessionId, log: &EventLog, from: usize) {
        if let Err(e) = self.parts.memory.append(sid, &log.events()[from..]).await {
            eprintln!("store: {e}");
        }
    }
}
