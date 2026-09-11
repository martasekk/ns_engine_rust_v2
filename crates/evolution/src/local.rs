//! The local scorer (M8 T2.3): the evaluation lane, paid for on loopback.
//!
//! `~/models` (nsmodels) serves a multilingual bi-encoder and a cross-encoder
//! reranker on `127.0.0.1:7374`. This is the client, and it does exactly two
//! things the symbolic layer cannot:
//!
//! - **Re-ask across vocabulary and across languages.** T2.1's lexical band
//!   caught seven re-asks in the recorded session and missed two, and both
//!   misses were the same shape: turn 15 asked in Czech what turn 14 asked in
//!   English, and turn 16 was turn 15 mangled by the console past the Jaccard
//!   threshold. Neither is reachable by comparing tokens. bge-m3 is
//!   multilingual by construction, which is the whole reason this is worth a
//!   network call.
//! - **Relevance of a reply to its question.** I5 by token overlap was
//!   measured at 0 for 2 on the recorded session, and both false positives
//!   were a *correct* answer that happened not to reuse the question's words
//!   ("what time is it?" answered with a time). A cross-encoder scores the
//!   pair rather than intersecting it.
//!
//! **What it deliberately does not do.** Grounding stays with
//! `nsengine::ground` — span attribution is "which words of this reply are
//! unsupported", and an encoder answers "are these two texts alike", which is
//! a different question wearing the same clothes. I6 stays structural: two
//! logged facts need no model. Adding a model to either would be spending a
//! dependency on a decision that was already exact.
//!
//! **And it decides nothing.** M6 §13 is not relaxed because the model is
//! local and free: output enters as a signal, becomes a candidate only above
//! `evaluator_min_kappa`, and never enters the guard chain or a turn.
//!
//! ## Thresholds, and why they are not constants
//!
//! Both signals reduce to a score and a cut. That is a fit, and the 2026
//! cross-dataset audit (`docs/research/2026-09-09-local-evaluator-findings.md`
//! §1) is blunt about what happens to a fit that is never validated on target
//! data: a clean MNLI scorer runs 0.904 AUROC on one dataset and 0.531 —
//! chance — on another, and choosing a metric by its average carried 0.172
//! AUROC of regret. So the cuts are configuration, they are chosen on the
//! development half of `nstestkit::grading` and nowhere else, and the number
//! anyone quotes comes from the held-out half.
//!
//! One asymmetry between the two is worth stating, because it looks like an
//! inconsistency. nsmodels' own measurement is that bi-encoder cosine ranges
//! are compressed — a true match at 0.898 against 0.862 for an unrelated
//! Czech sentence — and its rule is therefore *rank, never threshold*. A
//! cross-encoder is not that: it scores a pair jointly and its output is
//! spread, which is why it can carry a cut where the embedder is only used
//! for a margin between two texts that are both about the same turn.
use crate::evaluate::{
    ignores_question, reask_band, Evaluator, GradeError, SymbolicEvaluator, TurnGrade, TurnView,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Debug, Clone)]
pub struct LocalConfig {
    pub base_url: String,
    pub timeout_ms: u64,
    /// Cosine at or above which the follow-up is the same question again.
    ///
    /// Chosen on the development half. bge-m3 puts unrelated Czech sentences
    /// near 0.86 by nsmodels' own probe, so anything under about 0.88 would
    /// call every pair of sentences a re-ask.
    pub reask_cosine: f32,
    /// Cross-encoder score below which the reply is not about the question.
    pub relevance_cut: f32,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:7374".into(),
            timeout_ms: 2000,
            reask_cosine: 0.90,
            relevance_cut: 0.0,
        }
    }
}

