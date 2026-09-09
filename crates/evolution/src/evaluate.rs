//! The evaluation lane's symbolic half (M6 §8.2, M8 T2.1): checks that need
//! no model, run on every turn, and cost nothing.
//!
//! Mining ([`crate::mine`]) reads *what the harness did* — proposals, guard
//! denials, malformed arguments. It is blind to the two failures a user
//! actually notices: being answered with something they did not ask for, and
//! having to ask twice. This module reads those, and reads them out of the
//! same log with the same determinism, so a signature it produces can enter
//! the same gate as every other one.
//!
//! **The order matters and is the point.** M8 §5 builds this alone, before
//! the `Evaluator` trait and before any scorer, because M6's exit criterion
//! for Phase 5 — "≥ 10 `UserReask` / `BadReply` signatures where today it
//! reports zero" — names a *symbolic* signature first, and it may already be
//! met with nothing in the lane that costs a request. It also gives κ (T2.7)
//! something real to be computed against: an evaluator is calibrated against
//! these checks, so they cannot arrive with it.
//!
//! Prior art, and where it changed the design, is
//! `docs/research/2026-09-09-symbolic-evaluation-findings.md`:
//!
//! - **Re-ask is query reformulation** (§1). M6's rule — normalized text
//!   equal to one of the previous three — is the Jaccard = 1.0 corner of the
//!   feature set that literature classifies reformulations with, and M8
//!   Phase 1 measured what that corner costs here: lexical matching misses
//!   75–83% of the same question asked in different words. So the check
//!   reports a **band** rather than a boolean, and only the exact band is
//!   trusted as a proxy (see [`ReaskBand`]).
//! - **"Question ignored" is five categories, not one** (§2). Higashinaka et
//!   al.'s integrated taxonomy splits the ignore family into I5 question, I6
//!   request, I7 proposal, I8 greeting, I9 expectation. M6 specified I5 and
//!   called it weak, correctly. I6 is the one this harness can detect
//!   *precisely*, because unlike a chat corpus it has a log of what it did —
//!   and, better, of how it classified the turn before doing it.
//! - **Groundedness is precision-only** (§4). `UngroundedReply` counts
//!   inventions; it says nothing about a reply that avoided inventing by
//!   saying nothing. It is never a quality score, and the count going to zero
//!   is not good news on its own.
//!
//! **What it found, run against the recorded session (20 turns, 2026-09-09):**
//!
//! ```text
//! UserReask:       7  (turns 4, 9, 10, 17, 18, 19, 20)
//! IgnoredQuestion: 2  (turns 13, 17) — both false positives, see below
//! UngroundedReply: 0  — the log carries no ReplyFlagged
//! IgnoredRequest:  0  — the log predates ModelCall, so no turn has a tier
//! ```
//!
//! Six of the seven re-asks are the user paying for a failure: turn 4 rewords
//! turn 2's failed pointer move, turns 9 and 10 are "confirm" said again
//! because the session was not armed, and turns 17–19 are the fifth, sixth and
//! seventh attempt at emptying the recycle bin. Turn 20 is the arguable one —
//! it repeats turn 19, which had just *succeeded*.
//!
//! The two the check missed are as informative as the seven it caught, and
//! both are the vocabulary problem again: turn 15 asks in Czech what turn 14
//! asked in English, and turn 16 is turn 15 mojibake'd past the Jaccard
//! threshold. Neither is reachable without an embedder, which is T2.3.
use crate::mine::{Signature, SignatureKind, SYNTHETIC_ACTIONS};
use nscore::{query_tokens, Event, EventKind, SessionId, Tier};
use std::collections::HashSet;

