use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// What a transport attempt produced. Status carried separately so retry
/// policy can distinguish 429/5xx (retryable) from 4xx (not).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("network: {0}")]
    Network(String),
    #[error("bad response body: {0}")]
    BadBody(String),
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<HttpResponse, TransportError>;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<HttpResponse, TransportError> {
        let mut req = self.client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| TransportError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body: serde_json::Value =
            resp.json().await.map_err(|e| TransportError::BadBody(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

/// Test double: pops queued results front-to-back, records every request body.
pub struct MockTransport {
    queue: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
    pub requests: Mutex<Vec<serde_json::Value>>,
}

impl MockTransport {
    pub fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
        Arc::new(Self { queue: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) })
    }

    pub fn ok(bodies: Vec<serde_json::Value>) -> Arc<Self> {
        Self::new(
            bodies.into_iter().map(|body| Ok(HttpResponse { status: 200, body })).collect(),
        )
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn post(
        &self,
        _url: &str,
        _headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<HttpResponse, TransportError> {
        self.requests.lock().unwrap().push(body.clone());
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(TransportError::Network("mock queue exhausted".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_pops_in_order_and_records_requests() {
        let mock = MockTransport::ok(vec![
            serde_json::json!({"n": 1}),
            serde_json::json!({"n": 2}),
        ]);
        let r1 = mock.post("http://x", &[], &serde_json::json!({"req": "a"})).await.unwrap();
        assert_eq!(r1.body["n"], 1);
        let r2 = mock.post("http://x", &[], &serde_json::json!({"req": "b"})).await.unwrap();
        assert_eq!(r2.body["n"], 2);
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
        assert_eq!(mock.requests.lock().unwrap()[0]["req"], "a");
        // exhausted queue is a network error, not a panic
        assert!(mock.post("http://x", &[], &serde_json::json!({})).await.is_err());
    }
}
