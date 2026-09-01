use crate::action::{
    ActionSpec, ClassifiedProposal, Fact, Incoming, LegalActionSet, Proposal, StagedEffect,
    ToolOutput, Verdict,
};
use crate::event::{Event, SessionId};
use crate::value::ArtifactId;
use async_trait::async_trait;

pub struct ToolCtx {
    pub session: SessionId,
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

pub trait Guard: Send + Sync {
    fn name(&self) -> &str;
    /// M1 note: `state` is the projected state as JSON; a typed SessionState view
    /// replaces this parameter in M3 when guards need real structure.
    fn check(&self, p: &ClassifiedProposal, state: &serde_json::Value) -> Verdict;
}

pub struct EmitterContext {
    pub state_summary: String,
    /// (speaker, text), speaker: "user" | "assistant"
    pub recent_turns: Vec<(String, String)>,
    pub rejections_this_turn: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait]
pub trait Emitter: Send + Sync {
    async fn propose(
        &self,
        ctx: EmitterContext,
        legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError>;
}

pub struct ReplyContext {
    pub persona: String,
    pub facts: Vec<Fact>,
    pub session_summary: String,
    /// Outcomes AND refusal reasons, human-readable lines.
    pub turn_trace: String,
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

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("closed")]
    Closed,
    #[error("io: {0}")]
    Io(String),
}

#[async_trait]
pub trait Channel: Send + Sync {
    async fn recv(&mut self) -> Result<Incoming, ChannelError>;
    /// Callable WITHOUT a pending incoming turn (future proactive messages).
    async fn send(&mut self, session: &SessionId, text: &str) -> Result<(), ChannelError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("io: {0}")]
    Io(String),
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn append(&self, session: &SessionId, events: &[Event]) -> Result<(), StoreError>;
    async fn load(&self, session: &SessionId) -> Result<Vec<Event>, StoreError>;
    async fn facts(&self, key_prefix: &str) -> Result<Vec<Fact>, StoreError>;
    async fn put_fact(&self, fact: Fact) -> Result<(), StoreError>;
    async fn artifact(&self, id: &ArtifactId) -> Result<Vec<u8>, StoreError>;
    async fn put_artifact(&self, content: Vec<u8>) -> Result<ArtifactId, StoreError>;
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
            Ok(ToolOutput { summary: "ok".into(), artifact: None, trust: Trust::System })
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
        };
        let t: Box<dyn Tool> = Box::new(Dummy(spec));
        assert_eq!(t.spec().name, "dummy");
    }
}
