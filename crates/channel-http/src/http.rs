//! One HTTP/1.1 request in, one response out, under bounds.
//!
//! Small on purpose. This crate serves four routes to callers it has already
//! decided to distrust — a browser widget, a desktop app, a platform's
//! webhook — so what it needs from HTTP is a method, a target, a few headers
//! and a body whose length was declared up front. Everything else is
//! refused rather than supported: a request with no `Content-Length` on a
//! method that carries a body, a chunked body, a header block past the
//! bound, a body past the bound. Each refusal is a status, never a panic and
//! never an unbounded read.
//!
//! What is *not* here is as deliberate: no routing (that is [`crate::server`]),
//! no TLS (a reverse proxy's job, and said again in the crate docs), no
//! compression, no keep-alive pipelining beyond one request at a time.

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// The request line, at most. A target longer than this is a caller that has
/// mistaken this for a URL shortener.
const MAX_REQUEST_LINE: usize = 8 * 1024;
/// The whole header block.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// How many headers, so a caller cannot send ten thousand short ones.
const MAX_HEADERS: usize = 64;

/// Why a request was not served. Each carries the status it answers with:
/// the caller learns that it was refused and not why in any detail, which is
/// the same discipline `nsidentity::Denied` keeps for credentials.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HttpError {
    #[error("the request line or header block ran past its bound")]
    TooLarge,
    #[error("the body is longer than this endpoint accepts")]
    BodyTooLarge,
    #[error("malformed request")]
    Malformed,
    #[error("a body without a content-length is not accepted")]
    LengthRequired,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl HttpError {
    pub(crate) fn status(&self) -> u16 {
        match self {
            HttpError::TooLarge => 431,
            HttpError::BodyTooLarge => 413,
            HttpError::Malformed => 400,
            HttpError::LengthRequired => 411,
            HttpError::Io(_) => 400,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    /// The raw target, query string and all.
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    /// The bytes exactly as they arrived. A webhook signature is over these
    /// and not over a re-serialization of what they parsed into: a body that
    /// has been through a JSON round trip will not hash (plan T6.1).
    pub(crate) body: Vec<u8>,
    pub(crate) keep_alive: bool,
}

impl Request {
    /// A header by name, matched case-insensitively as HTTP requires. The
    /// first wins; a repeated header is a caller trying its luck.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The path, without the query string.
    pub(crate) fn path(&self) -> &str {
        match self.target.split_once('?') {
            Some((path, _)) => path,
            None => &self.target,
        }
    }

    /// One query parameter, percent-decoded. Used by the platform
    /// verification handshakes, which are a `GET` with everything in the
    /// query and nothing in the body.
    pub(crate) fn query(&self, key: &str) -> Option<String> {
        let (_, query) = self.target.split_once('?')?;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (percent_decode(k) == key).then(|| percent_decode(v))
        })
    }

    /// True when the client asked for an upgrade to WebSocket. Both headers
    /// are required by RFC 6455 and both are checked: `Upgrade` names the
    /// protocol, `Connection` says it is an upgrade at all.
    pub(crate) fn is_websocket_upgrade(&self) -> bool {
        let upgrade = self
            .header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let connection = self.header("connection").is_some_and(|v| {
            v.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
        });
        upgrade && connection
    }
}

/// Reads one request. `Ok(None)` is a clean end of connection — the peer
/// went between requests, which on a keep-alive socket is the normal way it
/// ends and not an error.
pub(crate) async fn read_request<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_body: usize,
) -> Result<Option<Request>, HttpError> {
    let mut line = String::new();
    let read = (&mut *reader)
        .take(MAX_REQUEST_LINE as u64)
        .read_line(&mut line)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err(HttpError::TooLarge);
    }
    let mut parts = line.trim_end().split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Malformed);
    }
    let http_10 = version == "HTTP/1.0";

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = (&mut *reader)
            .take((MAX_HEADER_BYTES - header_bytes.min(MAX_HEADER_BYTES)) as u64 + 1)
            .read_line(&mut line)
            .await?;
        if read == 0 {
            return Err(HttpError::Malformed);
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES || headers.len() > MAX_HEADERS {
            return Err(HttpError::TooLarge);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(HttpError::Malformed);
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let request = Request {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        body: Vec::new(),
        keep_alive: !http_10,
    };
    // A body whose length nobody declared is not read: chunked would mean
    // reading until a terminator this crate does not implement, and no
    // terminator at all would mean reading until the peer felt like
    // stopping.
    if request
        .header("transfer-encoding")
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        return Err(HttpError::LengthRequired);
    }
    let len = match request.header("content-length") {
        Some(v) => v
            .trim()
            .parse::<usize>()
            .map_err(|_| HttpError::Malformed)?,
        None => 0,
    };
    if len > max_body {
        return Err(HttpError::BodyTooLarge);
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).await?;
    }
    let keep_alive = match request.header("connection") {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => request.keep_alive,
    };

    Ok(Some(Request {
        body,
        keep_alive,
        ..request
    }))
}

