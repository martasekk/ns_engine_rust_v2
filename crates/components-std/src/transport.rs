use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Deliberate small duplicate of ns-llm's transport trait: workspace crates
/// depend only on ns-core, never on each other.
#[async_trait]
pub trait ToolTransport: Send + Sync {
    /// (status, body) on a completed HTTP exchange; Err on network failure.
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String>;
}

pub struct ReqwestToolTransport {
    client: reqwest::Client,
}

impl ReqwestToolTransport {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }
}

impl Default for ReqwestToolTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolTransport for ReqwestToolTransport {
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String> {
        let resp = self.client.post(url).json(body).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok((status, body))
    }
}

/// Test double: pops queued results front-to-back, records (url, body) pairs.
pub struct MockToolTransport {
    pub responses: Mutex<VecDeque<Result<(u16, serde_json::Value), String>>>,
    pub requests: Mutex<Vec<(String, serde_json::Value)>>,
}

impl MockToolTransport {
    pub fn new(responses: Vec<Result<(u16, serde_json::Value), String>>) -> Arc<Self> {
        Arc::new(Self { responses: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) })
    }
}

#[async_trait]
impl ToolTransport for MockToolTransport {
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String> {
        self.requests.lock().unwrap().push((url.to_string(), body.clone()));
        self.responses.lock().unwrap().pop_front().unwrap_or(Err("mock queue exhausted".into()))
    }
}
