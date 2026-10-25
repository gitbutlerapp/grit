//! `grit merge <rev>` accepts tags, commit ids and revision expressions, not only branches.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::{git, git_cmd, git_fsck, CliFixture};
use serde_json::Value;

/// main: c1..c4 with tag `keep` at c4; current branch `side` forks from c2.
fn diverged() -> CliFixture {
    let fixture = CliFixture::empty();
    for i in 1..=4 {
        std::fs::write(fixture.path().join(format!("f{i}")), format!("{i}\n")).unwrap();
        fixture.run_ok(&["commit", "-m", &format!("c{i}")]);
    }
    git(fixture.path(), &["tag", "keep", "HEAD"]);
    git(fixture.path(), &["switch", "-q", "-c", "side", "HEAD~2"]);
    std::fs::write(fixture.path().join("s"), "s\n").unwrap();
    fixture.run_ok(&["commit", "-m", "side"]);
    fixture
}

fn rev(fixture: &CliFixture, spec: &str) -> String {
    git(fixture.path(), &["rev-parse", spec]).trim().to_owned()
}

fn assert_merged(fixture: &CliFixture, spec: &str, expected_second_parent: &str) {
    let side_tip = rev(fixture, "HEAD");
    let out = fixture.run(&["--json", "merge", spec]);
    assert_eq!(
        out.status,
        Some(0),
        "{}",
        out.dump(&format!("merge {spec}"))
    );
    let v: Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(v["result"], "merged", "{v}");
    assert_eq!(v["branch"], spec, "{v}");

    let parents = git(fixture.path(), &["log", "-1", "--format=%P"]);
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        [side_tip.as_str(), expected_second_parent],
        "merge {spec}"
    );
    assert_eq!(
        git(fixture.path(), &["log", "-1", "--format=%s"]).trim(),
        format!("Merge {spec}")
    );
    let fsck = git_fsck(fixture.path(), true);
    assert!(fsck.ok, "git fsck after merge {spec}: {:?}", fsck.msg_ids);
}

#[test]
fn merge_tag() {
    let fixture = diverged();
    let keep = rev(&fixture, "keep");
    assert_merged(&fixture, "keep", &keep);
}

#[test]
fn merge_full_commit_id() {
    let fixture = diverged();
    let keep = rev(&fixture, "keep");
    assert_merged(&fixture, &keep, &keep);
}

#[test]
fn merge_revision_expression() {
    let fixture = diverged();
    let target = rev(&fixture, "keep~1");
    assert_merged(&fixture, "keep~1", &target);
}

#[test]
fn merge_annotated_tag_records_commit_parent() {
    let fixture = diverged();
    git(
        fixture.path(),
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.com",
            "tag",
            "-a",
            "-m",
            "release",
            "v1",
            "main~1",
        ],
    );
    let commit = rev(&fixture, "v1^{commit}");
    assert_merged(&fixture, "v1", &commit);
}

#[test]
fn merge_branch_still_works() {
    let fixture = diverged();
    let main = rev(&fixture, "main");
    assert_merged(&fixture, "main", &main);
}

#[test]
fn merge_unknown_revision_has_json_kind() {
    let fixture = diverged();
    let head = rev(&fixture, "HEAD");
    let out = fixture.run(&["--json", "merge", "nonexist"]);
    assert_eq!(out.status, Some(1), "{}", out.dump("merge nonexist"));
    let v: Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(v["kind"], "revision_not_found", "{v}");
    assert!(
        v["error"].as_str().is_some_and(|e| e.contains("nonexist")),
        "{v}"
    );
    assert_eq!(rev(&fixture, "HEAD"), head);

    for range in ["main..keep", "side...main"] {
        let out = fixture.run(&["--json", "merge", range]);
        assert_eq!(out.status, Some(1), "{}", out.dump(range));
        let v: Value = serde_json::from_str(out.stdout.trim()).expect("json");
        assert_eq!(v["kind"], "revision_not_found", "{range}: {v}");
    }
    assert_eq!(rev(&fixture, "HEAD"), head);

    let human = fixture.run(&["merge", "nonexist"]);
    assert_eq!(human.status, Some(1));
    assert!(human.stderr.contains("nonexist"), "{}", human.stderr);
}

#[test]
fn merge_check_and_dry_run_accept_revisions() {
    let fixture = diverged();
    let head = rev(&fixture, "HEAD");
    let check = fixture.run(&["--json", "merge", "--check", "keep~1"]);
    assert_eq!(check.status, Some(0), "{}", check.dump("merge --check"));
    let v: Value = serde_json::from_str(&check.stdout).expect("json");
    assert_eq!(v["result"], "clean", "{v}");

    let dry = fixture.run(&["--json", "merge", "--dry-run", "keep"]);
    assert_eq!(dry.status, Some(0), "{}", dry.dump("merge --dry-run"));
    let v: Value = serde_json::from_str(&dry.stdout).expect("json");
    assert_eq!(v["result"], "merged", "{v}");
    assert_eq!(v["dry_run"], true, "{v}");
    assert_eq!(rev(&fixture, "HEAD"), head);
}

#[test]
fn merge_dotted_message_selector() {
    let fixture = diverged();
    git(fixture.path(), &["switch", "-q", "main"]);
    std::fs::write(fixture.path().join("rc"), "rc\n").unwrap();
    fixture.run_ok(&["commit", "-m", "release..candidate"]);
    std::fs::write(fixture.path().join("later"), "later\n").unwrap();
    fixture.run_ok(&["commit", "-m", "later"]);
    git(fixture.path(), &["switch", "-q", "side"]);
    let spec = "main^{/release..candidate}";
    let target = rev(&fixture, spec);
    assert_eq!(target, rev(&fixture, "main~1"));
    assert_merged(&fixture, spec, &target);
}

#[test]
fn merge_rejects_ranges_over_brace_containing_branch_like_git() {
    let fixture = diverged();
    git(fixture.path(), &["check-ref-format", "--branch", "foo{bar"]);
    git(fixture.path(), &["branch", "foo{bar", "main~1"]);
    let head = rev(&fixture, "HEAD");

    for range in ["foo{bar...main", "foo{bar..main", "main...foo{bar"] {
        let git_out = git_cmd(&["merge", "-q", "--no-edit", range])
            .in_dir(fixture.path())
            .exec();
        assert_ne!(git_out.status, Some(0), "git merge {range} should fail");
        assert_eq!(rev(&fixture, "HEAD"), head, "git merge {range}");

        let out = fixture.run(&["--json", "merge", range]);
        assert_eq!(out.status, Some(1), "{}", out.dump(range));
        let v: Value = serde_json::from_str(out.stdout.trim()).expect("json");
        assert_eq!(v["kind"], "revision_not_found", "{range}: {v}");
        assert_eq!(rev(&fixture, "HEAD"), head, "grit merge {range}");
    }

    let target = rev(&fixture, "foo{bar");
    assert_merged(&fixture, "foo{bar", &target);
}
