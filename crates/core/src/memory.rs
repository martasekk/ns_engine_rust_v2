//! Working and episodic memory types that cross the trait boundary (M6 spec
//! §4–§5): one verbatim record per completed turn, the bounded window both
//! models read, and the rolling summary of the turns that fell out of it.
use crate::value::Trust;
use serde::{Deserialize, Serialize};

/// One completed turn as both models see it: what the user said, what the
/// engine did (with outcomes and refusals), what was replied. A pure
/// projection of the log — never persisted, always rebuilt by the fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn: u32,
    /// Verbatim user text.
    pub user: String,
    /// One line per action, in order, e.g.
    /// `get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday)`,
    /// `remember_fact user.age=17 -> ok`, `wipe -> denied (taint_policy: …)`,
    /// `asked: Could you clarify …`, `staged: 'wipe' awaits confirmation`.
    pub did: Vec<String>,
    /// Verbatim reply text.
    pub reply: String,
    /// Minimum trust over the turn's tool outputs; `User` when no tool ran.
    pub trust: Trust,
}

/// Rendering caps for the window (config `[memory]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    /// Whole rendered record, including the user text.
    pub record_max_chars: usize,
    /// Each `did` line and the reply.
    pub line_max_chars: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            record_max_chars: 300,
            line_max_chars: 120,
        }
    }
}

/// Truncate to `max` characters (not bytes), marking the cut with `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Render one record. Lines are capped individually; the whole record is
/// capped last so a long user text cannot crowd out the outcome lines.
pub fn render_record(r: &TurnRecord, caps: &Caps) -> String {
    let mut s = format!("[t{}] user: {}", r.turn, r.user.replace('\n', " "));
    if !r.did.is_empty() {
        let did: Vec<String> = r
            .did
            .iter()
            .map(|d| truncate_chars(&d.replace('\n', " "), caps.line_max_chars))
            .collect();
        s.push_str(&format!("\n      did:  {}", did.join(" | ")));
    }
    if !r.reply.is_empty() {
        s.push_str(&format!(
            "\n      bot:  {}",
            truncate_chars(&r.reply.replace('\n', " "), caps.line_max_chars)
        ));
    }
    truncate_chars(&s, caps.record_max_chars)
}

/// The last `k` records, oldest first, one blank line between records.
/// Empty string when there is nothing to show.
pub fn render_window(records: &[TurnRecord], k: usize, caps: &Caps) -> String {
    let start = records.len().saturating_sub(k);
    records[start..]
        .iter()
        .map(|r| render_record(r, caps))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rolling summary of the turns that have fallen out of the window (M6
/// §5.1). Written by a cheap model off the hot path, stored as a
/// `Summarized` event, rebuilt from verbatim records to bound drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Last turn the summary covers.
    pub through_turn: u32,
    /// What the user is trying to do, one sentence.
    pub topic: String,
    /// Decisions and answers given that are not already facts.
    #[serde(default)]
    pub established: Vec<String>,
    /// Pending questions and unconfirmed requests.
    #[serde(default)]
    pub open: Vec<String>,
    /// Minimum trust over the summarized records (a summary built from
    /// external tool output stays external: findings §5, laundering).
    pub trust: Trust,
    /// First turn of the verbatim range this summary was built from
    /// (`1` after a full rebuild).
    #[serde(default = "one")]
    pub rebuilt_from: u32,
}

fn one() -> u32 {
    1
}

/// Lowercase ASCII alphanumerics only: `memory_reset_requested` and
/// `Memory.Reset.Requested` squash equal. Used for key canonicalization at
/// write time (M6 §6.1) and for near-miss action names (M5).
pub fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Query tokens worth matching: lowercase alphanumeric runs of 3+ chars.
pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_string)
        .collect()
}

/// Czech diacritics mapped to their ASCII base letter; everything else is
/// passed through unchanged (M12 T1.5).
///
/// A match table rather than a Unicode normalization crate: the alphabet
/// this engine is spoken in is known and closed, and a dependency for
/// nineteen letters would be paid for on every build.
pub fn fold_diacritics(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' => 'a',
            'ä' => 'a',
            'č' => 'c',
            'ď' => 'd',
            'é' => 'e',
            'ě' => 'e',
            'í' => 'i',
            'ň' => 'n',
            'ó' => 'o',
            'ô' => 'o',
            'ř' => 'r',
            'š' => 's',
            'ť' => 't',
            'ú' => 'u',
            'ů' => 'u',
            'ý' => 'y',
            'ž' => 'z',
            'Á' => 'A',
            'Ä' => 'A',
            'Č' => 'C',
            'Ď' => 'D',
            'É' => 'E',
            'Ě' => 'E',
            'Í' => 'I',
            'Ň' => 'N',
            'Ó' => 'O',
            'Ô' => 'O',
            'Ř' => 'R',
            'Š' => 'S',
            'Ť' => 'T',
            'Ú' => 'U',
            'Ů' => 'U',
            'Ý' => 'Y',
            'Ž' => 'Z',
            other => other,
        })
        .collect()
}

