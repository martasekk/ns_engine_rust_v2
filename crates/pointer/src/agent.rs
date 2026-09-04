//! The agent side: everything that must be enforced where a caller cannot
//! reach it.
//!
//! The client limits what it sends. That is worth nothing as a guarantee —
//! anything that can open the socket can speak this protocol — so
//! authentication, the step cap, the rate limit, the local override, the
//! release of held keys and the audit log all live here, above `Platform`
//! and below the wire.
//!
//! Generic over `Platform`, so the same guards run against `NullPlatform` on
//! a machine with no display and against the real backend on Windows. An
//! agent written in another language reimplements this file; the protocol
//! document is its specification and `tests/agent.rs` is its conformance
//! suite.

use crate::platform::Platform;
use crate::wire::{
    Button, ErrorKind, InputError, Key, Op, Request, Response, ResultBody, Step, PROTOCOL,
};
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// An append-only record of what was actually injected. Mirrors the shape of
/// `nsllm::trace::Trace`: one JSON object per line, failures reported once and
/// never allowed to take a connection down.
pub trait Audit: Send + Sync {
    fn record(&self, entry: &serde_json::Value);
}

/// Discards. The default, and the wrong choice in production.
pub struct NoAudit;
impl Audit for NoAudit {
    fn record(&self, _entry: &serde_json::Value) {}
}

#[derive(Debug, Clone)]
pub struct Limits {
    /// A `perform` larger than this is refused outright. A 300ms eased move
    /// is ~75 steps; 4096 is generous and still bounded.
    pub max_steps: usize,
    /// Sustained `perform` calls per second, and how many may arrive at once.
    /// An agent in a loop should hit a wall, not flood the input queue.
    pub performs_per_sec: f64,
    pub burst: f64,
    /// How long remote input stays suspended after a human touches the
    /// machine. Long enough to finish a sentence.
    pub suspend_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 4096,
            performs_per_sec: 20.0,
            burst: 40.0,
            suspend_ms: 3_000,
        }
    }
}

pub struct AgentConfig {
    /// There is no unauthenticated mode. An empty token is a configuration
    /// error, not "auth off".
    pub token: String,
    pub limits: Limits,
}

/// Token bucket. Explicit clock so a test can prove the refill without
/// sleeping, the way `Engine::with_clock` does.
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

