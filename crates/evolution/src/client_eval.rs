//! The paid judge (M11 T1.3): an [`Evaluator`] that asks a strong model.
//!
//! Everything `local.rs` is, one step further out. The symbolic checks cost
//! nothing and are the baseline; the local scorer costs a loopback round trip
//! and buys cross-lingual re-ask and relevance; this one costs a *request*
//! and buys the one thing neither can produce — `Correction`, which needs the
//! follow-up to contradict the reply, an entailment judgement and not a
//! lexical one (M8's recorded blind spot, `evaluate.rs`'s own note on
//! `SymbolicEvaluator::grade`).
//!
//! Four rules, and they are the whole design:
//!
//! 1. **Off unless asked.** `[models] judge_model` is `Option<String>` and
//!    defaults to unset. [`ClientEvaluator::for_model`] returns `None` for
//!    `None`, so a config that does not name a judge cannot construct one and
//!    therefore cannot spend a request. That is a property of the
//!    constructor, not of a call site remembering to check.
//! 2. **The idle pass only.** It is added by `build_pass` through
//!    `with_evaluator`, which is reachable from nowhere a turn runs. It is
//!    never in the guard chain and never on a reply path — M6 §13 is not
//!    relaxed because the judge is good, and a scorer that could block a turn
//!    would be a second model in the critical path.
//! 3. **κ-gated exactly like the local scorer.** Its grades are recorded
//!    under its own id, `pass.rs` computes κ against `symbolic` and
//!    `evaluator_min_kappa` decides whether the notes gate may believe it.
//!    Below threshold it contributes observations and no candidates. A judge
//!    that costs money is not thereby calibrated.
//! 4. **A budget it shares.** `evaluate_budget_turns` is the same cap the
//!    grading queue already walks newest-first, and a turn already carrying a
//!    `Graded` event from this id is read back rather than re-asked — so a
//!    second pass over the same log spends nothing, which is T2.3a's replay
//!    property doing double duty as cost control.
//!
//! **What a bad answer means.** A malformed verdict is `Unavailable`, not a
//! grade and not `Invalid`. `local.rs` draws that line the other way and is
//! right to: a 4xx from nsmodels is a defect on this box worth seeing. Here
//! the answer comes from a model, a model's output is not a contract, and the
//! only honest reading of unparsable JSON is that this turn was not graded.
//! Counting it as a grade would put a guess in the log under a scorer's name;
//! counting it `Invalid` would spend it against the turn. It stays ungraded
//! and the next pass may try again.

use crate::evaluate::{Evaluator, GradeError, Issue, SymbolicEvaluator, TurnGrade, TurnView};
use std::sync::atomic::{AtomicU32, Ordering};

/// Output cap for one verdict. A verdict is two fields and a short list;
/// 1024 is the M11 T0.4 summarizer shape, and a reasoning model needs the
/// headroom because it spends output on reasoning first.
const JUDGE_MAX_TOKENS: u32 = 1024;

/// What the judge is asked for, verbatim. Kept as one constant because it is
/// the thing κ is a measurement *of*: change the words and the number is
/// about a different scorer.
const TASK: &str = "\
You are grading one turn of a recorded assistant conversation, offline.
You are shown the user's message, the material the assistant was given, its
reply, and the user's next message when there was one.

Answer with JSON only, no prose and no code fence:
{\"ok\": <true|false>, \"issues\": [<zero or more of the codes below>]}

Codes, and what each one means here:
  reask            - the next user message asks the same thing again.
  ungrounded       - the reply states something none of the shown material
                     supports.
  ignored_question - the reply is about something other than what was asked.
  ignored_request  - the turn was asked to do something and did not.
  correction       - the next user message contradicts a fact the reply
                     stated.

\"ok\" is true only when the issues list is empty. Judge the reply against
the material shown and nothing else: you are not being asked whether the
answer is true of the world, only whether the turn went wrong.";

/// How the judge is dialled. `model` is the only field without a default,
/// because it is the field whose absence means "do not construct this".
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    pub model: String,
    /// `low` | `medium` | `high`, or `None` for the provider's default.
    /// `low` is M11 T0.4's shape for a short structured answer.
    pub reasoning: Option<String>,
    pub max_tokens: u32,
    /// Whether the preset advertises `response_format: json_schema`
    /// (`provider.rs`'s `structured_output` column). Where it does, the
    /// schema is sent and the fence parser is still kept — identical
    /// verdicts from both paths is the M11 T0.5 property, applied here.
    pub structured_output: bool,
    /// Send no sampling param at all. Sonnet 5 rejects the *presence* of
    /// `temperature`, so the judge's default is `true` — the shape M11 P0
    /// resolved for every Sonnet role.
    pub unsampled: bool,
}

