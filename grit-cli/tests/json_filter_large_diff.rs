//! `--json --filter` on large diff outcomes (issue #2409).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::CliFixture;
use serde_json::Value;

#[test]
fn json_filter_counts_files_on_large_diff() {
    const N: usize = 400;
    let fixture = CliFixture::empty();
    for i in 0..N {
        std::fs::write(
            fixture.path().join(format!("f{i}.txt")),
            format!("v0-{i}\n"),
        )
        .unwrap();
    }
    fixture.run_ok(&["commit", "-m", "baseline"]);
    for i in 0..N {
        std::fs::write(
            fixture.path().join(format!("f{i}.txt")),
            format!("v1-{i}\n"),
        )
        .unwrap();
    }
    fixture.run_ok(&["commit", "-m", "changed"]);

    let out = fixture.run(&[
        "--json",
        "--filter",
        ".files|length",
        "diff",
        "HEAD~1",
        "HEAD",
    ]);
    assert_eq!(out.status, Some(0), "{}", out.dump("large diff filter"));
    let v: Value = serde_json::from_str(out.stdout.trim()).expect("filtered JSON");
    assert_eq!(v, Value::Number(N.into()));
}
