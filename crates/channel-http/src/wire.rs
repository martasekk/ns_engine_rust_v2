//! The JSON shapes a client sends and reads.
//!
//! Deliberately the same objects `ns-channel-tcp` puts on its lines, so a
//! client library written for one transport works on the other by changing
//! how it connects and nothing else.

/// A message from a client, on either the WebSocket or the JSON route.
#[derive(serde::Deserialize)]
pub(crate) struct Text {
    pub(crate) text: String,
}

/// A reply, out.
#[derive(serde::Serialize)]
pub(crate) struct Reply<'a> {
    pub(crate) session: &'a str,
    pub(crate) text: &'a str,
}

/// The body of a `POST /v1/messages`.
///
/// `session` is honoured only under the shared token, which proves nothing
/// and is loopback-only anyway; a verified credential derives the session
/// and this field is ignored. It is the same field, with the same rule, as
/// the hello's.
#[derive(serde::Deserialize)]
pub(crate) struct PostMessage {
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) session: Option<String>,
}
