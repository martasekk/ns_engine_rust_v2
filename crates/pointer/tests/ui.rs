//! The compression pipeline, against the rules `A11y-Compressor`
//! (arXiv 2605.00551) measured.

use nspointer::ui::{compress, Options, UiNode, UiView};
use nspointer::Point;

fn n(role: &str, name: &str, x: i32, y: i32) -> UiNode {
    UiNode {
        role: role.into(),
        name: name.into(),
        center: Point::new(x, y),
        h: 24,
        visible: true,
        enabled: true,
        ..Default::default()
    }
}

/// A window as UI Automation actually hands it over: containers wrapping the
/// controls they hold, invisible and disabled leftovers, a long document body.
fn raw_window() -> Vec<UiNode> {
    vec![
        n("Pane", "", 500, 10),
        n("Group", "Toolbar", 100, 40),
        // container and button at the same point — UIA reports both
        n("Group", "Save button container", 100, 44),
        n("Button", "Save", 100, 44),
        n("Button", "Open", 160, 44),
        n("Edit", "", 400, 300),
        n("Text", "Document body ".repeat(30).trim_end(), 400, 340),
        // far below: a different region
        n("Button", "Cancel", 700, 900),
        n("Button", "OK", 780, 900),
        UiNode {
            visible: false,
            ..n("Button", "Hidden", 10, 10)
        },
        UiNode {
            enabled: false,
            ..n("Button", "Greyed out", 20, 20)
        },
        n("Image", "", 900, 500),
    ]
}

fn view(query: Option<&str>) -> UiView {
    compress(
        raw_window(),
        &Options {
            query: query.map(str::to_string),
            ..Default::default()
        },
    )
}

/// Phase 1. Every rule the paper lists, on one tree.
#[tokio::test]
async fn redundancy_reduction_drops_noise_and_keeps_the_actionable_twin() {
    let v = view(None);
    let names: Vec<&str> = v.nodes.iter().map(|x| x.name.as_str()).collect();

    // Invisible, disabled, and unnamed non-interactive nodes are noise.
    assert!(!names.contains(&"Hidden"));
    assert!(!names.contains(&"Greyed out"));
    assert!(!v.nodes.iter().any(|x| x.role == "image"), "{names:?}");
    assert!(!v.nodes.iter().any(|x| x.role == "pane"), "{names:?}");

    // The container and the button share a point; the button survives.
    assert!(names.contains(&"Save"), "{names:?}");
    assert!(!names.contains(&"Save button container"), "{names:?}");

    // An unnamed Edit is kept: it is actionable even without a label.
    assert!(v.nodes.iter().any(|x| x.role == "edit"));

    assert!(v.nodes.len() < v.raw_count, "compression is visible");
}

/// Long text is truncated, and the window follows the caller's query — a
/// document body is worth nothing to a caller hunting a button and everything
/// to one searching for a phrase.
#[tokio::test]
async fn long_text_is_truncated_and_windowed_around_the_query() {
    let plain = view(None);
    let body = plain
        .nodes
        .iter()
        .find(|x| x.role == "text")
        .expect("body kept");
    assert!(body.name.ends_with('…'));
    assert!(body.name.chars().count() <= 101, "{}", body.name.len());

    let long = "prelude ".repeat(20) + "the needle is here " + &"tail ".repeat(20);
    let windowed = compress(
        vec![n("Text", &long, 10, 10)],
        &Options {
            query: Some("needle".into()),
            ..Default::default()
        },
    );
    let got = &windowed.nodes[0].name;
    assert!(got.contains("needle"), "{got}");
    assert!(got.starts_with('…') && got.ends_with('…'), "{got}");
    assert!(got.chars().count() < 60, "{got}");
}

/// Phase 2: reading order, and a `[BLOCK]` where the layout jumps. The
/// threshold is adaptive rather than a per-application coordinate table.
#[tokio::test]
async fn nodes_are_in_reading_order_with_blocks_at_layout_jumps() {
    let v = view(None);
    let ys: Vec<i32> = v.nodes.iter().map(|x| x.center.y).collect();
    assert!(ys.windows(2).all(|w| w[0] <= w[1]), "{ys:?}");

    // Two jumps in this layout: toolbar -> body, and body -> the row at 900.
    assert_eq!(v.blocks.len(), 2, "{ys:?}");
    let last = *v.blocks.last().unwrap();
    assert!(v.nodes[last].center.y < 400 && v.nodes[last + 1].center.y == 900);
    assert!(v.render().contains("[BLOCK]"));
}

