use crate::action::{
    ActionSpec, ClassifiedProposal, Fact, Incoming, LegalActionSet, Proposal, StagedEffect,
    ToolOutput, Verdict,
};
use crate::event::{Event, SessionId};
use crate::value::ArtifactId;
use async_trait::async_trait;

pub struct ToolCtx {
    pub session: SessionId,
    /// Artifact store for oversized tool content; None in unit tests.
    pub artifacts: Option<std::sync::Arc<dyn MemoryStore>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{kind}: {detail}")]
    Failed { kind: String, detail: String },
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ActionSpec;
    async fn call(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
    async fn stage(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> Option<StagedEffect> {
        None
    }
}

/// Typed view of session state for guards — replaces M1's serde_json::Value.
pub struct GuardCtx<'a> {
    /// Spec of the proposed action (synthetic actions get synthetic specs).
    pub spec: &'a ActionSpec,
    pub turn: u32,
    /// A Confirmed event was appended this turn (unlocks SideEffectGate).
    pub confirmed_this_turn: bool,
    /// Action names that have ToolCalled at least once this session.
    pub fired_actions: &'a std::collections::HashSet<String>,
    /// Active (non-expired) pending confirmation, if any.
    pub pending_confirmation: Option<crate::event::EventId>,
}

pub trait Guard: Send + Sync {
    fn name(&self) -> &str;
    fn check(&self, p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict;
}

/// What the action-selection model sees (M6 spec §4.2): the same projection
/// of the log the replier sees, rendered for choosing an action.
pub struct EmitterContext {
    /// Standing facts in scope, already ranked and budgeted (M6 §6.5).
    pub facts: Vec<crate::memory::FactView>,
    /// Rolling summary of the turns outside the window (M6 §5.1).
    pub summary: Option<crate::memory::SessionSummary>,
    /// The last few completed turns, verbatim, oldest first (M6 §4.1).
    pub window: Vec<crate::memory::TurnRecord>,
    pub caps: crate::memory::Caps,
    /// The current user message.
    pub user_text: String,
    /// This turn's actions so far, one line each with outcomes and refusals.
    pub trace_so_far: Vec<String>,
    /// A staged action awaits the user's yes/no.
    pub pending_confirmation: bool,
    pub rejections_this_turn: Vec<String>,
    /// Learned guidance notes (spec M5 §3.3): global + scoped to legal actions.
    pub guidance: Vec<String>,
    /// The emitter's own size against its own ceiling, and the results it
    /// could still page through (M7 T2.3). `None` unless
    /// `show_budget_line` is on — it is an experiment: VISTA reports a large
    /// gain from showing a model its budget, and whether a 3B emitter acts
    /// on the line or merely reads it is what the task set is for.
    pub budget_line: Option<String>,
    /// Where the provider client leaves what this call cost (M7 T0.1). The
    /// engine hands every call the sink of the turn it belongs to and drains
    /// it right after the call, so two turns in flight at once cannot mix
    /// their records. `None` — every scripted double, every caller outside a
    /// turn — leaves the client to the sink it was built with, or to none.
    pub usage: Option<std::sync::Arc<crate::usage::UsageSink>>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    /// The model answered, and the answer was unusable.
    #[error("malformed: {0}")]
    Malformed(String),
    /// The endpoint answered with an HTTP status. Structured, because the
    /// recovery differs by class and a parsed-out-of-a-string status is a
    /// recovery decision made on a formatting accident.
    #[error("status {status}: {detail}")]
    Provider { status: u16, detail: String },
    /// The endpoint could not be reached at all.
    #[error("transport: {0}")]
    Transport(String),
}

impl EmitError {
    /// Whether retrying the identical request could plausibly succeed.
    /// 429 and 5xx are transient; 408 is a timeout. Every other 4xx is a
    /// statement about the request or the account — a wrong model name, a
    /// missing key, an empty balance — and repeating it only spends the
    /// budget. Seen live: turns 154 and 155 of session `cli` each burned all
    /// three emit retries against a 404 for a model that did not exist.
    pub fn is_retryable(&self) -> bool {
        match self {
            EmitError::Provider { status, .. } => {
                *status == 429 || *status == 408 || *status >= 500
            }
            // A malformed answer is worth re-asking: the model may do better.
            EmitError::Malformed(_) | EmitError::Transport(_) => true,
        }
    }
}

#[async_trait]
pub trait Emitter: Send + Sync {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError>;
}

/// What the reply model sees (M6 spec §4.2–4.3). Block order is stable-first
/// for prefix caching: persona → facts → summary → window → current turn.
pub struct ReplyContext {
    pub persona: String,
    pub facts: Vec<crate::memory::FactView>,
    pub summary: Option<crate::memory::SessionSummary>,
    pub window: Vec<crate::memory::TurnRecord>,
    pub caps: crate::memory::Caps,
    /// The current user message — the one thing the reply must answer.
    pub user_text: String,
    /// Outcomes AND refusal reasons, human-readable lines.
    pub turn_trace: String,
    /// Reply-scoped learned notes (M6 §8.5).
    pub guidance: Vec<String>,
    /// Claims the grounding interceptor found unsupported in a first draft
    /// (M6 §4.5); non-empty only on the single regeneration.
    pub do_not_state: Vec<String>,
    /// Spans a first draft lifted verbatim out of its own prompt.
    /// **Currently never populated:** the copy check was demoted from gate to
    /// monitor after an ablation measured its true-positive rate at zero
    /// (plan §8), so nothing regenerates on an echo. Kept as the seam for a
    /// reference-aware copy check, which is what the entrainment literature
    /// actually operationalizes — overlap against a gold answer, not overlap
    /// in the abstract. Rendered by the replier when set.
    pub do_not_repeat: Vec<String>,
    /// The sink this call records into; see [`EmitterContext::usage`].
    pub usage: Option<std::sync::Arc<crate::usage::UsageSink>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplyError {
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait]
pub trait Replier: Send + Sync {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError>;
}

/// What the summarizer folds in (M6 §5.1): the previous summary for
/// continuity (None on a full rebuild), the verbatim records of the new
/// range, and the standing facts so they are not restated.
pub struct SummaryInput<'a> {
    pub previous: Option<&'a crate::memory::SessionSummary>,
    pub records: &'a [crate::memory::TurnRecord],
    pub caps: &'a crate::memory::Caps,
    pub facts: &'a [crate::memory::FactView],
    /// The sink this call records into; see [`EmitterContext::usage`].
    pub usage: Option<std::sync::Arc<crate::usage::UsageSink>>,
}

/// The model's part of a summary; the engine adds range, trust and turn.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct SummaryDraft {
    pub topic: String,
    #[serde(default)]
    pub established: Vec<String>,
    #[serde(default)]
    pub open: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Ok(None) = no summary (the no-op implementation); Err = try again at
    /// the next boundary with the larger range.
    async fn summarize(
        &self,
        input: SummaryInput<'_>,
    ) -> Result<Option<SummaryDraft>, SummarizeError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("closed")]
    Closed,
    #[error("io: {0}")]
    Io(String),
}