/// How a request failed, in the two classes that get different treatment.
///
/// Temporal's distinction (`2026-09-08-local-retrieval-and-lane-findings` §2.2)
/// and the reason T2.3b exists: retrying a refused connection twelve times
/// with backoff spends the whole idle window rediscovering what the first
/// refusal already established.
enum Wire {
    /// Nothing is listening, or the request could not be made at all.
    Refused(String),
    /// It answered, badly — a timeout, a 5xx, or a body that will not parse.
    Transient(String),
    /// It answered with something structurally wrong. Never retried.
    Malformed(String),
}

/// Whether a request failed because nothing was listening.
///
/// `reqwest::Error::is_connect` is the documented answer and is not the whole
/// one: the classification that matters here is the operating system's, and it
/// arrives as an `io::Error` several layers down a `source()` chain. So the
/// chain is walked and the io error asked directly.
///
/// Matching on the message is not an option and the reason is recorded here
/// rather than rediscovered: Windows localises it. This box answers a refused
/// connection with "Nemohlo byt vytvoreno zadne pripojeni, protoze cilovy
/// pocitac je aktivne odmitl. (os error 10061)", so a scorer that keyed on
/// "refused" would retry every failure on a Czech machine and none on an
/// English one.
fn is_refused(e: &reqwest::Error) -> bool {
    if e.is_connect() {
        return true;
    }
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = src {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) {
                return true;
            }
        }
        src = std::error::Error::source(err);
    }
    false
}

pub struct LocalEvaluator {
    cfg: LocalConfig,
    client: reqwest::Client,
    symbolic: SymbolicEvaluator,
    /// Set by the first refused connection. Once set, every later call in
    /// this pass reports `Unavailable` without dialling — the non-retryable
    /// half of T2.3b's table.
    disabled: AtomicBool,
    /// Calls in a row that exhausted their retry without reaching the
    /// service. The other way a dead service presents itself; see
    /// [`LocalEvaluator::give_up_after`].
    consecutive_transient: AtomicU32,
}

