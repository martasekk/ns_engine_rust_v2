//! What a platform is, as far as this shard is concerned.
//!
//! A platform — WhatsApp, Slack, Telegram, a customer's own back end — does
//! not hold a socket open. It posts when a user says something and expects a
//! `200` in a moment, and a reply goes back as a call to *its* API some
//! seconds later. That is the whole reason webhooks are a separate shape
//! here rather than a second flavour of the chat route: the trust chain, the
//! ordering and the outbound path are all different.
//!
//! **The trust chain is the part to get right.** The user id in the payload
//! is an *identifier*, not a credential: anyone can write one. What is
//! verified is the signature over the raw bytes, with the secret only the
//! platform and this shard know. The company is then taken from *inside the
//! verified payload* — the business account the message was addressed to —
//! and never from anything the sender chose. An adapter that reads the
//! tenant from an unverified field has handed one company's conversation to
//! whoever asked for it.
//!
//! An adapter is therefore both halves of one platform: the resolver at the
//! edge (`nsidentity`'s job for a token, an adapter's for a signature) and
//! the way back out.

use std::sync::Arc;

use async_trait::async_trait;
use nscore::SessionId;

/// One message a platform delivered, once its signature checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    /// Which company. From the verified payload, never from a header or a
    /// query parameter.
    pub tenant: String,
    /// Already shaped `<tenant>/<channel>/<subject>` by the adapter, with
    /// the subject an opaque per-tenant hash. A phone number must never
    /// become a session id or a fact scope key (plan T6.4).
    pub session: SessionId,
    pub text: String,
    /// The platform's own id for this message. The dedupe key: a retry
    /// carries the same one, and running the turn twice would reply twice
    /// and bill twice (plan H4).
    pub message_id: String,
    /// The platform's timestamp, unix seconds. A batch may arrive out of
    /// order and is sorted by this before anything is enqueued (plan T6.3).
    pub at: u64,
    /// Where a reply to this message goes. Carried per arrival because a
    /// platform's send target is per conversation, and because it is what
    /// the outbound sink needs and the engine must never see.
    pub reply_to: ReplyTo,
}

/// The address a reply is sent to, in the platform's own terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    /// The full URL to post to.
    pub url: String,
    /// Headers, the platform's access token among them. Never logged.
    pub headers: Vec<(String, String)>,
    /// The conversation, in whatever the platform calls it: a phone number,
    /// a channel id, a chat id.
    pub to: String,
}

/// Why a delivery was not accepted. Like `nsidentity::Denied`, these say
/// exactly what happened *in a log line* and are never echoed to the caller:
/// telling a forger whether the signature or the tenant was wrong hands them
/// an oracle.
#[derive(Debug, thiserror::Error)]
pub enum Rejected {
    #[error("no signature header")]
    Unsigned,
    #[error("the signature does not match the body")]
    BadSignature,
    #[error("no secret is configured for this platform")]
    NoSecret,
    #[error("the payload is not this platform's shape: {0}")]
    Malformed(String),
    #[error("the payload names business account {account:?}, which no tenant claims")]
    UnknownAccount { account: String },
}

/// One platform, both halves: what it delivers, and how a reply goes back.
#[async_trait]
pub trait Platform: Send + Sync {
    /// The last segment of the route, so a shard serving three platforms
    /// gives each its own URL: `/hooks/whatsapp`, `/hooks/slack`.
    fn name(&self) -> &str;

    /// Verify, then read. **Verification is over `raw` and nothing else**: a
    /// body that has been through a JSON round trip will not hash, so an
    /// implementation must not parse first and hash after.
    ///
    /// Returns every message the delivery carried, in whatever order it
    /// arrived; the caller sorts and de-duplicates.
    fn accept(&self, headers: &[(String, String)], raw: &[u8]) -> Result<Vec<Arrival>, Rejected>;

    /// The one-off handshake some platforms do before they will deliver
    /// anything: a `GET` carrying a challenge to echo. `None` means this
    /// platform has no such step, and the route answers 404.
    fn verification(&self, _query: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        None
    }

    /// One reply, out over the platform's API. The transport is injected so
    /// this is testable without the network (plan T6.5).
    async fn reply(
        &self,
        transport: &dyn ReplyTransport,
        to: &ReplyTo,
        text: &str,
    ) -> Result<(), String>;
}

/// The way out to a platform's API.
///
/// A deliberate small sibling of `nscomponents_std::ToolTransport`, for the
/// same reason that one is a sibling of `ns-llm`'s: these crates depend on
/// `ns-core` and not on each other. It differs in carrying headers, because
/// every platform API authenticates with one.
#[async_trait]
pub trait ReplyTransport: Send + Sync {
    /// (status, body) on a completed exchange; `Err` on a network failure.
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String>;
}

pub struct ReqwestReplyTransport {
    client: reqwest::Client,
}

impl ReqwestReplyTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestReplyTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ReplyTransport for ReqwestReplyTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String> {
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        // A platform that answers an error with prose rather than JSON is
        // still an answer, and the status is what the caller retries on.
        let body = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok((status, body))
    }
}

/// Test double: records what would have been sent, answers what it was
/// queued with.
pub struct MockReplyTransport {
    pub responses:
        std::sync::Mutex<std::collections::VecDeque<Result<(u16, serde_json::Value), String>>>,
    pub sent: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl MockReplyTransport {
    pub fn new(responses: Vec<Result<(u16, serde_json::Value), String>>) -> Arc<Self> {
        Arc::new(Self {
            responses: std::sync::Mutex::new(responses.into()),
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn sent(&self) -> Vec<(String, serde_json::Value)> {
        self.sent.lock().expect("a test's own lock").clone()
    }
}

#[async_trait]
impl ReplyTransport for MockReplyTransport {
    async fn post_json(
        &self,
        url: &str,
        _headers: &[(String, String)],
        body: &serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String> {
        self.sent
            .lock()
            .expect("a test's own lock")
            .push((url.to_string(), body.clone()));
        self.responses
            .lock()
            .expect("a test's own lock")
            .pop_front()
            .unwrap_or(Ok((200, serde_json::Value::Null)))
    }
}