/// How a re-ask was recognised. Recorded on the signature because the two
/// bands have different evidence behind them and must not be pooled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaskBand {
    /// The same content tokens in the same order as an earlier turn. Survives
    /// case, punctuation, a stray byte-order mark and the mojibake a Windows
    /// console produces from Czech — all of which the recorded session has,
    /// three times over ("vysyp muj koš" arrives as `\u{feff}vysyp muj koĹˇ`
    /// and as "vysyp muj ko??").
    ///
    /// This is the band T2.7 calibrates against. It is nearly free of false
    /// positives, which is the only property a κ proxy needs.
    Repeat,
    /// The same content, different words: Jaccard over content tokens at or
    /// above [`EvaluateConfig::reask_jaccard`], but not an exact repeat.
    ///
    /// Counted and reported, and deliberately **not** a proxy yet. It becomes
    /// one the way every check in this engine becomes a gate — on a measured
    /// true-positive rate (`2026-09-04-reply-entrainment.md` §8) — and being
    /// counted is how that rate gets measured. Adding it to the proxy set
    /// before then would move κ for reasons that have nothing to do with the
    /// evaluator being calibrated.
    Reformulated,
}

impl ReaskBand {
    pub fn as_str(self) -> &'static str {
        match self {
            ReaskBand::Repeat => "repeat",
            ReaskBand::Reformulated => "reformulated",
        }
    }

    /// Whether T2.7 may use this band as a calibration proxy.
    pub fn is_proxy(self) -> bool {
        matches!(self, ReaskBand::Repeat)
    }
}

#[derive(Debug, Clone)]
pub struct EvaluateConfig {
    /// User turns back a re-ask may match against (M6 §8.2: 3).
    pub reask_lookback: usize,
    /// Jaccard over content tokens at or above which a differently-worded
    /// question counts as [`ReaskBand::Reformulated`].
    ///
    /// A guess, and labelled one. The corpus that would settle it is twelve
    /// adversarial paraphrases, which is an instrument and not a training
    /// set (`2026-09-08-local-retrieval-and-lane-findings.md` §5) — so this
    /// is a key with a default rather than a constant with a claim, for the
    /// same reason `coarse_k` is.
    pub reask_jaccard: f32,
}

impl Default for EvaluateConfig {
    fn default() -> Self {
        Self {
            reask_lookback: 3,
            reask_jaccard: 0.6,
        }
    }
}

/// Openers that carry no request, so repeating one is not a re-ask.
///
/// M6 §5.2 already draws this line for observations — "greetings and repeats
/// are skipped" — and the recorded session shows why it belongs here too:
/// turn 5 is "hi" and so is turn 1. Nothing went wrong. Czech and English
/// both, with and without diacritics, because the console mangles them.
///
/// Confirmations are deliberately **absent**. "confirm" three turns running
/// (recorded session, turns 8–10) is the user paying for two failures, which
/// is exactly the signal this check exists to catch.
const PLEASANTRIES: &[&str] = &[
    "hello", "hey", "yes", "yeah", "yep", "nope", "thanks", "thank", "you", "please", "sorry",
    "bye", "goodbye", "morning", "evening", "ahoj", "cau", "čau", "dobry", "dobrý", "den", "diky",
    "díky", "dekuji", "děkuji", "prosim", "prosím", "ano", "jasne", "jasně", "nazdar", "zdravim",
    "zdravím",
];

/// Content tokens, in order. `query_tokens` is the engine's one tokenizer —
/// lowercase alphanumeric runs of three characters or more — and it is used
/// here rather than a second one so "what the retriever thinks two texts
/// share" and "what this check thinks two texts share" cannot drift apart.
///
/// Its three-character floor drops "hi" and "ok" to nothing, which is a
/// convenient accident and not the reason [`PLEASANTRIES`] exists: "hello"
/// and "děkuji" clear the floor.
fn content_tokens(text: &str) -> Vec<String> {
    query_tokens(text)
}

fn jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f32;
    let union = a.union(b).count() as f32;
    inter / union
}

/// Nothing but pleasantries, so there is no request to repeat.
fn is_pleasantry(tokens: &[String]) -> bool {
    tokens.is_empty() || tokens.iter().all(|t| PLEASANTRIES.contains(&t.as_str()))
}

