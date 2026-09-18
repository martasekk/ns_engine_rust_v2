//! WhatsApp, through Meta's Cloud API: the first platform, and the shape of
//! any other.
//!
//! What makes it worth implementing rather than describing is that it breaks
//! the assumptions a socket bakes in, and breaking them here is what keeps
//! the seams honest:
//!
//! - **The sender's id is not a credential.** `from` is a phone number in a
//!   JSON body; anyone can type one. What is verified is the HMAC-SHA256
//!   over the raw body in `X-Hub-Signature-256`, and the company is taken
//!   from the *business* phone number id inside that verified payload.
//! - **A secret proves one account, not any account.** A shard serving two
//!   companies with their own Meta apps holds two secrets, and the signature
//!   is checked against all of them — so after the payload is parsed, the
//!   account it names must be the one whose secret matched. Without that
//!   check, any company on the shard could sign a message claiming to be any
//!   other.
//! - **A phone number must not become a session id.** The subject is
//!   `HMAC(tenant salt, wa_id)`, so the id the engine sees, stores facts
//!   under and writes into its log is opaque and does not survive being
//!   moved to another company. The number itself lives only in the reply
//!   path, in memory, for as long as the conversation is live (plan T6.4).
//! - **There is no connection to reply on.** A reply is a `POST` to
//!   Graph with the company's own access token.
//!
//! **The send window.** Meta only allows free-form business messages inside
//! a window after the customer's last message (24 hours at the time of
//! writing); outside it, a pre-approved template is required. Every reply
//! this adapter sends is an answer to a message that just arrived, so it is
//! inside the window by construction. Anything *proactive* — a follow-up, a
//! nudge, a scheduled message — is not, and must not be bolted onto this
//! path without the template API. Confirm the current rule against Meta's
//! documentation before building it.

use async_trait::async_trait;
use nscore::SessionId;
use nsidentity::hmac_sha256;

use crate::platform::{Arrival, Platform, Rejected, ReplyTo, ReplyTransport};

/// The Graph API version replies are sent to. Named so that moving to the
/// next one is a config change and not a code change.
pub const DEFAULT_GRAPH_VERSION: &str = "v21.0";
/// The channel segment in a session id: `<tenant>/whatsapp/<subject>`.
pub const CHANNEL: &str = "whatsapp";

/// One business account this shard answers for, and the company it belongs
/// to.
#[derive(Debug, Clone)]
pub struct Account {
    /// Meta's id for the business number. The only thing in a delivery that
    /// says which company it is for, and it is inside the signed payload.
    pub phone_number_id: String,
    pub tenant: String,
    /// The token replies are sent with. Never logged.
    pub access_token: String,
    /// The app secret Meta signs this account's deliveries with.
    pub app_secret: String,
    /// Per-company, so the same customer talking to two companies on this
    /// shard is two unrelated subjects. Never logged, never derived from the
    /// tenant id.
    pub session_salt: String,
}

#[derive(Debug, Clone)]
pub struct WhatsApp {
    accounts: Vec<Account>,
    /// Echoed back during Meta's one-off `GET` handshake.
    verify_token: String,
    graph_version: String,
}

impl WhatsApp {
    pub fn new(accounts: Vec<Account>, verify_token: String) -> WhatsApp {
        WhatsApp {
            accounts,
            verify_token,
            graph_version: DEFAULT_GRAPH_VERSION.to_string(),
        }
    }

    pub fn with_graph_version(mut self, version: impl Into<String>) -> WhatsApp {
        self.graph_version = version.into();
        self
    }

    fn account(&self, phone_number_id: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|a| a.phone_number_id == phone_number_id)
    }
}

#[async_trait]
impl Platform for WhatsApp {
    fn name(&self) -> &str {
        CHANNEL
    }

