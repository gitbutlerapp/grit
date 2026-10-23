//! CLI parsing for `grit blame [<rev>] [--] <file>`.

use grit_test_support::cli_fixture::CliFixture;

#[test]
fn blame_positional_rev_before_separator() {
    let fx = CliFixture::empty();
    fx.write_repo_file("tracked.txt", "hello\n");
    fx.run_ok(&["add", "tracked.txt"]);
    fx.run_ok(&["commit", "-m", "add file"]);

    let out = fx.run_ok(&["--json", "blame", "HEAD~0", "--", "tracked.txt"]);
    let json: serde_json::Value = serde_json::from_str(&out.stdout).expect("json blame output");
    assert_eq!(json["file"], "tracked.txt");
    assert!(json["lines"].as_array().is_some_and(|a| !a.is_empty()));
}

#[test]
fn blame_file_only_without_rev() {
    let fx = CliFixture::empty();
    fx.write_repo_file("solo.txt", "x\n");
    fx.run_ok(&["add", "solo.txt"]);
    fx.run_ok(&["commit", "-m", "solo"]);

    let out = fx.run_ok(&["--json", "blame", "solo.txt"]);
    assert!(out.status == Some(0));
}

#[test]
fn blame_rejects_rev_flag_with_positional_rev() {
    let fx = CliFixture::empty();
    fx.write_repo_file("f.txt", "z\n");
    fx.run_ok(&["add", "f.txt"]);
    fx.run_ok(&["commit", "-m", "f"]);

    let out = fx.run(&["blame", "--rev=HEAD", "HEAD", "--", "f.txt"]);
    assert_ne!(out.status, Some(0));
    assert!(
        out.stderr.contains("cannot be used with") || out.stderr.contains("cannot use both"),
        "unexpected stderr: {}",
        out.stderr
    );
}
