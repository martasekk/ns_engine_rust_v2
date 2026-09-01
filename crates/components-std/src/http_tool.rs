use crate::transport::ToolTransport;
use async_trait::async_trait;
use nscore::{ActionSpec, SideEffect, Tool, ToolCtx, ToolError, ToolOutput, Trust};
use std::sync::Arc;

/// The Windmill/n8n escape hatch (spec §8): an external HTTP endpoint exposed
/// as a tool without recompiling.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HttpToolConfig {
    pub name: String,
    pub description: String,
    pub url: String,
    pub side_effect: SideEffect,
    pub args_schema: serde_json::Value,
}

pub struct HttpTool {
    spec: ActionSpec,
    url: String,
    transport: Arc<dyn ToolTransport>,
}

impl HttpTool {
    pub fn new(cfg: HttpToolConfig, transport: Arc<dyn ToolTransport>) -> Self {
        Self {
            spec: ActionSpec {
                name: cfg.name,
                description: cfg.description,
                args_schema: cfg.args_schema,
                side_effect: cfg.side_effect,
                residual_policy: Default::default(),
            },
            url: cfg.url,
            transport,
        }
    }
}

const MAX_SUMMARY: usize = 2000;

#[async_trait]
impl Tool for HttpTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(
        &self,
        args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let (status, body) = self
            .transport
            .post_json(&self.url, args)
            .await
            .map_err(|detail| ToolError::Failed { kind: "network".into(), detail })?;
        if !(200..300).contains(&status) {
            return Err(ToolError::Failed {
                kind: format!("http_{status}"),
                detail: body.to_string(),
            });
        }
        let mut summary = body.to_string();
        if summary.len() > MAX_SUMMARY {
            // truncate on a char boundary
            let mut end = MAX_SUMMARY;
            while !summary.is_char_boundary(end) {
                end -= 1;
            }
            summary.truncate(end);
        }
        Ok(ToolOutput {
            summary,
            artifact: None,
            // Spec: tools fetching external content MUST return External trust.
            trust: Trust::External,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockToolTransport;
    use nscore::SessionId;

    fn cfg() -> HttpToolConfig {
        HttpToolConfig {
            name: "check_stock".into(),
            description: "Check stock for a product".into(),
            url: "https://example.test/stock".into(),
            side_effect: SideEffect::Pure,
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {"product": {"type": "string"}},
                "required": ["product"]
            }),
        }
    }

    #[tokio::test]
    async fn posts_args_and_returns_external_trust_summary() {
        let mock = MockToolTransport::new(vec![Ok((
            200,
            serde_json::json!({"in_stock": true, "count": 3}),
        ))]);
        let t = HttpTool::new(cfg(), mock.clone());
        assert_eq!(t.spec().name, "check_stock");
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()) },
            )
            .await
            .unwrap();
        assert!(out.summary.contains("in_stock"));
        assert_eq!(out.trust, Trust::External, "external content MUST be External trust");
        let reqs = mock.requests.lock().unwrap();
        assert_eq!(reqs[0].0, "https://example.test/stock");
        assert_eq!(reqs[0].1, serde_json::json!({"product": "widget"}));
    }

    #[tokio::test]
    async fn non_2xx_is_a_tool_error_with_status_kind() {
        let mock = MockToolTransport::new(vec![Ok((503, serde_json::json!({"err": "down"})))]);
        let t = HttpTool::new(cfg(), mock);
        let err = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { ref kind, .. } if kind == "http_503"));
    }

    #[tokio::test]
    async fn network_error_is_a_tool_error() {
        let mock = MockToolTransport::new(vec![Err("connection refused".into())]);
        let t = HttpTool::new(cfg(), mock);
        let err = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { ref kind, .. } if kind == "network"));
    }

    #[tokio::test]
    async fn long_bodies_are_truncated_to_2000_chars() {
        let big = "x".repeat(5000);
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"blob": big})))]);
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx { session: SessionId("s".into()) },
            )
            .await
            .unwrap();
        assert!(out.summary.len() <= 2000);
    }
}
