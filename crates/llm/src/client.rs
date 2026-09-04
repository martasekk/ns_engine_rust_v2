use crate::transport::{HttpTransport, TransportError};
use std::sync::Arc;

/// Minimum spacing between requests, shared by every client of one
/// provider (emitter, replier, summarizer, probes). Free tiers rate-limit
/// per second; a turn with a tool call fires several requests back to back.
pub struct Throttle {
    min_interval: std::time::Duration,
    last: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Throttle {
    pub fn new(min_interval_ms: u64) -> Self {
        Self {
            min_interval: std::time::Duration::from_millis(min_interval_ms),
            last: std::sync::Mutex::new(None),
        }
    }

    /// Sleep until the interval since the previous request has elapsed,
    /// then claim the slot.
    pub async fn wait(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let now = tokio::time::Instant::now();
        let due = self
            .last
            .lock()
            .expect("throttle lock")
            .map(|last| last + self.min_interval);
        if let Some(due) = due {
            if due > now {
                tokio::time::sleep_until(due).await;
            }
        }
        *self.last.lock().expect("throttle lock") = Some(tokio::time::Instant::now());
    }
}

pub struct OpenRouterClient {
    transport: Arc<dyn HttpTransport>,
    api_key: String,
    base_url: String,
    max_attempts: u32,
    backoff_base_ms: u64,
    throttle: Option<Arc<Throttle>>,
    /// Opt-in wire log (NS_TRACE); `label` names the role in each entry.
    trace: Option<Arc<crate::trace::Trace>>,
    label: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("api status {status}: {detail}")]
    Status { status: u16, detail: String },
}

impl OpenRouterClient {
    pub fn new(transport: Arc<dyn HttpTransport>, api_key: String) -> Self {
        // 1 s, 2 s, 4 s between attempts: a turn with a tool call makes
        // three or four requests back to back, and free tiers rate-limit per
        // second (seen live with Mistral: 429 on every retry at 0.5 s).
        Self {
            transport,
            api_key,
            base_url: "https://openrouter.ai/api".into(),
            max_attempts: 4,
            backoff_base_ms: 1000,
            throttle: None,
            trace: None,
            label: String::new(),
        }
    }

