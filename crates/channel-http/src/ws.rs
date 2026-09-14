//! The WebSocket handshake, and frames in both directions.
//!
//! A browser cannot open a TCP socket, so this is how a custom chat window
//! holds a session the way a desktop app holds one over `ns-channel-tcp`:
//! same vocabulary, different envelope. RFC 6455, and only the part of it a
//! chat window uses — text, ping, pong, close, and continuation frames for a
//! message that arrived in pieces. A binary frame is refused, because
//! nothing here would know what to do with one.
//!
//! Three bounds, each a way a peer could otherwise cost this process
//! something: a frame's declared length, a whole message's length after
//! continuations, and the deadline on the hello that
//! [`crate::chat`] applies to the first message. An unmasked client frame is
//! a protocol error and closes the connection — the mask is not security,
//! but a peer that omits it is not speaking WebSocket.

use base64::Engine as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// RFC 6455 §1.3. Concatenated with the client's key and hashed; it proves
/// only that the server understood the handshake.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The most one frame may declare. A chat message is a sentence.
pub(crate) const MAX_FRAME: u64 = 256 * 1024;
/// The most a message may come to once its continuations are joined, so a
/// peer cannot send a gigabyte sixteen kilobytes at a time.
pub(crate) const MAX_MESSAGE: usize = 256 * 1024;

/// The `Sec-WebSocket-Accept` value for a client's key.
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// What came off the wire, once continuations are joined.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Message {
    Text(String),
    /// The peer is going, or the connection must end: the reason is for the
    /// log, and the caller answers with a close of its own.
    Close,
    /// A ping, with the payload a pong must carry back. The reader does not
    /// answer it itself: one task owns the write half of a chat socket —
    /// the one draining this session's replies — and a pong written from
    /// here would interleave its bytes with a half-written reply frame.
    Ping(Vec<u8>),
    /// A pong, which is a peer answering our own ping. Nothing to do but
    /// note that the peer is alive.
    Pong,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WsError {
    #[error("a frame declared {0} bytes, past the bound")]
    FrameTooLarge(u64),
    #[error("a message came to more than {MAX_MESSAGE} bytes")]
    MessageTooLarge,
    #[error("a client frame arrived unmasked")]
    Unmasked,
    #[error("a binary frame: this channel speaks text")]
    Binary,
    #[error("opcode {0:#x} is not one this channel speaks")]
    BadOpcode(u8),
    #[error("a text frame that is not utf-8")]
    NotUtf8,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Reads one whole message, answering pings as it goes.
///
/// `Ok(None)` is the peer closing cleanly or the socket ending.
pub(crate) async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<Message>, WsError> {
    let mut assembled: Vec<u8> = Vec::new();
    loop {
        let mut head = [0u8; 2];
        match r.read_exact(&mut head).await {
            Ok(_) => {}
            // A peer that vanished mid-frame is a closed connection, not a
            // protocol violation worth a log line of its own.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0f;
        let masked = head[1] & 0x80 != 0;
        let len = match head[1] & 0x7f {
            126 => {
                let mut ext = [0u8; 2];
                r.read_exact(&mut ext).await?;
                u16::from_be_bytes(ext) as u64
            }
            127 => {
                let mut ext = [0u8; 8];
                r.read_exact(&mut ext).await?;
                u64::from_be_bytes(ext)
            }
            n => n as u64,
        };
        if len > MAX_FRAME {
            return Err(WsError::FrameTooLarge(len));
        }
        // Every frame from a client is masked (RFC 6455 §5.1). Reading one
        // that is not would mean guessing at the framing of everything
        // after it.
        if !masked {
            return Err(WsError::Unmasked);
        }
        let mut mask = [0u8; 4];
        r.read_exact(&mut mask).await?;
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload).await?;
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }

        match opcode {
            // Continuation: it belongs to the message being assembled.
            0x0 => {}
            0x1 => assembled.clear(),
            0x2 => return Err(WsError::Binary),
            0x8 => return Ok(Some(Message::Close)),
            0x9 => return Ok(Some(Message::Ping(payload))),
            0xa => return Ok(Some(Message::Pong)),
            other => return Err(WsError::BadOpcode(other)),
        }

        if assembled.len() + payload.len() > MAX_MESSAGE {
            return Err(WsError::MessageTooLarge);
        }
        assembled.extend_from_slice(&payload);
        if fin {
            let text = String::from_utf8(assembled).map_err(|_| WsError::NotUtf8)?;
            return Ok(Some(Message::Text(text)));
        }
    }
}

/// One frame out, never masked: a server does not mask (RFC 6455 §5.1).
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut head = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n <= u16::MAX as usize => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    w.write_all(&head).await?;
    w.write_all(payload).await?;
    w.flush().await
}

pub(crate) async fn write_text<W: AsyncWrite + Unpin>(
    w: &mut W,
    text: &str,
) -> std::io::Result<()> {
    write_frame(w, 0x1, text.as_bytes()).await
}