/// Phase 3, the scoring half. A caller that misses a modal clicks straight
/// through it and wonders why nothing happened, so they are listed apart.
#[tokio::test]
async fn modals_are_separated_and_announced() {
    let mut raw = raw_window();
    raw.push(n("Dialog", "Unsaved changes", 500, 400));
    raw.push(n("Button", "accept", 520, 460));
    let v = compress(raw, &Options::default());

    let modal_names: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();
    assert!(modal_names.contains(&"Unsaved changes"), "{modal_names:?}");
    // Interactive and 60px from the dialog's centre. Not because it is
    // spelled "accept" — see `vocabulary_cannot_manufacture_a_modal`.
    assert!(modal_names.contains(&"accept"), "{modal_names:?}");
    assert!(!v.nodes.iter().any(|x| x.name == "Unsaved changes"));
    assert!(v.render().starts_with("MODAL"));
}

/// Phase 3, the temporal half: something that appeared while the rest of the
/// screen stayed put is what just interrupted the user.
#[tokio::test]
async fn a_control_that_just_appeared_counts_as_a_modal() {
    let before = compress(raw_window(), &Options::default());
    let mut after_raw = raw_window();
    after_raw.push(n("Button", "Send anyway", 500, 400));

    let after = compress(
        after_raw,
        &Options {
            previous: Some(before.nodes.clone()),
            ..Default::default()
        },
    );
    assert!(
        after.modals.iter().any(|m| m.name == "Send anyway"),
        "{:?}",
        after.modals
    );

    // ...but a whole new screen is a new screen, not a dialog over the old
    // one. Everything appearing at once must not all be called modal.
    let fresh = compress(
        vec![
            n("Button", "A", 1, 1),
            n("Button", "B", 2, 60),
            n("Button", "C", 3, 120),
        ],
        &Options {
            previous: Some(before.nodes.clone()),
            ..Default::default()
        },
    );
    assert!(fresh.modals.is_empty(), "{:?}", fresh.modals);
}

/// The point of the whole module: a name goes in, a clickable point comes out,
/// with no screenshot anywhere in the loop.
#[tokio::test]
async fn find_ranks_exact_over_prefix_over_substring_and_yields_a_click_target() {
    let v = view(None);
    let hits = v.find("Save");
    assert_eq!(hits[0].name, "Save");
    assert_eq!(hits[0].center, Point::new(100, 44));

    let mut raw = raw_window();
    raw.push(n("Button", "Save as…", 220, 44));
    let v = compress(raw, &Options::default());
    let names: Vec<&str> = v.find("save").iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["Save", "Save as…"], "exact before prefix");

    // Role search, and a query for something absent finds nothing rather than
    // something plausible.
    assert!(!v.find("button").is_empty());
    assert!(v.find("Frobnicate").is_empty());
}

/// A query steers text windows and must never filter: a bad guess cannot be
/// allowed to hide the control the caller actually needed.
#[tokio::test]
async fn a_query_never_removes_a_control() {
    let with = view(Some("something entirely unrelated"));
    let without = view(None);
    let names = |v: &UiView| -> Vec<String> { v.nodes.iter().map(|n| n.role.clone()).collect() };
    assert_eq!(names(&with), names(&without));
}

/// The gap the Windows verification exposed by accident: that machine reports
/// its taskbar as "Hlavní panel", and the modal keyword list is English.
///
/// A dialog is found by *role*, which carries no vocabulary. Its buttons must
/// join it the same way — by being interactive and near it — or the dialog is
/// announced while the two controls that dismiss it stay in the background
/// list, which is the half a caller needs.
#[tokio::test]
async fn a_dialog_in_any_language_brings_its_buttons_with_it() {
    let czech = vec![
        n("Button", "Uložit", 100, 44),
        n("Dialog", "Neuložené změny", 500, 400),
        n("Button", "Zrušit", 460, 460),
        n("Button", "Potvrdit", 560, 460),
        // Far away: a toolbar button, not part of the dialog.
        n("Button", "Nápověda", 1400, 950),
    ];
    let v = compress(czech, &Options::default());
    let modal: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();

    assert!(modal.contains(&"Neuložené změny"), "{modal:?}");
    assert!(
        modal.contains(&"Zrušit"),
        "no English keyword anywhere: {modal:?}"
    );
    assert!(modal.contains(&"Potvrdit"), "{modal:?}");
    assert!(
        !modal.contains(&"Nápověda"),
        "900px away is not part of the dialog: {modal:?}"
    );
    assert!(
        v.nodes.iter().any(|x| x.name == "Uložit"),
        "toolbar survives"
    );

    // The same shape in English behaves identically — the fix did not trade
    // one locale for another.
    let english = vec![
        n("Dialog", "Unsaved changes", 500, 400),
        n("Button", "Discard", 460, 460),
        n("Button", "Keep", 560, 460),
    ];
    let v = compress(english, &Options::default());
    let modal: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();
    assert!(
        modal.contains(&"Discard") && modal.contains(&"Keep"),
        "{modal:?}"
    );
}

