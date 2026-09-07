//! Reading the target's UI as text instead of as pixels.
//!
//! The capability the MCP survey (plan §8) found missing and rated above
//! screen capture: naming a control sidesteps DPI registration, image
//! transport, resolution differences and stale screenshots at once.
//!
//! **A raw accessibility tree is not usable as-is**, which is the whole
//! reason this module exists rather than a pass-through. `A11y-Compressor`
//! (arXiv 2605.00551, ACL 2026) measured the difference on OSWorld:
//! compressing the tree cut input to **22% of the linearized baseline while
//! raising task success 5.1 points**, 0.207 against 0.156. Handing a model
//! the uncompressed tree is the token-bloat failure that MCP design guidance
//! warns about, and it is *also* worse at the task.
//!
//! Their pipeline is three phases, and their ablation is the part worth
//! respecting: alone, redundancy reduction scores 0.156 — exactly baseline —
//! and modal detection and semantic structuring score 0.134 each, *below* it.
//! Only the combination helps. So all three are implemented, in the forms
//! that generalize:
//!
//! 1. **Redundancy reduction** — noise removal, spatial deduplication under
//!    20px preferring interactive roles, bounding boxes compressed to centres,
//!    attributes cut to role and name, long text truncated with a window
//!    around the caller's query.
//! 2. **Semantic structuring** — reading order, with a `[BLOCK]` separator
//!    wherever the vertical gap exceeds an adaptive threshold.
//! 3. **Modal detection** — role scoring and the temporal difference against
//!    a previous tree. No vocabulary anywhere: not in the scoring, and not in
//!    the attachment, which goes by *proximity and interactivity*. A Czech
//!    dialog's `Zrušit` joins its dialog exactly as an English `Cancel` does,
//!    and the paper's English keyword list is deliberately not implemented —
//!    see `modal_score`.
//!
//! Deliberately **not** copied: their per-application region maps (Chrome
//! `BROWSER_TABS`/`ADDRESS_BAR`, VS Code `ACTIVITY_BAR`, Calc `FORMULA_BAR`).
//! Those are coordinate thresholds fitted to the OSWorld application set, and
//! a table of magic numbers per application is not a general mechanism. The
//! adaptive gap threshold is the part of phase 3 that transfers.
//!
//! Because a phase is missing, **their numbers do not transfer to this
//! implementation** and are quoted as motivation, not as a claim about it.

use crate::geom::Point;
use serde::{Deserialize, Serialize};

/// One control, already flattened. Bounds arrive as a centre because that is
/// the only thing a caller does with them — click it — and carrying four
/// numbers per node where two will do is a third of the geometry budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiNode {
    /// Control type: "button", "edit", "menuitem", "dialog", …
    pub role: String,
    /// Accessible name — the label a person would call it by.
    pub name: String,
    /// Centre in absolute virtual-desktop pixels, ready for `pointer_click`.
    ///
    /// Not `Option<Point>`, and that is a decision rather than an oversight.
    /// The Windows end measured what it costs: of 5450 elements, 233 (4.3%)
    /// have a bounding rectangle with no extent and so no centre to report —
    /// concentrated in browser content, where zero-extent wrappers are common
    /// (154 in Firefox, 71 in one Talk window).
    ///
    /// Making this optional would not recover them. Every consumer of this
    /// field is spatial — deduplication within 20px, reading-order sort,
    /// the adaptive block gap, modal proximity, `find`'s click target, and
    /// the rendered line itself. A node with no position takes the "skip"
    /// branch at all six, so the type would grow an `Option` and the node
    /// would still not be usable; the module's contract is that a name goes
    /// in and a clickable point comes out, and something with no point cannot
    /// take part in it.
    ///
    /// Note this is the opposite call from `visible`, and the difference is
    /// the reason. Offscreen nodes are *flagged rather than dropped* because a
    /// caller can still act on one — scroll it into view, or understand why a
    /// click missed. There is no comparable move for a node that has no
    /// location at all.
    ///
    /// What would change this: evidence that those 233 are not wrappers but
    /// *named controls* whose own children are unnamed, in which case the name
    /// is real information being lost. The cheaper fix even then is on the
    /// agent side and needs no type change — a zero-extent element can inherit
    /// its nearest ancestor's rectangle, which is where a person would click
    /// for it anyway.
    pub center: Point,
    /// Height, kept only to compute the adaptive block gap. Not rendered.
    #[serde(default)]
    pub h: u32,
    /// Off-screen, invisible or disabled controls are noise.
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The control that will receive typed text. At most one per tree.
    /// Rendered as `[FOCUS]`, so a caller knows where `type` lands without a
    /// screenshot.
    #[serde(default)]
    pub focused: bool,
    /// Can take keyboard focus — the platform's own word on "interactive",
    /// in any language, including custom controls whose role is `Custom` or
    /// `Pane`. Read alongside the role list, never instead of it: an older
    /// agent sends nothing here and the default reads as it did before.
    #[serde(default)]
    pub focusable: bool,
    /// Depth below the top-level window; 0 for the window itself.
    #[serde(default)]
    pub depth: u16,
    /// Index of the top-level window in walk order; 0 is the foreground
    /// window. With `depth` this is the containment a flat list otherwise
    /// lacks: a dialog's controls are in its window, below it.
    #[serde(default)]
    pub window: u16,
}

