//! Sign-off and `(cherry picked from commit …)` rewriting vs system `git commit -s` and
//! `git cherry-pick -x`.

use std::path::Path;

use grit_lib::commit::trailers::{
    append_signoff_trailer, finalize_cherry_pick_message, format_signoff_line,
};
use grit_lib::config::environment::Environment;
use grit_lib::config::ConfigSet;
use grit_test_support::git;
use tempfile::TempDir;

/// Identity `grit_test_support::git` commits with.
const NAME: &str = "Test";
const EMAIL: &str = "test@example.com";

const MESSAGES: &[&str] = &[
    "Subject\n",
    "Subject\n\nBody text.\n",
    "Subject\n\nOneWordBodyThatsNotA-S-o-B\n",
    "Subject\n\nSigned-off-by: Test <test@example.com>\n",
    "Subject\n\nSigned-off-by: Test <test@example.com>\nAcked-by: X\n",
    "Subject\n\nSigned-off-by: A <a@example.com>\n",
    "Subject\n\nAcked-by: X\n",
    "Subject\n\nBody\n\nack: someone\n",
    "Subject\n\nfree text\nmore text\nmore\nSigned-off-by: A <a@example.com>\n",
    "Subject\n\nBody\n\nReviewed-by: Y\n  continued\n",
    "Subject\n\n(cherry picked from commit 0123456789abcdef0123456789abcdef01234567)\n",
];

/// Messages compared only against `git commit --signoff`.
const SIGNOFF_ONLY_MESSAGES: &[&str] = &["Signed-off-by: Test <test@example.com>\n"];

fn init_repo(config: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    for (key, value) in config {
        git(dir.path(), &["config", key, value]);
    }
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
    git(dir.path(), &["tag", "base"]);
    dir
}

fn repo_config(repo: &Path) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(&repo.join(".git")), false).expect("load config")
}

/// Commits `message` verbatim on top of `base` and returns the new commit id.
fn commit_verbatim(repo: &Path, message: &str, extra: &[&str]) -> String {
    let msg_path = repo.join(".git/TEST_MSG");
    std::fs::write(&msg_path, message).expect("write message");
    let msg_arg = msg_path.to_string_lossy().into_owned();
    let mut args = vec!["commit", "-q", "--cleanup=verbatim", "-F", &msg_arg];
    args.extend_from_slice(extra);
    git(repo, &args);
    git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn head_message(repo: &Path) -> String {
    let raw = git(repo, &["cat-file", "commit", "HEAD"]);
    raw.split_once("\n\n")
        .map(|(_, message)| message.trim_end().to_string())
        .expect("commit message")
}

fn assert_signoff_matches_git(config: &[(&str, &str)]) {
    let dir = init_repo(config);
    let repo = dir.path();
    let cfg = repo_config(repo);
    let sob = format_signoff_line(NAME, EMAIL);
    for message in MESSAGES.iter().chain(SIGNOFF_ONLY_MESSAGES) {
        git(repo, &["reset", "-q", "--hard", "base"]);
        commit_verbatim(repo, message, &["--allow-empty", "--signoff"]);
        let mut grit = (*message).to_string();
        append_signoff_trailer(&mut grit, &sob, &cfg);
        // Trailing newlines are compared loosely: the git helper may trim its output.
        assert_eq!(grit.trim_end(), head_message(repo), "message {message:?}");
    }
}

fn assert_cherry_pick_matches_git(config: &[(&str, &str)]) {
    let dir = init_repo(config);
    let repo = dir.path();
    let cfg = repo_config(repo);
    for (i, message) in MESSAGES.iter().enumerate() {
        for signoff in [false, true] {
            git(repo, &["reset", "-q", "--hard", "base"]);
            let file = format!("f{i}.txt");
            std::fs::write(repo.join(&file), format!("{i}\n")).expect("write file");
            git(repo, &["add", &file]);
            let picked = commit_verbatim(repo, message, &[]);
            git(repo, &["reset", "-q", "--hard", "base"]);
            let mut args = vec!["cherry-pick", "-x"];
            if signoff {
                args.push("-s");
            }
            args.push(&picked);
            git(repo, &args);
            let grit =
                finalize_cherry_pick_message(message, true, signoff, NAME, EMAIL, &cfg, &picked);
            assert_eq!(
                grit.trim_end(),
                head_message(repo),
                "message {message:?}, signoff {signoff}"
            );
        }
    }
}

#[test]
fn signoff_matches_git_commit_signoff() {
    assert_signoff_matches_git(&[]);
}

#[test]
fn signoff_with_trailer_alias_matches_git_commit_signoff() {
    assert_signoff_matches_git(&[("trailer.ack.key", "Acked-by")]);
}

#[test]
fn cherry_pick_origin_line_matches_git_cherry_pick_x() {
    assert_cherry_pick_matches_git(&[]);
}

#[test]
fn cherry_pick_origin_line_with_trailer_alias_matches_git_cherry_pick_x() {
    assert_cherry_pick_matches_git(&[("trailer.ack.key", "Acked-by")]);
}