/// The other half of `a_dialog_in_any_language_brings_its_buttons_with_it`,
/// and the one that keeps the fix from being re-broken by a helpful word list.
///
/// Detection must key on role, position and time — never on what a control is
/// called. Two things are pinned here. The toolbar case guards the threshold:
/// no word list may ever score high enough to call a dialog into being on its
/// own, and its Czech twin asserts the same verdict is reached when no word is
/// recognizable at all. The just-appeared case pins the one behaviour that
/// actually changed when the keywords went: a decorative container is no
/// longer rescued into modal status by being spelled in English.
#[tokio::test]
async fn vocabulary_cannot_manufacture_a_modal() {
    // Every word here was in the old MODAL_WORDS list, and there is no
    // dialog-roled node anywhere.
    let toolbar = vec![
        n("Button", "OK", 100, 44),
        n("Button", "Cancel", 160, 44),
        n("Button", "Accept cookies", 220, 44),
        n("Group", "Consent", 280, 44),
    ];
    let v = compress(toolbar, &Options::default());
    assert!(
        v.modals.is_empty(),
        "no dialog present, so nothing is modal: {:?}",
        v.modals
    );
    // And they are still reachable, not swept behind a MODAL banner.
    assert!(v.nodes.iter().any(|x| x.name == "Accept cookies"));

    // The identical layout in Czech reaches the identical verdict, which is
    // the property the word list could not have.
    let czech = vec![
        n("Button", "Budiž", 100, 44),
        n("Button", "Zrušit", 160, 44),
        n("Button", "Přijmout cookies", 220, 44),
        n("Group", "Souhlas", 280, 44),
    ];
    let v = compress(czech, &Options::default());
    assert!(v.modals.is_empty(), "{:?}", v.modals);
    assert!(v.nodes.iter().any(|x| x.name == "Přijmout cookies"));

    // The deliberate loss, asserted rather than left to be discovered. A
    // `Group` scores -0.5 for being decorative, and the temporal signal alone
    // does not clear that. Under the old keyword list the English spelling
    // reached 0.1 and was promoted; the Czech one stayed at -0.5 and was not.
    // Neither is promoted now, which is the point: the same markup gets the
    // same answer on every desktop. A banner that wants to be found as a modal
    // should carry a modal role.
    let before = compress(raw_window(), &Options::default());
    for banner in ["Accept cookies", "Přijmout cookies"] {
        let mut after_raw = raw_window();
        after_raw.push(n("Group", banner, 500, 400));
        let after = compress(
            after_raw,
            &Options {
                previous: Some(before.nodes.clone()),
                ..Default::default()
            },
        );
        assert!(
            !after.modals.iter().any(|m| m.name == banner),
            "{banner}: decorative role is not rescued by vocabulary: {:?}",
            after.modals
        );
        assert!(
            after.nodes.iter().any(|x| x.name == banner),
            "{banner}: still reported, just not as a modal"
        );
    }
}

/// The four structural fields an agent may send, and what each buys. An
/// older agent sends none of them, and every assertion here has a twin that
/// checks the old behaviour survives their absence.
#[tokio::test]
async fn focus_is_rendered_and_findable() {
    let mut raw = raw_window();
    raw.push(UiNode {
        focused: true,
        ..n("Edit", "Search", 400, 120)
    });
    let v = compress(raw, &Options::default());
    assert_eq!(v.focused().map(|f| f.name.as_str()), Some("Search"));
    let text = v.render();
    let line = text.lines().find(|l| l.contains("\"Search\"")).unwrap();
    assert!(line.ends_with("[FOCUS]"), "{line}");
    assert_eq!(text.matches("[FOCUS]").count(), 1);

    // Nothing reported: not known, which is not "nothing focused".
    let v = view(None);
    assert!(v.focused().is_none());
    assert!(!v.render().contains("[FOCUS]"));
}

#[tokio::test]
async fn focusable_is_the_platforms_word_on_interactive_in_any_language() {
    // An unnamed custom control. By role alone it is noise; the platform
    // says it takes focus, so it is something a caller can act on.
    let mut raw = raw_window();
    raw.push(UiNode {
        focusable: true,
        ..n("Custom", "", 600, 600)
    });
    // And a focusable pane sharing a point with a named, non-interactive
    // container: the pane wins the dedup, as a button would have.
    raw.push(n("Group", "Canvas holder", 800, 500));
    raw.push(UiNode {
        focusable: true,
        ..n("Pane", "Canvas", 805, 505)
    });
    let v = compress(raw, &Options::default());
    assert!(
        v.nodes
            .iter()
            .any(|x| x.role == "custom" && x.center == Point::new(600, 600)),
        "{}",
        v.render()
    );
    let names: Vec<&str> = v.nodes.iter().map(|x| x.name.as_str()).collect();
    assert!(names.contains(&"Canvas"), "{names:?}");
    assert!(!names.contains(&"Canvas holder"), "{names:?}");

    // Without the bit, the same unnamed custom control is still noise.
    let mut raw = raw_window();
    raw.push(n("Custom", "", 600, 600));
    let v = compress(raw, &Options::default());
    assert!(!v.nodes.iter().any(|x| x.role == "custom"));
}

