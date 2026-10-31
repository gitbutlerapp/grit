//! Issue #2397: pick must stop when git merge-tree conflicts on delete-tail vs append.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    predict_replay_sequence, start_replay_sequence, ReplayOptions, ReplaySequenceRequest,
    SequenceOutcome, SequencePrediction,
};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";
const BASE: &str = include_str!("fixtures/issue_2397_base.rs");
const OURS: &str = include_str!("fixtures/issue_2397_ours.rs");
const THEIRS: &str = include_str!("fixtures/issue_2397_theirs.rs");
const PATH: &str = "precise_pre_release.rs";

fn git(root: &Path, args: &[&str]) -> String {
    let home = root.join(".git-home");
    let _ = fs::create_dir_all(&home);
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "grit.changeIds", "false"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn oid_from_rev(root: &Path, rev: &str) -> ObjectId {
    let hex = git(root, &["rev-parse", rev]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("oid")
}

#[test]
fn issue_2397_pick_predict_and_apply_stop_on_content_conflict() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());

    fs::write(root.path().join(PATH), BASE).expect("base");
    git(root.path(), &["add", PATH]);
    git(root.path(), &["commit", "-qm", "base"]);

    git(root.path(), &["switch", "-c", "topic"]);
    fs::write(root.path().join(PATH), THEIRS).expect("theirs");
    git(root.path(), &["add", PATH]);
    git(root.path(), &["commit", "-qm", "pick me"]);
    let pick_oid = oid_from_rev(root.path(), "HEAD");

    git(root.path(), &["switch", "main"]);
    fs::write(root.path().join(PATH), OURS).expect("ours");
    git(root.path(), &["add", PATH]);
    git(root.path(), &["commit", "-qm", "target"]);
    let main_oid = oid_from_rev(root.path(), "HEAD");
    assert_ne!(main_oid, pick_oid);

    let repo = open_repo(root.path());
    let req = ReplaySequenceRequest {
        commits: vec![pick_oid],
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("IDENT"),
        options: ReplayOptions::default(),
    };

    let prediction = predict_replay_sequence(&repo, &req).expect("predict");
    match prediction {
        SequencePrediction::Conflicts { conflicts, .. } => {
            assert!(
                conflicts.iter().any(|c| c.path == PATH),
                "expected content conflict on {PATH}, got {conflicts:?}"
            );
        }
        other => panic!("expected conflict prediction like git merge-tree, got {other:?}"),
    }

    let outcome =
        start_replay_sequence(&repo, &req, &mut NullProgress, &NeverCancel).expect("pick");
    assert!(
        matches!(outcome, SequenceOutcome::Conflicts { .. }),
        "pick must stop with conflicts, not silently merge"
    );

    let merged = fs::read_to_string(root.path().join(PATH)).expect("worktree file");
    assert!(
        merged.contains("<<<<<<<"),
        "conflicted pick should leave markers in the working tree"
    );
    assert_ne!(
        merged, OURS,
        "must not leave the target file unchanged after a conflicting pick"
    );
}