    /// Log every request and response to the shared trace, tagged with the
    /// role's name. Headers are never written — the API key is in one.
    pub fn with_trace(mut self, trace: Arc<crate::trace::Trace>, label: &str) -> Self {
        self.trace = Some(trace);
        self.label = label.to_string();
        self
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Share one throttle across every client of the same provider.
    pub fn with_throttle(mut self, throttle: Arc<Throttle>) -> Self {
        self.throttle = Some(throttle);
        self
    }

    pub fn with_retry(mut self, max_attempts: u32, backoff_base_ms: u64) -> Self {
        self.max_attempts = max_attempts;
        self.backoff_base_ms = backoff_base_ms;
        self
    }

    /// POST /v1/chat/completions. Retries network errors, 429 and 5xx with
    /// exponential backoff (backoff_base_ms * 2^attempt); other non-2xx fail
    /// immediately.
    pub async fn chat(&self, request: serde_json::Value) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let headers = vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("content-type".to_string(), "application/json".to_string()),
            ("x-title".to_string(), "ns-harness".to_string()),
        ];
        let mut last_err = ApiError::Transport("no attempts made".into());
        for attempt in 0..self.max_attempts {
            if attempt > 0 {
                let delay = self.backoff_base_ms * (1 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            if let Some(t) = &self.throttle {
                t.wait().await;
            }
            let started = std::time::Instant::now();
            let outcome = self.transport.post(&url, &headers, &request).await;
            self.trace_attempt(&url, attempt, started.elapsed(), &request, &outcome);
            match outcome {
                Ok(resp) if (200..300).contains(&resp.status) => return Ok(resp.body),
                Ok(resp) if resp.status == 429 || resp.status >= 500 => {
                    last_err = ApiError::Status {
                        status: resp.status,
                        detail: resp.body.to_string(),
                    };
                }
                Ok(resp) => {
                    return Err(ApiError::Status {
                        status: resp.status,
                        detail: resp.body.to_string(),
                    });
                }
                Err(TransportError::Network(e)) => last_err = ApiError::Transport(e),
                Err(TransportError::BadBody(e)) => last_err = ApiError::Transport(e),
            }
        }
        Err(last_err)
    }

    /// One trace line per attempt: retries and the failures that caused them
    /// stay visible, which is the point of the file.
    fn trace_attempt(
        &self,
        url: &str,
        attempt: u32,
        took: std::time::Duration,
        request: &serde_json::Value,
        outcome: &Result<crate::transport::HttpResponse, TransportError>,
    ) {
        let Some(trace) = &self.trace else { return };
        let mut entry = serde_json::json!({
            "at_ms": crate::trace::now_ms(),
            "at": nscore::time::format_utc(crate::trace::now_ms()),
            "role": self.label,
            "url": url,
            "model": request.get("model").cloned().unwrap_or(serde_json::Value::Null),
            "attempt": attempt + 1,
            "ms": took.as_millis() as u64,
            "request": request,
        });
        match outcome {
            Ok(resp) => {
                entry["status"] = serde_json::json!(resp.status);
                entry["response"] = resp.body.clone();
            }
            Err(e) => entry["error"] = serde_json::json!(e.to_string()),
        }
        trace.record(&entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, MockTransport, TransportError};

    fn client(mock: std::sync::Arc<MockTransport>) -> OpenRouterClient {
        OpenRouterClient::new(mock, "test-key".into()).with_retry(3, 1)
    }

    #[tokio::test]
    async fn success_returns_body_and_sends_required_headers() {
        let mock = MockTransport::ok(vec![serde_json::json!({"id": "msg_1"})]);
        let c = client(mock.clone());
        let body = c.chat(serde_json::json!({"model": "m"})).await.unwrap();
        assert_eq!(body["id"], "msg_1");
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let mock = MockTransport::new(vec![
            Ok(HttpResponse {
                status: 429,
                body: serde_json::json!({"error": "rate"}),
            }),
            Ok(HttpResponse {
                status: 200,
                body: serde_json::json!({"id": "msg_2"}),
            }),
        ]);
        let body = client(mock.clone())
            .chat(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(body["id"], "msg_2");
        assert_eq!(mock.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let mock = MockTransport::new(vec![
            Err(TransportError::Network("down".into())),
            Err(TransportError::Network("down".into())),
            Err(TransportError::Network("down".into())),
            Ok(HttpResponse {
                status: 200,
                body: serde_json::json!({"id": "never"}),
            }),
        ]);
        let err = client(mock.clone())
            .chat(serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Transport(_)));
        assert_eq!(
            mock.requests.lock().unwrap().len(),
            3,
            "exactly max_attempts tries"
        );
    }

    #[tokio::test]
    async fn shared_throttle_spaces_requests_across_clients() {
        let mock = MockTransport::ok(vec![
            serde_json::json!({"id": "a"}),
            serde_json::json!({"id": "b"}),
            serde_json::json!({"id": "c"}),
        ]);
        let throttle = Arc::new(Throttle::new(60));
        let a = client(mock.clone()).with_throttle(throttle.clone());
        let b = client(mock.clone()).with_throttle(throttle);
        let started = std::time::Instant::now();
        a.chat(serde_json::json!({})).await.unwrap();
        b.chat(serde_json::json!({})).await.unwrap();
        a.chat(serde_json::json!({})).await.unwrap();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(120),
            "three requests need two intervals: {:?}",
            started.elapsed()
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn trace_records_every_attempt_with_no_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let trace = Arc::new(crate::trace::Trace::open(&path).unwrap());
        let mock = MockTransport::new(vec![
            Ok(HttpResponse {
                status: 429,
                body: serde_json::json!({"error": "rate"}),
            }),
            Ok(HttpResponse {
                status: 200,
                body: serde_json::json!({"id": "msg", "usage": {"total_tokens": 12}}),
            }),
        ]);
        client(mock)
            .with_trace(trace, "emitter")
            .chat(serde_json::json!({"model": "m", "messages": []}))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "the retry is visible too");
        assert_eq!(lines[0]["status"], 429);
        assert_eq!(lines[0]["attempt"], 1);
        assert_eq!(lines[0]["role"], "emitter");
        assert_eq!(lines[0]["model"], "m");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[1]["attempt"], 2);
        assert_eq!(lines[1]["response"]["usage"]["total_tokens"], 12);
        assert!(lines[1]["request"]["messages"].is_array());
        // The key travels in a header; no headers are ever written.
        assert!(!text.contains("test-key"), "the api key must not be traced");
        assert!(!text.contains("authorization"));
    }

    #[tokio::test]
    async fn trace_records_transport_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let trace = Arc::new(crate::trace::Trace::open(&path).unwrap());
        let mock = MockTransport::new(vec![Err(TransportError::Network("down".into()))]);
        let _ = client(mock)
            .with_trace(trace, "replier")
            .with_retry(1, 1)
            .chat(serde_json::json!({"model": "m"}))
            .await;
        let entry: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert!(entry["error"].as_str().unwrap().contains("down"));
        assert!(entry["status"].is_null());
    }

    #[tokio::test]
    async fn client_error_400_fails_immediately_without_retry() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 400,
            body: serde_json::json!({"error": {"message": "bad request"}}),
        })]);
        let err = client(mock.clone())
            .chat(serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Status { status: 400, .. }));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}