/// Whether `again` is a re-ask of `first`, and in which band.
///
/// The one definition of "asked twice" in the engine. [`evaluate`] applies it
/// backwards over a session's user turns; [`SymbolicEvaluator`] applies it
/// forwards from a turn to its follow-up. Two vantages on the same relation,
/// and they must not be able to disagree — a corpus that measured one while
/// the pass ran the other would be measuring nothing.
pub fn reask_band(first: &str, again: &str, min_jaccard: f32) -> Option<ReaskBand> {
    let a = content_tokens(first);
    let b = content_tokens(again);
    if is_pleasantry(&a) || is_pleasantry(&b) {
        return None;
    }
    if a == b {
        return Some(ReaskBand::Repeat);
    }
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    (jaccard(&sa, &sb) >= min_jaccard).then_some(ReaskBand::Reformulated)
}

/// I5: the user asked something and the reply shares no content word with it.
///
/// Weak, and measured weak — see [`SignatureKind::IgnoredQuestion`].
pub fn ignores_question(user: &str, reply: &str) -> bool {
    if !user.contains('?') {
        return false;
    }
    let asked = content_tokens(user);
    let answered: HashSet<String> = content_tokens(reply).into_iter().collect();
    !asked.is_empty() && !answered.is_empty() && !asked.iter().any(|t| answered.contains(t))
}

/// One user turn of a session log, reduced to what the checks compare.
///
/// The pass-side view. [`TurnView`] is the evaluator-side one — same turn,
/// different vantage: this is assembled from events, that one is handed to a
/// scorer.
struct SessionTurn {
    turn: u32,
    user_event: nscore::EventId,
    user_tokens: Vec<String>,
    user_text: String,
    reply: Option<String>,
    /// The tier the router chose, from the `ModelCall` manifest. `None` on a
    /// log written before M7 Phase 3, or by a build with no router installed.
    tier: Option<Tier>,
    tool_calls: usize,
    /// Proposals for something other than the synthetic actions — the
    /// harness deciding to *act*, as distinct from deciding to answer.
    real_proposals: usize,
    flagged: Vec<(nscore::EventId, Vec<String>)>,
}

fn views(events: &[Event]) -> Vec<SessionTurn> {
    let mut out: Vec<SessionTurn> = Vec::new();
    for e in events {
        // A turn enters the list when its `UserSaid` does, so a log that
        // opens mid-turn contributes nothing rather than a half view.
        if let EventKind::UserSaid { text } = &e.kind {
            out.push(SessionTurn {
                turn: e.turn,
                user_event: e.id,
                user_tokens: content_tokens(text),
                user_text: text.clone(),
                reply: None,
                tier: None,
                tool_calls: 0,
                real_proposals: 0,
                flagged: Vec::new(),
            });
            continue;
        }
        let Some(v) = out.iter_mut().rev().find(|v| v.turn == e.turn) else {
            continue;
        };
        match &e.kind {
            EventKind::Replied { text } => v.reply = Some(text.clone()),
            EventKind::ToolCalled { .. } => v.tool_calls += 1,
            EventKind::Proposed { proposal } => {
                if !SYNTHETIC_ACTIONS.contains(&proposal.action.as_str()) {
                    v.real_proposals += 1;
                }
            }
            EventKind::ReplyFlagged { spans, .. } => v.flagged.push((e.id, spans.clone())),
            EventKind::ModelCall { manifest, .. } => {
                // The first call of the turn is the emitter's, and the tier
                // is the router's decision about the message — the same for
                // every call in the turn unless `run_turn` widened it, and a
                // widening is `Misrouted`'s business, not this check's.
                v.tier = v.tier.or(manifest.tier);
            }
            _ => {}
        }
    }
    out
}

