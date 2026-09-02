use crate::transport::{HttpTransport, TransportError};
use std::sync::Arc;

pub struct OpenRouterClient {
    transport: Arc<dyn HttpTransport>,
    api_key: String,
    base_url: String,
    max_attempts: u32,
    backoff_base_ms: u64,
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
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
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
            match self.transport.post(&url, &headers, &request).await {
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
