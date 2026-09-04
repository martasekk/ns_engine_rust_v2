//! `RemotePointer`: a `Pointer` backed by an agent at the other end of a
//! stream. The client half of `docs/pointer-protocol.md`.

use crate::geom::{Point, Screens};
use crate::wire::{ErrorKind, InputError, Op, Request, Response, ResultBody, Step, PROTOCOL};
use crate::Pointer;
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

pub struct RemotePointer<R, W> {
    io: Mutex<(R, W)>,
    next_id: std::sync::atomic::AtomicU64,
}

impl<R, W> RemotePointer<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// Connects and authenticates. A `RemotePointer` that exists has said
    /// hello and been answered, so no other call has to wonder.
    pub async fn connect(read: R, write: W, token: &str) -> Result<Self, InputError> {
        let p = Self {
            io: Mutex::new((read, write)),
            next_id: std::sync::atomic::AtomicU64::new(1),
        };
        match p
            .call(Op::Hello {
                token: token.to_string(),
                protocol: PROTOCOL,
            })
            .await?
        {
            ResultBody::Ready { protocol, .. } if protocol == PROTOCOL => Ok(p),
            ResultBody::Ready { protocol, .. } => Err(InputError::Transport(format!(
                "agent speaks protocol {protocol}, this client speaks {PROTOCOL}"
            ))),
            other => Err(InputError::Transport(format!(
                "unexpected hello reply: {other:?}"
            ))),
        }
    }

    async fn call(&self, op: Op) -> Result<ResultBody, InputError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // One lock for the whole exchange: this protocol pairs by id, but a
        // single connection is still a single ordered stream, and interleaving
        // two writes would corrupt it.
        let mut io = self.io.lock().await;
        let (read, write) = &mut *io;

        let mut buf = serde_json::to_vec(&Request { id, op })
            .map_err(|e| InputError::Transport(e.to_string()))?;
        buf.push(b'\n');
        write
            .write_all(&buf)
            .await
            .map_err(|e| InputError::Transport(e.to_string()))?;
        write
            .flush()
            .await
            .map_err(|e| InputError::Transport(e.to_string()))?;

        let mut line = String::new();
        let n = read
            .read_line(&mut line)
            .await
            .map_err(|e| InputError::Transport(e.to_string()))?;
        if n == 0 {
            return Err(InputError::Transport("agent closed the connection".into()));
        }
        let resp: Response =
            serde_json::from_str(&line).map_err(|e| InputError::Transport(e.to_string()))?;
        if resp.id != id {
            return Err(InputError::Transport(format!(
                "reply id {} for request {id}",
                resp.id
            )));
        }
        match (resp.ok, resp.result, resp.error) {
            (true, Some(r), _) => Ok(r),
            (false, _, Some(e)) => Err(e.into()),
            _ => Err(InputError::Agent {
                kind: ErrorKind::Protocol,
                detail: "response was neither a result nor an error".into(),
            }),
        }
    }
}

#[async_trait]
impl<R, W> Pointer for RemotePointer<R, W>
where
    R: AsyncBufRead + Unpin + Send + Sync,
    W: AsyncWrite + Unpin + Send + Sync,
{
    async fn screens(&self) -> Result<Screens, InputError> {
        match self.call(Op::Screens).await? {
            ResultBody::Screens { screens, state } => Ok(Screens { screens, state }),
            other => Err(unexpected(other)),
        }
    }

    async fn position(&self) -> Result<Point, InputError> {
        match self.call(Op::Position).await? {
            ResultBody::Position { x, y, .. } => Ok(Point::new(x, y)),
            other => Err(unexpected(other)),
        }
    }

    async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
        match self
            .call(Op::Perform {
                steps: steps.to_vec(),
            })
            .await?
        {
            ResultBody::Performed { state, .. } => Ok(state),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(body: ResultBody) -> InputError {
    InputError::Agent {
        kind: ErrorKind::Protocol,
        detail: format!("unexpected reply: {body:?}"),
    }
}