/// Both methods take `&self`: one `recv` is pending for the whole run while
/// the session tasks `send` through the same handle, so an implementor keeps
/// whatever it mutates behind its own lock or atomic (multi-conversation plan
/// Phase 2, D2.1).
#[async_trait]
pub trait Channel: Send + Sync {
    async fn recv(&self) -> Result<Incoming, ChannelError>;
    /// Callable WITHOUT a pending incoming turn (future proactive messages).
    async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("io: {0}")]
    Io(String),
}

/// One verbatim line of a recorded turn matched by `recall` (M6 §7).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnHit {
    /// Which conversation the line was said in. Recall reaches across
    /// sessions (M7 §8), and a turn number alone does not identify a line
    /// once more than one session can answer the query.
    pub session: SessionId,
    pub turn: u32,
    /// "user" or "bot".
    pub speaker: &'static str,
    pub text: String,
    /// Higher is better; comparable only within one query.
    pub score: f64,
}

/// The last `SessionSummary` of a closed session, filed under its scope
/// (M7 §8) — one level of coarsening above the session, and the only
/// episodic memory that crosses sessions besides facts. No model call: the
/// rolling summarizer already paid for the summary, and the digest is that
/// summary made searchable after the session ends.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionDigest {
    pub session: SessionId,
    pub scope: String,
    pub summary: crate::memory::SessionSummary,
    /// Last turn of the session the digest covers.
    pub last_turn: u32,
    /// When the digest was written (unix ms).
    pub at: crate::event::Timestamp,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<(), StoreError>;
    async fn load(&self, session: &SessionId) -> Result<Vec<Event>, StoreError>;
    /// Full-text search over what was said in `session` (UserSaid and
    /// Replied text), best first, at most `k` (M6 §7: verbatim first).
    async fn search_turns(
        &self,
        session: &SessionId,
        query: &str,
        k: usize,
    ) -> Result<Vec<TurnHit>, StoreError>;
    /// `search_turns` over several sessions at once, merged into one ranking
    /// — what cross-session recall reads (M7 §8).
    ///
    /// The default is correct but spends one query per session; a store with
    /// a real index overrides it with a single query, because recall runs on
    /// the hot path and `recall_sessions` is 3 today only because nothing
    /// cheaper existed.
    async fn search_turns_in(
        &self,
        sessions: &[SessionId],
        query: &str,
        k: usize,
    ) -> Result<Vec<TurnHit>, StoreError> {
        let mut out: Vec<TurnHit> = Vec::new();
        for session in sessions {
            // `k` from each session is enough: a hit in the merged top-k is
            // necessarily in the top-k of its own session.
            out.extend(self.search_turns(session, query, k).await?);
        }
        // Stable sort, so equal scores keep the caller's session order —
        // callers pass the current session first, and M6 §7 ranks what was
        // said in this conversation above what was said in an older one.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(k);
        Ok(out)
    }
    /// Write the digest of a closed session (M7 §8), **replacing** any
    /// digest that session already has. Idempotent by session id because a
    /// session is closed and re-digested on every consolidator run, and two
    /// rows for one session would double-count it in `session_digests`.
    async fn put_session_digest(&self, digest: &SessionDigest) -> Result<(), StoreError>;
    /// The `limit` most recently written digests of `scope`, newest first.
    async fn session_digests(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<SessionDigest>, StoreError>;
    /// Digests of `scope` relevant to `query`, best first, at most `k`.
    /// A digest is an index into older sessions, not a replacement for them:
    /// callers rank these below the verbatim hits of `search_turns_in`
    /// (M6 §7, verbatim first).
    async fn search_digests(
        &self,
        scope: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<SessionDigest>, StoreError>;
    /// Live facts (`current` or `cold`) in `scope` whose key starts with
    /// `key_prefix`, key order. Superseded and forgotten versions are
    /// reachable through `fact_history` only.
    async fn facts(&self, scope: &str, key_prefix: &str) -> Result<Vec<Fact>, StoreError>;
    /// Every version of one fact, newest `valid_from` first (M6 §6.1).
    async fn fact_history(&self, scope: &str, key: &str) -> Result<Vec<Fact>, StoreError>;
    /// Make `fact` the current version of `(scope, key)`: an existing row
    /// with the same `valid_from` is updated in place; otherwise every
    /// current row of that key is marked superseded (`valid_to =
    /// fact.valid_from`) and the new row inserted. Never deletes.
    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError>;
    /// Soft-delete the current version (`state = forgotten`, `valid_to = at`).
    /// Ok(false) when there is no current version.
    async fn forget_fact(
        &self,
        scope: &str,
        key: &str,
        at: crate::event::Timestamp,
    ) -> Result<bool, StoreError>;
    /// Hard-delete every version of every fact in `scope`; rows removed.
    async fn purge_facts(&self, scope: &str) -> Result<usize, StoreError>;
    /// Current facts in `scope` relevant to `query`, best first, at most `k`
    /// (M6 §6.5; lexical until FTS5 lands).
    async fn search_facts(
        &self,
        scope: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<Fact>, StoreError>;
    /// Every scope holding at least one fact row.
    async fn scopes(&self) -> Result<Vec<String>, StoreError>;
    async fn artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, StoreError>;
    async fn put_artifact(&self, content: Vec<u8>) -> Result<ArtifactId, StoreError>;
    /// All sessions with at least one event, most recently active first.
    async fn sessions(&self) -> Result<Vec<SessionId>, StoreError>;
}

#[async_trait]
pub trait Consolidator: Send + Sync {
    async fn run(&self, store: &dyn MemoryStore) -> Result<(), StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{SideEffect, ToolOutput};
    use crate::value::Trust;

    struct Dummy(ActionSpec);

    #[async_trait]
    impl Tool for Dummy {
        fn spec(&self) -> &ActionSpec {
            &self.0
        }
        async fn call(
            &self,
            _a: &serde_json::Value,
            _c: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "ok".into(),
                artifact: None,
                trust: Trust::System,
            })
        }
    }

    #[test]
    fn tool_is_object_safe() {
        let spec = ActionSpec {
            name: "dummy".into(),
            description: "d".into(),
            args_schema: serde_json::json!({}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        };
        let t: Box<dyn Tool> = Box::new(Dummy(spec));
        assert_eq!(t.spec().name, "dummy");
    }

    struct AlwaysDeny;
    impl Guard for AlwaysDeny {
        fn name(&self) -> &str {
            "always_deny"
        }
        fn check(&self, _p: &ClassifiedProposal, ctx: &GuardCtx) -> Verdict {
            Verdict::Deny {
                reason: format!("turn {}", ctx.turn),
            }
        }
    }

    #[test]
    fn guard_sees_typed_ctx() {
        let spec = ActionSpec {
            name: "x".into(),
            description: "d".into(),
            args_schema: serde_json::json!({}),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        };
        let fired = std::collections::HashSet::new();
        let ctx = GuardCtx {
            spec: &spec,
            turn: 3,
            confirmed_this_turn: false,
            fired_actions: &fired,
            pending_confirmation: None,
        };
        let p = ClassifiedProposal {
            proposal: Proposal {
                rationale: "".into(),
                action: "x".into(),
                args: serde_json::json!({}),
            },
            args: vec![],
        };
        let g: Box<dyn Guard> = Box::new(AlwaysDeny);
        assert!(matches!(g.check(&p, &ctx), Verdict::Deny { reason } if reason == "turn 3"));
    }
}
