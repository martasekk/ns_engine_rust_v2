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
    /// When set, this tool fires at most once per session (DedupeGate).
    #[serde(default)]
    pub dedupe_tag: Option<String>,
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
                dedupe_tag: cfg.dedupe_tag,
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

    async fn call(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let (status, body) = self
            .transport
            .post_json(&self.url, args)
            .await
            .map_err(|detail| ToolError::Failed {
                kind: "network".into(),
                detail,
            })?;
        if !(200..300).contains(&status) {
            return Err(ToolError::Failed {
                kind: format!("http_{status}"),
                detail: body.to_string(),
            });
        }
        let full = body.to_string();
        // Oversized bodies are kept whole as content-addressed artifacts
        // (spec §4); the summary stays bounded either way. Store errors
        // degrade gracefully — the summary is still useful.
        let mut artifact = None;
        if full.len() > MAX_SUMMARY {
            if let Some(store) = &ctx.artifacts {
                artifact = store.put_artifact(full.clone().into_bytes()).await.ok();
            }
        }
        let mut summary = full;
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
            artifact,
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
            dedupe_tag: None,
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
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
            )
            .await
            .unwrap();
        assert!(out.summary.contains("in_stock"));
        assert_eq!(
            out.trust,
            Trust::External,
            "external content MUST be External trust"
        );
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
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
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
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { ref kind, .. } if kind == "network"));
    }

    struct ArtifactSink(std::sync::Mutex<Vec<Vec<u8>>>);
    #[async_trait]
    impl nscore::MemoryStore for ArtifactSink {
        async fn append(
            &self,
            _s: &nscore::SessionId,
            _e: &[nscore::Event],
        ) -> Result<(), nscore::StoreError> {
            Ok(())
        }
        async fn load(
            &self,
            _s: &nscore::SessionId,
        ) -> Result<Vec<nscore::Event>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn search_turns(
            &self,
            _s: &nscore::SessionId,
            _q: &str,
            _k: usize,
        ) -> Result<Vec<nscore::TurnHit>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn facts(&self, _s: &str, _p: &str) -> Result<Vec<nscore::Fact>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn fact_history(
            &self,
            _s: &str,
            _k: &str,
        ) -> Result<Vec<nscore::Fact>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn put_fact(&self, _f: nscore::Fact) -> Result<(), nscore::StoreError> {
            Ok(())
        }
        async fn forget_fact(
            &self,
            _s: &str,
            _k: &str,
            _at: nscore::Timestamp,
        ) -> Result<bool, nscore::StoreError> {
            Ok(false)
        }
        async fn purge_facts(&self, _s: &str) -> Result<usize, nscore::StoreError> {
            Ok(0)
        }
        async fn search_facts(
            &self,
            _s: &str,
            _q: &str,
            _k: usize,
        ) -> Result<Vec<nscore::Fact>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn scopes(&self) -> Result<Vec<String>, nscore::StoreError> {
            Ok(vec![])
        }
        async fn artifact(&self, _id: &nscore::ArtifactId) -> Result<Vec<u8>, nscore::StoreError> {
            Err(nscore::StoreError::NotFound)
        }
        async fn put_artifact(
            &self,
            content: Vec<u8>,
        ) -> Result<nscore::ArtifactId, nscore::StoreError> {
            let id = nscore::ArtifactId::for_content(&content);
            self.0.lock().unwrap().push(content);
            Ok(id)
        }
        async fn sessions(&self) -> Result<Vec<nscore::SessionId>, nscore::StoreError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn oversized_body_is_stored_as_content_addressed_artifact() {
        let big = "x".repeat(5000);
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"blob": big})))]);
        let sink = std::sync::Arc::new(ArtifactSink(std::sync::Mutex::new(Vec::new())));
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: Some(sink.clone()),
                },
            )
            .await
            .unwrap();
        assert!(out.summary.len() <= 2000, "summary still truncated");
        let stored = sink.0.lock().unwrap();
        assert_eq!(stored.len(), 1, "full body stored once");
        let expected_id = nscore::ArtifactId::for_content(&stored[0]);
        assert_eq!(
            out.artifact,
            Some(expected_id),
            "artifact id is the content hash"
        );
        assert!(stored[0].len() > 5000, "the FULL body was stored");
    }

    #[tokio::test]
    async fn small_body_stores_no_artifact() {
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"ok": true})))]);
        let sink = std::sync::Arc::new(ArtifactSink(std::sync::Mutex::new(Vec::new())));
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: Some(sink.clone()),
                },
            )
            .await
            .unwrap();
        assert_eq!(out.artifact, None);
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn long_bodies_are_truncated_to_2000_chars() {
        let big = "x".repeat(5000);
        let mock = MockToolTransport::new(vec![Ok((200, serde_json::json!({"blob": big})))]);
        let t = HttpTool::new(cfg(), mock);
        let out = t
            .call(
                &serde_json::json!({"product": "widget"}),
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
            )
            .await
            .unwrap();
        assert!(out.summary.len() <= 2000);
    }
}