    fn accept(&self, headers: &[(String, String)], raw: &[u8]) -> Result<Vec<Arrival>, Rejected> {
        let signature = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-hub-signature-256"))
            .map(|(_, v)| v.trim())
            .ok_or(Rejected::Unsigned)?;
        let presented = signature
            .strip_prefix("sha256=")
            .ok_or(Rejected::BadSignature)?;
        if self.accounts.is_empty() {
            return Err(Rejected::NoSecret);
        }
        // Which secrets match. Checked before the body is parsed, and kept,
        // because a signature proves the account whose secret made it and
        // not merely that *somebody* on this shard made it.
        let signed_by: Vec<&str> = self
            .accounts
            .iter()
            .filter(|a| ct_eq_hex(presented, &hex(&hmac_sha256(a.app_secret.as_bytes(), raw))))
            .map(|a| a.phone_number_id.as_str())
            .collect();
        if signed_by.is_empty() {
            return Err(Rejected::BadSignature);
        }

        let payload: Payload =
            serde_json::from_slice(raw).map_err(|e| Rejected::Malformed(e.to_string()))?;
        let mut arrivals = Vec::new();
        for entry in payload.entry {
            for change in entry.changes {
                let value = change.value;
                let Some(metadata) = value.metadata else {
                    // A delivery receipt or a status change: no metadata we
                    // route on, and nothing a customer said.
                    continue;
                };
                let Some(account) = self.account(&metadata.phone_number_id) else {
                    return Err(Rejected::UnknownAccount {
                        account: metadata.phone_number_id,
                    });
                };
                // The account named must be the one whose secret signed
                // this. Otherwise one company on the shard could speak as
                // another simply by naming its number.
                if !signed_by.contains(&account.phone_number_id.as_str()) {
                    return Err(Rejected::BadSignature);
                }
                for message in value.messages {
                    // Text only for now. A photo or a location is a message
                    // this engine has no turn for, and guessing at a
                    // transcription of it would be worse than ignoring it.
                    let Some(text) = message.text.map(|t| t.body) else {
                        continue;
                    };
                    let at = message.timestamp.parse::<u64>().unwrap_or(0);
                    arrivals.push(Arrival {
                        tenant: account.tenant.clone(),
                        session: session_for(account, &message.from),
                        text,
                        message_id: message.id,
                        at,
                        reply_to: ReplyTo {
                            url: format!(
                                "https://graph.facebook.com/{}/{}/messages",
                                self.graph_version, account.phone_number_id
                            ),
                            headers: vec![(
                                "authorization".into(),
                                format!("Bearer {}", account.access_token),
                            )],
                            to: message.from,
                        },
                    });
                }
            }
        }
        Ok(arrivals)
    }

    fn verification(&self, query: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        // All three, and the token compared in constant time: this endpoint
        // is public and the token is a secret.
        (query("hub.mode").as_deref() == Some("subscribe")
            && query("hub.verify_token")
                .is_some_and(|t| ct_eq(t.as_bytes(), self.verify_token.as_bytes())))
        .then(|| query("hub.challenge"))
        .flatten()
    }

    async fn reply(
        &self,
        transport: &dyn ReplyTransport,
        to: &ReplyTo,
        text: &str,
    ) -> Result<(), String> {
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to.to,
            "type": "text",
            "text": { "preview_url": false, "body": text },
        });
        let (status, answered) = transport.post_json(&to.url, &to.headers, &body).await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        // The status is what a retry is decided on; Meta's own error message
        // is what makes the log line useful. Neither reaches the customer.
        Err(format!("graph answered {status}: {answered}"))
    }
}