fn yes() -> bool {
    true
}

/// What an agent that sends none of the optional fields is taken to mean:
/// visible, enabled, nothing known about focus or structure.
impl Default for UiNode {
    fn default() -> Self {
        Self {
            role: String::new(),
            name: String::new(),
            center: Point::new(0, 0),
            h: 0,
            visible: true,
            enabled: true,
            focused: false,
            focusable: false,
            depth: 0,
            window: 0,
        }
    }
}

/// Roles a caller can act on. Deduplication keeps these over containers: two
/// nodes at the same point are usually a button and the panel behind it, and
/// the button is the one anybody wants.
const INTERACTIVE: &[&str] = &[
    "button",
    "edit",
    "text",
    "combobox",
    "checkbox",
    "radiobutton",
    "menuitem",
    "tabitem",
    "listitem",
    "hyperlink",
    "link",
    "slider",
    "spinner",
    "treeitem",
    "splitbutton",
];

/// Roles that make a node a modal candidate.
const MODAL_ROLES: &[&str] = &["dialog", "alertdialog", "alert", "modal", "popup"];

fn is_interactive(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    INTERACTIVE.iter().any(|i| r.contains(i))
}

/// Whether a caller can act on this node. Two signals, either suffices: the
/// role list, which is what an older agent gives us to go on, and the
/// platform's own `focusable`, which catches what the list cannot name — a
/// `Custom` control, a `Pane` that is really a canvas — and does so in every
/// locale, since it is a bit and not a word.
fn actionable(n: &UiNode) -> bool {
    n.focusable || is_interactive(&n.role)
}

/// Whether this tree carries containment at all. An older agent sends every
/// node at depth 0 in window 0, and that is indistinguishable from one
/// window with nothing under it — so structure is trusted only where some
/// node says it is somewhere.
fn structured(nodes: &[UiNode]) -> bool {
    nodes.iter().any(|n| n.depth > 0 || n.window > 0)
}

/// How much a node looks like part of a modal. `A11y-Compressor` scores +2.0
/// for an interactive dialog tag and -0.5 for decorative ones, and adds a
/// name-based score for English decision keywords; the threshold is 1.0. The
/// keyword term is **not** implemented, and its absence is the considered
/// half of this function.
///
/// A word list scores `Cancel` and reads `Zrušit` as nothing, so the same
/// dialog is a modal in English and background noise in Czech. That is worse
/// than a signal that never fires: detection quality becomes a property of
/// the target machine's locale, and the failure is silent on exactly the
/// desktops nobody tests on. `role` is an id-mapped enum and proximity is
/// arithmetic — both mean the same thing in every language, and between them
/// they already carry the two cases the keywords were reaching for. A dialog
/// scores on its role, and its buttons join it by sitting next to it.
///
/// What is genuinely lost: a consent banner marked up as a plain `group`,
/// which has no dialog-ish role to score on. In English that used to be
/// rescued by "cookie"/"accept" when it also just appeared. It is now caught
/// only by the temporal signal, or not at all — the same way it was already
/// never caught in Czech.
fn modal_score(n: &UiNode) -> f32 {
    let role = n.role.to_ascii_lowercase();
    let mut score = 0.0;
    if MODAL_ROLES.iter().any(|m| role.contains(m)) {
        score += 2.0;
    } else if role.contains("group") || role.contains("pane") || role.contains("image") {
        score -= 0.5;
    }
    score
}

/// How near a control must sit to a detected modal to count as part of it. A
/// flat node list carries no parent links, so proximity stands in for
/// containment — a dialog's buttons are within arm's reach of its centre, a
/// toolbar's are not.
const MODAL_RADIUS: i32 = 300;

fn near(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() <= MODAL_RADIUS && (a.y - b.y).abs() <= MODAL_RADIUS
}