impl LocalEvaluator {
    pub fn new(cfg: LocalConfig, symbolic: SymbolicEvaluator) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(cfg.timeout_ms))
            .build()
            .unwrap_or_default();
        Self {
            cfg,
            client,
            symbolic,
            disabled: AtomicBool::new(false),
            consecutive_transient: AtomicU32::new(0),
        }
    }

    /// Consecutive unreachable calls after which the lane takes itself out
    /// for the rest of the pass, the same way a refusal does immediately.
    ///
    /// T2.3b was written around `connection refused`, on the assumption that
    /// a dead service refuses. **On this box it does not.** Dialling a closed
    /// loopback port here yields `is_timeout`, with no `ConnectionRefused`
    /// anywhere in the error's source chain — Windows drops rather than
    /// resets — so the refusal branch never fires and the timeout branch
    /// retries, politely, once per turn, forever.
    ///
    /// The arithmetic is the argument: at the 2 s default and one retry, a
    /// hundred-turn pass against a service that is merely *off* would spend
    /// four hundred seconds discovering it, which is precisely the waste the
    /// retry table exists to prevent. Two is enough — one call could be a
    /// genuine stall under load, two in a row is a service that is not there.
    const fn give_up_after() -> u32 {
        2
    }

    /// Whether the lane has taken itself out for the rest of this pass.
    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, Wire> {
        let url = format!("{}{path}", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if is_refused(&e) {
                    Wire::Refused(e.to_string())
                } else {
                    Wire::Transient(e.to_string())
                }
            })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Wire::Transient(e.to_string()))?;
        if status.is_server_error() {
            return Err(Wire::Transient(format!("HTTP {status}: {text}")));
        }
        if !status.is_success() {
            // A 4xx is the service saying the request was wrong, or that the
            // model it needs was never loaded (`--rerank` omitted). Both are
            // defects to see, not conditions to wait out.
            return Err(Wire::Malformed(format!("HTTP {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| Wire::Malformed(format!("{e}: {text}")))
    }

    /// One retry for a transient failure, none for a refusal, none for a
    /// malformed answer — T2.3b's table, in the one place it is applied.
    async fn call(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, GradeError> {
        if self.is_disabled() {
            return Err(GradeError::Unavailable(
                "lane disabled after a refused connection".into(),
            ));
        }
        let first = self.post(path, body.clone()).await;
        let outcome = match first {
            Ok(v) => Ok(v),
            // A refusal is settled on the first answer: nothing is listening,
            // and asking again cannot change that.
            Err(Wire::Refused(e)) => Err(self.give_up(format!("connection refused: {e}"))),
            Err(Wire::Malformed(e)) => Err(GradeError::Invalid(e)),
            // A timeout or a 5xx gets exactly one retry.
            Err(Wire::Transient(_)) => match self.post(path, body).await {
                Ok(v) => Ok(v),
                Err(Wire::Refused(e)) => Err(self.give_up(format!("connection refused: {e}"))),
                Err(Wire::Malformed(e)) => Err(GradeError::Invalid(e)),
                Err(Wire::Transient(e)) => Err(self.note_unreachable(e)),
            },
        };
        if outcome.is_ok() {
            self.consecutive_transient.store(0, Ordering::Relaxed);
        }
        outcome
    }

    /// Take the lane out for the rest of the pass and say why.
    fn give_up(&self, why: String) -> GradeError {
        self.disabled.store(true, Ordering::Relaxed);
        GradeError::Unavailable(why)
    }

    /// A call that never reached the service. Disables the lane once these
    /// stop looking like bad luck — see [`LocalEvaluator::give_up_after`].
    fn note_unreachable(&self, why: String) -> GradeError {
        let n = self.consecutive_transient.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= Self::give_up_after() {
            return self.give_up(format!("unreachable {n} calls running: {why}"));
        }
        GradeError::Unavailable(why)
    }

    /// Cosine between two texts, both embedded as queries.
    ///
    /// Both as queries deliberately: a re-ask is question against question,
    /// and E5-family models want the matching prefix on both sides of a
    /// symmetric comparison. nsmodels applies the right prefix per model, so
    /// the only thing this has to get right is not to mix the two kinds.
    pub async fn similarity(&self, a: &str, b: &str) -> Result<f32, GradeError> {
        let v = self
            .call(
                "/embed",
                serde_json::json!({"texts": [a, b], "kind": "query"}),
            )
            .await?;
        let rows = v
            .get("vectors")
            .and_then(|x| x.as_array())
            .ok_or_else(|| GradeError::Invalid("no vectors in /embed reply".into()))?;
        if rows.len() != 2 {
            return Err(GradeError::Invalid(format!(
                "/embed returned {} vectors for 2 texts",
                rows.len()
            )));
        }
        let vec_of = |i: usize| -> Result<Vec<f32>, GradeError> {
            rows[i]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_f64())
                        .map(|x| x as f32)
                        .collect()
                })
                .ok_or_else(|| GradeError::Invalid("a vector was not an array".into()))
        };
        let (x, y) = (vec_of(0)?, vec_of(1)?);
        if x.len() != y.len() || x.is_empty() {
            return Err(GradeError::Invalid("vector lengths disagree".into()));
        }
        // nsmodels normalises at encode time, so the dot product is cosine.
        Ok(x.iter().zip(&y).map(|(p, q)| p * q).sum())
    }

    /// The cross-encoder's score for one document against a query.
    pub async fn relevance(&self, query: &str, doc: &str) -> Result<f32, GradeError> {
        let v = self
            .call(
                "/rerank",
                serde_json::json!({"query": query, "docs": [doc], "k": 1}),
            )
            .await?;
        v.get("ranked")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .and_then(|r| r.get("score"))
            .and_then(|s| s.as_f64())
            .map(|s| s as f32)
            .ok_or_else(|| GradeError::Invalid("no score in /rerank reply".into()))
    }
}