impl JudgeConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            reasoning: Some("low".into()),
            max_tokens: JUDGE_MAX_TOKENS,
            structured_output: false,
            unsampled: true,
        }
    }
}

pub struct ClientEvaluator {
    client: nsllm::client::OpenRouterClient,
    cfg: JudgeConfig,
    /// The structural half stays symbolic, for `local.rs`'s reasons exactly:
    /// I6 is two logged facts and grounding is span attribution. It is kept
    /// here to render the turn for the prompt, not to override the verdict —
    /// the judge is being measured, so it is asked the whole question.
    symbolic: SymbolicEvaluator,
    /// Requests this evaluator has spent in this pass. Reported by
    /// `pass.rs`, because a scorer that costs money has to be readable in
    /// the same report as the κ it bought.
    requests: AtomicU32,
}

impl ClientEvaluator {
    /// The only constructor: `None` in, `None` out.
    ///
    /// T1.3's exit criterion, as a type. There is no path from an unset
    /// `judge_model` to an object that can dial anything.
    pub fn for_model(
        judge_model: Option<&str>,
        client: nsllm::client::OpenRouterClient,
        cfg: impl FnOnce(JudgeConfig) -> JudgeConfig,
        symbolic: SymbolicEvaluator,
    ) -> Option<Self> {
        let model = judge_model.map(str::trim).filter(|m| !m.is_empty())?;
        Some(Self {
            client,
            cfg: cfg(JudgeConfig::new(model)),
            symbolic,
            requests: AtomicU32::new(0),
        })
    }

    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    /// The turn, as the judge sees it.
    ///
    /// `view.shown` and nothing else: T2.3a's invariant is that an evaluator
    /// judges the *recorded* material, so this renders what the replier was
    /// given rather than rebuilding a context at grading time.
    fn prompt(&self, view: &TurnView<'_>) -> String {
        let mut s = String::new();
        s.push_str("<user_message>\n");
        s.push_str(view.user);
        s.push_str("\n</user_message>\n<material_shown>\n");
        if view.shown.is_empty() {
            s.push_str("(nothing — no facts, no summary, no trace)");
        } else {
            for line in view.shown {
                s.push_str(line);
                s.push('\n');
            }
        }
        s.push_str("</material_shown>\n<reply>\n");
        s.push_str(view.reply);
        s.push_str("\n</reply>\n");
        match view.next_user {
            Some(next) => {
                s.push_str("<next_user_message>\n");
                s.push_str(next);
                s.push_str("\n</next_user_message>\n");
            }
            None => s.push_str(
                "<next_user_message>(none — the session ended here)\
                               </next_user_message>\n",
            ),
        }
        // Two structural facts the log already settled, given as context so
        // the judge is not guessing at them. Stated, never asserted as the
        // verdict: `ignored_request` is still its call to make.
        s.push_str(&format!(
            "<recorded>tool_called_or_proposed={} router_tier_wanted_action={}</recorded>\n",
            view.acted, view.task_tier
        ));
        s
    }

    fn request(&self, view: &TurnView<'_>) -> serde_json::Value {
        let mut req = serde_json::json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": TASK},
                {"role": "user", "content": self.prompt(view)},
            ],
        });
        // The one place any of these keys is written, M11 T0.2's rule.
        let mut shape = if self.cfg.unsampled {
            nsllm::provider::RequestShape::unsampled(self.cfg.max_tokens)
        } else {
            nsllm::provider::RequestShape::pinned(self.cfg.max_tokens)
        };
        shape.reasoning_effort = self.cfg.reasoning.clone();
        shape.apply(&mut req);
        if self.cfg.structured_output {
            req.as_object_mut().expect("a request is an object").insert(
                "response_format".into(),
                serde_json::json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "verdict",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["ok", "issues"],
                            "properties": {
                                "ok": {"type": "boolean"},
                                "issues": {
                                    "type": "array",
                                    "items": {
                                        "type": "string",
                                        "enum": [
                                            "reask", "ungrounded", "ignored_question",
                                            "ignored_request", "correction",
                                        ],
                                    },
                                },
                            },
                        },
                    },
                }),
            );
        }
        req
    }
}

