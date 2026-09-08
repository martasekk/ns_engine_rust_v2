//! The memory task set as a test (M7 plan §9, T5.1).
//!
//! The set itself is `nsengine::eval` — library code, so that `ns-app eval`
//! (T5.2) can run the same six fixtures as a release gate without a test
//! runner. This file is the other caller: `cargo test` fails the build when
//! an ability regresses, which is what keeps a regression from reaching the
//! gate in the first place.

use nsengine::eval::{render_table, run_all};

#[tokio::test]
async fn the_six_memory_abilities_pass_against_a_fixed_model() {
    let rows = run_all().await;
    let table = render_table(&rows);
    // Printed on every run, not only on a failure: `cargo test -- --nocapture`
    // is how the numbers are read off a passing harness, and comparing two
    // passing runs is the point of the set (plan §9, "harness release runs it
    // once"). On a failure the same table is the panic message.
    println!("{table}");
    assert_eq!(rows.len(), 6, "the set is six abilities:\n{table}");
    assert!(
        rows.iter().all(|r| r.passed),
        "the memory task set regressed:\n{table}"
    );
}
