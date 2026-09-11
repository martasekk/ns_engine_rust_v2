//! The fenced reference block, shared by the replier and — on a chat-tier
//! act-or-answer call — by the emitter (M12 T4.1).
//!
//! Extracted verbatim out of `replier.rs`, and the extraction is the whole
//! point: an emitter that may answer has to see the same background under
//! the same rule, and two copies of a prompt block drift. The replier's
//! request bytes do not move.

use nscore::{Caps, FactView, SessionSummary, TurnRecord};

/// Reference material: the stable blocks, tagged and fenced. M6 §4.3 order
/// (facts → summary → verbatim window) is preserved; what changed is the
/// role it is sent in and the fence around it.
///
/// Both changes answer the same failure. Flat, untagged, in the same `user`
/// message as the task, the window reads as a document to continue rather
/// than as background to draw on, and the model continues it — 16 replies of
/// `fact user.previous_name = Tomas` in the live `cli` session, and t135's
/// `"hi\nwhat time is it?"`, which is the user's own line handed back.
/// Fencing untrusted or reference content behind explicit markers is
/// spotlighting's *delimiting* variant (Microsoft, CEUR Vol-3920), reported
/// at minimal task cost; datamarking and encoding are not used here — the
/// problem is a lazy model, not an adversary, and both cost legibility.
pub fn render_reference(
    facts: &[FactView],
    summary: Option<&SessionSummary>,
    window: &[TurnRecord],
    caps: &Caps,
) -> String {
    let mut s = String::from(
        "<reference>\nBackground, so you know what is true and what has already been \
         said. Draw on it; never reproduce a line of it.\n",
    );
    if !facts.is_empty() {
        s.push_str("\n<facts>\n");
        for f in facts {
            s.push_str(&format!("- {}\n", nscore::render_fact(f)));
        }
        s.push_str("</facts>\n");
    }
    if let Some(summary) = summary {
        s.push_str("\n<summary>\n");
        s.push_str(&nscore::render_summary(summary));
        s.push_str("\n</summary>\n");
    }
    if !window.is_empty() {
        s.push_str("\n<transcript>\n");
        s.push_str(&nscore::render_window(window, window.len(), caps));
        s.push_str("\n</transcript>\n");
    }
    s.push_str("</reference>");
    s
}

/// M11 T1.2. The sentence added when every reference block is empty.
///
/// An empty `<reference>` is not the same as a reference that happens to say
/// nothing relevant, and a model that is shown nothing tends to fill the gap
/// from its own weights (2606.06055). Saying so is cheap — one sentence, and
/// only on the turns that have nothing to draw on.
pub const MEMORY_SILENCE: &str = "Nothing in memory bears on this; say so rather than guessing.";

/// The replier's closing instruction, word for word (M12 T4.1).
///
/// The old line asked the model to "state only outcomes and values that
/// appear above", which read literally is a request for a copy — and got
/// one. Answering comes first now; grounding is the constraint on the
/// answer, not the task. An emitter that may answer is answering under the
/// same constraint, which is why this is a const and not two paragraphs.
pub const ANSWER_INSTRUCTION: &str =
    "Answer the user's message, in your own words, speaking to them. Never reproduce \
     a line from <reference>, <did> or the user's message verbatim — a copied line is \
     not an answer. State no outcome, value or name that does not appear above; if \
     something failed or was refused, say so plainly. Do not invent tool results, \
     names, or numbers. Plain text, no markdown.";

/// Whether this turn has any reference material at all: no facts, no
/// summary, and no `recall` in the trace, which is the one action that goes
/// looking for material the context did not carry.
pub fn memory_is_silent(facts: &[FactView], summary: Option<&SessionSummary>, trace: &str) -> bool {
    facts.is_empty() && summary.is_none() && !trace.lines().any(|l| l.contains("recall"))
}