/// The symbolic checks over one session's log. Deterministic, offline, and
/// free: no model, no network, no store access.
pub fn evaluate(session: &SessionId, events: &[Event], cfg: &EvaluateConfig) -> Vec<Signature> {
    let views = views(events);
    let mut sigs = Vec::new();

    for (i, v) in views.iter().enumerate() {
        // ---- Re-ask (M6 §8.2, findings §1) -------------------------------
        if !is_pleasantry(&v.user_tokens) {
            let from = i.saturating_sub(cfg.reask_lookback);
            let mut times = 0u32;
            let mut band: Option<ReaskBand> = None;
            for prior in &views[from..i] {
                match reask_band(&prior.user_text, &v.user_text, cfg.reask_jaccard) {
                    Some(ReaskBand::Repeat) => {
                        times += 1;
                        band = Some(ReaskBand::Repeat);
                    }
                    Some(ReaskBand::Reformulated) => {
                        times += 1;
                        // An exact repeat anywhere in the window wins: the
                        // band names the strongest evidence, not the last.
                        band.get_or_insert(ReaskBand::Reformulated);
                    }
                    None => {}
                }
            }
            if let Some(band) = band {
                sigs.push(Signature {
                    session: session.clone(),
                    turn: v.turn,
                    event_id: v.user_event,
                    // `times` counts the askings, so the turn itself is one
                    // of them: a question asked twice reads `times: 2`.
                    kind: SignatureKind::UserReask {
                        times: times + 1,
                        band,
                    },
                });
            }
        }

        // ---- Ungrounded reply (M6 §8.2, findings §4) ---------------------
        //
        // The signature is the *recorded* interceptor result, not a re-run of
        // it. That is T2.3a's invariant arriving one task early, and here it
        // is not a precaution but the only correct reading: `Material` is
        // built from the persona, the facts and the window the replier was
        // shown, and the log carries the manifest's fact *keys* — not their
        // values, and not the persona at all. A check run against a
        // partially reconstructed context would flag claims as unsupported
        // because the support was not recoverable, which is a false positive
        // manufactured by the checker. M6 §8.3 says the view is rebuilt "so
        // that grounded is judged against exactly what the replier saw", and
        // exactly is the operative word.
        //
        // So the offline lane surfaces what the live lane established. This
        // is not a small thing: today `ReplyFlagged` reaches the pass only as
        // a line of rendered text for a note proposer to read. As a signature
        // it can be counted, gated and calibrated against.
        for (id, spans) in &v.flagged {
            sigs.push(Signature {
                session: session.clone(),
                turn: v.turn,
                event_id: *id,
                kind: SignatureKind::UngroundedReply {
                    spans: spans.clone(),
                },
            });
        }

        // ---- Ignored question, I5 (M6 §8.2, findings §2) -----------------
        //
        // Weak by construction and marked so in the spec. It shares the
        // failure mode of the lexical re-ask band — a reply that answers in
        // different words looks like a reply that ignored the question — and
        // unlike that band it has no exact half to fall back on. It is
        // counted, it is note-lane, and it is not a κ proxy.
        if let Some(reply) = &v.reply {
            if ignores_question(&v.user_text, reply) {
                sigs.push(Signature {
                    session: session.clone(),
                    turn: v.turn,
                    event_id: v.user_event,
                    kind: SignatureKind::IgnoredQuestion,
                });
            }
        }

        // ---- Ignored request, I6 (findings §2) ---------------------------
        //
        // No text is compared. The router already decided this message wanted
        // something done and recorded the decision; the log then shows
        // nothing being done. Two structural facts, both written by the
        // engine about itself.
        //
        // `Task` exactly, never `Deep`: a `Deep` turn runs recall inside the
        // engine before the first proposal (M7 Phase 3), so answering one
        // without calling a tool is the tier working, not a request ignored.
        // Precision is the whole value of this check — it is a κ proxy — and
        // one structurally explicable false positive would be enough to lose
        // it.
        if v.tier == Some(Tier::Task)
            && v.tool_calls == 0
            && v.real_proposals == 0
            && v.reply.is_some()
        {
            sigs.push(Signature {
                session: session.clone(),
                turn: v.turn,
                event_id: v.user_event,
                kind: SignatureKind::IgnoredRequest,
            });
        }
    }

    sigs
}

// ---------------------------------------------------------------------------
// The evaluator layer (M6 §8.3, M8 T2.2)
// ---------------------------------------------------------------------------

