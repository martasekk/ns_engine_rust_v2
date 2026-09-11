//! Mining (spec M5 §3.1): scan a session log for failure signatures and
//! route each to a lane — symbolic (a deterministic patch could fix it) or
//! note (only guidance to the emitter could).
use nscore::{
    validate_args, ActionSpec, Event, EventId, EventKind, Op, Proposal, RejectReason, ReplyPolicy,
    SessionId, ToolOutcome,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum SignatureKind {
    /// A normalize candidate that makes `validate_args` pass.
    MalformedArg {
        action: String,
        arg: String,
        ops: Vec<Op>,
    },
    IllegalNearTool {
        proposed: String,
        candidate: String,
    },
    FallbackReply,
    RepeatedGuardDenial {
        guard: String,
        reason: String,
    },
    ToolErrArgs {
        action: String,
        detail: String,
    },
    Corrected {
        text: String,
    },
    /// The budget dropped something (or, under `report` mode, said it would)
    /// and the turn or the next one then went wrong (M7 T5.3).
    ///
    /// The correlation is the signature; the drop alone is not. Self-GC
    /// reports 85% of its prunes leaving the continuation unaffected, and
    /// M7's T2.2 makes that share — the no-impact rate — the condition for
    /// switching `budget_mode` to `enforce`. A drop nothing followed is
    /// evidence *for* enforcing, so mining it as a failure would invert the
    /// number the phase exists to measure.
    BudgetDropped {
        /// `window`, `facts` or `summary` — [`nscore::Dropped::block`].
        block: String,
        /// The turn number or fact key that went.
        detail: String,
    },
    /// A tool result was clipped and the model then spent an iteration on
    /// `inspect_result` to get the rest (M7 T5.3).
    ///
    /// Named by the action whose result was clipped, because that is the
    /// shape of the fix: `tool_result_max_chars` is one number for every
    /// tool, and the recorded desktop session says it should not be — a
    /// `pointer_ui_read` runs 14,425 characters against a median result of
    /// 24. An inspection that follows a clip is the cap saying, in the only
    /// currency the free tier meters, that it was too low for that action.
    ResultClippedThenInspected {
        action: String,
    },
    /// The router put the turn in a tier that hid a tool the model then
    /// asked for, and `run_turn` widened the tier mid-turn (M7 T5.3).
    ///
    /// The escalation already bounds the damage to one iteration, so this is
    /// not a failure the user saw — it is a request spent on a wrong guess
    /// about the message, on a fifty-a-day tier. The fix is a cue in
    /// `[router] recall_cues` or the tool-family list, and the user text of
    /// the turn is where it comes from.
    Misrouted {
        from: nscore::Tier,
        to: nscore::Tier,
    },
    /// The user asked the same thing again (M6 §8.2, M8 T2.1).
    ///
    /// Produced by [`crate::evaluate`], not by this module: mining reads what
    /// the harness did, and this reads what the user had to do about it. It
    /// lives in the same enum because a signature is a signature — the gate,
    /// the ledger and the note proposer should not learn two vocabularies for
    /// "this turn went wrong".
    ///
    /// `band` is load-bearing. Only [`crate::evaluate::ReaskBand::Repeat`] is
    /// a calibration proxy for T2.7; the reformulated band is counted while
    /// its true-positive rate is unmeasured.
    UserReask {
        times: u32,
        band: crate::evaluate::ReaskBand,
    },
    /// The in-turn grounding interceptor flagged this turn's first draft
    /// (M6 §4.5, §8.2) and the reply was regenerated with the spans named.
    ///
    /// A recorded value, surfaced — never a re-run of the check. See
    /// [`crate::evaluate`] for why the pass cannot rebuild the material the
    /// replier was shown, and T2.3a for why it would be wrong to try.
    UngroundedReply {
        spans: Vec<String>,
    },
    /// The user asked a question and the reply shares no content word with it
    /// (M6 §8.2; I5 in Higashinaka et al.'s taxonomy).
    ///
    /// Weak, and the spec says so: a reply that answers in different words is
    /// indistinguishable from one that ignored the question, by exactly the
    /// mechanism M8 Phase 1 measured at a 75–83% miss rate.
    ///
    /// **Measured 0 for 2 on the recorded session** (T2.1, 2026-09-09), and
    /// both false positives are instructive rather than fixable. Turn 13 asks
    /// "what time is it?" and is answered "It is currently 09:36:12 UTC on
    /// Tuesday, 2026-09-08" — a correct answer states the *value*, not the
    /// question's words. Turn 17's question mark is not a question mark at
    /// all: it is the console's rendering of "koš", and Czech inflection then
    /// keeps "vysyp" from matching "vysypání".
    ///
    /// So it follows `crates/engine/src/echo.rs`'s precedent exactly — counted, never
    /// acting, because the observability is what found this and it is free.
    /// It is not a κ proxy and must not become one on this evidence.
    IgnoredQuestion,
    /// The router put the turn in the `Task` tier — the engine's own recorded
    /// judgement that the message wanted something done — and the turn then
    /// proposed nothing and called nothing (I6 in the same taxonomy).
    ///
    /// The mirror of [`SignatureKind::Misrouted`], out of the same manifest
    /// field: there the tier was too narrow and the turn widened it; here it
    /// was wide enough and nothing happened.
    IgnoredRequest,
}

impl SignatureKind {
    pub fn lane(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } | SignatureKind::IllegalNearTool { .. } => {
                "symbolic"
            }
            // The three M7 signatures are notes, and the reason is the
            // symbolic lane's own type rather than a judgement about them.
            // `Patch` is `NormalizeArg | AliasAction`, and `verify_patch`
            // proves a candidate by replaying recorded sessions against a
            // patched `LearnedRules`. A per-action `tool_result_max_chars`
            // and a router cue are neither: they are `EngineConfig` read at
            // startup, outside the object the gate can patch and outside the
            // one replay varies. Plan §9 wants both in the symbolic lane
            // eventually; that needs `Patch` to grow the variants first, and
            // having the signatures counted is what makes it worth doing.
            _ => "note",
        }
    }

    /// Is this a signature about the *reply*, rather than about what the
    /// emitter emitted (M9 T1.2)?
    ///
    /// The four that [`crate::evaluate`] produces all are: a re-ask, an
    /// ungrounded reply, an ignored question, an ignored request. They are
    /// what the user had to do about a bad answer, and the notes gate cannot
    /// measure a candidate for one of them without a recorded grade — the
    /// probe runs a live emitter against a scripted replier, so nothing in
    /// its own classification looks at reply quality at all.
    ///
    /// Everything mined from what the harness *did* — a malformed argument,
    /// an illegal action near a tool, a guard denial — is false here and its
    /// path through the gate is exactly what it was before M9.
    pub fn from_grades(&self) -> bool {
        matches!(
            self,
            SignatureKind::UserReask { .. }
                | SignatureKind::UngroundedReply { .. }
                | SignatureKind::IgnoredQuestion
                | SignatureKind::IgnoredRequest
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            SignatureKind::MalformedArg { .. } => "MalformedArg",
            SignatureKind::IllegalNearTool { .. } => "IllegalNearTool",
            SignatureKind::FallbackReply => "FallbackReply",
            SignatureKind::RepeatedGuardDenial { .. } => "RepeatedGuardDenial",
            SignatureKind::ToolErrArgs { .. } => "ToolErrArgs",
            SignatureKind::Corrected { .. } => "Corrected",
            SignatureKind::BudgetDropped { .. } => "BudgetDropped",
            SignatureKind::ResultClippedThenInspected { .. } => "ResultClippedThenInspected",
            SignatureKind::Misrouted { .. } => "Misrouted",
            SignatureKind::UserReask { .. } => "UserReask",
            SignatureKind::UngroundedReply { .. } => "UngroundedReply",
            SignatureKind::IgnoredQuestion => "IgnoredQuestion",
            SignatureKind::IgnoredRequest => "IgnoredRequest",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub session: SessionId,
    pub turn: u32,
    pub event_id: EventId,
    pub kind: SignatureKind,
}

pub const SYNTHETIC_ACTIONS: [&str; 6] = [
    "respond_directly",
    "ask_clarification",
    "remember_fact",
    "forget_fact",
    "forget_all",
    "recall",
];

/// Lowercase ASCII alphanumerics only: "getTime" and "get_time" squash equal.
use nscore::squash;

/// The legal name a near-miss most plausibly meant: exact squashed match,
/// else the unique name within Damerau-Levenshtein 2. Ties yield nothing —
/// an alias must be unambiguous to be a patch.
pub fn near_tool(proposed: &str, names: &[String]) -> Option<String> {
    if names.iter().any(|n| n == proposed) {
        return None;
    }
    let p = squash(proposed);
    if let Some(exact) = names.iter().find(|n| squash(n) == p) {
        return Some(exact.clone());
    }
    let mut best: Vec<(usize, &String)> = names
        .iter()
        .map(|n| (strsim::damerau_levenshtein(&p, &squash(n)), n))
        .filter(|(d, _)| *d <= 2)
        .collect();
    best.sort_by_key(|(d, _)| *d);
    match best.as_slice() {
        [(d0, _), (d1, _), ..] if d0 == d1 => None,
        [(_, n0), ..] => Some((*n0).clone()),
        [] => None,
    }
}

/// Tried in order; the first sequence that validates wins (shortest fix).
const OP_SEQUENCES: [&[Op]; 3] = [
    &[Op::Trim],
    &[Op::Trim, Op::StripPunct],
    &[Op::Trim, Op::StripPunct, Op::Lowercase],
];

/// An (arg, ops) pair such that rewriting exactly that one string arg makes
/// the args validate. None when the args already validate or nothing helps.
pub fn normalize_candidate(
    spec: &ActionSpec,
    args: &serde_json::Value,
) -> Option<(String, Vec<Op>)> {
    let obj = args.as_object()?;
    if validate_args(&spec.args_schema, args).is_ok() {
        return None;
    }
    for (name, value) in obj {
        let Some(s) = value.as_str() else { continue };
        for ops in OP_SEQUENCES {
            let mut patched = args.clone();
            patched[name] = serde_json::Value::String(nscore::apply_ops(ops, s));
            if validate_args(&spec.args_schema, &patched).is_ok() {
                return Some((name.clone(), ops.to_vec()));
            }
        }
    }
    None
}

pub fn is_fallback(policy: &ReplyPolicy) -> bool {
    match policy {
        ReplyPolicy::Verbatim { text } => text.starts_with(nsengine::turn::FALLBACK_REPLY),
        ReplyPolicy::Template { id, .. } => id == "cant_help",
        ReplyPolicy::Generate => false,
    }
}

/// The user's message reduced to its words: lowercased, every run of
/// non-alphanumerics collapsed to one space.
///
/// Unicode-aware rather than [`nscore::squash`], which keeps ASCII
/// alphanumerics only. The re-asks on this machine are Czech, and squashing
/// turns "napiš" into "napi" and "nápis" into "npis" — a normalizer that
/// deletes the diacritics reports two different questions as one, which is
/// exactly the false positive a re-ask signature must not have.
fn normalized_user_text(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turns that ended badly: a fallback reply, a reply that failed, a
/// correction, or a message the user had already sent in an earlier turn.
///
/// The re-ask is the one worth explaining. When the harness answers around a
/// question the user asked, the user asks it again — recorded turns 85 and
/// 104 of the 2026-09-02 session are both that — and the second asking is
/// the only signal in the log that the first answer was no good. Nothing
/// else marks it: the turn settled, the reply was sent, no guard fired.
fn troubled_turns(events: &[Event]) -> HashSet<u32> {
    let mut asked: HashMap<String, u32> = HashMap::new();
    let mut out = HashSet::new();
    for e in events {
        match &e.kind {
            EventKind::UserSaid { text } => match asked.entry(normalized_user_text(text)) {
                std::collections::hash_map::Entry::Occupied(first) => {
                    if *first.get() != e.turn {
                        out.insert(e.turn);
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(e.turn);
                }
            },
            EventKind::Settled { policy } if is_fallback(policy) => {
                out.insert(e.turn);
            }
            // A reply the model could not produce is a fallback the user saw
            // (F7), the same reading `FallbackReply` already takes.
            EventKind::ReplyFailed { .. } => {
                out.insert(e.turn);
            }
            EventKind::Corrected { .. } => {
                out.insert(e.turn);
            }
            _ => {}
        }
    }
    out
}

/// The event id behind a trace handle: `r42` and `42` alike, the same pair
/// `run_turn` accepts from the model, so a proposal the engine honoured is
/// one this can follow.
fn parse_result_handle(raw: &str) -> Option<u64> {
    raw.trim()
        .trim_start_matches(['r', 'R'])
        .parse::<u64>()
        .ok()
}

/// The `id` argument of an `inspect_result` call, if it named one.
fn inspected_handle(args: &[(String, nscore::TaggedValue)]) -> Option<u64> {
    args.iter()
        .find(|(k, _)| k == "id")
        .and_then(|(_, tv)| tv.value.as_str())
        .and_then(parse_result_handle)
}

/// One line per event of `turn`: the trace text a note proposer reads.
pub fn render_turn(events: &[Event], turn: u32) -> String {
    events
        .iter()
        .filter(|e| e.turn == turn)
        // What the turn cost is not something a note proposer can act on,
        // and this text is itself a prompt.
        .filter(|e| {
            !matches!(
                e.kind,
                EventKind::ModelCall { .. }
                    | EventKind::Graded { .. }
                    | EventKind::ReplyCited { .. }
            )
        })
        .map(|e| match &e.kind {
            EventKind::ModelCall { .. }
            | EventKind::Graded { .. }
            | EventKind::ReplyCited { .. } => {
                unreachable!("filtered above")
            }
            EventKind::UserSaid { text } => format!("UserSaid: {text}"),
            EventKind::Proposed { proposal } => {
                format!("Proposed: {} {}", proposal.action, proposal.args)
            }
            EventKind::Rejected { reason, .. } => match reason {
                RejectReason::Malformed { detail } => format!("Rejected: malformed — {detail}"),
                RejectReason::ProviderUnavailable { status, detail } => {
                    format!("Rejected: provider HTTP {status} — {detail}")
                }
                RejectReason::IllegalAction { action } => {
                    format!("Rejected: illegal action {action}")
                }
                RejectReason::GuardDenied { guard, reason } => {
                    format!("Rejected: guard {guard} — {reason}")
                }
            },
            EventKind::ToolCalled { action, .. } => format!("ToolCalled: {action}"),
            EventKind::ToolReturned { outcome, .. } => match outcome {
                ToolOutcome::Ok { output } => format!("ToolReturned: ok — {}", output.summary),
                ToolOutcome::Err { kind, detail } => format!("ToolReturned: err {kind} — {detail}"),
            },
            EventKind::PendingConfirmation { .. } => "PendingConfirmation".into(),
            EventKind::Confirmed { .. } => "Confirmed".into(),
            EventKind::Corrected { text, .. } => format!("Corrected: {text}"),
            EventKind::Settled { policy } => match policy {
                ReplyPolicy::Verbatim { text } => format!("Settled: verbatim — {text}"),
                ReplyPolicy::Template { id, .. } => format!("Settled: template {id}"),
                ReplyPolicy::Generate => "Settled: generate".into(),
            },
            EventKind::Replied { text } => format!("Replied: {text}"),
            EventKind::ReplyFailed { detail } => format!("ReplyFailed: {detail}"),
            EventKind::ReplyFlagged { spans, .. } => format!("ReplyFlagged: {}", spans.join(", ")),
            EventKind::ReplyEchoed { span, ratio, .. } => {
                format!("ReplyEchoed ({ratio:.2}): {span}")
            }
            EventKind::Summarized { summary } => {
                format!("Summarized: through turn {}", summary.through_turn)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn mine(session: &SessionId, events: &[Event], known_specs: &[ActionSpec]) -> Vec<Signature> {
    let specs: HashMap<&str, &ActionSpec> =
        known_specs.iter().map(|s| (s.name.as_str(), s)).collect();
    let mut legal_names: Vec<String> = known_specs.iter().map(|s| s.name.clone()).collect();
    legal_names.extend(SYNTHETIC_ACTIONS.iter().map(|s| s.to_string()));
    let proposals: HashMap<u64, &Proposal> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Proposed { proposal } => Some((e.id.0, proposal)),
            _ => None,
        })
        .collect();
    let calls: HashMap<u64, &str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolCalled { action, .. } => Some((e.id.0, action.as_str())),
            _ => None,
        })
        .collect();
    // Which `ToolCalled` a `ToolReturned` answered, so the handle `r42` in an
    // `inspect_result` argument can be resolved back to the action whose cap
    // was too low.
    let returned_of: HashMap<u64, u64> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolReturned { call, .. } => Some((e.id.0, call.0)),
            _ => None,
        })
        .collect();
    // The turn after a given turn, taken from the log rather than assumed to
    // be `turn + 1`: a store that ever hands back a gap would otherwise make
    // every drop look like it was followed by nothing.
    let mut turn_order: Vec<u32> = Vec::new();
    for e in events {
        if !turn_order.contains(&e.turn) {
            turn_order.push(e.turn);
        }
    }
    let next_turn: HashMap<u32, u32> = turn_order.windows(2).map(|w| (w[0], w[1])).collect();
    let troubled = troubled_turns(events);
    let followed_by_trouble = |turn: u32| {
        troubled.contains(&turn)
            || next_turn
                .get(&turn)
                .is_some_and(|next| troubled.contains(next))
    };

    let mut seen_denials: HashSet<(u32, String, String)> = HashSet::new();
    let mut counted_denials: HashSet<(u32, String, String)> = HashSet::new();
    // Turns in which a `ModelCall` has already reported clipped characters.
    // Built as the walk goes, which is what makes "and an `inspect_result`
    // call followed" mean followed rather than merely co-occurred.
    let mut clipped_so_far: HashSet<u32> = HashSet::new();
    // Highest tier a turn's manifests have shown so far. A rise is the
    // escalation `run_turn` performs when a tiered-out tool is proposed.
    let mut tier_so_far: HashMap<u32, nscore::Tier> = HashMap::new();
    let mut counted_drops: HashSet<(u32, String, String)> = HashSet::new();
    let mut counted_inspections: HashSet<(u32, String)> = HashSet::new();
    let mut out = Vec::new();
    let sig = |e: &Event, kind: SignatureKind| Signature {
        session: session.clone(),
        turn: e.turn,
        event_id: e.id,
        kind,
    };
    for e in events {
        match &e.kind {
            EventKind::Rejected {
                proposal_of,
                reason,
            } => match reason {
                // A provider outage teaches the evolution pass nothing about
                // the emitter's behaviour: no proposal was ever made.
                RejectReason::ProviderUnavailable { .. } => {}
                RejectReason::Malformed { .. } => {
                    if let Some(p) = proposals.get(&proposal_of.0) {
                        if let Some(spec) = specs.get(p.action.as_str()) {
                            if let Some((arg, ops)) = normalize_candidate(spec, &p.args) {
                                let kind = SignatureKind::MalformedArg {
                                    action: p.action.clone(),
                                    arg,
                                    ops,
                                };
                                out.push(sig(e, kind));
                            }
                        }
                    }
                }
                RejectReason::IllegalAction { action } => {
                    if let Some(candidate) = near_tool(action, &legal_names) {
                        let kind = SignatureKind::IllegalNearTool {
                            proposed: action.clone(),
                            candidate,
                        };
                        out.push(sig(e, kind));
                    }
                }
                RejectReason::GuardDenied { guard, reason } => {
                    let key = (e.turn, guard.clone(), reason.clone());
                    if !seen_denials.insert(key.clone()) && counted_denials.insert(key) {
                        let kind = SignatureKind::RepeatedGuardDenial {
                            guard: guard.clone(),
                            reason: reason.clone(),
                        };
                        out.push(sig(e, kind));
                    }
                }
            },
            EventKind::Settled { policy } if is_fallback(policy) => {
                out.push(sig(e, SignatureKind::FallbackReply))
            }
            // A failed reply is a fallback the user saw (F7); same lane.
            EventKind::ReplyFailed { .. } => out.push(sig(e, SignatureKind::FallbackReply)),
            EventKind::ToolReturned {
                call,
                outcome: ToolOutcome::Err { kind, detail },
            } if kind == "bad_args" => {
                let action = calls.get(&call.0).unwrap_or(&"?").to_string();
                out.push(sig(
                    e,
                    SignatureKind::ToolErrArgs {
                        action,
                        detail: detail.clone(),
                    },
                ));
            }
            EventKind::Corrected { text, .. } => {
                out.push(sig(e, SignatureKind::Corrected { text: text.clone() }))
            }
            // M7 T5.3. Three signatures out of one event: `ModelCall` is the
            // only record of what a context cost and what was taken out of
            // it, and all three questions the phase leaves open are about
            // that.
            EventKind::ModelCall { manifest, .. } => {
                if let Some(budget) = &manifest.budget {
                    // Under the default `report` mode nothing was actually
                    // dropped and `dropped` lists what enforcing would have
                    // taken. That is the case worth mining: the whole point
                    // of reporting first is to learn what enforcing would
                    // cost before paying it.
                    if followed_by_trouble(e.turn) {
                        for d in &budget.dropped {
                            // One signature per item, not per manifest: the
                            // no-impact rate's denominator is drops, so its
                            // numerator has to be drops too. Deduped across
                            // the turn's iterations, which re-compute the
                            // same fit against the same context.
                            let key = (e.turn, d.block.clone(), d.detail.clone());
                            if counted_drops.insert(key) {
                                out.push(sig(
                                    e,
                                    SignatureKind::BudgetDropped {
                                        block: d.block.clone(),
                                        detail: d.detail.clone(),
                                    },
                                ));
                            }
                        }
                    }
                }
                if manifest.clipped_chars > 0 {
                    clipped_so_far.insert(e.turn);
                }
                if let Some(tier) = manifest.tier {
                    let highest = tier_so_far.entry(e.turn).or_insert(tier);
                    if tier > *highest {
                        out.push(sig(
                            e,
                            SignatureKind::Misrouted {
                                from: *highest,
                                to: tier,
                            },
                        ));
                        *highest = tier;
                    }
                }
            }
            // `clipped_so_far` is filled by the walk itself, which is what
            // makes this "an `inspect_result` call followed" rather than
            // merely occurred in the same turn.
            EventKind::ToolCalled { action, args }
                if action == nsengine::turn::INSPECT_RESULT && clipped_so_far.contains(&e.turn) =>
            {
                // Which action's cap was too low. The `id` argument names the
                // `ToolReturned` that was clipped, and that names the
                // `ToolCalled` behind it. `?` when the model named a handle
                // that is not in this log — the cap was still too low, and
                // dropping the signature over an unresolvable argument would
                // lose the count.
                let clipped_action = inspected_handle(args)
                    .and_then(|id| returned_of.get(&id))
                    .and_then(|call| calls.get(call))
                    .unwrap_or(&"?")
                    .to_string();
                if counted_inspections.insert((e.turn, clipped_action.clone())) {
                    out.push(sig(
                        e,
                        SignatureKind::ResultClippedThenInspected {
                            action: clipped_action,
                        },
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nscore::*;

    fn spec(name: &str) -> ActionSpec {
        ActionSpec {
            name: name.into(),
            description: "".into(),
            args_schema: serde_json::json!({
                "type": "object",
                "properties": {"unit": {"type": "string", "enum": ["c", "f"]}, "city": {"type": "string"}},
                "required": ["city"]
            }),
            side_effect: SideEffect::Pure,
            residual_policy: Default::default(),
            dedupe_tag: None,
        }
    }

    fn log() -> EventLog {
        EventLog::new(SessionId("m".into()))
    }

    fn proposal(action: &str, args: serde_json::Value) -> Proposal {
        Proposal {
            rationale: "".into(),
            action: action.into(),
            args,
        }
    }

    #[test]
    fn near_tool_prefers_exact_normalized_match_then_unique_edit_distance() {
        let names: Vec<String> = ["get_time", "get_weather", "wipe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(near_tool("getTime", &names), Some("get_time".into()));
        assert_eq!(near_tool("get_tme", &names), Some("get_time".into()));
        assert_eq!(
            near_tool("get_time", &names),
            None,
            "already legal is not a near miss"
        );
        assert_eq!(near_tool("zzzzzz", &names), None);
        let ambiguous: Vec<String> = ["ab", "ac"].iter().map(|s| s.to_string()).collect();
        assert_eq!(near_tool("ad", &ambiguous), None, "ties yield nothing");
    }

    #[test]
    fn normalize_candidate_finds_the_shortest_op_sequence_that_validates() {
        let s = spec("get_weather");
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": " \"C\" "})),
            Some(("unit".into(), vec![Op::Trim, Op::StripPunct, Op::Lowercase]))
        );
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"city": "Brno", "unit": "kelvin"})),
            None
        );
        assert_eq!(
            normalize_candidate(&s, &serde_json::json!({"unit": "c"})),
            None,
            "missing required cannot be normalized"
        );
    }

    #[test]
    fn mines_malformed_illegal_fallback_repeat_toolerr_and_corrected() {
        let mut l = log();
        // turn 1: malformed unit, then illegal near-miss, then repeated guard denial, fallback.
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "weather".into(),
            },
        );
        let p1 = l
            .append(
                1,
                Timestamp(2),
                EventKind::Proposed {
                    proposal: proposal(
                        "get_weather",
                        serde_json::json!({"city": "Brno", "unit": " \"C\" "}),
                    ),
                },
            )
            .id;
        l.append(
            1,
            Timestamp(3),
            EventKind::Rejected {
                proposal_of: p1,
                reason: RejectReason::Malformed { detail: "x".into() },
            },
        );
        let p2 = l
            .append(
                1,
                Timestamp(4),
                EventKind::Proposed {
                    proposal: proposal("getWeather", serde_json::json!({})),
                },
            )
            .id;
        l.append(
            1,
            Timestamp(5),
            EventKind::Rejected {
                proposal_of: p2,
                reason: RejectReason::IllegalAction {
                    action: "getWeather".into(),
                },
            },
        );
        for _ in 0..2 {
            let p = l
                .append(
                    1,
                    Timestamp(6),
                    EventKind::Proposed {
                        proposal: proposal("wipe", serde_json::json!({})),
                    },
                )
                .id;
            l.append(
                1,
                Timestamp(7),
                EventKind::Rejected {
                    proposal_of: p,
                    reason: RejectReason::GuardDenied {
                        guard: "taint".into(),
                        reason: "external".into(),
                    },
                },
            );
        }
        l.append(
            1,
            Timestamp(8),
            EventKind::Settled {
                policy: ReplyPolicy::Verbatim {
                    text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY),
                },
            },
        );
        l.append(1, Timestamp(9), EventKind::Replied { text: "…".into() });
        // turn 2: tool error attributable to args, then a correction.
        l.append(
            2,
            Timestamp(10),
            EventKind::UserSaid {
                text: "again".into(),
            },
        );
        let c = l
            .append(
                2,
                Timestamp(11),
                EventKind::ToolCalled {
                    action: "get_weather".into(),
                    args: vec![],
                },
            )
            .id;
        l.append(
            2,
            Timestamp(12),
            EventKind::ToolReturned {
                call: c,
                outcome: ToolOutcome::Err {
                    kind: "bad_args".into(),
                    detail: "city unknown".into(),
                },
            },
        );
        l.append(
            2,
            Timestamp(13),
            EventKind::Corrected {
                target: None,
                text: "user.city = Brno".into(),
            },
        );

        let sigs = mine(
            &SessionId("m".into()),
            l.events(),
            &[spec("get_weather"), spec("wipe")],
        );
        let names: Vec<&str> = sigs.iter().map(|s| s.kind.name()).collect();
        assert_eq!(
            names,
            vec![
                "MalformedArg",
                "IllegalNearTool",
                "RepeatedGuardDenial",
                "FallbackReply",
                "ToolErrArgs",
                "Corrected"
            ]
        );
        assert!(matches!(
            &sigs[0].kind,
            SignatureKind::MalformedArg { action, arg, ops }
                if action == "get_weather" && arg == "unit"
                    && ops == &vec![Op::Trim, Op::StripPunct, Op::Lowercase]
        ));
        assert!(matches!(
            &sigs[1].kind,
            SignatureKind::IllegalNearTool { proposed, candidate }
                if proposed == "getWeather" && candidate == "get_weather"
        ));
        assert_eq!(sigs[0].turn, 1);
        assert_eq!(sigs[4].turn, 2);
        assert!(sigs.iter().all(|s| s.session.0 == "m"));
        assert_eq!(sigs[0].kind.lane(), "symbolic");
        assert_eq!(sigs[3].kind.lane(), "note");
    }

    // -----------------------------------------------------------------
    // M7 T5.3
    // -----------------------------------------------------------------

    fn usage() -> Usage {
        Usage {
            role: "emitter".into(),
            model: "m".into(),
            prompt_tokens: 1_000,
            completion_tokens: 10,
            estimated: false,
            attempts: 1,
            latency_ms: 5,
            tools_tokens: 0,
            cached_tokens: 0,
        }
    }

    fn model_call(manifest: ContextManifest) -> EventKind {
        EventKind::ModelCall {
            usage: usage(),
            manifest,
        }
    }

    /// A manifest whose fit reported one drop. `Report` mode on purpose: it
    /// is the default and the mode the no-impact rate has to be measured in.
    fn would_drop(block: &str, detail: &str) -> ContextManifest {
        ContextManifest {
            budget: Some(BudgetReport {
                limit: 6_000,
                before: 7_000,
                after: 6_400,
                mode: BudgetMode::Report,
                dropped: vec![Dropped {
                    block: block.into(),
                    detail: detail.into(),
                    tokens: 150,
                }],
            }),
            ..Default::default()
        }
    }

    fn fallback() -> EventKind {
        EventKind::Settled {
            policy: ReplyPolicy::Verbatim {
                text: format!("{} Reason: x.", nsengine::turn::FALLBACK_REPLY),
            },
        }
    }

    fn kinds(sigs: &[Signature]) -> Vec<&SignatureKind> {
        sigs.iter().map(|s| &s.kind).collect()
    }

    fn mined(l: &EventLog) -> Vec<Signature> {
        mine(&SessionId("m".into()), l.events(), &[spec("get_weather")])
    }

    /// The correlation is the signature. A drop followed by a fallback is the
    /// evidence that the priority order took something the turn needed; a
    /// drop followed by a normal turn is evidence that it did not, and is
    /// exactly the 85% Self-GC reports and T2.2 needs before `budget_mode`
    /// can go to `enforce`. Mining the second as a failure would invert the
    /// number the phase exists to produce.
    #[test]
    fn a_drop_is_mined_only_when_trouble_follows_it() {
        let mut l = log();
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "click the save button".into(),
            },
        );
        l.append(1, Timestamp(2), model_call(would_drop("window", "t3")));
        l.append(1, Timestamp(3), fallback());
        l.append(1, Timestamp(4), EventKind::Replied { text: "…".into() });
        // Turn 2 drops just as much and then works.
        l.append(
            2,
            Timestamp(5),
            EventKind::UserSaid {
                text: "thanks".into(),
            },
        );
        l.append(
            2,
            Timestamp(6),
            model_call(would_drop("facts", "order.43.status")),
        );
        l.append(2, Timestamp(7), EventKind::Replied { text: "ok".into() });

        let sigs = mined(&l);
        let dropped: Vec<&SignatureKind> = kinds(&sigs)
            .into_iter()
            .filter(|k| matches!(k, SignatureKind::BudgetDropped { .. }))
            .collect();
        assert_eq!(
            dropped,
            vec![&SignatureKind::BudgetDropped {
                block: "window".into(),
                detail: "t3".into(),
            }],
            "only the drop the fallback followed: {sigs:?}"
        );
    }

    /// The user asking the same thing twice is the only trace a bad answer
    /// leaves in a log that otherwise looks clean — the turn settled, the
    /// reply was sent, no guard fired. Recorded turns 85 and 104 of the
    /// 2026-09-02 session are both re-asks.
    #[test]
    fn a_drop_followed_by_the_same_question_again_is_a_re_ask() {
        let ask = |l: &mut EventLog, turn: u32, text: &str| {
            l.append(
                turn,
                Timestamp(turn as u64 * 10),
                EventKind::UserSaid { text: text.into() },
            );
        };
        let mut reasked = log();
        ask(&mut reasked, 1, "co jsem ti řekl o rozpočtu?");
        reasked.append(
            1,
            Timestamp(2),
            model_call(would_drop("facts", "budget.total")),
        );
        reasked.append(1, Timestamp(3), EventKind::Replied { text: "…".into() });
        // Same question, different punctuation and case.
        ask(&mut reasked, 2, "Co jsem ti řekl o rozpočtu");
        reasked.append(2, Timestamp(4), EventKind::Replied { text: "…".into() });

        assert_eq!(
            kinds(&mined(&reasked)),
            vec![&SignatureKind::BudgetDropped {
                block: "facts".into(),
                detail: "budget.total".into(),
            }]
        );

        let mut moved_on = log();
        ask(&mut moved_on, 1, "co jsem ti řekl o rozpočtu?");
        moved_on.append(
            1,
            Timestamp(2),
            model_call(would_drop("facts", "budget.total")),
        );
        moved_on.append(1, Timestamp(3), EventKind::Replied { text: "…".into() });
        ask(&mut moved_on, 2, "díky, to stačí");
        moved_on.append(2, Timestamp(4), EventKind::Replied { text: "…".into() });

        assert!(
            mined(&moved_on).is_empty(),
            "a drop the conversation carried on past is not a signature"
        );
    }

    /// Deduped per item per turn, because the emitter re-fits the same
    /// context on every iteration and would otherwise report one drop as
    /// twelve. The count is a rate's numerator; inflating it would move the
    /// number `budget_mode = enforce` is decided on.
    #[test]
    fn the_same_drop_across_iterations_of_one_turn_counts_once() {
        let mut l = log();
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "open the folder".into(),
            },
        );
        for i in 0..3 {
            l.append(1, Timestamp(2 + i), model_call(would_drop("window", "t3")));
        }
        l.append(1, Timestamp(9), fallback());

        let sigs = mined(&l);
        assert_eq!(
            sigs.iter()
                .filter(|s| matches!(s.kind, SignatureKind::BudgetDropped { .. }))
                .count(),
            1,
            "{sigs:?}"
        );
    }

    /// The cap is one number for every tool and the recorded desktop session
    /// says it should not be: `pointer_ui_read` runs 14,425 characters
    /// against a median result of 24. An inspection following a clip is that
    /// action asking for a bigger cap, in the currency the free tier meters.
    #[test]
    fn a_clipped_result_the_model_then_inspected_names_the_action() {
        let mut l = log();
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "what is on the screen".into(),
            },
        );
        let call = l
            .append(
                1,
                Timestamp(2),
                EventKind::ToolCalled {
                    action: "pointer_ui_read".into(),
                    args: vec![],
                },
            )
            .id;
        let returned = l
            .append(
                1,
                Timestamp(3),
                EventKind::ToolReturned {
                    call,
                    outcome: ToolOutcome::Ok {
                        output: ToolOutput {
                            summary: "node ".repeat(3_000),
                            artifact: None,
                            trust: Trust::External,
                        },
                    },
                },
            )
            .id;
        l.append(
            1,
            Timestamp(4),
            model_call(ContextManifest {
                clipped_chars: 13_225,
                ..Default::default()
            }),
        );
        l.append(
            1,
            Timestamp(5),
            EventKind::ToolCalled {
                action: nsengine::turn::INSPECT_RESULT.into(),
                args: vec![(
                    "id".into(),
                    TaggedValue {
                        value: serde_json::json!(format!("r{}", returned.0)),
                        prov: Provenance::Residual,
                        trust: Trust::External,
                    },
                )],
            },
        );

        assert_eq!(
            kinds(&mined(&l)),
            vec![&SignatureKind::ResultClippedThenInspected {
                action: "pointer_ui_read".into(),
            }],
            "the handle resolves back to the action whose cap was too low"
        );
    }

    /// The negative that keeps the signature meaning something. A clip
    /// nobody inspected is the cap working: the model read the head, decided
    /// it had enough, and spent no extra request. That is what a cap is for,
    /// and mining it would propose raising the number every time it did its
    /// job.
    #[test]
    fn a_clipped_result_nobody_inspected_is_not_a_signature() {
        let mut l = log();
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "what is on the screen".into(),
            },
        );
        let call = l
            .append(
                1,
                Timestamp(2),
                EventKind::ToolCalled {
                    action: "pointer_ui_read".into(),
                    args: vec![],
                },
            )
            .id;
        l.append(
            1,
            Timestamp(3),
            EventKind::ToolReturned {
                call,
                outcome: ToolOutcome::Ok {
                    output: ToolOutput {
                        summary: "node ".repeat(3_000),
                        artifact: None,
                        trust: Trust::External,
                    },
                },
            },
        );
        l.append(
            1,
            Timestamp(4),
            model_call(ContextManifest {
                clipped_chars: 13_225,
                ..Default::default()
            }),
        );
        l.append(1, Timestamp(5), EventKind::Replied { text: "…".into() });
        assert!(mined(&l).is_empty(), "{:?}", mined(&l));
    }

    /// And the other way round: an inspection with nothing clipped before it
    /// says nothing about any cap. Ordering is what separates the two, so it
    /// is checked rather than assumed.
    #[test]
    fn an_inspection_before_anything_was_clipped_is_not_a_signature() {
        let mut l = log();
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hm".into() });
        l.append(
            1,
            Timestamp(2),
            EventKind::ToolCalled {
                action: nsengine::turn::INSPECT_RESULT.into(),
                args: vec![(
                    "id".into(),
                    TaggedValue {
                        value: serde_json::json!("r7"),
                        prov: Provenance::Residual,
                        trust: Trust::External,
                    },
                )],
            },
        );
        l.append(
            1,
            Timestamp(3),
            model_call(ContextManifest {
                clipped_chars: 13_225,
                ..Default::default()
            }),
        );
        assert!(mined(&l).is_empty(), "{:?}", mined(&l));
    }

    /// The escalation in `run_turn` costs one emitter iteration — a request
    /// off a fifty-a-day tier, spent on a wrong guess about the message. The
    /// signature is what turns that into a cue the router can be given.
    #[test]
    fn a_tier_that_rises_inside_a_turn_is_a_misroute_and_a_steady_one_is_not() {
        let mut l = log();
        l.append(
            1,
            Timestamp(1),
            EventKind::UserSaid {
                text: "mohl bys otevřít ten soubor".into(),
            },
        );
        for tier in [Tier::Chat, Tier::Task] {
            l.append(
                1,
                Timestamp(2),
                model_call(ContextManifest {
                    tier: Some(tier),
                    ..Default::default()
                }),
            );
        }
        // Turn 2 routes to Task and stays there over three iterations.
        l.append(
            2,
            Timestamp(5),
            EventKind::UserSaid {
                text: "a ten druhý taky".into(),
            },
        );
        for _ in 0..3 {
            l.append(
                2,
                Timestamp(6),
                model_call(ContextManifest {
                    tier: Some(Tier::Task),
                    ..Default::default()
                }),
            );
        }

        assert_eq!(
            kinds(&mined(&l)),
            vec![&SignatureKind::Misrouted {
                from: Tier::Chat,
                to: Tier::Task,
            }],
            "one signature, for the turn that escalated"
        );
    }

    /// Names and lanes are the pass's whole interface to these: `Report`
    /// counts by `name()` and the notes lane selects by `lane()`. A typo in
    /// either is a signature that is mined and then never acted on.
    #[test]
    fn the_three_m7_signatures_are_named_and_sit_in_the_note_lane() {
        for kind in [
            SignatureKind::BudgetDropped {
                block: "window".into(),
                detail: "t3".into(),
            },
            SignatureKind::ResultClippedThenInspected {
                action: "pointer_ui_read".into(),
            },
            SignatureKind::Misrouted {
                from: Tier::Chat,
                to: Tier::Task,
            },
        ] {
            assert_eq!(
                kind.lane(),
                "note",
                "{} is guidance, not a patch the symbolic gate can replay",
                kind.name()
            );
        }
        assert_eq!(
            SignatureKind::BudgetDropped {
                block: String::new(),
                detail: String::new()
            }
            .name(),
            "BudgetDropped"
        );
        assert_eq!(
            SignatureKind::ResultClippedThenInspected {
                action: String::new()
            }
            .name(),
            "ResultClippedThenInspected"
        );
        assert_eq!(
            SignatureKind::Misrouted {
                from: Tier::Chat,
                to: Tier::Deep
            }
            .name(),
            "Misrouted"
        );
    }

    #[test]
    fn render_turn_is_one_line_per_event_of_that_turn() {
        let mut l = log();
        l.append(1, Timestamp(1), EventKind::UserSaid { text: "hi".into() });
        l.append(
            2,
            Timestamp(2),
            EventKind::UserSaid {
                text: "again".into(),
            },
        );
        l.append(2, Timestamp(3), EventKind::Replied { text: "ok".into() });
        let t = render_turn(l.events(), 2);
        assert_eq!(t, "UserSaid: again\nReplied: ok");
    }
}