#[tokio::test]
async fn windows_are_read_whole_and_a_window_boundary_is_a_block() {
    // Two windows whose rows interleave by y. Read by y alone they would
    // alternate; with `window` the foreground one is read first, whole.
    let raw = vec![
        UiNode {
            window: 1,
            depth: 1,
            ..n("Button", "Behind A", 100, 100)
        },
        UiNode {
            window: 0,
            depth: 1,
            ..n("Button", "Front A", 700, 110)
        },
        UiNode {
            window: 1,
            depth: 1,
            ..n("Button", "Behind B", 100, 130)
        },
        UiNode {
            window: 0,
            depth: 1,
            ..n("Button", "Front B", 700, 140)
        },
    ];
    let v = compress(raw, &Options::default());
    let names: Vec<&str> = v.nodes.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["Front A", "Front B", "Behind A", "Behind B"]);
    // One block boundary, between the windows, though the gap is 40px.
    assert_eq!(v.blocks, vec![1], "{}", v.render());

    // The same four with no structure: plain reading order, no block.
    let raw = vec![
        n("Button", "Behind A", 100, 100),
        n("Button", "Front A", 700, 110),
        n("Button", "Behind B", 100, 130),
        n("Button", "Front B", 700, 140),
    ];
    let v = compress(raw, &Options::default());
    let names: Vec<&str> = v.nodes.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["Behind A", "Front A", "Behind B", "Front B"]);
    assert!(v.blocks.is_empty());
}

#[tokio::test]
async fn modal_attachment_uses_containment_where_the_agent_reports_it() {
    // A dialog that is its own top-level window (window 1, depth 0), wide
    // enough that its far button is outside the 300px radius; and a toolbar
    // button in the main window that is *inside* the radius.
    let raw = vec![
        UiNode {
            window: 0,
            depth: 2,
            ..n("Button", "Toolbar button", 900, 300)
        },
        UiNode {
            window: 1,
            depth: 0,
            ..n("Dialog", "Uložit změny?", 960, 500)
        },
        UiNode {
            window: 1,
            depth: 2,
            ..n("Button", "Zrušit", 1400, 700)
        },
        UiNode {
            window: 1,
            depth: 2,
            ..n("Button", "Potvrdit", 960, 700)
        },
    ];
    let v = compress(raw, &Options::default());
    let modal: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();
    assert!(
        modal.contains(&"Zrušit"),
        "far button belongs by containment: {modal:?}"
    );
    assert!(modal.contains(&"Potvrdit"), "{modal:?}");
    assert!(
        !modal.contains(&"Toolbar button"),
        "near, but another window: {modal:?}"
    );

    // Same tree with no structure: the radius is all there is, so the near
    // toolbar button joins and the far dialog button is left behind. That is
    // the old behaviour, unchanged for an old agent.
    let raw = vec![
        n("Button", "Toolbar button", 900, 300),
        n("Dialog", "Uložit změny?", 960, 500),
        n("Button", "Zrušit", 1400, 700),
        n("Button", "Potvrdit", 960, 700),
    ];
    let v = compress(raw, &Options::default());
    let modal: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();
    assert!(modal.contains(&"Toolbar button"), "{modal:?}");
    assert!(!modal.contains(&"Zrušit"), "{modal:?}");
    assert!(modal.contains(&"Potvrdit"), "{modal:?}");
}

#[tokio::test]
async fn a_nested_dialog_needs_proximity_as_well_as_depth() {
    // A dialog at depth 3 inside the main window. Depth alone is not a
    // parent link: a deep control across the window is not the dialog's.
    let raw = vec![
        UiNode {
            depth: 3,
            ..n("Dialog", "Confirm", 960, 500)
        },
        UiNode {
            depth: 4,
            ..n("Button", "OK", 960, 560)
        },
        UiNode {
            depth: 6,
            ..n("Button", "Deep elsewhere", 100, 1000)
        },
    ];
    let v = compress(raw, &Options::default());
    let modal: Vec<&str> = v.modals.iter().map(|x| x.name.as_str()).collect();
    assert!(modal.contains(&"OK"), "{modal:?}");
    assert!(!modal.contains(&"Deep elsewhere"), "{modal:?}");
}
