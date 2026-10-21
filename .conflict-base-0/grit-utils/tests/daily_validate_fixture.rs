//! Integration check: committed daily example validates.

use std::path::PathBuf;

#[test]
fn daily_example_fixture_validates() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/daily-example.json");
    grit_utils::validate_daily_report(&path).expect("daily-example.json");
}
