//! CLI commits carry stable change ids checked with system git.

use grit_test_support::{git, grit, unique_tmp};

fn change_id_from_cat(cat: &str) -> Option<String> {
    cat.lines()
        .find(|l| l.starts_with("change-id "))
        .map(|l| l.strip_prefix("change-id ").unwrap().to_owned())
}

#[test]
fn cli_commit_and_amend_keep_change_id() {
    let dir = unique_tmp("commit-change-id", "cli");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "T"]);
    git(&dir, &["config", "user.email", "t@e.com"]);
    std::fs::write(dir.join("a.txt"), b"1\n").unwrap();

    assert!(grit(&dir, &["commit", "-m", "first"]).ok());

    std::fs::write(dir.join("a.txt"), b"2\n").unwrap();
    assert!(grit(&dir, &["commit", "-m", "second"]).ok());
    let tip_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let tip_id = change_id_from_cat(&tip_cat).expect("tip change-id");

    assert!(grit(&dir, &["commit", "--amend", "-m", "second amended"]).ok());

    git(&dir, &["fsck", "--strict"]);
    let amended_cat = git(&dir, &["cat-file", "-p", "HEAD"]);
    let amended_id = change_id_from_cat(&amended_cat).expect("amended change-id");
    assert_eq!(amended_id, tip_id);
}