/// One code, or `None` for a word this lane has no name for. An unknown code
/// is dropped rather than mapped to the nearest one: the vocabulary is what κ
/// is computed over, and a guess would be agreement this scorer did not earn.
fn issue_of(code: &str) -> Option<Issue> {
    match code.trim().to_ascii_lowercase().as_str() {
        "reask" => Some(Issue::Reask),
        "ungrounded" => Some(Issue::Ungrounded),
        "ignored_question" => Some(Issue::IgnoredQuestion),
        "ignored_request" => Some(Issue::IgnoredRequest),
        "correction" => Some(Issue::Correction),
        "none" => Some(Issue::None),
        _ => None,
    }
}

/// `SymbolicEvaluator::resolve`'s precedence, extended by one.
///
/// The same order for the same reason — strongest evidence first — with
/// `Correction` below the structural checks and above the lexical ones: it is
/// the user's own next message contradicting the reply, which is stronger
/// evidence than a token-overlap heuristic and weaker than two logged facts.
fn strongest(issues: &[Issue]) -> Issue {
    for want in [
        Issue::IgnoredRequest,
        Issue::Ungrounded,
        Issue::Correction,
        Issue::Reask,
        Issue::IgnoredQuestion,
    ] {
        if issues.contains(&want) {
            return want;
        }
    }
    Issue::None
}

/// A model's answer → a grade, or the reason there is none.
///
/// Separate from the call so the parsing is testable without a transport,
/// and so every rejection path is visible in one place.
pub fn parse_verdict(content: &str, scorer: String) -> Result<TurnGrade, GradeError> {
    let body = strip_fence(content);
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| GradeError::Unavailable(format!("verdict is not JSON ({e})")))?;
    let ok = v
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| GradeError::Unavailable("verdict has no boolean `ok`".into()))?;
    let raw = v
        .get("issues")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| GradeError::Unavailable("verdict has no `issues` array".into()))?;
    let mut issues: Vec<Issue> = Vec::new();
    for item in raw {
        let code = item
            .as_str()
            .ok_or_else(|| GradeError::Unavailable("an issue was not a string".into()))?;
        if let Some(i) = issue_of(code) {
            if i.is_problem() {
                issues.push(i);
            }
        }
    }
    // `ok` and `issues` disagreeing is the model contradicting itself, and
    // the list is the half a grade is made of — but a bare `ok: false` with
    // nothing named is not a code this lane has, and inventing one would be
    // inventing agreement. Both are unusable answers, so both are the same
    // answer: not graded.
    if ok != issues.is_empty() {
        return Err(GradeError::Unavailable(format!(
            "verdict contradicts itself: ok={ok} with {} issues",
            issues.len()
        )));
    }
    let issue = strongest(&issues);
    Ok(TurnGrade {
        issue,
        answers_user: match issue {
            Issue::None => 2,
            Issue::IgnoredQuestion | Issue::IgnoredRequest => 0,
            _ => 1,
        },
        grounded: !issues.contains(&Issue::Ungrounded),
        scorer,
    })
}

/// ```` ```json … ``` ```` → the JSON. `summarizer::strip_fence`'s shape,
/// kept here rather than shared because that one is private and this is four
/// lines; both exist because a model asked for JSON sometimes fences it.
fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

#[async_trait::async_trait]
impl Evaluator for ClientEvaluator {
    fn id(&self) -> String {
        format!("client:{}", self.cfg.model)
    }

    /// The model id. A judge has exactly one thing that drifts under it, and
    /// M8 T2.3a's rule is that whatever drifts is recorded beside the grade.
    fn revision(&self) -> String {
        self.cfg.model.clone()
    }

    fn requests(&self) -> Option<u32> {
        Some(self.requests())
    }

    async fn grade(&self, view: &TurnView<'_>) -> Result<TurnGrade, GradeError> {
        // Counted before the call, not after it: a request that fails was
        // still spent, and a count that only saw successes would understate
        // the one budget that runs out. `client.chat`'s own retries are
        // counted in the `Usage` record, which is where attempts live.
        self.requests.fetch_add(1, Ordering::Relaxed);
        let body = self
            .client
            .chat(self.request(view))
            .await
            .map_err(|e| GradeError::Unavailable(e.to_string()))?;
        let content = body
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GradeError::Unavailable("no message content in the reply".into()))?;
        parse_verdict(content, self.id())
    }
}

