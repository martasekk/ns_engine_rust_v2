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
    assert!(
        modal_names.contains(&"accept"),
        "role -0.5 but keyword +1.0"
    );
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