/// What went wrong with a turn, in the one vocabulary the lane uses.
///
/// The codes are Higashinaka et al.'s where they have one
/// (`docs/research/2026-09-09-symbolic-evaluation-findings.md` §2); the
/// projection the gate consumes is [`Issue::is_problem`], because "did the
/// evaluator see a problem here" is the binary κ is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    None,
    /// The user asked the same thing again.
    Reask,
    /// I5 — the reply is about something else.
    IgnoredQuestion,
    /// I6 — the turn was asked to act and did not.
    IgnoredRequest,
    /// The reply states something nothing it was shown supports.
    Ungrounded,
    /// The user's next message contradicts a fact the reply stated.
    Correction,
}

impl Issue {
    pub fn as_str(self) -> &'static str {
        match self {
            Issue::None => "none",
            Issue::Reask => "reask",
            Issue::IgnoredQuestion => "ignored_question",
            Issue::IgnoredRequest => "ignored_request",
            Issue::Ungrounded => "ungrounded",
            Issue::Correction => "correction",
        }
    }
    pub fn is_problem(self) -> bool {
        !matches!(self, Issue::None)
    }
}

/// Everything an evaluator of any kind may look at.
///
/// M6 §8.3's `TurnView` carries the window, the facts and the summary; this
/// one carries `shown`, which is those three already rendered the way the
/// replier saw them. The distinction is T2.3a's: an evaluator must judge
/// against the recorded material, not against a context rebuilt at grading
/// time, and a view that hands it the pieces invites the rebuild.
#[derive(Debug, Clone, Copy)]
pub struct TurnView<'a> {
    pub user: &'a str,
    /// Facts and this turn's trace, as rendered into the reply prompt.
    pub shown: &'a [&'a str],
    /// The turn called a tool or proposed a real action.
    pub acted: bool,
    /// The router put this turn in the tier that means "something is to be
    /// done".
    pub task_tier: bool,
    pub reply: &'a str,
    pub next_user: Option<&'a str>,
}

/// A grade, and who produced it.
///
/// The identity field is not bookkeeping. T2.3a's invariant is that a grade is
/// a recorded value and no replay path recomputes it; recording *which* scorer
/// produced it is the `MutableSideEffect` half, so a weight change shows up as
/// a diff instead of disappearing into a number. The 2026 transfer audit
/// (`docs/research/2026-09-09-local-evaluator-findings.md` §1) is why that
/// matters more than it looks: a metric whose ranking inverts between datasets
/// will certainly move when its weights do.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnGrade {
    pub issue: Issue,
    /// 0 no, 1 partly, 2 yes (M6 §8.3).
    pub answers_user: u8,
    pub grounded: bool,
    /// The evaluator's id, e.g. `symbolic` or `local:bge-m3`.
    pub scorer: String,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GradeError {
    /// The scorer could not be reached. Not a failure of the turn, and not
    /// counted against anything — the pass completes and the ledger records
    /// the gap.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The scorer answered with something unusable. Counted unverified, and
    /// deliberately *not* `Unavailable`: a malformed answer is a defect worth
    /// seeing, and folding it into "the service was down" would hide it.
    #[error("invalid: {0}")]
    Invalid(String),
}

#[async_trait::async_trait]
pub trait Evaluator: Send + Sync {
    /// Stable identity, recorded with every grade.
    fn id(&self) -> String;
    async fn grade(&self, view: &TurnView<'_>) -> Result<TurnGrade, GradeError>;
}

/// The checks of [`evaluate`], applied to one turn instead of a session.
///
/// No model, no network, never fails. It is the baseline every other evaluator
/// is measured against, and the proxy set T2.7 calibrates them on.
#[derive(Debug, Clone, Default)]
pub struct SymbolicEvaluator {
    pub cfg: EvaluateConfig,
}

impl SymbolicEvaluator {
    /// Structural facts about the turn that no scorer improves on, so every
    /// evaluator shares them rather than re-deciding them.
    ///
    /// I6 is structural by construction (the router's recorded tier against
    /// the log of what was done) and grounding is span attribution, which an
    /// embedding cannot do — it can say two texts are alike, not which words
    /// of one are unsupported by the other. A local scorer that re-derived
    /// either would be adding a model to a decision that did not need one.
    pub fn structural(&self, view: &TurnView<'_>) -> (bool, Vec<String>) {
        let ignored_request = view.task_tier && !view.acted;
        let material = nsengine::ground::Material::from_parts(view.shown);
        let spans = nsengine::ground::ungrounded(view.reply, &material);
        (ignored_request, spans)
    }

