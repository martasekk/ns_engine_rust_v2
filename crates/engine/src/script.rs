use async_trait::async_trait;
use nscore::{
    ActionSpec, ClassifiedProposal, Emission, EmitError, Emitter, EmitterContext, Guard, GuardCtx,
    LegalActionSet, Proposal, Replier, ReplyContext, ReplyError, SideEffect, Tool, ToolCtx,
    ToolError, ToolOutput, Trust, Verdict,
};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Pops proposals front-to-back; when exhausted, always proposes "respond_directly".
/// If a popped proposal's action is not legal, it is returned anyway —
/// the ENGINE must reject it (that's what we're testing).
pub struct ScriptedEmitter {
    queue: Mutex<VecDeque<Emission>>,
}

impl ScriptedEmitter {
    pub fn new(proposals: Vec<Proposal>) -> Self {
        Self::answering(
            proposals
                .into_iter()
                .map(|proposal| Emission {
                    proposal,
                    answer: None,
                })
                .collect(),
        )
    }

    /// M12 T4.4a: a script whose entries may answer instead of acting, for
    /// the chat-tier act-or-answer path. `new` is this with every `answer`
    /// set to `None`, which is what every test written before M12 gets.
    pub fn answering(emissions: Vec<Emission>) -> Self {
        Self {
            queue: Mutex::new(emissions.into()),
        }
    }

    fn pop(&self) -> Emission {
        let popped = self
            .queue
            .lock()
            .expect("scripted emitter lock")
            .pop_front();
        popped.unwrap_or_else(|| Emission {
            proposal: Proposal {
                rationale: "nothing left to do".into(),
                action: "respond_directly".into(),
                args: serde_json::json!({}),
            },
            answer: None,
        })
    }
}

#[async_trait]
impl Emitter for ScriptedEmitter {
    async fn propose(
        &self,
        _ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Proposal, EmitError> {
        Ok(self.pop().proposal)
    }

    async fn propose_or_answer(
        &self,
        _ctx: EmitterContext,
        _legal: &LegalActionSet,
    ) -> Result<Emission, EmitError> {
        Ok(self.pop())
    }
}

/// Echoes the turn trace back: "TRACE:\n<turn_trace>".
pub struct ScriptedReplier;

#[async_trait]
impl Replier for ScriptedReplier {
    async fn reply(&self, ctx: ReplyContext) -> Result<String, ReplyError> {
        Ok(format!("TRACE:\n{}", ctx.turn_trace))
    }
}

/// Summarizer double: returns a fixed topic naming the range it was given
/// and records every input it saw (previous present?, first and last turn).
#[derive(Default)]
pub struct ScriptedSummarizer {
    /// Shared with the test that owns the engine.
    pub calls: std::sync::Arc<Mutex<Vec<(bool, u32, u32)>>>,
    /// When set, every call fails with this transport detail.
    pub fail_with: Option<String>,
}

#[async_trait]
impl nscore::Summarizer for ScriptedSummarizer {
    async fn summarize(
        &self,
        input: nscore::SummaryInput<'_>,
    ) -> Result<Option<nscore::SummaryDraft>, nscore::SummarizeError> {
        if let Some(detail) = &self.fail_with {
            return Err(nscore::SummarizeError::Transport(detail.clone()));
        }
        let first = input.records.first().map(|r| r.turn).unwrap_or(0);
        let last = input.records.last().map(|r| r.turn).unwrap_or(0);
        self.calls.lock().expect("scripted summarizer lock").push((
            input.previous.is_some(),
            first,
            last,
        ));
        Ok(Some(nscore::SummaryDraft {
            topic: format!("scripted summary of turns {first}-{last}"),
            established: input.records.iter().map(|r| r.user.clone()).collect(),
            open: vec![],
        }))
    }
}

/// Pure tool "echo": args {"text": string} -> summary = "echo: <text>".
pub struct EchoTool {
    spec: ActionSpec,
}

impl EchoTool {
    pub fn new() -> Self {
        Self {
            spec: ActionSpec {
                name: "echo".into(),
                description: "echo text back".into(),
                args_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }),
                side_effect: SideEffect::Pure,
                residual_policy: Default::default(),
                dedupe_tag: None,
            },
        }
    }
}