/// Whether two words are the same word, allowing for Czech inflection
/// (M12 T1.5).
///
/// Both sides are folded and lowercased; equal words match. Otherwise the
/// two must share a prefix of at least 3 characters, differ by at most 3
/// characters on *each* side, and the shorter of the two must be at least 4
/// characters long. That is a stemmer's rule with no stemmer: it accepts
/// `Praze`/`Praha` and `Tomáše`/`Tomáš` and refuses `Canberra`/`Canada` and
/// `Bob`/`Bobby`. Tight on purpose — the grounding check is what this feeds,
/// and a wrong match is an invented name passed off as supported.
pub fn stem_match(a: &str, b: &str) -> bool {
    let a: Vec<char> = fold_diacritics(a).to_lowercase().chars().collect();
    let b: Vec<char> = fold_diacritics(b).to_lowercase().chars().collect();
    if a == b {
        return !a.is_empty();
    }
    let shorter = a.len().min(b.len());
    if shorter < 4 {
        return false;
    }
    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    prefix >= 3 && a.len() - prefix <= 3 && b.len() - prefix <= 3
}

/// Verbs that open an imperative clause, Czech and English.
///
/// Deliberately a short closed list rather than a parser. The recorded risk
/// for M9 T2.1 is over-firing on cs+en mixed text, and the cost of a missed
/// obligation is one line absent from a prompt while the cost of a wrong one
/// is a regenerated reply. The list grows only when a real session shows a
/// request it missed.
const IMPERATIVE_OPENERS: &[&str] = &[
    // English
    "add",
    "call",
    "check",
    "close",
    "create",
    "delete",
    "explain",
    "find",
    "fix",
    "give",
    "list",
    "make",
    "open",
    "read",
    "remove",
    "run",
    "search",
    "send",
    "set",
    "show",
    "start",
    "stop",
    "tell",
    "translate",
    "update",
    "write",
    // Czech — both spellings, because both occur live
    "dej",
    "najdi",
    "napis",
    "napiš",
    "nastav",
    "oprav",
    "otevri",
    "otevři",
    "posli",
    "pošli",
    "preloz",
    "přelož",
    "pridej",
    "přidej",
    "rekni",
    "řekni",
    "smaz",
    "smaž",
    "spust",
    "spusť",
    "udelej",
    "udělej",
    "ukaz",
    "ukaž",
    "vysvetli",
    "vysvětli",
    "vytvor",
    "vytvoř",
    "zavolej",
    "zavri",
    "zavři",
    "zjisti",
    "zkontroluj",
];

/// What this turn owes the user, as a pure function of their message
/// (M9 T2.1).
///
/// Each clause ending in `?` becomes `answer: <clause>`; each clause opening
/// with one of [`IMPERATIVE_OPENERS`] becomes `do: <clause>`. Nothing else
/// becomes anything, so small talk yields an empty list. Deterministic and
/// capped at `max`.
///
/// Symbolic on purpose: obligations are checkable without a model, which is
/// why they are not `SessionSummary.open` — that is model-written prose the
/// summarizer rebuilds. Clauses split on `?`, `.`, `!`, `;` and newlines; a
/// clause with no content token (`query_tokens`) is dropped, so a bare "?"
/// owes nothing.
pub fn obligations_for(user_text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut clause = String::new();
    for ch in user_text.chars() {
        match ch {
            '?' | '.' | '!' | ';' | '\n' => {
                push_obligation(&mut out, &clause, ch == '?', max);
                clause.clear();
            }
            _ => clause.push(ch),
        }
    }
    push_obligation(&mut out, &clause, false, max);
    out
}

fn push_obligation(out: &mut Vec<String>, clause: &str, asked: bool, max: usize) {
    if out.len() >= max {
        return;
    }
    let clause = clause.trim();
    if clause.is_empty() || query_tokens(clause).is_empty() {
        return;
    }
    if asked {
        out.push(format!("answer: {clause}"));
        return;
    }
    let opener: String = clause
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if IMPERATIVE_OPENERS.contains(&opener.as_str()) {
        out.push(format!("do: {clause}"));
    }
}