/// The two scores a grade is made of, before any cut is applied.
///
/// Exposed so the cuts can be *chosen* rather than guessed: one pass over the
/// corpus collects these, and every candidate threshold is then arithmetic.
/// Without it a sweep would be one HTTP round trip per case per grid point,
/// which is slow enough that nobody would run it, which is how a guessed
/// constant survives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawScores {
    /// Cosine between the turn's question and the follow-up, when there is a
    /// follow-up.
    pub follow_up_cosine: Option<f32>,
    /// Cross-encoder score for the reply against the question.
    pub relevance: f32,
    /// The structural findings, which need no service and no cut.
    pub ignored_request: bool,
    pub ungrounded: bool,
    /// Whether the lexical band already called this a re-ask. The cut only
    /// ever *adds* re-asks, so a sweep must know which were already free.
    pub lexical_reask: bool,
}

impl LocalEvaluator {
    /// Everything the service can say about a turn, in one pass.
    pub async fn raw(&self, view: &TurnView<'_>) -> Result<RawScores, GradeError> {
        let (ignored_request, spans) = self.symbolic.structural(view);
        let lexical_reask = view
            .next_user
            .and_then(|n| reask_band(view.user, n, self.symbolic.cfg.reask_jaccard))
            .is_some();
        let follow_up_cosine = match view.next_user {
            Some(next) => Some(self.similarity(view.user, next).await?),
            None => None,
        };
        Ok(RawScores {
            follow_up_cosine,
            relevance: self.relevance(view.user, view.reply).await?,
            ignored_request,
            ungrounded: !spans.is_empty(),
            lexical_reask,
        })
    }
}

impl RawScores {
    /// The issue these scores imply under a given pair of cuts. Pure, so a
    /// sweep is arithmetic over a single collection pass.
    pub fn issue_at(&self, reask_cosine: f32, relevance_cut: f32) -> crate::evaluate::Issue {
        let reask = self.lexical_reask || self.follow_up_cosine.is_some_and(|c| c >= reask_cosine);
        SymbolicEvaluator::resolve(
            self.ignored_request,
            self.ungrounded,
            reask,
            self.relevance < relevance_cut,
        )
    }
}

#[async_trait::async_trait]
impl Evaluator for LocalEvaluator {
    fn id(&self) -> String {
        "local".into()
    }

    /// The two fitted cuts, exactly as configured. They are what changes when
    /// a sweep is re-run, so they are what a recorded grade has to carry: a
    /// metric whose ranking inverts between datasets will certainly move when
    /// its cuts do (`2026-09-09-local-evaluator-findings.md` §1).
    fn revision(&self) -> String {
        format!(
            "reask_cosine={};relevance_cut={}",
            self.cfg.reask_cosine, self.cfg.relevance_cut
        )
    }

    async fn grade(&self, view: &TurnView<'_>) -> Result<TurnGrade, GradeError> {
        // The exact facts first, and without a network call: if the turn was
        // told to act and did not, or stated something it was not shown, that
        // is settled before any model is asked anything.
        let (ignored_request, spans) = self.symbolic.structural(view);

        // Re-ask: the lexical band still fires where it can — it is free and
        // it is the proxy T2.7 calibrates against — and the embedder is asked
        // only about the pairs it could not settle.
        let mut reask = view
            .next_user
            .and_then(|n| reask_band(view.user, n, self.symbolic.cfg.reask_jaccard))
            .is_some();
        if !reask {
            if let Some(next) = view.next_user {
                let cos = self.similarity(view.user, next).await?;
                reask = cos >= self.cfg.reask_cosine;
            }
        }

        // Relevance: the cross-encoder replaces the token intersection
        // outright rather than backing it up. The lexical rule's measured
        // failures were both *false positives*, so keeping it as an "or"
        // would keep exactly the errors this call is here to remove.
        let score = self.relevance(view.user, view.reply).await?;
        let ignored_q = score < self.cfg.relevance_cut;

        let issue =
            SymbolicEvaluator::resolve(ignored_request, !spans.is_empty(), reask, ignored_q);
        Ok(TurnGrade {
            issue,
            answers_user: if ignored_q || ignored_request { 0 } else { 2 },
            grounded: spans.is_empty(),
            scorer: self.id(),
        })
    }
}

