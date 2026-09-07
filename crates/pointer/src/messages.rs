//! The client half of the messages service: the owner's way to talk back.
//!
//! Everything else across this seam flows one way. A client moves the pointer,
//! types, reads the tree; the person at the machine watches a badge and, if it
//! goes wrong, reaches for the pie menu. The pie is a brake, not a channel.
//! There has been no way for the owner to *tell* the model anything — not "the
//! dialog you want is on the second monitor", not "stop, that is the wrong
//! file", not "yes, that one".
//!
//! The Windows side built that channel and has been waiting for a reader: a
//! second chord (`ctrl+shift+T` by default) opens a one-line box on the
//! overlay, and a committed line goes into an in-memory queue and is appended
//! to `outbox.jsonl`. Until this module nothing ever collected it. A line
//! typed on 2026-09-07 sat in that file with no `take` ever issued against the
//! agent, and from the owner's side "I sent it and nothing happened" was the
//! whole experience.
//!
//! This is a separate service on its own port (7374 beside the pointer's
//! 7373), not an op on the pointer protocol, because that is how the Windows
//! side shipped it. One line of JSON in, one line out.
//!
//! **The queue is memory, not the file.** `take` drains what the agent holds
//! in memory and leaves `outbox.jsonl` alone; the file is the owner's record,
//! not a mailbox. An agent that restarts starts with an empty queue even
//! though the file still has every line, and this client does not read the
//! file — replaying it would deliver messages the owner typed in some earlier
//! session, at whatever moment the process happened to come back.

use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// How long any single request may take. The service answers from memory, so
/// this is only ever a broken connection or a hung agent.
const TIMEOUT: Duration = Duration::from_secs(5);

/// One line the owner typed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Message {
    /// RFC 3339, from the machine that took it down.
    pub at: String,
    pub text: String,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Body>,
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Body {
    Ready { protocol: u32 },
    Messages { messages: Vec<Message> },
    Said,
}

#[derive(Deserialize)]
struct ErrorBody {
    detail: String,
}

/// A connection to one agent's messages service.
#[derive(Debug)]
pub struct Messages {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
    next_id: u64,
}