/// A close frame with a status code. Sent before the socket goes so the
/// browser's `onclose` carries a code rather than 1006.
pub(crate) async fn write_close<W: AsyncWrite + Unpin>(
    w: &mut W,
    code: u16,
) -> std::io::Result<()> {
    write_frame(w, 0x8, &code.to_be_bytes()).await
}

/// Close codes this channel uses. 1000 is a normal close; the rest say which
/// of our own rules the peer ran into, and none of them says *why* a
/// credential was refused.
pub(crate) mod close {
    pub(crate) const NORMAL: u16 = 1000;
    pub(crate) const PROTOCOL: u16 = 1002;
    pub(crate) const TOO_BIG: u16 = 1009;
    /// 4001-4999 are for the application. This one is "the hello was not
    /// accepted", whatever was wrong with it.
    pub(crate) const REFUSED: u16 = 4001;
    /// The shard is shutting down.
    pub(crate) const GOING_AWAY: u16 = 1001;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from RFC 6455 §1.3, which is the one thing in the
    /// handshake that can be checked against something other than our own
    /// implementation.
    #[test]
    fn the_accept_key_matches_the_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// A masked text frame, as every browser sends.
    fn masked_text(text: &str) -> Vec<u8> {
        let mask = [0xa1, 0xb2, 0xc3, 0xd4];
        let mut out = vec![0x81];
        let payload: Vec<u8> = text
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        out.push(0x80 | payload.len() as u8);
        out.extend_from_slice(&mask);
        out.extend_from_slice(&payload);
        out
    }

    #[tokio::test]
    async fn a_masked_text_frame_comes_back_as_its_text() {
        let mut r = std::io::Cursor::new(masked_text("{\"text\":\"hello\"}"));
        let msg = read_message(&mut r).await.expect("reads");
        assert_eq!(msg, Some(Message::Text("{\"text\":\"hello\"}".into())));
    }

    /// A message split across frames is one message, and the bound applies
    /// to the whole of it rather than to each piece.
    #[tokio::test]
    async fn continuation_frames_join_into_one_message() {
        let mask = [1u8, 2, 3, 4];
        let mut raw = Vec::new();
        for (opcode, fin, text) in [(0x1u8, false, "he"), (0x0, false, "ll"), (0x0, true, "o")] {
            let mut head = vec![if fin { 0x80 | opcode } else { opcode }];
            let payload: Vec<u8> = text
                .as_bytes()
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ mask[i % 4])
                .collect();
            head.push(0x80 | payload.len() as u8);
            head.extend_from_slice(&mask);
            head.extend_from_slice(&payload);
            raw.extend(head);
        }
        let mut r = std::io::Cursor::new(raw);
        assert_eq!(
            read_message(&mut r).await.expect("reads"),
            Some(Message::Text("hello".into()))
        );
    }

    #[tokio::test]
    async fn an_unmasked_client_frame_is_a_protocol_error() {
        // The same frame as `masked_text`, with the mask bit cleared.
        let mut raw = masked_text("hi");
        raw[1] &= 0x7f;
        let mut r = std::io::Cursor::new(raw);
        let err = read_message(&mut r).await.expect_err("refused");
        assert!(matches!(err, WsError::Unmasked), "{err:?}");
    }

    /// A peer that declares a huge frame is refused on the declaration, and
    /// never by allocating what it claimed.
    #[tokio::test]
    async fn an_oversized_frame_is_refused_on_its_declared_length() {
        let mut raw = vec![0x81, 0xff];
        raw.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        let mut r = std::io::Cursor::new(raw);
        let err = read_message(&mut r).await.expect_err("refused");
        assert!(matches!(err, WsError::FrameTooLarge(_)), "{err:?}");
    }

    #[tokio::test]
    /// The ping comes back with its payload, for the writing task to pong
    /// with: a chat socket has one writer, and a pong squeezed in from the
    /// reader would land inside a half-written reply.
    async fn a_ping_carries_its_payload_out_for_the_writer_to_answer() {
        let mask = [0u8; 4];
        let mut raw = vec![0x89, 0x82];
        raw.extend_from_slice(&mask);
        raw.extend_from_slice(b"hi");
        let mut r = std::io::Cursor::new(raw);
        assert_eq!(
            read_message(&mut r).await.expect("reads"),
            Some(Message::Ping(b"hi".to_vec()))
        );

        // And the frame the writer then sends is an unmasked pong.
        let mut out: Vec<u8> = Vec::new();
        write_frame(&mut out, 0xa, b"hi").await.expect("writes");
        assert_eq!(out, vec![0x8a, 0x02, b'h', b'i']);
    }

    #[tokio::test]
    async fn a_socket_that_ends_mid_frame_is_a_close_not_an_error() {
        let mut r = std::io::Cursor::new(vec![0x81]);
        assert_eq!(read_message(&mut r).await.expect("no error"), None);
    }
}
