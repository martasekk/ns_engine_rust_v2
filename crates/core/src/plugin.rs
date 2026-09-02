use crate::traits::{Channel, Consolidator, Emitter, Guard, MemoryStore, Replier, Tool};
use std::collections::HashSet;
use std::sync::Arc;

pub trait HarnessPlugin {
    fn build(&self, app: &mut HarnessBuilder);
}

#[derive(Default)]
pub struct HarnessBuilder {
    emitter: Option<Box<dyn Emitter>>,
    replier: Option<Box<dyn Replier>>,
    memory: Option<Arc<dyn MemoryStore>>,
    channel: Option<Box<dyn Channel>>,
    consolidator: Option<Box<dyn Consolidator>>,
    tools: Vec<Arc<dyn Tool>>,
    guards: Vec<Box<dyn Guard>>,
    dup: Vec<&'static str>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BuildError {
    #[error("missing required slot: {0}")]
    MissingSlot(&'static str),
    #[error("duplicate slot: {0}")]
    DuplicateSlot(&'static str),
    #[error("duplicate tool name: {0}")]
    DuplicateTool(String),
}

/// Validated wiring, consumed by the engine crate.
pub struct HarnessParts {
    pub emitter: Box<dyn Emitter>,
    pub replier: Box<dyn Replier>,
    pub memory: Arc<dyn MemoryStore>,
    pub channel: Box<dyn Channel>,
    pub consolidator: Box<dyn Consolidator>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub guards: Vec<Box<dyn Guard>>,
}

impl HarnessBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_plugin(mut self, p: &dyn HarnessPlugin) -> Self {
        p.build(&mut self);
        self
    }

    pub fn set_emitter(&mut self, e: Box<dyn Emitter>) {
        if self.emitter.is_some() {
            self.dup.push("emitter");
        }
        self.emitter = Some(e);
    }

    pub fn set_replier(&mut self, r: Box<dyn Replier>) {
        if self.replier.is_some() {
            self.dup.push("replier");
        }
        self.replier = Some(r);
    }

    pub fn set_memory(&mut self, m: Arc<dyn MemoryStore>) {
        if self.memory.is_some() {
            self.dup.push("memory");
        }
        self.memory = Some(m);
    }

    pub fn set_channel(&mut self, c: Box<dyn Channel>) {
        if self.channel.is_some() {
            self.dup.push("channel");
        }
        self.channel = Some(c);
    }

    pub fn set_consolidator(&mut self, c: Box<dyn Consolidator>) {
        if self.consolidator.is_some() {
            self.dup.push("consolidator");
        }
        self.consolidator = Some(c);
    }

    pub fn add_tool(&mut self, t: Arc<dyn Tool>) {
        self.tools.push(t);
    }

    pub fn add_guard(&mut self, g: Box<dyn Guard>) {
        self.guards.push(g);
    }

    pub fn build(self) -> Result<HarnessParts, BuildError> {
        for slot in ["emitter", "replier", "memory", "channel", "consolidator"] {
            if self.dup.contains(&slot) {
                return Err(BuildError::DuplicateSlot(slot));
            }
        }
        let emitter = self.emitter.ok_or(BuildError::MissingSlot("emitter"))?;
        let replier = self.replier.ok_or(BuildError::MissingSlot("replier"))?;
        let memory = self.memory.ok_or(BuildError::MissingSlot("memory"))?;
        let channel = self.channel.ok_or(BuildError::MissingSlot("channel"))?;
        let consolidator = self
            .consolidator
            .ok_or(BuildError::MissingSlot("consolidator"))?;
        let mut seen = HashSet::new();
        for t in &self.tools {
            let name = t.spec().name.clone();
            if !seen.insert(name.clone()) {
                return Err(BuildError::DuplicateTool(name));
            }
        }
        Ok(HarnessParts {
            emitter,
            replier,
            memory,
            channel,
            consolidator,
            tools: self.tools,
            guards: self.guards,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::*;
    use crate::event::{Event, SessionId};
    use crate::traits::*;
    use crate::value::{ArtifactId, Trust};
    use async_trait::async_trait;

    struct NullEmitter;
    #[async_trait]
    impl Emitter for NullEmitter {
        async fn propose(
            &self,
            _ctx: EmitterContext,
            _legal: &LegalActionSet,
        ) -> Result<Proposal, EmitError> {
            Err(EmitError::Malformed("null".into()))
        }
    }

    struct NullReplier;
    #[async_trait]
    impl Replier for NullReplier {
        async fn reply(&self, _ctx: ReplyContext) -> Result<String, ReplyError> {
            Ok("".into())
        }
    }

    struct NullChannel;
    #[async_trait]
    impl Channel for NullChannel {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    struct NullStore;
    #[async_trait]
    impl MemoryStore for NullStore {
        async fn append(&self, _s: &SessionId, _e: &[Event]) -> Result<(), StoreError> {
            Ok(())
        }
        async fn load(&self, _s: &SessionId) -> Result<Vec<Event>, StoreError> {
            Ok(vec![])
        }
        async fn facts(&self, _s: &str, _p: &str) -> Result<Vec<Fact>, StoreError> {
            Ok(vec![])
        }
        async fn fact_history(&self, _s: &str, _k: &str) -> Result<Vec<Fact>, StoreError> {
            Ok(vec![])
        }
        async fn put_fact(&self, _f: Fact) -> Result<(), StoreError> {
            Ok(())
        }
        async fn forget_fact(
            &self,
            _s: &str,
            _k: &str,
            _at: crate::event::Timestamp,
        ) -> Result<bool, StoreError> {
            Ok(false)
        }
        async fn purge_facts(&self, _s: &str) -> Result<usize, StoreError> {
            Ok(0)
        }
        async fn search_facts(
            &self,
            _s: &str,
            _q: &str,
            _k: usize,
        ) -> Result<Vec<Fact>, StoreError> {
            Ok(vec![])
        }
        async fn scopes(&self) -> Result<Vec<String>, StoreError> {
            Ok(vec![])
        }
        async fn artifact(&self, _id: &ArtifactId) -> Result<Vec<u8>, StoreError> {
            Err(StoreError::NotFound)
        }
        async fn put_artifact(&self, c: Vec<u8>) -> Result<ArtifactId, StoreError> {
            Ok(ArtifactId::for_content(&c))
        }
        async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
            Ok(vec![])
        }
    }

    struct NullConsolidator;
    #[async_trait]
    impl Consolidator for NullConsolidator {
        async fn run(&self, _s: &dyn MemoryStore) -> Result<(), StoreError> {
            Ok(())
        }
    }

    struct NullTool(ActionSpec);
    impl NullTool {
        fn named(name: &str) -> Self {
            NullTool(ActionSpec {
                name: name.into(),
                description: "null".into(),
                args_schema: serde_json::json!({}),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            })
        }
    }
    #[async_trait]
    impl Tool for NullTool {
        fn spec(&self) -> &ActionSpec {
            &self.0
        }
        async fn call(
            &self,
            _a: &serde_json::Value,
            _c: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "".into(),
                artifact: None,
                trust: Trust::System,
            })
        }
    }

    fn fill_rest(b: &mut HarnessBuilder) {
        b.set_replier(Box::new(NullReplier));
        b.set_memory(Arc::new(NullStore));
        b.set_channel(Box::new(NullChannel));
        b.set_consolidator(Box::new(NullConsolidator));
    }

    fn fill_all(b: &mut HarnessBuilder) {
        b.set_emitter(Box::new(NullEmitter));
        fill_rest(b);
    }

    #[test]
    fn build_fails_on_missing_slot() {
        let b = HarnessBuilder::new();
        match b.build() {
            Err(BuildError::MissingSlot(s)) => assert_eq!(s, "emitter"),
            other => panic!("expected MissingSlot, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn build_fails_on_duplicate_slot() {
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(NullEmitter));
        b.set_emitter(Box::new(NullEmitter));
        fill_rest(&mut b);
        assert_eq!(b.build().err(), Some(BuildError::DuplicateSlot("emitter")));
    }

    #[test]
    fn build_fails_on_duplicate_tool_name() {
        let mut b = HarnessBuilder::new();
        fill_all(&mut b);
        b.add_tool(Arc::new(NullTool::named("echo")));
        b.add_tool(Arc::new(NullTool::named("echo")));
        assert_eq!(
            b.build().err(),
            Some(BuildError::DuplicateTool("echo".into()))
        );
    }

    #[test]
    fn build_succeeds_when_complete() {
        let mut b = HarnessBuilder::new();
        fill_all(&mut b);
        let parts = b.build().unwrap();
        assert_eq!(parts.tools.len(), 0);
    }
}