impl Bucket {
    fn take(&mut self, now_ms: u64, l: &Limits) -> bool {
        let dt = now_ms.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + dt * l.performs_per_sec).min(l.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Keys and buttons this connection has pressed and not released.
#[derive(Default)]
struct Held {
    keys: Vec<Key>,
    buttons: Vec<Button>,
}

impl Held {
    fn key(&mut self, k: &Key, down: bool) {
        if down {
            if !self.keys.contains(k) {
                self.keys.push(k.clone());
            }
        } else {
            self.keys.retain(|x| x != k);
        }
    }

    fn button(&mut self, b: Button, down: bool) {
        if down {
            if !self.buttons.contains(&b) {
                self.buttons.push(b);
            }
        } else {
            self.buttons.retain(|x| *x != b);
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

pub struct Agent<P: Platform> {
    platform: P,
    cfg: AgentConfig,
    audit: Box<dyn Audit>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    suspended_until: Mutex<u64>,
}

impl<P: Platform> Agent<P> {
    pub fn new(platform: P, cfg: AgentConfig) -> Self {
        Self {
            platform,
            cfg,
            audit: Box::new(NoAudit),
            clock: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            }),
            suspended_until: Mutex::new(0),
        }
    }

    pub fn with_audit(mut self, audit: Box<dyn Audit>) -> Self {
        self.audit = audit;
        self
    }

    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Serve one connection until it closes. Whatever the reason for
    /// leaving — clean close, parse failure, a dropped socket — every key and
    /// button this connection pressed is released on the way out.
    pub async fn serve<R, W>(&self, read: R, write: W) -> std::io::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(read).lines();
        let mut out = write;
        let mut authed = false;
        let mut held = Held::default();
        let mut bucket = Bucket {
            tokens: self.cfg.limits.burst,
            last_ms: (self.clock)(),
        };

        let result = loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Request>(&line) {
                Ok(req) => self.dispatch(req, &mut authed, &mut held, &mut bucket),
                // No id to answer against: say so and move on rather than
                // guessing which request this was.
                Err(e) => Response::err(0, ErrorKind::Protocol, e.to_string()),
            };
            let mut buf = serde_json::to_vec(&resp)?;
            buf.push(b'\n');
            if let Err(e) = out.write_all(&buf).await {
                break Err(e);
            }
            if let Err(e) = out.flush().await {
                break Err(e);
            }
        };

        self.release(&mut held, "disconnect");
        result
    }

    /// Release everything still held, innermost first. Best effort by
    /// definition: this runs when something has already gone wrong, and a
    /// failure here must not mask it.
    fn release(&self, held: &mut Held, why: &str) {
        if held.is_empty() {
            return;
        }
        self.audit.record(&serde_json::json!({
            "at": (self.clock)(),
            "event": "release_held",
            "why": why,
            "keys": held.keys.len(),
            "buttons": held.buttons.len(),
        }));
        for k in held.keys.drain(..).rev() {
            let _ = self.platform.key(&k, false);
        }
        for b in held.buttons.drain(..).rev() {
            let _ = self.platform.button(b, false);
        }
    }

    fn dispatch(
        &self,
        req: Request,
        authed: &mut bool,
        held: &mut Held,
        bucket: &mut Bucket,
    ) -> Response {
        let id = req.id;
        if let Op::Hello { token, protocol } = &req.op {
            if *protocol != PROTOCOL {
                return Response::err(
                    id,
                    ErrorKind::Protocol,
                    format!("protocol {protocol}, this agent speaks {PROTOCOL}"),
                );
            }
            // Length-independent comparison. The token is a shared secret and
            // an early-exit compare leaks its prefix to anything that can time
            // a reply.
            if self.cfg.token.is_empty() || !constant_time_eq(token, &self.cfg.token) {
                self.audit.record(&serde_json::json!({
                    "at": (self.clock)(), "event": "auth_failed",
                }));
                return Response::err(id, ErrorKind::Unauthorized, "bad token");
            }
            *authed = true;
            return Response::ok(
                id,
                ResultBody::Ready {
                    agent: concat!("ns-pointerd ", env!("CARGO_PKG_VERSION")).into(),
                    platform: std::env::consts::OS.into(),
                    protocol: PROTOCOL,
                },
            );
        }
        if !*authed {
            return Response::err(id, ErrorKind::Unauthorized, "no hello");
        }

        match req.op {
            Op::Hello { .. } => unreachable!("handled above"),
            Op::Screens => match self.platform.screens() {
                Ok(s) => Response::ok(
                    id,
                    ResultBody::Screens {
                        screens: s.screens,
                        state: s.state,
                    },
                ),
                Err(e) => err_response(id, e),
            },
            Op::Position => {
                let state = self.platform.screens().map(|s| s.state).unwrap_or(0);
                match self.platform.position() {
                    Ok(p) => Response::ok(
                        id,
                        ResultBody::Position {
                            x: p.x,
                            y: p.y,
                            state,
                        },
                    ),
                    Err(e) => err_response(id, e),
                }
            }
            Op::Perform { steps } => self.perform(id, steps, held, bucket),
        }
    }

    fn perform(&self, id: u64, steps: Vec<Step>, held: &mut Held, bucket: &mut Bucket) -> Response {
        let now = (self.clock)();

        // The person at the keyboard outranks the socket. Checked before the
        // rate limit so a suspended agent reports why rather than "slow down".
        if self.platform.local_activity() {
            *self.suspended_until.lock().unwrap() = now + self.cfg.limits.suspend_ms;
            self.release(held, "local_activity");
        }
        let until = *self.suspended_until.lock().unwrap();
        if now < until {
            return Response::err(
                id,
                ErrorKind::Suspended,
                format!("local override, {}ms remaining", until - now),
            );
        }

        if steps.len() > self.cfg.limits.max_steps {
            return Response::err(
                id,
                ErrorKind::Protocol,
                format!(
                    "{} steps exceeds the cap of {}",
                    steps.len(),
                    self.cfg.limits.max_steps
                ),
            );
        }
        if !bucket.take(now, &self.cfg.limits) {
            return Response::err(id, ErrorKind::Internal, "rate limit");
        }

        let n = steps.len() as u32;
        for step in &steps {
            if let Err(e) = self.apply(step, held) {
                self.audit.record(&serde_json::json!({
                    "at": now, "event": "refused", "step": step, "error": e.to_string(),
                }));
                // Whatever this batch had already pressed is still down, and
                // there is no second half of the batch coming to release it.
                self.release(held, "failed_perform");
                return err_response(id, e);
            }
        }
        self.audit.record(&serde_json::json!({
            "at": now, "event": "performed", "steps": n,
        }));
        let state = self.platform.screens().map(|s| s.state).unwrap_or(0);
        Response::ok(id, ResultBody::Performed { steps: n, state })
    }

    /// The step interpreter — the part that genuinely is a `match` in a loop.
    fn apply(&self, step: &Step, held: &mut Held) -> Result<(), InputError> {
        match step {
            Step::Move { x, y } => self.platform.move_to(crate::geom::Point::new(*x, *y)),
            Step::Button { button, down } => {
                self.platform.button(*button, *down)?;
                held.button(*button, *down);
                Ok(())
            }
            Step::Scroll { dx, dy } => self.platform.scroll(*dx, *dy),
            Step::Key { key, down } => {
                self.platform.key(key, *down)?;
                held.key(key, *down);
                Ok(())
            }
            Step::Text { text } => self.platform.text(text),
            Step::Sleep { ms } => {
                std::thread::sleep(std::time::Duration::from_millis(*ms as u64));
                Ok(())
            }
        }
    }
}

fn err_response(id: u64, e: InputError) -> Response {
    match e {
        InputError::Agent { kind, detail } => Response::err(id, kind, detail),
        other => Response::err(id, ErrorKind::Internal, other.to_string()),
    }
}

/// Compares every byte regardless of where they first differ.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}