/// Whether `reply` shares a content word with `asked`.
///
/// The ignored-question predicate, moved down here from
/// `nsevolution::evaluate::ignores_question` (M9 T2.1) so that the in-turn
/// obligation interceptor and the offline signature share one definition
/// instead of two that drift. Weak by construction — a reply that answers in
/// other words looks like one that ignored the question — which is why the
/// interceptor it gates is off by default and regenerates at most once.
pub fn addresses(asked: &str, reply: &str) -> bool {
    let asked: Vec<String> = query_tokens(asked);
    let answered: Vec<String> = query_tokens(reply);
    // M12 T1.5: a pair overlaps when the words share a stem, so a reply that
    // declines the asked word still counts as addressing it.
    //
    // Stems only on words of five characters or more. `stem_match` accepts a
    // three-character common prefix with up to three characters differing on
    // each side, which on short words is not inflection but coincidence:
    // "order" and "arrive" are safe, but the four-letter tail of an English
    // question matches half the dictionary, and a false overlap here is an
    // obligation silently reported as met. Five is where the rule stops
    // buying anything on Czech inflection ("vysyp"/"vysypání" survives it)
    // and starts costing precision.
    let overlaps = |t: &String, a: &String| {
        if t.chars().count() >= 5 && a.chars().count() >= 5 {
            stem_match(t, a)
        } else {
            fold_diacritics(t).to_lowercase() == fold_diacritics(a).to_lowercase()
        }
    };
    asked.is_empty()
        || answered.is_empty()
        || asked
            .iter()
            .any(|t| answered.iter().any(|a| overlaps(t, a)))
}

/// The activation prior's knobs (M9 T3.1).
///
/// `weight` is `[memory] activation_weight` and defaults to **0.0**, which
/// is today's behaviour exactly: the term is multiplied by it, so at zero it
/// contributes a hard `0.0` to every fact and the integer hit count is the
/// whole score. vstash's negative result on BEIR is why the default is off
/// and `ns-app eval --activation <w>` decides it, not this file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Activation {
    /// `w` in `hits + w · ln(1 + freq) · exp(−Δdays / half_life)`.
    pub weight: f32,
    /// Days for the recency term to halve — `[memory]
    /// activation_half_life_days`, default 7.0.
    pub half_life_days: f32,
    /// The clock's reading for this query. `Δ` is measured from the fact's
    /// `last_used` to here and clamped at zero, so a clock that went
    /// backwards costs a fact nothing.
    pub now: crate::event::Timestamp,
}

impl Default for Activation {
    fn default() -> Self {
        Self {
            weight: 0.0,
            half_life_days: 7.0,
            now: crate::event::Timestamp(0),
        }
    }
}

impl Activation {
    /// The prior with the term switched off — today's ranking.
    pub fn off() -> Self {
        Self::default()
    }

    /// `w · ln(1 + freq) · exp(−Δdays / half_life)`, or a hard `0.0` at
    /// `w = 0`.
    ///
    /// The early return is the byte-identity guarantee, not an optimization:
    /// it keeps a zero weight from ever producing a `NaN` (a zero half-life
    /// at `Δ = 0` would) and so from reordering two facts that today tie.
    fn bonus(&self, freq: u32, last_used: crate::event::Timestamp) -> f32 {
        if self.weight == 0.0 {
            return 0.0;
        }
        let days = self.now.0.saturating_sub(last_used.0) as f32 / 86_400_000.0;
        self.weight * (1.0 + freq as f32).ln() * (-days / self.half_life_days).exp()
    }
}

/// The frequency column the prior reads. Since M9 P4 it is `credits`, not
/// `uses`: recall ranks by how often a fact *helped*, not by how often it was
/// shown. `activation_weight` is 0.0 by default, so at today's config the
/// column changes nothing observable — the switch matters only on the day
/// T3.3's corpus moves the weight off zero.
fn activation_freq(f: &crate::action::Fact) -> u32 {
    f.credits
}

/// Half-life of the recency term in `search_turns`, in turns (M9 T3.2).
///
/// A constant rather than a knob: the turn number is not a clock, and the
/// only question the suite can answer is whether recency helps at all. One
/// knob (`activation_weight`) decides that for facts and turns together;
/// a second half-life would be a parameter nothing measured.
pub const RECENCY_HALF_LIFE_TURNS: f32 = 20.0;

/// M9 T3.2: rescore turn hits by how recent they are, then cut to `k`.
///
/// `score += w · exp(−(latest − turn) / half_life_turns)`, where `latest` is
/// the newest turn in the candidate set — the stores hand over their top
/// candidates, not the whole log, so this is the newest turn that *matched*,
/// which is the only recency anchor both stores can produce without a second
/// query.
///
/// At `w = 0` nothing is touched and nothing is re-sorted: the truncation is
/// the only thing that happens, exactly as before.
pub fn rescore_by_recency(hits: &mut Vec<crate::traits::TurnHit>, weight: f32, k: usize) {
    if weight != 0.0 && !hits.is_empty() {
        let latest = hits.iter().map(|h| h.turn).max().unwrap_or(0);
        for h in hits.iter_mut() {
            let behind = latest.saturating_sub(h.turn) as f32;
            h.score += (weight * (-behind / RECENCY_HALF_LIFE_TURNS).exp()) as f64;
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.turn.cmp(&a.turn))
        });
    }
    hits.truncate(k);
}

