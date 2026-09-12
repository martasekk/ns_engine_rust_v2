//! The engine: one turn at a time.
//!
//! This file is the map. `Engine` and how it is built live here; everything
//! a turn actually does lives in a sibling named after the step:
//!
//! | module         | the step it owns                                  |
//! |----------------|---------------------------------------------------|
//! | [`run`]        | the turn loop, start to reply                     |
//! | [`routing`]    | what kind of turn this is                         |
//! | [`memory`]     | which facts it sees, and the running summary      |
//! | [`retrieval`]  | the searches the engine runs for itself           |
//! | [`gate`]       | provenance and the guard chain                    |
//! | [`reply`]      | writing the sentence the user reads               |
//! | [`accounting`] | what it spent, and persisting what it did         |
//! | [`specs`]      | the actions the engine itself owns                |
//! | [`config`]     | every knob, and the defaults                      |
//! | [`diagnostics`]| turning a failure into a sentence                  |

mod accounting;
mod builtins;
pub mod config;
mod diagnostics;
mod gate;
mod memory;
mod reply;
mod retrieval;
mod routing;
mod run;
pub mod specs;

// The names this module was a single file under, kept exactly as they were:
// `turn::EngineConfig` and `turn::REMEMBER_FACT` are what the app, the
// replayer and the eval harness import, and a split is not a reason to
// rewrite their imports.
pub use config::{EngineConfig, RememberResidual};
pub use reply::FALLBACK_REPLY;
pub use specs::{
    synthetic_specs, ASK_CLARIFICATION, CONFIRM_PENDING, EXEMPLARS, FORGET_ALL, FORGET_FACT,
    INSPECT_RESULT, RECALL, REMEMBER_FACT,
};

use gate::builtin_guards;
use nscore::{HarnessParts, Timestamp};

pub struct Engine {
    parts: HarnessParts,
    cfg: EngineConfig,
    clock: Box<dyn Fn() -> Timestamp + Send + Sync>,
    /// Always-on guard chain, checked before plugin guards. Plugins cannot
    /// remove these (spec §5.4).
    builtin_guards: Vec<Box<dyn nscore::Guard>>,
    /// M12 T6.1: model requests recorded since this engine was built, what
    /// `max_requests` is measured against. Every role counts, and it is
    /// incremented where the calls are already counted once —
    /// `record_model_calls` — so a role that books usage is capped by the
    /// fact that it books usage, with nothing to keep in step.
    spent: std::sync::atomic::AtomicU32,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("store: {0}")]
    Store(#[from] nscore::StoreError),
    #[error("channel: {0}")]
    Channel(String),
    /// M12 T6.1: `max_requests` is spent, so no further turn is started.
    /// Not a failure of the turn it is returned from — that turn made no
    /// call at all — but the end of a metered run.
    #[error("request cap {cap} reached after {spent} requests")]
    RequestCap { spent: u32, cap: u32 },
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
        let guards = builtin_guards(cfg.confirm_irreversible);
        Self {
            parts,
            cfg,
            clock,
            builtin_guards: guards,
            spent: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// The wiring, for the dispatcher (`dispatch.rs`): it runs the
    /// consolidator against the memory and hands the channel to the session
    /// tasks. Crate-private so the roles stay the engine's to call.
    pub(crate) fn parts(&self) -> &HarnessParts {
        &self.parts
    }

    pub(crate) fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// Identity of a call within a turn: action plus its args as JSON
    /// (serde_json's Map is ordered, so equal objects serialize identically).
    fn call_key(p: &nscore::Proposal) -> String {
        format!("{}\u{0}{}", p.action, p.args)
    }
}