impl ClientEvaluator {
    /// The structural findings, for a caller that wants them beside a
    /// verdict — the same pair `local.rs` exposes, so a sweep over the judge
    /// can be arithmetic rather than one request per grid point.
    pub fn structural(&self, view: &TurnView<'_>) -> (bool, Vec<String>) {
        self.symbolic.structural(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluate::EvaluateConfig;
    use nsllm::transport::MockTransport;
    use std::sync::Arc;

    fn symbolic() -> SymbolicEvaluator {
        SymbolicEvaluator {
            cfg: EvaluateConfig::default(),
        }
    }

    fn said(content: &str) -> serde_json::Value {
        serde_json::json!({"choices": [{"message": {"content": content}}]})
    }

    fn judge(bodies: Vec<serde_json::Value>) -> (ClientEvaluator, Arc<MockTransport>) {
        let transport = MockTransport::ok(bodies);
        let client =
            nsllm::client::OpenRouterClient::new(transport.clone(), "k".into()).with_retry(1, 0);
        let e = ClientEvaluator::for_model(
            Some("anthropic/claude-sonnet-5"),
            client,
            |c| JudgeConfig {
                structured_output: true,
                ..c
            },
            symbolic(),
        )
        .expect("a named judge constructs");
        (e, transport)
    }

    fn view<'a>(user: &'a str, reply: &'a str, shown: &'a [&'a str]) -> TurnView<'a> {
        TurnView {
            user,
            shown,
            acted: false,
            task_tier: false,
            reply,
            next_user: None,
        }
    }

    /// T1.3's first exit criterion, and the one that costs nothing to hold:
    /// no judge model, no evaluator, therefore no request. The constructor
    /// is the gate, so no call site can forget it.
    #[test]
    fn it_is_never_constructed_without_a_judge_model() {
        for absent in [None, Some(""), Some("   ")] {
            let transport = MockTransport::ok(vec![said("{\"ok\": true, \"issues\": []}")]);
            let client = nsllm::client::OpenRouterClient::new(transport.clone(), "k".into());
            let built = ClientEvaluator::for_model(absent, client, |c| c, symbolic());
            assert!(
                built.is_none(),
                "judge_model {absent:?} must not construct an evaluator"
            );
            assert!(
                transport.requests.lock().unwrap().is_empty(),
                "and nothing may be dialled on the way to not constructing one"
            );
        }
        assert!(ClientEvaluator::for_model(
            Some("anthropic/claude-sonnet-5"),
            nsllm::client::OpenRouterClient::new(MockTransport::ok(vec![]), "k".into()),
            |c| c,
            symbolic(),
        )
        .is_some());
    }

    /// A verdict that will not parse is `Unavailable` — the turn was not
    /// graded — and never a grade. Four shapes of bad answer, because the
    /// one that matters is `ok: false` with an empty list: it looks like a
    /// grade and carries no finding, and reading it as `Issue::None` would
    /// record the opposite of what the model said.
    #[tokio::test]
    async fn a_malformed_verdict_is_unavailable_not_a_grade() {
        for bad in [
            "not json at all",
            "{\"issues\": []}",
            "{\"ok\": false, \"issues\": []}",
            "{\"ok\": true, \"issues\": [\"ungrounded\"]}",
            "{\"ok\": true, \"issues\": [7]}",
        ] {
            let (e, _t) = judge(vec![said(bad)]);
            let err = e
                .grade(&view("kde jsem?", "v Praze", &["user.city Praha"]))
                .await
                .expect_err("a bad verdict is not a grade");
            assert!(
                matches!(err, GradeError::Unavailable(_)),
                "{bad:?} gave {err:?}, wanted Unavailable"
            );
            assert_eq!(e.requests(), 1, "and the request it cost is still counted");
        }

        // An HTTP failure is the same answer, for a different reason.
        let transport = MockTransport::new(vec![Err(nsllm::transport::TransportError::Network(
            "down".into(),
        ))]);
        let client = nsllm::client::OpenRouterClient::new(transport, "k".into()).with_retry(1, 0);
        let e = ClientEvaluator::for_model(Some("m"), client, |c| c, symbolic()).unwrap();
        assert!(matches!(
            e.grade(&view("a", "b", &[])).await,
            Err(GradeError::Unavailable(_))
        ));
    }

    /// A well-formed verdict does become a grade, under this scorer's own
    /// id, and the request carries the P0 shape: no `temperature` key at all
    /// for a Sonnet judge, an effort block, a capped `max_tokens`, and the
    /// json_schema the preset advertises.
    #[tokio::test]
    async fn a_well_formed_verdict_becomes_a_grade_in_the_p0_request_shape() {
        let (e, transport) = judge(vec![said(
            "```json\n{\"ok\": false, \"issues\": [\"correction\"]}\n```",
        )]);
        let g = e
            .grade(&view(
                "kdy má sestra narozeniny?",
                "14. března",
                &["sister.birthday 15. března"],
            ))
            .await
            .expect("a parseable verdict is a grade");
        assert_eq!(g.issue, Issue::Correction);
        assert_eq!(g.scorer, "client:anthropic/claude-sonnet-5");
        assert_eq!(e.id(), "client:anthropic/claude-sonnet-5");
        assert_eq!(e.revision(), "anthropic/claude-sonnet-5");
        assert_eq!(Evaluator::requests(&e), Some(1));

        let sent = transport.requests.lock().unwrap()[0].clone();
        assert!(
            sent.get("temperature").is_none(),
            "Sonnet rejects the presence of the key: {sent}"
        );
        assert_eq!(sent["max_tokens"], serde_json::json!(JUDGE_MAX_TOKENS));
        assert_eq!(sent["reasoning"], serde_json::json!({"effort": "low"}));
        assert_eq!(sent["response_format"]["json_schema"]["name"], "verdict");
        let prompt = sent["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("sister.birthday 15. března"), "{prompt}");
        assert!(prompt.contains("14. března"), "{prompt}");
    }

    /// T1.3's headline exit criterion, with the gate `pass.rs` applies
    /// standing in front of it: a judge whose grades disagree with the
    /// symbolic reference contributes observations and no candidates.
    ///
    /// Driven through the real pass rather than asserted on κ directly —
    /// the number is only worth anything if the thing that reads it is the
    /// thing that gates on it.
    #[tokio::test]
    async fn a_paid_evaluator_below_kappa_yields_observations_only() {
        use crate::pass::{EvolutionPass, PassConfig};
        use nscore::{EventKind, EventLog, MemoryStore, SessionId, Timestamp};

        // Ten turns the symbolic checks call clean. The judge, scripted
        // through the transport, calls every one of them ungrounded — so the
        // two disagree on every turn both graded and κ cannot clear 0.4.
        let store = nsengine::store::InMemoryStore::new();
        let sid = SessionId("judged".into());
        let mut log = EventLog::new(sid.clone());
        for t in 1..=10u32 {
            log.append(
                t,
                Timestamp(t as u64),
                EventKind::UserSaid {
                    text: format!("message number {t}"),
                },
            );
            log.append(
                t,
                Timestamp(t as u64),
                EventKind::Replied {
                    text: format!("message number {t}"),
                },
            );
        }
        store.append(&sid, log.events()).await.unwrap();

        let bodies: Vec<serde_json::Value> = (0..10)
            .map(|_| said("{\"ok\": false, \"issues\": [\"ungrounded\"]}"))
            .collect();
        let (judge, _t) = judge(bodies);

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = PassConfig {
            dry_run: true,
            evaluator_min_kappa: 0.4,
            ..Default::default()
        };
        cfg.evaluate.budget_turns = 10;
        let pass = EvolutionPass::new(
            Arc::new(nsengine::arc_swap::ArcSwap::from_pointee(
                nscore::LearnedRules::default(),
            )),
            vec![],
            dir.path().join("learned.toml"),
            dir.path().join("ledger.json"),
            cfg,
        )
        .with_evaluator(Arc::new(judge));

        let report = pass.run_report(&store).await.unwrap();
        let row = report
            .agreement
            .iter()
            .find(|a| a.evaluator == "client:anthropic/claude-sonnet-5")
            .expect("the judge has a κ row of its own");
        assert!(
            !row.trusted(0.4),
            "a judge that disagrees with the reference everywhere is not trusted: {row}"
        );
        assert!(
            row.caveat.contains("observations only, no candidates"),
            "and the report says so: {row}"
        );
        assert_eq!(
            report.authoritative_evaluator, "symbolic",
            "the gate keeps believing the thing it is calibrated against"
        );
        // And the requests it spent are in the report, beside the κ they
        // bought — the number that makes a paid lane affordable to read.
        assert_eq!(
            report
                .evaluator_requests
                .get("client:anthropic/claude-sonnet-5")
                .copied(),
            Some(10),
            "one request per graded turn, capped by evaluate_budget_turns"
        );
        let printed = report.to_string();
        assert!(
            printed.contains("requests spent: client:anthropic/claude-sonnet-5 10"),
            "{printed}"
        );
    }
}