/// How many candidates a store fetches before [`rescore_by_recency`] cuts
/// them to `k`: rescoring the top `k` alone could only reorder them, never
/// bring a recent hit up from below the cut.
pub fn recency_candidates(k: usize) -> usize {
    (k * 4).max(20)
}

/// Lexical relevance of current facts to a query (M6 §6.5 "query-relevant"):
/// number of query tokens found in the key (dots and underscores read as
/// spaces) or the value; zero-score facts are dropped; ties go to the most
/// recently validated. Shared by both stores until FTS5 lands (Phase 4).
///
/// M9 T3.1 adds the activation prior: `score = hits + w · ln(1 + freq) ·
/// exp(−Δdays / half_life)`. The prior **reorders, it never admits** — a
/// fact with no query token in it is dropped before the term is computed,
/// however recently it was used. At `w = 0` every bonus is exactly `0.0`, so
/// equal-hit facts hold exactly equal float scores and fall through to the
/// same `last_validated` / key tiebreak they always did.
pub fn lexical_rank(
    facts: &[crate::action::Fact],
    query: &str,
    k: usize,
    activation: Activation,
) -> Vec<crate::action::Fact> {
    let tokens = query_tokens(query);
    if tokens.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(f32, &crate::action::Fact)> = facts
        .iter()
        .map(|f| {
            let hay = format!(
                "{} {}",
                f.key.replace(['.', '_', '-'], " ").to_lowercase(),
                match &f.value {
                    serde_json::Value::String(s) => s.to_lowercase(),
                    other => other.to_string().to_lowercase(),
                }
            );
            (
                tokens.iter().filter(|t| hay.contains(t.as_str())).count(),
                f,
            )
        })
        // Before the prior, not after: activation reorders the facts the
        // query already matched, it never admits one the query missed.
        .filter(|(hits, _)| *hits > 0)
        .map(|(hits, f)| {
            (
                hits as f32 + activation.bonus(activation_freq(f), f.last_used),
                f,
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.last_validated.cmp(&a.1.last_validated))
            .then_with(|| a.1.key.cmp(&b.1.key))
    });
    scored.into_iter().take(k).map(|(_, f)| f.clone()).collect()
}

/// A fact as the models see it (M6 §6.5): the current version plus, for
/// pinned keys, the value it superseded — "what was my name before" is
/// answerable from the context alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactView {
    pub key: String,
    pub value: serde_json::Value,
    pub confidence: f32,
    pub uses: u32,
    pub state: crate::action::FactState,
    /// The superseded value and when it stopped being current.
    #[serde(default)]
    pub previous: Option<(serde_json::Value, crate::event::Timestamp)>,
}

impl From<&crate::action::Fact> for FactView {
    fn from(f: &crate::action::Fact) -> Self {
        Self {
            key: f.key.clone(),
            value: f.value.clone(),
            confidence: f.confidence,
            uses: f.uses,
            state: f.state,
            previous: None,
        }
    }
}

impl From<crate::action::Fact> for FactView {
    fn from(f: crate::action::Fact) -> Self {
        (&f).into()
    }
}

/// `user.name: "Peter" (was "Martin" until 17:35 UTC)`, plus `(unverified)`
/// below confidence 0.75 and `(stale)` for a cold fact (M6 §6.1, §6.3).
pub fn render_fact(f: &FactView) -> String {
    let mut s = format!("{}: {}", f.key, f.value);
    if f.confidence < 0.75 {
        s.push_str(" (unverified)");
    }
    if f.state == crate::action::FactState::Cold {
        s.push_str(" (stale)");
    }
    if let Some((prev, until)) = &f.previous {
        s.push_str(&format!(
            " (was {prev} until {})",
            crate::time::format_utc_short(until.0)
        ));
    }
    s
}

impl SessionSummary {
    /// Deterministic size cap (M6 §5.1): drop `open` items from the end,
    /// then `established` items, then cut the topic. The model's output
    /// length is never trusted.
    pub fn clamp(&mut self, max_chars: usize) {
        while render_summary(self).chars().count() > max_chars {
            if self.open.pop().is_some() {
                continue;
            }
            if self.established.pop().is_some() {
                continue;
            }
            let keep = self.topic.chars().count().saturating_sub(20);
            if keep == 0 {
                break;
            }
            self.topic = truncate_chars(&self.topic, keep);
        }
    }
}