    /// Precedence when several checks fire, strongest evidence first.
    ///
    /// Structural before recorded before follow-up before lexical: I6 is two
    /// logged facts, grounding is the shipping interceptor, a re-ask is the
    /// user's own next message, and I5 is a token-overlap heuristic measured
    /// at 0 for 2 (`SignatureKind::IgnoredQuestion`). A single label has to
    /// pick one, and picking the weakest would make the corpus grade the
    /// heuristic rather than the evaluator.
    pub fn resolve(ignored_request: bool, ungrounded: bool, reask: bool, ignored_q: bool) -> Issue {
        if ignored_request {
            Issue::IgnoredRequest
        } else if ungrounded {
            Issue::Ungrounded
        } else if reask {
            Issue::Reask
        } else if ignored_q {
            Issue::IgnoredQuestion
        } else {
            Issue::None
        }
    }
}

#[async_trait::async_trait]
impl Evaluator for SymbolicEvaluator {
    fn id(&self) -> String {
        "symbolic".into()
    }

    async fn grade(&self, view: &TurnView<'_>) -> Result<TurnGrade, GradeError> {
        let (ignored_request, spans) = self.structural(view);
        let reask = view
            .next_user
            .and_then(|n| reask_band(view.user, n, self.cfg.reask_jaccard))
            .is_some();
        let ignored_q = ignores_question(view.user, view.reply);
        // `Correction` is deliberately unreachable here, and the corpus will
        // say so: it needs the follow-up to *contradict* the reply, which is
        // an entailment judgement and not a lexical one. Producing it from a
        // heuristic would be inventing agreement.
        let issue = SymbolicEvaluator::resolve(ignored_request, !spans.is_empty(), reask, ignored_q);
        Ok(TurnGrade {
            issue,
            answers_user: if ignored_q || ignored_request { 0 } else { 2 },
            grounded: spans.is_empty(),
            scorer: self.id(),
        })
    }
}

/// A fixed answer per user text, for tests that need an evaluator with known
/// behaviour rather than a real one (M6 §8.3's `ScriptedEvaluator`).
#[derive(Default)]
pub struct ScriptedEvaluator {
    pub answers: std::collections::HashMap<String, Result<Issue, GradeError>>,
    pub default: Option<Issue>,
}

#[async_trait::async_trait]
impl Evaluator for ScriptedEvaluator {
    fn id(&self) -> String {
        "scripted".into()
    }
    async fn grade(&self, view: &TurnView<'_>) -> Result<TurnGrade, GradeError> {
        let issue = match self.answers.get(view.user) {
            Some(Ok(i)) => *i,
            Some(Err(e)) => return Err(e.clone()),
            None => self.default.unwrap_or(Issue::None),
        };
        Ok(TurnGrade {
            issue,
            answers_user: if issue.is_problem() { 0 } else { 2 },
            grounded: issue != Issue::Ungrounded,
            scorer: self.id(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::{EventLog, Proposal, ReplyPolicy, Timestamp};

    fn log() -> EventLog {
        EventLog::new(SessionId("t".into()))
    }

    /// A `ModelCall` carries a tier and nothing else this check reads; the
    /// usage block is there because the event type requires one.
    fn routed(l: &mut EventLog, turn: u32, tier: Tier) {
        l.append(
            turn,
            Timestamp(turn as u64),
            EventKind::ModelCall {
                usage: nscore::Usage {
                    role: "emitter".into(),
                    model: "m".into(),
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    estimated: false,
                    attempts: 1,
                    latency_ms: 1,
                    tools_tokens: 0,
                },
                manifest: nscore::ContextManifest {
                    tier: Some(tier),
                    ..Default::default()
                },
            },
        );
    }

    fn said(l: &mut EventLog, turn: u32, text: &str) {
        l.append(
            turn,
            Timestamp(turn as u64),
            EventKind::UserSaid { text: text.into() },
        );
    }

    fn replied(l: &mut EventLog, turn: u32, text: &str) {
        l.append(
            turn,
            Timestamp(turn as u64),
            EventKind::Settled {
                policy: ReplyPolicy::Generate,
            },
        );
        l.append(
            turn,
            Timestamp(turn as u64),
            EventKind::Replied { text: text.into() },
        );
    }

    fn sigs(l: &EventLog) -> Vec<SignatureKind> {
        evaluate(
            &SessionId("t".into()),
            l.events(),
            &EvaluateConfig::default(),
        )
        .into_iter()
        .map(|s| s.kind)
        .collect()
    }

    #[test]
    fn an_exact_repeat_is_a_repeat_band_reask() {
        let mut l = log();
        said(&mut l, 1, "vysyp muj koš");
        replied(&mut l, 1, "nepodařilo se");
        said(&mut l, 2, "vysyp muj koš");
        replied(&mut l, 2, "nepodařilo se");
        assert_eq!(
            sigs(&l),
            vec![SignatureKind::UserReask {
                times: 2,
                band: ReaskBand::Repeat
            }]
        );
    }

    #[test]
    fn punctuation_case_and_a_byte_order_mark_do_not_hide_a_repeat() {
        // All three shapes are from the recorded session: a leading BOM, a
        // trailing space, and the console's rendering of the same sentence.
        let mut l = log();
        said(&mut l, 1, "Vysyp muj koš.");
        replied(&mut l, 1, "ne");
        said(&mut l, 2, "\u{feff}vysyp   muj  koš ");
        replied(&mut l, 2, "ne");
        assert!(matches!(
            sigs(&l).as_slice(),
            [SignatureKind::UserReask {
                band: ReaskBand::Repeat,
                ..
            }]
        ));
    }

    #[test]
    fn a_rewording_is_the_reformulated_band_and_not_the_proxy_one() {
        let mut l = log();
        said(&mut l, 1, "can you move my mouse pointer to the corner");
        replied(&mut l, 1, "sorry");
        said(&mut l, 2, "can you move my cursor pointer to the corner");
        replied(&mut l, 2, "sorry");
        let s = sigs(&l);
        assert!(matches!(
            s.as_slice(),
            [SignatureKind::UserReask {
                band: ReaskBand::Reformulated,
                ..
            }]
        ));
        assert!(!ReaskBand::Reformulated.is_proxy());
        assert!(ReaskBand::Repeat.is_proxy());
    }

    #[test]
    fn a_repeated_greeting_is_not_a_reask() {
        // Recorded session turns 1 and 5. Nothing went wrong.
        let mut l = log();
        said(&mut l, 1, "hello");
        replied(&mut l, 1, "hi there");
        said(&mut l, 2, "hello");
        replied(&mut l, 2, "hi there");
        assert!(sigs(&l).is_empty());
    }

    #[test]
    fn a_repeated_confirmation_is_a_reask() {
        // Recorded session turns 8–10: the user said "confirm" three times
        // because the first two did not take. That is the signal, not noise.
        let mut l = log();
        said(&mut l, 1, "confirm");
        replied(&mut l, 1, "not armed");
        said(&mut l, 2, "confirm");
        replied(&mut l, 2, "confirm to proceed");
        said(&mut l, 3, "confirm");
        replied(&mut l, 3, "done");
        let s = sigs(&l);
        assert_eq!(
            s,
            vec![
                SignatureKind::UserReask {
                    times: 2,
                    band: ReaskBand::Repeat
                },
                SignatureKind::UserReask {
                    times: 3,
                    band: ReaskBand::Repeat
                },
            ]
        );
    }

    #[test]
    fn the_lookback_window_is_three_user_turns() {
        let mut l = log();
        said(&mut l, 1, "empty the recycle bin now");
        replied(&mut l, 1, "no");
        for (t, text) in [
            (2, "what time is it"),
            (3, "open firefox"),
            (4, "close edge"),
        ] {
            said(&mut l, t, text);
            replied(&mut l, t, "ok");
        }
        said(&mut l, 5, "empty the recycle bin now");
        replied(&mut l, 5, "no");
        assert!(sigs(&l).is_empty(), "four turns back is outside the window");
    }

    #[test]
    fn a_recorded_flag_becomes_the_ungrounded_signature_and_is_not_recomputed() {
        let mut l = log();
        said(&mut l, 1, "when is it");
        l.append(
            1,
            Timestamp(1),
            EventKind::ReplyFlagged {
                draft: "It is 14:05 in Oslo.".into(),
                spans: vec!["14:05".into(), "Oslo".into()],
            },
        );
        // The second draft is what stands, and it says nothing invented. A
        // checker that re-ran over this text would report nothing; the flag
        // is the record that the first draft did.
        replied(&mut l, 1, "I do not have the time to hand.");
        assert_eq!(
            sigs(&l),
            vec![SignatureKind::UngroundedReply {
                spans: vec!["14:05".into(), "Oslo".into()]
            }]
        );
    }

    #[test]
    fn a_task_turn_that_did_nothing_is_an_ignored_request() {
        let mut l = log();
        said(&mut l, 1, "empty my recycle bin");
        routed(&mut l, 1, Tier::Task);
        replied(&mut l, 1, "Sure, I can help with that.");
        assert_eq!(sigs(&l), vec![SignatureKind::IgnoredRequest]);
    }

    #[test]
    fn a_task_turn_that_acted_is_not() {
        let mut l = log();
        said(&mut l, 1, "empty my recycle bin");
        routed(&mut l, 1, Tier::Task);
        l.append(
            1,
            Timestamp(1),
            EventKind::Proposed {
                proposal: Proposal {
                    rationale: "empty it".into(),
                    action: "pointer_click".into(),
                    args: serde_json::json!({}),
                },
            },
        );
        replied(&mut l, 1, "Emptied.");
        assert!(sigs(&l).is_empty());
    }

    #[test]
    fn a_deep_turn_that_answered_from_memory_is_not_an_ignored_request() {
        let mut l = log();
        said(&mut l, 1, "what did I say my name was");
        routed(&mut l, 1, Tier::Deep);
        replied(&mut l, 1, "Martin.");
        assert!(sigs(&l).is_empty());
    }

    #[test]
    fn a_log_with_no_manifest_yields_no_ignored_request() {
        // Every session recorded before M7 Phase 3 is this shape. The check
        // reports nothing rather than guessing a tier.
        let mut l = log();
        said(&mut l, 1, "empty my recycle bin");
        replied(&mut l, 1, "Sure, I can help with that.");
        assert!(sigs(&l).is_empty());
    }

    #[test]
    fn a_question_the_reply_shares_no_word_with_is_ignored() {
        let mut l = log();
        said(&mut l, 1, "which browser windows are open?");
        replied(&mut l, 1, "Emptied the recycle bin.");
        assert_eq!(sigs(&l), vec![SignatureKind::IgnoredQuestion]);
    }

    #[test]
    fn a_question_answered_in_its_own_words_is_not() {
        let mut l = log();
        said(&mut l, 1, "which browser windows are open?");
        replied(&mut l, 1, "Firefox and Edge browser windows are open.");
        assert!(sigs(&l).is_empty());
    }

    #[test]
    fn a_statement_is_never_an_ignored_question() {
        let mut l = log();
        said(&mut l, 1, "empty the recycle bin");
        replied(&mut l, 1, "Done.");
        assert!(sigs(&l).is_empty());
    }
}