/// The lexical relevance rule, exposed so a run can report what the local
/// scorer changed rather than only what it concluded.
pub fn lexical_ignores_question(user: &str, reply: &str) -> bool {
    ignores_question(user, reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluate::EvaluateConfig;

    fn view<'a>(user: &'a str, reply: &'a str, next: Option<&'a str>) -> TurnView<'a> {
        TurnView {
            user,
            shown: &[],
            acted: true,
            task_tier: false,
            reply,
            next_user: next,
        }
    }

    fn evaluator(base_url: &str) -> LocalEvaluator {
        LocalEvaluator::new(
            LocalConfig {
                base_url: base_url.into(),
                timeout_ms: 200,
                ..Default::default()
            },
            SymbolicEvaluator {
                cfg: EvaluateConfig::default(),
            },
        )
    }

    /// T2.3b: a dead service stops the lane quickly, whichever way it dies.
    ///
    /// The task was written for `connection refused`. Measured on this box, a
    /// closed loopback port does not refuse — it times out, with no io error
    /// in the chain at all — so the test asserts the property the table is
    /// *for* (the lane gives up fast and stays given up) rather than the one
    /// mechanism it was assumed to arrive by.
    #[tokio::test]
    async fn a_dead_service_takes_the_lane_out_and_it_stays_out() {
        let e = evaluator("http://127.0.0.1:1");
        assert!(!e.is_disabled());
        for i in 0..LocalEvaluator::give_up_after() {
            let g = e.grade(&view("kolik je hodin?", "je 9:36", None)).await;
            assert!(matches!(g, Err(GradeError::Unavailable(_))), "{i}: {g:?}");
        }
        assert!(
            e.is_disabled(),
            "a service that answered nothing twice is not there"
        );
        // And now nothing is dialled at all, which is the point: the cost of
        // a hundred more turns is zero rather than two seconds each.
        let started = std::time::Instant::now();
        let after = e.grade(&view("a teď?", "pořád 9:36", None)).await;
        assert!(matches!(after, Err(GradeError::Unavailable(_))));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "a disabled lane still dialled"
        );
    }

    /// A grade that cannot be produced is `Unavailable`, never a verdict. The
    /// pass completes; the ledger records the gap.
    #[tokio::test]
    async fn an_unavailable_scorer_produces_no_issue_rather_than_a_clean_one() {
        let e = evaluator("http://127.0.0.1:1");
        let g = e.grade(&view("empty my recycle bin", "sure", None)).await;
        assert!(g.is_err(), "silence must not read as Issue::None");
    }

    /// A base URL that is not dialable at all is a refusal, not a panic.
    #[tokio::test]
    async fn a_nonsense_base_url_degrades() {
        let e = evaluator("http://127.0.0.1:2/nsmodels");
        assert!(e.grade(&view("hi", "hello", None)).await.is_err());
    }

    /// The structural checks do not need the service, and a turn that is
    /// already settled structurally still reports through the same grade
    /// shape once the service answers. Here the service is down, so the
    /// contract under test is that we fail rather than quietly grade.
    #[tokio::test]
    async fn structural_findings_do_not_license_a_grade_without_the_service() {
        let e = evaluator("http://127.0.0.1:1");
        let v = TurnView {
            user: "empty my recycle bin",
            shown: &[],
            acted: false,
            task_tier: true,
            reply: "Sure, I can help with that.",
            next_user: None,
        };
        // I6 is decided with no model, but the grade as a whole is not, and
        // half a grade is not a grade.
        assert!(e.grade(&v).await.is_err());
    }
}