/// `<tenant>/whatsapp/<opaque>`, where the opaque part is the customer id
/// under this company's salt. Two companies on one shard therefore see two
/// unrelated subjects for the same person, and neither sees the number.
fn session_for(account: &Account, customer_id: &str) -> SessionId {
    let digest = hex(&hmac_sha256(
        account.session_salt.as_bytes(),
        customer_id.as_bytes(),
    ));
    // 128 bits of it. Long enough that two customers of one company will not
    // collide, short enough to read in a log line, and `[0-9a-f]` is inside
    // the charset a session segment may use (`nsidentity::valid_claim`).
    let subject: String = digest.chars().take(32).collect();
    SessionId(format!("{}/{CHANNEL}/{subject}", account.tenant))
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant time over two byte strings. Length is not a secret here — both
/// sides are fixed-width digests or a configured token — but the comparison
/// still must not stop at the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The same, for two hex strings, case-insensitively: Meta sends lower case
/// and a proxy in between has been known to change it.
fn ct_eq_hex(a: &str, b: &str) -> bool {
    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    ct_eq(a.as_bytes(), b.as_bytes())
}

// The part of Meta's payload this adapter reads. Everything else in it is
// ignored by omission, which is what keeps a change on their side from
// breaking a delivery that carries a field we never wanted.
#[derive(serde::Deserialize)]
struct Payload {
    #[serde(default)]
    entry: Vec<Entry>,
}

#[derive(serde::Deserialize)]
struct Entry {
    #[serde(default)]
    changes: Vec<Change>,
}

#[derive(serde::Deserialize)]
struct Change {
    value: ChangeValue,
}

#[derive(serde::Deserialize)]
struct ChangeValue {
    metadata: Option<Metadata>,
    #[serde(default)]
    messages: Vec<WaMessage>,
}

#[derive(serde::Deserialize)]
struct Metadata {
    phone_number_id: String,
}

#[derive(serde::Deserialize)]
struct WaMessage {
    from: String,
    id: String,
    /// Unix seconds, as a string. Meta sends it quoted.
    #[serde(default)]
    timestamp: String,
    text: Option<WaText>,
}

#[derive(serde::Deserialize)]
struct WaText {
    body: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::MockReplyTransport;

    fn account(tenant: &str, number: &str, secret: &str) -> Account {
        Account {
            phone_number_id: number.into(),
            tenant: tenant.into(),
            access_token: format!("{tenant}-token"),
            app_secret: secret.into(),
            session_salt: format!("{tenant}-salt"),
        }
    }

    fn delivery(number: &str, from: &str, id: &str, at: &str, text: &str) -> String {
        serde_json::json!({
            "object": "whatsapp_business_account",
            "entry": [{
                "id": "WABA",
                "changes": [{
                    "field": "messages",
                    "value": {
                        "messaging_product": "whatsapp",
                        "metadata": { "display_phone_number": "1555", "phone_number_id": number },
                        "messages": [{
                            "from": from,
                            "id": id,
                            "timestamp": at,
                            "type": "text",
                            "text": { "body": text },
                        }],
                    },
                }],
            }],
        })
        .to_string()
    }

    fn signed(secret: &str, body: &str) -> Vec<(String, String)> {
        vec![(
            "X-Hub-Signature-256".into(),
            format!(
                "sha256={}",
                hex(&hmac_sha256(secret.as_bytes(), body.as_bytes()))
            ),
        )]
    }

    fn shard() -> WhatsApp {
        WhatsApp::new(
            vec![
                account("acme", "111", "acme-secret"),
                account("globex", "222", "globex-secret"),
            ],
            "verify-me".into(),
        )
    }

    #[test]
    fn a_payload_with_a_bad_signature_is_refused_before_it_is_parsed() {
        let wa = shard();
        let body = delivery("111", "15551234", "wamid.1", "1700000000", "hello");
        // Right shape, wrong secret.
        let err = wa
            .accept(&signed("not-the-secret", &body), body.as_bytes())
            .expect_err("refused");
        assert!(matches!(err, Rejected::BadSignature), "{err:?}");

        // And an unsigned delivery is refused as unsigned rather than read.
        let err = wa.accept(&[], body.as_bytes()).expect_err("refused");
        assert!(matches!(err, Rejected::Unsigned), "{err:?}");
    }

    /// The signature must prove *this* account, not any account the shard
    /// happens to hold a secret for. Without the check, a company on the
    /// shard could sign a payload naming another company's number and speak
    /// as it.
    #[test]
    fn a_payload_signed_by_another_tenants_secret_is_refused() {
        let wa = shard();
        let body = delivery("111", "15551234", "wamid.1", "1700000000", "hello");
        let err = wa
            .accept(&signed("globex-secret", &body), body.as_bytes())
            .expect_err("refused");
        assert!(matches!(err, Rejected::BadSignature), "{err:?}");
    }

    #[test]
    fn two_tenants_on_one_endpoint_are_separated_by_phone_number_id() {
        let wa = shard();
        for (number, secret, tenant) in [
            ("111", "acme-secret", "acme"),
            ("222", "globex-secret", "globex"),
        ] {
            let body = delivery(number, "15551234", "wamid.1", "1700000000", "hello");
            let arrivals = wa
                .accept(&signed(secret, &body), body.as_bytes())
                .expect("accepted");
            assert_eq!(arrivals.len(), 1);
            assert_eq!(arrivals[0].tenant, tenant);
            assert!(
                arrivals[0]
                    .session
                    .0
                    .starts_with(&format!("{tenant}/whatsapp/")),
                "{}",
                arrivals[0].session.0
            );
        }
    }

    /// A number is a person's identity everywhere else; here it must reach
    /// neither the session id (which is the fact scope key) nor anything
    /// downstream of it.
    #[test]
    fn a_customer_id_never_appears_in_the_session_id() {
        let wa = shard();
        let number = "15551234567";
        let body = delivery("111", number, "wamid.1", "1700000000", "hello");
        let arrivals = wa
            .accept(&signed("acme-secret", &body), body.as_bytes())
            .expect("accepted");
        let session = &arrivals[0].session.0;
        assert!(!session.contains(number), "{session}");
        assert!(!session.contains("5551"), "{session}");
        // The reply path is the one place it exists, and it is in memory.
        assert_eq!(arrivals[0].reply_to.to, number);

        // The same person at another company is another subject entirely.
        let other = delivery("222", number, "wamid.2", "1700000000", "hello");
        let theirs = wa
            .accept(&signed("globex-secret", &other), other.as_bytes())
            .expect("accepted");
        let a = session.rsplit('/').next().expect("a subject");
        let b = theirs[0].session.0.rsplit('/').next().expect("a subject");
        assert_ne!(a, b, "one salt per company");
    }

    #[test]
    fn a_delivery_naming_an_account_nobody_configured_is_refused() {
        let wa = WhatsApp::new(vec![account("acme", "111", "s")], "v".into());
        let body = delivery("999", "15551234", "wamid.1", "1700000000", "hello");
        let err = wa
            .accept(&signed("s", &body), body.as_bytes())
            .expect_err("refused");
        assert!(matches!(err, Rejected::UnknownAccount { .. }), "{err:?}");
    }

    /// Status callbacks and non-text messages carry no turn. They are
    /// ignored rather than refused: refusing them would make Meta retry
    /// something that will never be accepted.
    #[test]
    fn a_status_callback_is_accepted_and_carries_no_message() {
        let wa = shard();
        let body = serde_json::json!({
            "entry": [{ "changes": [{ "value": {
                "metadata": { "phone_number_id": "111" },
                "statuses": [{ "id": "wamid.1", "status": "delivered" }],
            }}]}],
        })
        .to_string();
        let arrivals = wa
            .accept(&signed("acme-secret", &body), body.as_bytes())
            .expect("accepted");
        assert!(arrivals.is_empty());
    }

    #[test]
    fn the_verification_handshake_echoes_the_challenge_only_for_the_right_token() {
        let wa = shard();
        let query = |mode: &str, token: &str| {
            let (mode, token) = (mode.to_string(), token.to_string());
            move |key: &str| match key {
                "hub.mode" => Some(mode.clone()),
                "hub.verify_token" => Some(token.clone()),
                "hub.challenge" => Some("1158201444".to_string()),
                _ => None,
            }
        };
        assert_eq!(
            wa.verification(&query("subscribe", "verify-me")).as_deref(),
            Some("1158201444")
        );
        assert_eq!(wa.verification(&query("subscribe", "guess")), None);
        assert_eq!(wa.verification(&query("unsubscribe", "verify-me")), None);
    }

    #[tokio::test]
    async fn a_reply_is_a_graph_call_with_the_companys_own_token() {
        let wa = shard();
        let body = delivery("111", "15551234", "wamid.1", "1700000000", "hello");
        let arrivals = wa
            .accept(&signed("acme-secret", &body), body.as_bytes())
            .expect("accepted");
        let transport = MockReplyTransport::new(vec![Ok((200, serde_json::json!({})))]);
        wa.reply(transport.as_ref(), &arrivals[0].reply_to, "hi there")
            .await
            .expect("sent");
        let sent = transport.sent();
        assert_eq!(sent.len(), 1);
        let (url, sent_body) = &sent[0];
        assert!(url.contains("/111/messages"), "{url}");
        assert_eq!(sent_body["to"], "15551234");
        assert_eq!(sent_body["text"]["body"], "hi there");
        assert_eq!(sent_body["messaging_product"], "whatsapp");
        assert_eq!(
            arrivals[0].reply_to.headers,
            vec![("authorization".to_string(), "Bearer acme-token".to_string())],
        );
    }

    /// An error from Graph is an error here, so the sender retries it rather
    /// than treating a 401 as a delivered message.
    #[tokio::test]
    async fn a_graph_error_is_reported_rather_than_swallowed() {
        let wa = shard();
        let to = ReplyTo {
            url: "https://graph.facebook.com/v21.0/111/messages".into(),
            headers: Vec::new(),
            to: "15551234".into(),
        };
        let transport = MockReplyTransport::new(vec![Ok((
            401,
            serde_json::json!({ "error": { "message": "token expired" } }),
        ))]);
        let err = wa
            .reply(transport.as_ref(), &to, "hi")
            .await
            .expect_err("an error");
        assert!(err.contains("401"), "{err}");
    }
}
