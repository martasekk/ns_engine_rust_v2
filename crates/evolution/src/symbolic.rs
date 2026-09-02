//! Symbolic lane (spec M5 §3.2): deterministic patches proposed from mined
//! signatures and gated by replay — the patch must flip its own evidence and
//! must not change any other baseline-clean recording.
use crate::ledger::Evidence;
use crate::mine::{Signature, SignatureKind};
use nscore::{ActionSpec, AliasAction, Event, LearnedRules, NormalizeArg, SessionId};
use nsengine::replay::{diff, normalize, replay_with, ReplayOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Patch {
    NormalizeArg(NormalizeArg),
    AliasAction(AliasAction),
}

impl Patch {
    /// "sha256:" + hex(sha256(canonical json)) — the ledger key.
    pub fn hash(&self) -> String {
        let json = serde_json::to_string(self).expect("patch serializes");
        format!("sha256:{:x}", Sha256::digest(json.as_bytes()))
    }
    /// Push unless an equal rule already exists.
    pub fn apply_to(&self, rules: &mut LearnedRules) {
        match self {
            Patch::NormalizeArg(n) => {
                if !rules.normalize_arg.contains(n) {
                    rules.normalize_arg.push(n.clone());
                }
            }
            Patch::AliasAction(a) => {
                if !rules.alias_action.contains(a) {
                    rules.alias_action.push(a.clone());
                }
            }
        }
    }
    /// The action a flipped evidence line must show as `ToolCalled`.
    pub fn expected_call(&self) -> &str {
        match self {
            Patch::NormalizeArg(n) => &n.action,
            Patch::AliasAction(a) => &a.to,
        }
    }
    pub fn summary(&self) -> String {
        match self {
            Patch::NormalizeArg(n) => format!(
                "normalize_arg {}.{} [{}]",
                n.action,
                n.arg,
                n.ops
                    .iter()
                    .map(|o| o.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Patch::AliasAction(a) => format!("alias_action {} -> {}", a.from, a.to),
        }
    }
}

/// Symbolic-lane signatures → patches, deduped by equality with evidence
/// merged; input order kept.
pub fn propose_patches(sigs: &[Signature]) -> Vec<(Patch, Vec<Evidence>)> {
    let mut out: Vec<(Patch, Vec<Evidence>)> = Vec::new();
    for s in sigs {
        let patch = match &s.kind {
            SignatureKind::MalformedArg { action, arg, ops } => Patch::NormalizeArg(NormalizeArg {
                action: action.clone(),
                arg: arg.clone(),
                ops: ops.clone(),
            }),
            SignatureKind::IllegalNearTool {
                proposed,
                candidate,
            } => Patch::AliasAction(AliasAction {
                from: proposed.clone(),
                to: candidate.clone(),
            }),
            _ => continue,
        };
        let ev = Evidence {
            session: s.session.0.clone(),
            turn: s.turn,
            event_id: s.event_id.0,
        };
        match out.iter_mut().find(|(p, _)| *p == patch) {
            Some((_, evs)) => evs.push(ev),
            None => out.push((patch, vec![ev])),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolicVerdict {
    pub accepted: bool,
    pub flipped: usize,
    pub not_flipped: usize,
    pub regressions: usize,
    /// Sessions that already diverge from their recording under `base`;
    /// they cannot witness a regression and are not counted.
    pub skipped_baseline: usize,
    pub detail: String,
}

pub type Recorded = (SessionId, Vec<Event>);

pub async fn verify_patch(
    patch: &Patch,
    evidence: &[Evidence],
    sessions: &[Recorded],
    base: &LearnedRules,
    known_specs: &[ActionSpec],
    regression_cap: usize,
) -> SymbolicVerdict {
    let mut candidate = base.clone();
    patch.apply_to(&mut candidate);
    let mut v = SymbolicVerdict {
        accepted: false,
        flipped: 0,
        not_flipped: 0,
        regressions: 0,
        skipped_baseline: 0,
        detail: String::new(),
    };
    let want = format!("ToolCalled {}", patch.expected_call());

    // 1. Flip check on every evidence pointer: under the candidate rules the
    //    rejected line must become the expected tool call.
    for ev in evidence {
        let Some((sid, recorded)) = sessions.iter().find(|(s, _)| s.0 == ev.session) else {
            v.not_flipped += 1;
            v.detail
                .push_str(&format!("evidence session {} not loaded; ", ev.session));
            continue;
        };
        let Some(idx) = recorded.iter().position(|e| e.id.0 == ev.event_id) else {
            v.not_flipped += 1;
            continue;
        };
        match replay_with(sid.clone(), recorded, opts(&candidate, known_specs, true)).await {
            Ok(r) => {
                let lines = normalize(&r.events);
                if lines.get(idx).map(|l| l == &want).unwrap_or(false) {
                    v.flipped += 1;
                } else {
                    v.not_flipped += 1;
                    v.detail.push_str(&format!(
                        "{}@{}: {} (wanted {want}); ",
                        ev.session,
                        idx,
                        lines.get(idx).cloned().unwrap_or_default()
                    ));
                }
            }
            Err(e) => {
                v.not_flipped += 1;
                v.detail
                    .push_str(&format!("{}: replay error {e}; ", ev.session));
            }
        }
    }

    // 2. Regression check on every other session, baseline-clean ones only.
    let evidence_sessions: std::collections::HashSet<&str> =
        evidence.iter().map(|e| e.session.as_str()).collect();
    for (sid, recorded) in sessions
        .iter()
        .filter(|(s, _)| !evidence_sessions.contains(s.0.as_str()))
        .take(regression_cap)
    {
        let baseline =
            match replay_with(sid.clone(), recorded, opts(base, known_specs, false)).await {
                Ok(r) => diff(recorded, &r.events).is_ok(),
                Err(_) => false,
            };
        if !baseline {
            v.skipped_baseline += 1;
            continue;
        }
        let same =
            match replay_with(sid.clone(), recorded, opts(&candidate, known_specs, false)).await {
                Ok(r) => diff(recorded, &r.events).is_ok(),
                Err(_) => false,
            };
        if !same {
            v.regressions += 1;
            v.detail.push_str(&format!("regression in {}; ", sid.0));
        }
    }

    v.accepted = v.flipped >= 1 && v.regressions == 0;
    v
}

fn opts(rules: &LearnedRules, specs: &[ActionSpec], synthetic_ok: bool) -> ReplayOptions {
    ReplayOptions {
        learned: Arc::new(rules.clone()),
        extra_guards: vec![],
        known_specs: specs.to_vec(),
        synthetic_ok_for_new_calls: synthetic_ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mine::{mine, Signature, SignatureKind};
    use nscore::*;
    use nsengine::script::{EchoTool, ScriptedEmitter, ScriptedReplier};
    use nsengine::store::{InMemoryStore, NoopConsolidator};
    use nsengine::turn::{Engine, EngineConfig};
    use std::sync::Arc;

    struct Closed;
    #[async_trait::async_trait]
    impl Channel for Closed {
        async fn recv(&mut self) -> Result<Incoming, ChannelError> {
            Err(ChannelError::Closed)
        }
        async fn send(&mut self, _s: &SessionId, _t: &str) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    /// Record one session: each proposal list entry is one turn's first proposal.
    async fn record(name: &str, turns: Vec<Proposal>, guards: Vec<Box<dyn Guard>>) -> Recorded {
        let store = Arc::new(InMemoryStore::new());
        let sid = SessionId(name.into());
        let mut b = HarnessBuilder::new();
        b.set_emitter(Box::new(ScriptedEmitter::new(turns.clone())));
        b.set_replier(Box::new(ScriptedReplier));
        b.set_memory(store.clone());
        b.set_channel(Box::new(Closed));
        b.set_consolidator(Box::new(NoopConsolidator));
        b.add_tool(Arc::new(EchoTool::new()));
        for g in guards {
            b.add_guard(g);
        }
        let mut e = Engine::with_clock(
            b.build().unwrap(),
            EngineConfig::default(),
            Box::new(|| Timestamp(1)),
        );
        for i in 0..turns.len() {
            e.run_turn(Incoming {
                session: sid.clone(),
                text: format!("turn {i}"),
            })
            .await
            .unwrap();
        }
        (sid, store.load(&SessionId(name.into())).await.unwrap())
    }

    fn typo() -> Proposal {
        Proposal {
            rationale: "".into(),
            action: "eko".into(),
            args: serde_json::json!({"text": "hi"}),
        }
    }
    fn good() -> Proposal {
        Proposal {
            rationale: "".into(),
            action: "echo".into(),
            args: serde_json::json!({"text": "hi"}),
        }
    }
    fn specs() -> Vec<ActionSpec> {
        vec![EchoTool::new().spec().clone()]
    }

    #[test]
    fn propose_dedupes_by_hash_and_merges_evidence() {
        let sig = |id: u64| Signature {
            session: SessionId("s".into()),
            turn: 1,
            event_id: EventId(id),
            kind: SignatureKind::IllegalNearTool {
                proposed: "eko".into(),
                candidate: "echo".into(),
            },
        };
        let note = Signature {
            session: SessionId("s".into()),
            turn: 1,
            event_id: EventId(9),
            kind: SignatureKind::FallbackReply,
        };
        let out = propose_patches(&[sig(3), note, sig(7)]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].0, Patch::AliasAction(a) if a.from == "eko" && a.to == "echo"));
        assert_eq!(
            out[0].1.iter().map(|e| e.event_id).collect::<Vec<_>>(),
            vec![3, 7]
        );
        assert!(out[0].0.hash().starts_with("sha256:"));
        assert_eq!(out[0].0.summary(), "alias_action eko -> echo");
    }

    #[tokio::test]
    async fn alias_patch_flips_its_evidence_and_passes_a_clean_regression_set() {
        let bad = record("bad", vec![typo()], vec![]).await;
        let clean = record("clean", vec![good()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(
            &patch,
            &evidence,
            &[bad.clone(), clean.clone()],
            &LearnedRules::default(),
            &specs(),
            200,
        )
        .await;
        assert!(v.accepted, "{}", v.detail);
        assert_eq!((v.flipped, v.regressions, v.skipped_baseline), (1, 0, 0));
    }

    #[tokio::test]
    async fn a_patch_that_changes_another_recording_is_rejected() {
        // "eko" was recorded as illegal in BOTH sessions, but the second one
        // is not evidence (we pass only the first as evidence): aliasing it
        // changes that recording → regression.
        let bad = record("bad", vec![typo()], vec![]).await;
        let other = record("other", vec![typo()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(
            &patch,
            &evidence,
            &[bad.clone(), other.clone()],
            &LearnedRules::default(),
            &specs(),
            200,
        )
        .await;
        assert!(!v.accepted);
        assert_eq!(v.regressions, 1);
    }

    #[tokio::test]
    async fn a_patch_that_does_not_flip_its_evidence_is_rejected() {
        // With no known specs the alias target "echo" has no double in replay,
        // so the rewritten proposal is still illegal: the line does not flip.
        let bad = record("bad", vec![typo()], vec![]).await;
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(
            &patch,
            &evidence,
            std::slice::from_ref(&bad),
            &LearnedRules::default(),
            &[],
            200,
        )
        .await;
        assert!(!v.accepted);
        assert_eq!((v.flipped, v.not_flipped), (0, 1));
    }

    #[tokio::test]
    async fn composition_verifies_against_base_plus_patch() {
        // Base already aliases eko→echo; a second, identical patch flips
        // nothing new but must not regress.
        let bad = record("bad", vec![typo()], vec![]).await;
        let base = LearnedRules {
            alias_action: vec![AliasAction {
                from: "eko".into(),
                to: "echo".into(),
            }],
            ..Default::default()
        };
        let sigs = mine(&bad.0, &bad.1, &specs());
        let (patch, evidence) = propose_patches(&sigs).remove(0);
        let v = verify_patch(
            &patch,
            &evidence,
            std::slice::from_ref(&bad),
            &base,
            &specs(),
            200,
        )
        .await;
        assert_eq!(v.flipped, 1);
        assert_eq!(v.regressions, 0);
    }
}
