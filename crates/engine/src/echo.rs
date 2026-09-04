//! Copy detector: how much of a reply is a verbatim span of its own prompt.
//!
//! `ground.rs` bounds overlap from *below* — it flags claims the material does
//! not support. That check is one-sided, and a one-sided check selects for
//! copying: a reply lifted whole out of the context is maximally grounded and
//! sails through. Sixteen replies of `fact user.previous_name = Tomas` did
//! exactly that (session `cli`, turns 137–158, `ReplyFlagged` never fired).
//! This module supplies the upper bound.
//!
//! The pull it measures is *contextual entrainment*: models raise the
//! probability of any token or sentence present in context, relevance be
//! damned (Niu et al., ACL 2025; sentence-level, arXiv 2606.24077). Two
//! results say why this has to be a runtime check rather than a better prompt.
//! Entrainment on *irrelevant* context grows with model size rather than
//! shrinking (arXiv 2604.13275), so a larger replier is not the fix; and
//! copying is far cheaper to induce than recall — steering α = −3.0 against
//! α = +30.0 (arXiv 2601.12075) — so instructions are a nudge, not a control.

/// Case-folded alphanumeric words. Punctuation separates, so
/// `fact user.previous_name = "Tomas"` and `fact user previous name Tomas`
/// are the same span: a model reformats what it copies.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Runs shorter than this are ordinary language, not copying: "your name is"
/// occurs in any honest answer built from the same facts. The cost of the
/// floor is that a copied line of three words or fewer escapes — worth it
/// against flagging every reply that says "yes".
const MIN_SPAN_WORDS: usize = 4;

/// Longest run of consecutive words the two share, as `(length, end index in
/// `a`)`. Rolling row: `a` is a reply and `b` a whole prompt, so the table is
/// never materialized.
fn longest_common_run(a: &[String], b: &[String]) -> (usize, usize) {
    let mut best = (0usize, 0usize);
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for (i, aw) in a.iter().enumerate() {
        for (j, bw) in b.iter().enumerate() {
            cur[j + 1] = if aw == bw { prev[j] + 1 } else { 0 };
            if cur[j + 1] > best.0 {
                best = (cur[j + 1], i + 1);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
        cur.iter_mut().for_each(|c| *c = 0);
    }
    best
}

/// Longest run of words shared with `material`, over the reply's word count.
/// `1.0` = the reply is nothing but a verbatim span of its own prompt; `0.0`
/// = nothing worth calling a copy. An empty reply scores `0.0`.
pub fn echo_ratio(reply: &str, material: &str) -> f32 {
    match copied_span(reply, material) {
        Some((len, _)) => {
            let total = words(reply).len();
            if total == 0 {
                0.0
            } else {
                len as f32 / total as f32
            }
        }
        None => 0.0,
    }
}

/// `(length, span text)` of the longest copied run, or `None` below the floor.
fn copied_span(reply: &str, material: &str) -> Option<(usize, String)> {
    let r = words(reply);
    if r.is_empty() {
        return None;
    }
    let (len, end) = longest_common_run(&r, &words(material));
    if len < MIN_SPAN_WORDS {
        return None;
    }
    Some((len, r[end - len..end].join(" ")))
}

/// The copied span, when the reply is at or over `max_ratio` lifted from its
/// own prompt. The span is what gets named back to the model on the single
/// regeneration, so it is returned rather than a bare verdict.
pub fn echoed(reply: &str, material: &str, max_ratio: f32) -> Option<String> {
    let (len, span) = copied_span(reply, material)?;
    let total = words(reply).len();
    let ratio = len as f32 / total.max(1) as f32;
    (ratio >= max_ratio).then_some(span)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live failure: session `cli` t141. The reply is the recall line.
    #[test]
    fn a_reply_lifted_whole_from_the_prompt_scores_one() {
        let material = "Recent turns:\n[t140] user: what was my name last time\n      \
                        did:  ToolReturned(ok: fact user.previous_name = \"Tomas\")";
        assert_eq!(echo_ratio("fact user.previous_name = Tomas", material), 1.0);
        assert!(echoed("fact user.previous_name = Tomas", material, 0.6).is_some());
    }

    /// Punctuation and case are not a disguise: t150 dropped the spaces
    /// around `=` and stayed the same copy.
    #[test]
    fn reformatting_does_not_hide_the_copy() {
        let material = "fact user.previous_name = \"Tomas\"";
        assert_eq!(echo_ratio("Fact user previous_name=Tomas", material), 1.0);
    }

    /// An answer built from the same material, in the model's own words.
    #[test]
    fn an_honest_answer_over_the_same_facts_is_not_an_echo() {
        let material = "Standing facts:\n- user.previous_name: \"Tomas\"\n- user.age: \"17\"";
        let reply = "You went by Tomas before, and you told me you are 17.";
        assert!(echo_ratio(reply, material) < 0.6, "{reply}");
        assert!(echoed(reply, material, 0.6).is_none());
    }

    /// Short replies are not copies, whatever the prompt contains.
    #[test]
    fn short_replies_are_below_the_floor() {
        let material = "Recent turns:\n[t2] user: hi\n      bot:  hi";
        assert_eq!(echo_ratio("hi", material), 0.0);
        assert_eq!(echo_ratio("Yes, that is right.", material), 0.0);
        assert_eq!(echo_ratio("", material), 0.0);
    }

    /// A partly-copied reply scores the copied fraction, not all-or-nothing.
    #[test]
    fn the_ratio_is_the_copied_fraction() {
        let material = "the quick brown fox jumps over the lazy dog";
        // 5 of the reply's 10 words are one verbatim run.
        let r = echo_ratio("the quick brown fox jumps and made up five words", material);
        assert!((r - 0.5).abs() < 0.01, "{r}");
    }

    #[test]
    fn the_span_returned_is_the_copied_text() {
        let material = "ToolReturned(ok: fact user.previous_name = \"Tomas\")";
        assert_eq!(
            echoed("fact user.previous_name = Tomas", material, 0.6).as_deref(),
            Some("fact user previous name tomas")
        );
    }

    #[test]
    fn a_reply_sharing_nothing_scores_zero() {
        assert_eq!(
            echo_ratio("completely unrelated prose here", "abc def"),
            0.0
        );
    }
}
