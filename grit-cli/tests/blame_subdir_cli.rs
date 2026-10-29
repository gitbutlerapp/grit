//! `grit blame` keeps Git cwd-relative path operands (issue #2325 CLI side).

use grit_test_support::{git_cmd, grit_cmd};

#[test]
fn blame_from_subdirectory_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_cmd(&["init", "-q", "-b", "main"]).in_dir(root).suc();
    git_cmd(&["config", "user.name", "Blame Test"])
        .in_dir(root)
        .suc();
    git_cmd(&["config", "user.email", "blame@example.com"])
        .in_dir(root)
        .suc();

    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/f.txt"), "line one\n").unwrap();
    git_cmd(&["add", "-A"]).in_dir(root).suc();
    git_cmd(&["commit", "-m", "add sub file"])
        .in_dir(root)
        .suc();
    std::fs::write(root.join("sub/f.txt"), "line two\n").unwrap();
    git_cmd(&["commit", "-am", "edit sub file"])
        .in_dir(root)
        .suc();

    let sub = root.join("sub");
    let grit_json = grit_cmd(&["--json", "blame", "f.txt"]).in_dir(&sub).suc();
    let git_out = git_cmd(&["blame", "--line-porcelain", "f.txt"])
        .in_dir(&sub)
        .suc();
    let grit: serde_json::Value = serde_json::from_str(&grit_json.stdout).expect("grit blame json");
    assert_eq!(grit["file"], "sub/f.txt");
    assert_eq!(grit["lines"].as_array().map(|a| a.len()), Some(1));
    let commit = grit["lines"][0]["commit"].as_str().expect("commit hex");
    assert!(
        git_out.stdout.contains(commit),
        "git porcelain should name the same commit\n{}",
        git_out.stdout
    );

    let ranged = grit_cmd(&["--json", "blame", "-L", "1,1", "f.txt"])
        .in_dir(&sub)
        .suc();
    let ranged_json: serde_json::Value =
        serde_json::from_str(&ranged.stdout).expect("ranged blame json");
    assert_eq!(ranged_json["lines"].as_array().map(|a| a.len()), Some(1));
}