/// One block: "Conversation so far (turns 1–8): …\nEstablished: …\nOpen: …".
pub fn render_summary(s: &SessionSummary) -> String {
    let mut out = format!(
        "Conversation so far (turns {}–{}): {}",
        s.rebuilt_from, s.through_turn, s.topic
    );
    if !s.established.is_empty() {
        out.push_str(&format!("\nEstablished: {}", s.established.join("; ")));
    }
    if !s.open.is_empty() {
        out.push_str(&format!("\nOpen: {}", s.open.join("; ")));
    }
    out
}

/// M8 §6 T3.2: the RRF constant this repo fuses with.
///
/// Not the customary 60. That constant is tuned for lists of hundreds of
/// candidates, where a rank-60 document still deserves weight; here the lists
/// are `coarse_k` long — ten by default — and 60 would flatten every rank
/// difference the two arms disagree about into noise. The M8 retrieval sweep
/// fixed the band at `k ∈ [1, 10]`; 5 sits in the middle of it.
pub const RRF_K: f64 = 5.0;

/// Fuse ranked lists **by rank, never by score** (M8 §6).
///
/// Every list contributes `1 / (RRF_K + rank)` to each key it ranks, rank
/// counted from 1. Rank and not score because of nsmodels' own measurement: a
/// bi-encoder's cosine range is compressed enough — a true match at 0.898
/// against 0.862 for an unrelated Czech sentence — that a threshold across
/// arms is meaningless, and bm25's scale is unrelated to a cosine's, so
/// normalising the two is a guess wearing arithmetic.
///
/// Ties break on first appearance, so fusing one list returns that list
/// unchanged — the property the "service down" fallback rests on.
pub fn rrf_fuse<K: Clone + Eq + std::hash::Hash>(lists: &[Vec<K>]) -> Vec<K> {
    let mut score: std::collections::HashMap<K, f64> = Default::default();
    let mut order: Vec<K> = Vec::new();
    for list in lists {
        for (i, key) in list.iter().enumerate() {
            if !score.contains_key(key) {
                order.push(key.clone());
            }
            *score.entry(key.clone()).or_insert(0.0) += 1.0 / (RRF_K + (i as f64 + 1.0));
        }
    }
    let mut out = order;
    out.sort_by(|a, b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Cosine between two vectors nsmodels has already normalised, i.e. their dot
/// product. `None` when the lengths disagree — which is what a model change
/// looks like from here, and is why a stored vector records the model that
/// produced it (M8 T3.1).
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    Some(a.iter().zip(b).map(|(x, y)| x * y).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M8 §6: fusing one list must be that list, or the "service down"
    /// fallback is not a fallback.
    #[test]
    fn fusing_one_list_returns_it_unchanged() {
        let one = vec!["a", "b", "c"];
        assert_eq!(rrf_fuse(std::slice::from_ref(&one)), one);
        assert!(rrf_fuse::<&str>(&[]).is_empty());
    }

    /// Rank, not score: the key both arms rank beats the key one arm puts
    /// first, because two agreements outweigh one confident opinion at k = 5.
    #[test]
    fn rrf_ranks_agreement_above_either_arms_own_first() {
        let lexical = vec!["x", "shared"];
        let vector = vec!["y", "shared"];
        let fused = rrf_fuse(&[lexical, vector]);
        assert_eq!(fused[0], "shared", "fused = {fused:?}");
    }

    #[test]
    fn cosine_is_a_dot_product_and_refuses_a_dimension_change() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), None);
        assert_eq!(cosine(&[], &[]), None);
    }

    fn rec(turn: u32, user: &str, did: &[&str], reply: &str) -> TurnRecord {
        TurnRecord {
            turn,
            user: user.into(),
            did: did.iter().map(|s| s.to_string()).collect(),
            reply: reply.into(),
            trust: Trust::User,
        }
    }

    #[test]
    fn record_renders_user_did_and_bot_lines() {
        let r = rec(
            74,
            "tell me the time",
            &[
                "get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday)",
                "get_time -> denied (repeat_gate)",
            ],
            "It is 10:41 UTC.",
        );
        assert_eq!(
            render_record(&r, &Caps::default()),
            "[t74] user: tell me the time\n      did:  get_time -> ok: 2026-09-02 10:41:28 UTC (Wednesday) | get_time -> denied (repeat_gate)\n      bot:  It is 10:41 UTC."
        );
        let plain = rec(1, "hi", &[], "hello");
        assert_eq!(
            render_record(&plain, &Caps::default()),
            "[t1] user: hi\n      bot:  hello"
        );
    }

    #[test]
    fn caps_truncate_lines_and_whole_record_with_ellipsis() {
        let long = "x".repeat(500);
        let r = rec(2, &long, &[&long], &long);
        let caps = Caps {
            record_max_chars: 100,
            line_max_chars: 20,
        };
        let out = render_record(&r, &caps);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
        // a short user text with a long did line: only the line is cut
        let r = rec(3, "short", &[&long], "ok");
        let out = render_record(&r, &caps);
        assert!(out.contains(&format!("did:  {}…", "x".repeat(19))));
        assert!(out.ends_with("bot:  ok"));
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "ab…");
        // multi-byte safe
        assert_eq!(truncate_chars("žluťoučký", 4), "žlu…");
    }

    #[test]
    fn window_keeps_the_last_k_records_oldest_first() {
        let records: Vec<TurnRecord> = (1..=10)
            .map(|i| rec(i, &format!("u{i}"), &[], &format!("b{i}")))
            .collect();
        let w = render_window(&records, 3, &Caps::default());
        assert!(w.starts_with("[t8] user: u8"));
        assert!(w.contains("[t9] user: u9"));
        assert!(w.ends_with("[t10] user: u10\n      bot:  b10"));
        assert!(!w.contains("[t7]"));
        assert_eq!(render_window(&records, 0, &Caps::default()), "");
        assert_eq!(render_window(&[], 6, &Caps::default()), "");
        // fewer records than k: all of them
        assert_eq!(
            render_window(&records[..2], 6, &Caps::default())
                .matches("[t")
                .count(),
            2
        );
    }

    #[test]
    fn newlines_inside_fields_are_flattened() {
        let r = rec(4, "line1\nline2", &["a\nb"], "x\ny");
        let out = render_record(&r, &Caps::default());
        assert_eq!(out.lines().count(), 3);
        assert!(out.contains("user: line1 line2"));
    }

    #[test]
    fn squash_and_lexical_rank() {
        assert_eq!(squash("Memory.Reset-Requested"), "memoryresetrequested");
        assert_eq!(squash("memory_reset_requested"), "memoryresetrequested");
        let fact = |key: &str, value: &str, at: u64| crate::action::Fact {
            key: key.into(),
            value: serde_json::json!(value),
            last_validated: crate::event::Timestamp(at),
            ..Default::default()
        };
        let facts = vec![
            fact("user.name", "Martin", 1),
            fact("user.city", "Brno", 2),
            fact("user.previous_name", "Tomas", 3),
            fact("order.42.status", "shipped", 4),
        ];
        let off = Activation::off();
        let hits = lexical_rank(&facts, "what was my previous name?", 5, off);
        let keys: Vec<&str> = hits.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, vec!["user.previous_name", "user.name"]);
        assert!(
            lexical_rank(&facts, "hi", 5, off).is_empty(),
            "no 3-char tokens"
        );
        assert_eq!(lexical_rank(&facts, "is my order shipped", 1, off).len(), 1);
        assert_eq!(
            lexical_rank(&facts, "brno", 5, off)[0].key,
            "user.city",
            "values match too"
        );
    }

    /// A dozen facts with mixed hit counts, `uses` and `last_used`, ranked at
    /// `w = 0`. The expected order is written out from the pre-M9 rule alone
    /// — hits desc, then `last_validated` desc, then key — so the assertion
    /// fails if the prior leaks into a zero weight *or* if the old ordering
    /// was quietly restated to match a new one.
    #[test]
    fn activation_weight_zero_reproduces_todays_order_exactly() {
        let facts = ranking_corpus();
        let day = 86_400_000u64;
        let now = crate::event::Timestamp(100 * day);

        let expected = vec![
            // two hits ("user" and "name"), by `last_validated` desc
            "user.name",
            "user.previous_name",
            "user.pet.name",
            // one hit ("user"), by `last_validated` desc
            "user.city",
            "user.employer",
            "user.language",
            "user.timezone",
            // `last_validated` ties at 1 → the key breaks it
            "user.alias",
            "user.birthday",
        ];
        let at = |w: f32| -> Vec<String> {
            lexical_rank(
                &facts,
                "user name",
                20,
                Activation {
                    weight: w,
                    half_life_days: 7.0,
                    now,
                },
            )
            .iter()
            .map(|f| f.key.clone())
            .collect()
        };
        assert_eq!(at(0.0), expected, "w = 0 is today's order");
        // And today's order is what a default `Activation` gives, whatever
        // clock it is handed: the default weight is the off switch.
        assert_eq!(
            lexical_rank(&facts, "user name", 20, Activation::off())
                .iter()
                .map(|f| f.key.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert_ne!(at(1.0), expected, "a live weight does reorder");
    }

    #[test]
    fn a_recently_credited_fact_outranks_an_equal_hit_count_at_weight_one() {
        let day = 86_400_000u64;
        let now = crate::event::Timestamp(100 * day);
        let fact = |key: &str, credits: u32, used_days_ago: u64| crate::action::Fact {
            key: key.into(),
            value: serde_json::json!("x"),
            credits,
            // Identical `last_validated`, so the only thing that can move
            // these two is the prior.
            last_validated: crate::event::Timestamp(7),
            last_used: crate::event::Timestamp(now.0 - used_days_ago * day),
            ..Default::default()
        };
        // One token, one hit each: "user" is in both keys.
        let facts = vec![
            fact("user.stale", 9, 60),
            fact("user.credited", 9, 1),
            fact("user.cold", 0, 1),
        ];
        let keys = |w: f32| -> Vec<String> {
            lexical_rank(
                &facts,
                "user",
                5,
                Activation {
                    weight: w,
                    half_life_days: 7.0,
                    now,
                },
            )
            .iter()
            .map(|f| f.key.clone())
            .collect()
        };
        // At zero the key alone breaks the three-way tie.
        assert_eq!(keys(0.0), vec!["user.cold", "user.credited", "user.stale"]);
        // At one, recency × frequency does: nine uses a day ago beats nine
        // uses two months ago, and both beat a fact never used.
        assert_eq!(keys(1.0), vec!["user.credited", "user.stale", "user.cold"]);
    }

    /// The prior reorders; it never admits. A fact used a thousand times an
    /// hour ago is still invisible to a query that does not mention it.
    #[test]
    fn activation_never_admits_a_zero_hit_fact() {
        let day = 86_400_000u64;
        let now = crate::event::Timestamp(100 * day);
        let facts = vec![
            crate::action::Fact {
                key: "order.42.status".into(),
                value: serde_json::json!("shipped"),
                credits: 1000,
                last_used: now,
                ..Default::default()
            },
            crate::action::Fact {
                key: "user.city".into(),
                value: serde_json::json!("Brno"),
                credits: 0,
                last_used: crate::event::Timestamp(0),
                ..Default::default()
            },
        ];
        for w in [0.0f32, 1.0, 100.0] {
            let hits = lexical_rank(
                &facts,
                "brno",
                5,
                Activation {
                    weight: w,
                    half_life_days: 7.0,
                    now,
                },
            );
            let keys: Vec<&str> = hits.iter().map(|f| f.key.as_str()).collect();
            assert_eq!(keys, vec!["user.city"], "w = {w} admitted a zero-hit fact");
        }
    }

    /// Mixed hits (2, 1 and 0), mixed `uses`, mixed `last_used`, and three
    /// `last_validated` ties so the key tiebreak is exercised too.
    fn ranking_corpus() -> Vec<crate::action::Fact> {
        let day = 86_400_000u64;
        let f = |key: &str, value: &str, validated: u64, credits: u32, used_days_ago: u64| {
            crate::action::Fact {
                key: key.into(),
                value: serde_json::json!(value),
                credits,
                last_validated: crate::event::Timestamp(validated),
                last_used: crate::event::Timestamp(100 * day - used_days_ago * day),
                ..Default::default()
            }
        };
        vec![
            f("user.name", "Martin", 9, 1, 30),
            f("user.city", "Brno", 6, 12, 0),
            f("user.previous_name", "Tomas", 8, 0, 90),
            f("order.42.status", "shipped", 7, 40, 0),
            f("user.pet.name", "Fido", 7, 2, 14),
            f("user.employer", "Acme", 5, 3, 3),
            f("user.language", "Czech", 4, 0, 1),
            f("user.timezone", "Europe/Prague", 2, 7, 2),
            f("user.alias", "Maty", 1, 5, 1),
            f("user.birthday", "1990-01-01", 1, 1, 40),
            f("project.deadline", "friday", 3, 9, 0),
            f("weather.brno", "rain", 10, 30, 0),
        ]
    }

    #[test]
    fn fact_view_renders_markers() {
        let mut v: FactView = crate::action::Fact {
            key: "user.name".into(),
            value: serde_json::json!("Peter"),
            ..Default::default()
        }
        .into();
        assert_eq!(render_fact(&v), "user.name: \"Peter\"");
        v.previous = Some((
            serde_json::json!("Martin"),
            crate::event::Timestamp(1_788_370_524_628),
        ));
        assert_eq!(
            render_fact(&v),
            "user.name: \"Peter\" (was \"Martin\" until 17:35 UTC)"
        );
        v.confidence = 0.5;
        v.state = crate::action::FactState::Cold;
        assert_eq!(
            render_fact(&v),
            "user.name: \"Peter\" (unverified) (stale) (was \"Martin\" until 17:35 UTC)"
        );
    }

    #[test]
    fn summary_clamp_drops_lists_before_cutting_the_topic() {
        let mut s = SessionSummary {
            through_turn: 8,
            topic: "The user is testing memory and asking about times.".into(),
            established: vec!["a".repeat(40), "b".repeat(40)],
            open: vec!["c".repeat(40), "d".repeat(40)],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        s.clamp(200);
        assert!(render_summary(&s).chars().count() <= 200);
        assert_eq!(s.open.len(), 0, "open items go first");
        assert_eq!(s.established.len(), 2, "established survives while it fits");
        assert!(s.topic.starts_with("The user is testing"));
        s.clamp(120);
        assert!(s.established.is_empty(), "then established items");
        assert!(
            s.topic.starts_with("The user is testing"),
            "topic untouched so far"
        );
        s.clamp(50);
        assert!(
            render_summary(&s).chars().count() <= 50,
            "topic is cut last"
        );
        assert!(s.topic.ends_with('…'));
    }

    #[test]
    fn summary_renders_and_round_trips() {
        let s = SessionSummary {
            through_turn: 8,
            topic: "The user is testing memory.".into(),
            established: vec!["name is Martin".into(), "wants 24h times".into()],
            open: vec!["confirm forget_all".into()],
            trust: Trust::User,
            rebuilt_from: 1,
        };
        assert_eq!(
            render_summary(&s),
            "Conversation so far (turns 1–8): The user is testing memory.\nEstablished: name is Martin; wants 24h times\nOpen: confirm forget_all"
        );
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SessionSummary>(&json).unwrap(), s);
        // old rows without the optional fields still parse
        let minimal: SessionSummary =
            serde_json::from_str(r#"{"through_turn":2,"topic":"t","trust":"User"}"#).unwrap();
        assert_eq!(minimal.rebuilt_from, 1);
        assert!(minimal.established.is_empty());
        assert_eq!(
            render_summary(&minimal),
            "Conversation so far (turns 1–2): t"
        );
    }

    /// M9 T2.1. Two questions in one message are two things owed, and the
    /// clause is carried verbatim so the check downstream compares against
    /// the user's own words.
    #[test]
    fn two_questions_yield_two_answer_obligations() {
        assert_eq!(
            obligations_for("where is my order? and what did it cost?", 5),
            vec![
                "answer: where is my order".to_string(),
                "answer: and what did it cost".to_string()
            ]
        );
        // Czech, and an imperative beside a question.
        assert_eq!(
            obligations_for("kolik je hodin? pošli mi ten report", 5),
            vec![
                "answer: kolik je hodin".to_string(),
                "do: pošli mi ten report".to_string()
            ]
        );
    }

    /// The recorded risk is over-firing. A greeting, a thank-you and a bare
    /// question mark owe nothing, and neither does a statement.
    #[test]
    fn small_talk_yields_no_obligations() {
        for text in ["hi", "díky!", "?", "ok.", "my name is Martin", ""] {
            assert!(
                obligations_for(text, 5).is_empty(),
                "{text:?} should owe nothing"
            );
        }
    }

    #[test]
    fn obligations_are_capped_at_max() {
        let text = "a co tohle? a tohle? a tamto? a jeste tohle? a posledni? a uplne posledni?";
        assert_eq!(obligations_for(text, 3).len(), 3);
        assert_eq!(obligations_for(text, 6).len(), 6);
        assert!(obligations_for(text, 0).is_empty());
    }

    #[test]
    fn addresses_is_lexical_overlap_and_abstains_when_it_cannot_tell() {
        assert!(addresses("where is my order", "your order shipped"));
        assert!(!addresses("where is my order", "nothing to report"));
        // No content token on either side: the predicate abstains rather
        // than accusing.
        assert!(addresses("hi", "hello"));
        assert!(addresses("where is my order", "ok"));
        // M12 T1.5: an inflected Czech form of the asked word counts as
        // overlap, which is what the exact-set version missed.
        assert!(addresses("kdy je vysyp", "vysypání je ve čtvrtek"));
        // And the stem rule is off below five characters, where a
        // three-character prefix is coincidence rather than inflection:
        // "order" against "arrive" must not read as an answer.
        assert!(!addresses("where is my order", "when will it arrive"));
    }

    /// M12 T1.5. Czech inflects the words a reply is checked against, so
    /// exact equality is the wrong test for a single word: `Praze` and
    /// `Praha` are the same place. The rule stays deliberately tight - a
    /// shared stem of 3, at most 3 characters differing on either side, on
    /// words of at least 4 - because the cost of a wrong match is an
    /// invented name passed off as grounded.
    #[test]
    fn stem_match_folds_diacritics_and_accepts_czech_inflection() {
        assert_eq!(
            fold_diacritics("Příliš žluťoučký kůň"),
            "Prilis zlutoucky kun"
        );
        assert!(stem_match("Praze", "Praha"));
        assert!(stem_match("Prahy", "Praha"));
        assert!(stem_match("Brna", "Brno"));
        assert!(stem_match("Martine", "Martin"));
        assert!(stem_match("Tomáše", "Tomáš"));
        // Different words that happen to share an opening.
        assert!(!stem_match("Canberra", "Canada"));
        assert!(!stem_match("Melbourne", "Melanie"));
        // Too short to tell an inflection from a different name.
        assert!(!stem_match("Bob", "Bobby"));
    }
}
