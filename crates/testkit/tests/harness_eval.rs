//! The task set as a test (M7 plan §9, T5.1).
//!
//! The set itself is `nstestkit::eval` — library code, so that `ns-app eval`
//! (T5.2) can run the same fixtures as a release gate without a test runner.
//! This file is the other caller: `cargo test` fails the build when an
//! ability regresses, which is what keeps a regression from reaching the gate
//! in the first place.
//!
//! Nine abilities: the six memory ones from M6 §11 Phase 7, and three desktop
//! tasks over a control tree of the recorded size, which are what exercise
//! M7's own phases — the clip and its handle, the fold, and the router.

use nstestkit::eval::{render_table, run_all};

#[tokio::test]
async fn the_memory_and_desktop_abilities_pass_against_a_fixed_model() {
    let rows = run_all().await;
    let table = render_table(&rows);
    // Printed on every run, not only on a failure: `cargo test -- --nocapture`
    // is how the numbers are read off a passing harness, and comparing two
    // passing runs is the point of the set (plan §9, "harness release runs it
    // once"). On a failure the same table is the panic message.
    println!("{table}");
    assert_eq!(
        rows.len(),
        9,
        "six memory abilities and three desktop:\n{table}"
    );
    assert!(
        rows.iter().all(|r| r.passed),
        "the task set regressed:\n{table}"
    );
    // The desktop half must actually be exercising the clip, or it is six
    // memory fixtures with three expensive no-ops beside them.
    assert!(
        rows.iter().any(|r| r.clipped_chars > 0),
        "no ability clipped anything:\n{table}"
    );
}