/// What goes back. Built by the routes, written by the connection loop.
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) content_type: &'static str,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn json(status: u16, value: &serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub(crate) fn text(status: u16, body: impl Into<String>) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.into().into_bytes(),
        }
    }

    /// A refusal the caller learns nothing from. Every route answers a bad
    /// credential, an unknown company and a bad signature with one of these:
    /// which of the three it was belongs in the log, not on the wire.
    pub(crate) fn refused(status: u16) -> Response {
        Response::json(
            status,
            &serde_json::json!({ "error": reason_phrase(status) }),
        )
    }

    pub(crate) fn with_header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub(crate) async fn write_to<W: AsyncWrite + Unpin>(
        &self,
        w: &mut W,
        keep_alive: bool,
    ) -> std::io::Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: {}\r\n",
            self.status,
            reason_phrase(self.status),
            self.content_type,
            self.body.len(),
            if keep_alive { "keep-alive" } else { "close" },
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        w.write_all(head.as_bytes()).await?;
        w.write_all(&self.body).await?;
        w.flush().await
    }
}

pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

/// `%xx` and `+`, which is all a query string of ours ever holds.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &str, max_body: usize) -> Result<Option<Request>, HttpError> {
        let mut reader = BufReader::new(std::io::Cursor::new(raw.as_bytes().to_vec()));
        read_request(&mut reader, max_body).await
    }

    #[tokio::test]
    async fn a_post_is_read_with_its_body_exactly_as_it_arrived() {
        let raw = "POST /hooks/whatsapp HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let req = parse(raw, 1024).await.expect("parses").expect("a request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/hooks/whatsapp");
        // Byte for byte: a signature is taken over this and not over a
        // re-serialization of it.
        assert_eq!(req.body, b"{\"a\":1}");
        assert_eq!(req.header("HOST"), Some("x"), "headers match either case");
    }

    #[tokio::test]
    async fn a_body_past_the_bound_is_refused_rather_than_buffered() {
        let raw = "POST /x HTTP/1.1\r\nContent-Length: 9999\r\n\r\n";
        let err = parse(raw, 16).await.expect_err("refused");
        assert_eq!(err.status(), 413);
    }

    /// Chunked is not implemented, so it is refused rather than half-read:
    /// the alternative is a body that does not match its signature, or a
    /// read with no end.
    #[tokio::test]
    async fn a_chunked_body_is_refused_and_not_guessed_at() {
        let raw = "POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        let err = parse(raw, 1024).await.expect_err("refused");
        assert_eq!(err.status(), 411);
    }

    #[tokio::test]
    async fn a_clean_end_of_connection_is_not_an_error() {
        assert!(parse("", 16).await.expect("no error").is_none());
    }

    #[tokio::test]
    async fn the_query_is_decoded_and_the_path_excludes_it() {
        let raw = "GET /hooks/whatsapp?hub.mode=subscribe&hub.challenge=a%20b HTTP/1.1\r\n\r\n";
        let req = parse(raw, 16).await.expect("parses").expect("a request");
        assert_eq!(req.path(), "/hooks/whatsapp");
        assert_eq!(req.query("hub.mode").as_deref(), Some("subscribe"));
        assert_eq!(req.query("hub.challenge").as_deref(), Some("a b"));
        assert_eq!(req.query("absent"), None);
    }

    #[tokio::test]
    async fn an_upgrade_is_recognised_only_with_both_headers() {
        let with =
            "GET /chat HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Keep-Alive, Upgrade\r\n\r\n";
        assert!(parse(with, 16)
            .await
            .expect("parses")
            .expect("a request")
            .is_websocket_upgrade());
        let without = "GET /chat HTTP/1.1\r\nUpgrade: websocket\r\n\r\n";
        assert!(!parse(without, 16)
            .await
            .expect("parses")
            .expect("a request")
            .is_websocket_upgrade());
    }
}
