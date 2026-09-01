use async_trait::async_trait;
use nscore::{ActionSpec, SideEffect, Tool, ToolCtx, ToolError, ToolOutput, Trust};

pub struct GetTimeTool {
    spec: ActionSpec,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl GetTimeTool {
    pub fn new() -> Self {
        Self::with_clock(Box::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }))
    }

    pub fn with_clock(clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            spec: ActionSpec {
                name: "get_time".into(),
                description: "Get the current date and time".into(),
                args_schema: serde_json::json!({
                    "type": "object", "properties": {}, "required": []
                }),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
            clock,
        }
    }
}

impl Default for GetTimeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GetTimeTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: format!("current unix time (ms): {}", (self.clock)()),
            artifact: None,
            trust: Trust::System,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::SessionId;

    #[tokio::test]
    async fn reports_injected_clock_as_system_trust() {
        let t = GetTimeTool::with_clock(Box::new(|| 1_756_700_000_000));
        assert_eq!(t.spec().name, "get_time");
        assert_eq!(t.spec().side_effect, SideEffect::Pure);
        let out = t
            .call(&serde_json::json!({}), &ToolCtx { session: SessionId("s".into()) })
            .await
            .unwrap();
        assert_eq!(out.summary, "current unix time (ms): 1756700000000");
        assert_eq!(out.trust, Trust::System);
    }
}