/// Truncation for long text. The paper keeps the first 100 characters, and a
/// 50-character window around a match when the caller has said what it is
/// looking for — a document body is worth nothing to a caller hunting for a
/// button, and worth everything to one searching for a phrase.
fn shorten(text: &str, query: Option<&str>) -> String {
    const HEAD: usize = 100;
    const WINDOW: usize = 50;
    if text.chars().count() <= HEAD {
        return text.to_string();
    }
    if let Some(q) = query {
        let hay = text.to_lowercase();
        for term in q.to_lowercase().split_whitespace().filter(|t| t.len() >= 3) {
            if let Some(byte_at) = hay.find(term) {
                let at = text[..byte_at].chars().count();
                let start = at.saturating_sub(WINDOW / 2);
                let window: String = text.chars().skip(start).take(WINDOW).collect();
                return format!("…{}…", window.trim());
            }
        }
    }
    let head: String = text.chars().take(HEAD).collect();
    format!("{head}…")
}

fn line(n: &UiNode) -> String {
    let focus = if n.focused { " [FOCUS]" } else { "" };
    format!(
        "{} \"{}\" ({},{}){focus}",
        n.role, n.name, n.center.x, n.center.y
    )
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// What the caller is looking for. Steers text windows; never filters, so
    /// a bad query cannot hide the control that was needed.
    pub query: Option<String>,
    /// A previous tree, for temporal-difference modal detection: elements
    /// that appeared without the rest of the screen changing are what just
    /// interrupted the user.
    pub previous: Option<Vec<UiNode>>,
    /// Hard cap after compression. 0 = no cap.
    pub max_nodes: usize,
}

/// The compressed observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiView {
    /// Anything that looks like it is blocking the rest of the screen. Listed
    /// separately because a caller that ignores a modal clicks straight
    /// through it and wonders why nothing happened.
    pub modals: Vec<UiNode>,
    pub nodes: Vec<UiNode>,
    /// Indices in `nodes` after which a `[BLOCK]` separator falls.
    pub blocks: Vec<usize>,
    /// Nodes in, nodes out — so the compression is visible rather than
    /// asserted.
    pub raw_count: usize,
}

/// Phase 1: noise removal, spatial deduplication, attribute and text
/// compression.
fn reduce(mut nodes: Vec<UiNode>, query: Option<&str>) -> Vec<UiNode> {
    // Noise: invisible, disabled, or unnamed and not actionable.
    nodes.retain(|n| n.visible && n.enabled && (!n.name.trim().is_empty() || actionable(n)));
    for n in nodes.iter_mut() {
        n.name = shorten(n.name.trim(), query);
        n.role = n.role.to_ascii_lowercase();
    }
    // Spatial dedup: nodes within 20px of one another are one control seen
    // twice. Keep the interactive one; on a tie keep the shorter name, which
    // is the label rather than the container's concatenation of its children.
    let mut kept: Vec<UiNode> = Vec::with_capacity(nodes.len());
    for n in nodes {
        let near = kept
            .iter_mut()
            .find(|k| (k.center.x - n.center.x).abs() < 20 && (k.center.y - n.center.y).abs() < 20);
        match near {
            Some(k) => {
                let better = match (actionable(&n), actionable(k)) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => n.name.len() < k.name.len() && !n.name.is_empty(),
                };
                if better {
                    *k = n;
                }
            }
            None => kept.push(n),
        }
    }
    kept
}