impl Default for EchoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    async fn call(
        &self,
        args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::Failed {
                kind: "bad_args".into(),
                detail: "missing text".into(),
            })?;
        Ok(ToolOutput {
            summary: format!("echo: {text}"),
            artifact: None,
            trust: Trust::System,
        })
    }
}

/// Guard that denies a named action with a fixed reason.
pub struct DenyAction {
    pub action: String,
    pub reason: String,
}

impl Guard for DenyAction {
    fn name(&self) -> &str {
        "deny_action"
    }

    fn check(&self, p: &ClassifiedProposal, _ctx: &GuardCtx) -> Verdict {
        if p.proposal.action == self.action {
            Verdict::Deny {
                reason: self.reason.clone(),
            }
        } else {
            Verdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn legal_echo() -> LegalActionSet {
        LegalActionSet {
            actions: vec![EchoTool::new().spec().clone()],
        }
    }

    fn ctx() -> EmitterContext {
        EmitterContext {
            facts: vec![],
            summary: None,
            window: vec![],
            caps: Default::default(),
            user_text: "".into(),
            obligations: vec![],
            trace_so_far: vec![],
            pending_confirmation: false,
            rejections_this_turn: vec![],
            guidance: vec![],
            budget_line: None,
            usage: None,
            answer: None,
        }
    }

    #[tokio::test]
    async fn scripted_emitter_pops_then_responds_directly() {
        let e = ScriptedEmitter::new(vec![Proposal {
            rationale: "r".into(),
            action: "echo".into(),
            args: serde_json::json!({"text":"hi"}),
        }]);
        let p1 = e.propose(ctx(), &legal_echo()).await.unwrap();
        assert_eq!(p1.action, "echo");
        let p2 = e.propose(ctx(), &legal_echo()).await.unwrap();
        assert_eq!(p2.action, "respond_directly");
    }

    /// M12 T4.4a. `answering` is the only way a script carries a reply text,
    /// and `new` must keep meaning "act, never answer" — every test written
    /// before M12 builds its emitter that way.
    #[tokio::test]
    async fn a_scripted_emission_can_answer_instead_of_acting() {
        let e = ScriptedEmitter::answering(vec![Emission {
            proposal: Proposal {
                rationale: "answered".into(),
                action: "respond_directly".into(),
                args: serde_json::json!({}),
            },
            answer: Some("it is 10:41".into()),
        }]);
        let first = e.propose_or_answer(ctx(), &legal_echo()).await.unwrap();
        assert_eq!(first.answer.as_deref(), Some("it is 10:41"));
        assert_eq!(first.proposal.action, "respond_directly");
        // Exhausted, it falls back to the same respond_directly as `new`.
        let second = e.propose_or_answer(ctx(), &legal_echo()).await.unwrap();
        assert_eq!(second.answer, None);
        assert_eq!(second.proposal.action, "respond_directly");

        // And a plain script never answers, whichever method is called.
        let plain = ScriptedEmitter::new(vec![Proposal {
            rationale: "r".into(),
            action: "echo".into(),
            args: serde_json::json!({"text":"hi"}),
        }]);
        let emission = plain.propose_or_answer(ctx(), &legal_echo()).await.unwrap();
        assert_eq!(emission.answer, None);
        assert_eq!(emission.proposal.action, "echo");
    }

    #[tokio::test]
    async fn echo_tool_echoes() {
        let t = EchoTool::new();
        let out = t
            .call(
                &serde_json::json!({"text":"ahoj"}),
                &ToolCtx {
                    session: SessionId("s".into()),
                    artifacts: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(out.summary, "echo: ahoj");
        assert_eq!(out.trust, Trust::System);
    }

    #[test]
    fn deny_guard_denies_only_named_action() {
        let g = DenyAction {
            action: "echo".into(),
            reason: "not today".into(),
        };
        let tool = EchoTool::new();
        let fired = std::collections::HashSet::new();
        let ctx = GuardCtx {
            spec: tool.spec(),
            turn: 1,
            confirmed_this_turn: false,
            fired_actions: &fired,
            pending_confirmation: None,
        };
        let cp = |action: &str| ClassifiedProposal {
            proposal: Proposal {
                rationale: "".into(),
                action: action.into(),
                args: serde_json::json!({}),
            },
            args: vec![],
        };
        assert!(matches!(g.check(&cp("echo"), &ctx), Verdict::Deny { .. }));
        assert!(matches!(g.check(&cp("other"), &ctx), Verdict::Allow));
    }
}