impl Messages {
    /// Dial and authenticate. The token is the agent's, the same one the
    /// pointer connection uses: one secret per machine.
    pub async fn connect(addr: &str, token: &str) -> Result<Messages, String> {
        let dial = TcpStream::connect(addr);
        let stream = tokio::time::timeout(TIMEOUT, dial)
            .await
            .map_err(|_| format!("no answer from the messages service at {addr} within 5s"))?
            .map_err(|e| format!("cannot reach the messages service at {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let (r, w) = stream.into_split();
        let mut m = Messages {
            read: BufReader::new(r),
            write: w,
            next_id: 1,
        };
        match m
            .call(&format!(
                r#"{{"op":"hello","token":{}}}"#,
                json_string(token)
            ))
            .await?
        {
            Body::Ready { protocol: 1 } => Ok(m),
            Body::Ready { protocol } => Err(format!(
                "the messages service at {addr} speaks protocol {protocol}, this client speaks 1"
            )),
            _ => Err(format!("{addr} answered hello with something else")),
        }
    }

    /// Take everything queued, emptying the agent's queue.
    ///
    /// An empty vector is the normal answer and is not an error: most polls
    /// find nothing.
    pub async fn take(&mut self) -> Result<Vec<Message>, String> {
        match self.call(r#"{"op":"take"}"#).await? {
            Body::Messages { messages } => Ok(messages),
            _ => Err("take was answered with something else".into()),
        }
    }

    /// Put a line on the machine's badge, where the owner is already looking.
    pub async fn say(&mut self, text: &str) -> Result<(), String> {
        match self
            .call(&format!(r#"{{"op":"say","text":{}}}"#, json_string(text)))
            .await?
        {
            Body::Said => Ok(()),
            _ => Err("say was answered with something else".into()),
        }
    }

    /// One request, one response. `body` is the object without its `id`, which
    /// is added here so ids stay this connection's business.
    async fn call(&mut self, body: &str) -> Result<Body, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = format!("{{\"id\":{id},{}", &body[1..]);

        tokio::time::timeout(TIMEOUT, async {
            self.write.write_all(line.as_bytes()).await?;
            self.write.write_all(b"\n").await?;
            self.write.flush().await
        })
        .await
        .map_err(|_| "the messages service did not accept the request within 5s".to_string())?
        .map_err(|e| format!("writing to the messages service: {e}"))?;

        let mut answer = String::new();
        let n = tokio::time::timeout(TIMEOUT, self.read.read_line(&mut answer))
            .await
            .map_err(|_| "the messages service did not answer within 5s".to_string())?
            .map_err(|e| format!("reading from the messages service: {e}"))?;
        if n == 0 {
            return Err("the messages service closed the connection".into());
        }

        let env: Envelope = serde_json::from_str(answer.trim())
            .map_err(|e| format!("the messages service sent something unreadable: {e}"))?;
        if !env.ok {
            return Err(env
                .error
                .map(|e| e.detail)
                .unwrap_or_else(|| "refused, with no reason given".into()));
        }
        env.result
            .ok_or_else(|| "an ok answer with no result".to_string())
    }
}

/// A JSON string literal. `serde_json` does the escaping, including the
/// `\uXXXX` forms the service's hand-rolled parser expects to be able to
/// decode.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// A stand-in service that answers each line from a script and records
    /// what it was asked.
    async fn service(answers: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (r, mut w) = socket.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut asked = Vec::new();
            for answer in answers {
                match lines.next_line().await.unwrap() {
                    Some(line) => asked.push(line),
                    None => break,
                }
                w.write_all(answer.as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
            }
            asked
        });
        (addr, handle)
    }

    const READY: &str =
        r#"{"id":1,"ok":true,"result":{"kind":"ready","service":"messages","protocol":1}}"#;

    #[tokio::test]
    async fn a_queued_line_comes_back_with_its_timestamp() {
        let (addr, h) = service(vec![
            READY.into(),
            r#"{"id":2,"ok":true,"result":{"kind":"messages","messages":[{"at":"2026-09-07T14:13:27Z","text":"move my mouse to the top right"}],"remaining":0}}"#.into(),
        ])
        .await;
        let mut m = Messages::connect(&addr, "tok").await.expect("connects");
        let got = m.take().await.expect("takes");
        assert_eq!(
            got,
            vec![Message {
                at: "2026-09-07T14:13:27Z".into(),
                text: "move my mouse to the top right".into(),
            }]
        );
        let asked = h.await.unwrap();
        assert!(asked[0].contains(r#""op":"hello""#), "{}", asked[0]);
        assert!(asked[0].contains(r#""token":"tok""#), "{}", asked[0]);
        assert!(asked[1].contains(r#""op":"take""#), "{}", asked[1]);
    }

    /// The common case by far: nothing typed since the last poll.
    #[tokio::test]
    async fn an_empty_queue_is_not_an_error() {
        let (addr, _h) = service(vec![
            READY.into(),
            r#"{"id":2,"ok":true,"result":{"kind":"messages","messages":[],"remaining":0}}"#.into(),
        ])
        .await;
        let mut m = Messages::connect(&addr, "tok").await.unwrap();
        assert_eq!(m.take().await.unwrap(), vec![]);
    }

    /// The reply goes back to the badge, and the text is escaped by
    /// `serde_json` rather than by hand: the service's parser refuses a
    /// malformed escape outright rather than guessing.
    #[tokio::test]
    async fn say_escapes_what_it_sends() {
        let (addr, h) = service(vec![
            READY.into(),
            r#"{"id":2,"ok":true,"result":{"kind":"said"}}"#.into(),
        ])
        .await;
        let mut m = Messages::connect(&addr, "tok").await.unwrap();
        m.say("a \"quoted\" line\nand Zrušit").await.expect("says");
        let asked = h.await.unwrap();
        assert!(asked[1].contains(r#"\"quoted\""#), "{}", asked[1]);
        assert!(asked[1].contains(r#"\n"#), "{}", asked[1]);
        assert!(
            !asked[1].contains('\n') || asked[1].lines().count() == 1,
            "a raw newline would split the request in two: {}",
            asked[1]
        );
    }

    #[tokio::test]
    async fn a_refusal_carries_its_reason() {
        let (addr, _h) = service(vec![
            READY.into(),
            r#"{"id":2,"ok":false,"error":{"kind":"refused","detail":"unauthorized"}}"#.into(),
        ])
        .await;
        let mut m = Messages::connect(&addr, "tok").await.unwrap();
        let e = m.take().await.expect_err("refused");
        assert!(e.contains("unauthorized"), "{e}");
    }

    /// A version this client cannot read is said plainly rather than limped
    /// along with.
    #[tokio::test]
    async fn a_protocol_this_client_does_not_speak_is_refused_at_hello() {
        let (addr, _h) = service(vec![
            r#"{"id":1,"ok":true,"result":{"kind":"ready","service":"messages","protocol":9}}"#
                .into(),
        ])
        .await;
        let e = Messages::connect(&addr, "tok").await.expect_err("refused");
        assert!(e.contains("protocol 9"), "{e}");
    }

    #[tokio::test]
    async fn a_bad_token_is_reported_from_hello() {
        let (addr, _h) = service(vec![
            r#"{"id":1,"ok":false,"error":{"kind":"refused","detail":"bad token"}}"#.into(),
        ])
        .await;
        let e = Messages::connect(&addr, "tok").await.expect_err("refused");
        assert!(e.contains("bad token"), "{e}");
    }

    #[test]
    fn ids_are_this_connections_business() {
        // `call` splices the id in ahead of the body's own fields; the body
        // never carries one.
        let body = r#"{"op":"take"}"#;
        let spliced = format!("{{\"id\":{},{}", 7, &body[1..]);
        assert_eq!(spliced, r#"{"id":7,"op":"take"}"#);
    }
}
