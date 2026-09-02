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
        Self {
            client: reqwest::Client::new(),
        }
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
        let mut map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| TransportError::Network(format!("invalid header name {k}: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(v).map_err(|e| {
                TransportError::Network(format!("invalid header value for {k}: {e}"))
            })?;
            map.insert(name, value);
        }
        // `.json()` already sets content-type. `.headers()` replaces
        // same-named headers rather than appending (unlike `.header()`), so a
        // caller-supplied content-type can't produce a duplicate — strict
        // providers (Mistral) reject the body as a string on duplicates.
        let req = self.client.post(url).json(body).headers(map);
        let resp = req
            .send()
            .await
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| TransportError::BadBody(e.to_string()))?;
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
        Arc::new(Self {
            queue: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub fn ok(bodies: Vec<serde_json::Value>) -> Arc<Self> {
        Self::new(
            bodies
                .into_iter()
                .map(|body| Ok(HttpResponse { status: 200, body }))
                .collect(),
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

    /// One-shot HTTP server on a std thread: captures the raw request and
    /// answers `{}`. No tokio `net` feature needed.
    fn one_shot_server() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= head_end + 4 + len || n == 0 {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            s.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .unwrap();
            tx.send(String::from_utf8_lossy(&raw).to_string()).unwrap();
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn reqwest_transport_sends_content_type_exactly_once() {
        // `.json()` already sets content-type; a caller-supplied one must
        // replace it, not duplicate it — Mistral answers 422 on duplicates.
        let (addr, rx) = one_shot_server();
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-title".to_string(), "ns-harness".to_string()),
        ];
        let resp = ReqwestTransport::new()
            .post(
                &format!("http://{addr}/v1/chat/completions"),
                &headers,
                &serde_json::json!({"a": 1}),
            )
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let raw = rx.recv().unwrap();
        let content_types = raw
            .lines()
            .filter(|l| l.to_ascii_lowercase().starts_with("content-type:"))
            .count();
        assert_eq!(content_types, 1, "raw request:\n{raw}");
        assert!(raw.to_ascii_lowercase().contains("x-title: ns-harness"));
    }

    #[tokio::test]
    async fn mock_pops_in_order_and_records_requests() {
        let mock = MockTransport::ok(vec![
            serde_json::json!({"n": 1}),
            serde_json::json!({"n": 2}),
        ]);
        let r1 = mock
            .post("http://x", &[], &serde_json::json!({"req": "a"}))
            .await
            .unwrap();
        assert_eq!(r1.body["n"], 1);
        let r2 = mock
            .post("http://x", &[], &serde_json::json!({"req": "b"}))
            .await
            .unwrap();
        assert_eq!(r2.body["n"], 2);
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
        assert_eq!(mock.requests.lock().unwrap()[0]["req"], "a");
        // exhausted queue is a network error, not a panic
        assert!(mock
            .post("http://x", &[], &serde_json::json!({}))
            .await
            .is_err());
    }
}