/// Compress a raw tree into something worth putting in a prompt.
pub fn compress(raw: Vec<UiNode>, opts: &Options) -> UiView {
    let raw_count = raw.len();
    let mut nodes = reduce(raw, opts.query.as_deref());

    // Phase 3: modal detection. Score first, then let anything that appeared
    // since the previous tree join them — the temporal half.
    let appeared: Vec<UiNode> = match &opts.previous {
        Some(prev) => {
            let before: Vec<(String, String)> = prev
                .iter()
                .map(|n| (n.role.to_ascii_lowercase(), n.name.clone()))
                .collect();
            nodes
                .iter()
                .filter(|n| !before.contains(&(n.role.clone(), n.name.clone())))
                .cloned()
                .collect()
        }
        None => Vec::new(),
    };
    let mut modals: Vec<UiNode> = Vec::new();
    nodes.retain(|n| {
        // The temporal signal only counts when the screen did not otherwise
        // change: if everything is new this is a different screen, not a
        // dialog over the old one.
        let just_appeared = !appeared.is_empty()
            && appeared.len() * 2 < raw_count.max(1)
            && appeared.iter().any(|a| a == n);
        if modal_score(n) >= 1.0 || (just_appeared && modal_score(n) > -0.5) {
            modals.push(n.clone());
            false
        } else {
            true
        }
    });
    // Second pass: the controls that belong to a detected modal are that
    // modal's own buttons. Without it the dialog is announced and the
    // controls that dismiss it are left in the background list, which is the
    // half a caller actually needs.
    if !modals.is_empty() {
        let has_structure = structured(&nodes) || structured(&modals);
        let anchors: Vec<&UiNode> = modals.iter().collect();
        let mut joined = Vec::new();
        nodes.retain(|n| {
            // Any *actionable* control that belongs to a detected modal, not
            // only one carrying an English decision keyword.
            //
            // The keyword version was silently locale-bound. The verification
            // machine reports its taskbar as "Hlavní panel"; a Czech dialog's
            // `Zrušit` and `Potvrdit` score nothing against a list of `cancel`
            // and `confirm`, so the dialog was announced and the two buttons
            // that dismiss it were left in the background list — the half a
            // caller actually needs. Role, focus and position carry no
            // vocabulary, so this works in every language.
            //
            // "Belongs to" is containment where the agent reports it — same
            // window, deeper than the dialog node — and a 300px radius where
            // it does not. A dialog that is its own top-level window owns
            // everything in it; one nested inside a window is told apart from
            // the rest of that window's deep tree by proximity as well, since
            // depth alone is not a parent link.
            let belongs = anchors.iter().any(|a| {
                if has_structure {
                    let inside = n.window == a.window && n.depth > a.depth;
                    inside && (a.depth == 0 || near(a.center, n.center))
                } else {
                    near(a.center, n.center)
                }
            });
            if (actionable(n) || modal_score(n) > 0.0) && belongs {
                joined.push(n.clone());
                false
            } else {
                true
            }
        });
        modals.extend(joined);
    }

    // Phase 2: reading order, then blocks at an adaptive vertical gap. The
    // threshold is the median height, floored at 40px and tripled — a gap
    // three rows tall is a change of region in any layout, without a table of
    // per-application coordinates.
    //
    // Window first, where the agent says which is which: the foreground
    // window is read whole before anything behind it, instead of two
    // windows' rows interleaving by y. A window boundary is a block boundary
    // regardless of the gap. An older agent puts everything in window 0 and
    // gets the plain y-then-x order it always had.
    nodes.sort_by_key(|n| (n.window, n.center.y, n.center.x));
    let mut heights: Vec<u32> = nodes.iter().map(|n| n.h).filter(|h| *h > 0).collect();
    heights.sort_unstable();
    let median = heights.get(heights.len() / 2).copied().unwrap_or(0);
    let gap = (median.max(40) as i32) * 3;
    let blocks: Vec<usize> = nodes
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[1].window != w[0].window || w[1].center.y - w[0].center.y > gap)
        .map(|(i, _)| i)
        .collect();

    if opts.max_nodes > 0 && nodes.len() > opts.max_nodes {
        nodes.truncate(opts.max_nodes);
    }
    UiView {
        modals,
        nodes,
        blocks,
        raw_count,
    }
}

impl UiView {
    /// One line per control, `[BLOCK]` between regions. Modals first and
    /// labelled, because a caller that misses one clicks through it. The
    /// control with keyboard focus is marked `[FOCUS]`: it is where `type`
    /// will land, and the one fact about the screen a caller most often
    /// guesses wrong.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.modals.is_empty() {
            out.push_str("MODAL (handle this before anything behind it):\n");
            for m in &self.modals {
                out.push_str(&format!("  {}\n", line(m)));
            }
        }
        for (i, n) in self.nodes.iter().enumerate() {
            out.push_str(&line(n));
            out.push('\n');
            if self.blocks.contains(&i) {
                out.push_str("[BLOCK]\n");
            }
        }
        out
    }

    /// The control that will receive typed text, if the agent reports focus
    /// and something has it. `None` from an older agent means "not known",
    /// not "nothing focused".
    pub fn focused(&self) -> Option<&UiNode> {
        self.modals
            .iter()
            .chain(self.nodes.iter())
            .find(|n| n.focused)
    }

    /// Controls matching `query`, best first: an exact name, then a prefix,
    /// then a substring. Modals are searched too — the thing you are looking
    /// for is often the thing in the way.
    pub fn find(&self, query: &str) -> Vec<&UiNode> {
        let q = query.trim().to_lowercase();
        let mut hits: Vec<(u8, &UiNode)> = self
            .modals
            .iter()
            .chain(self.nodes.iter())
            .filter_map(|n| {
                let name = n.name.to_lowercase();
                let rank = if name == q {
                    0
                } else if name.starts_with(&q) {
                    1
                } else if name.contains(&q) {
                    2
                } else if n.role.contains(&q) {
                    3
                } else {
                    return None;
                };
                Some((rank, n))
            })
            .collect();
        hits.sort_by_key(|(r, _)| *r);
        hits.into_iter().map(|(_, n)| n).collect()
    }
}
