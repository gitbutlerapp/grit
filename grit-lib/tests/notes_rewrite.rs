//! Notes copy onto rewritten commits (`notes.rewriteRef`), checked with system `git`.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::notes::read_notes_tree;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, drop as history_drop, reword, squash, start_rebase, ApplyOptions, EmptyPolicy,
    RebaseOntoOptions, RebaseOutcome, RebaseRequest, RefScope, RewriteOptions, SquashMessage,
    SquashOptions, StartRebaseOptions,
};
use grit_lib::{HookName, PostOperationHooks};
use grit_test_support::git;

const IDENT: &str = "Notes Tester <notes@grit.test> 1700000000 +0000";

fn git_cmd(root: &std::path::Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "a@b.c"]);
    git_cmd(root, &["config", "user.name", "T"]);
    git_cmd(root, &["config", "commit.gpgsign", "false"]);
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn assert_fsck_strict(root: &std::path::Path) {
    let out = git_cmd(root, &["fsck", "--strict"]);
    assert!(
        !out.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{out}"
    );
}

fn rewrite_options() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: IDENT.parse().expect("signature"),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_options() -> ApplyOptions {
    ApplyOptions {
        identity: IDENT.parse().expect("signature"),
        reflog_message: "history".into(),
        record_operation: None,
        hooks: PostOperationHooks::Skip,
    }
}

fn note_on_object(root: &std::path::Path, object: &ObjectId) -> Option<String> {
    let out = git_cmd(root, &["notes", "show", &object.to_hex()]);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn note_list_contains(root: &std::path::Path, object: &ObjectId) -> bool {
    git_cmd(root, &["notes", "list"]).contains(&object.to_hex())
}

/// Issue #2418 reproduction: note on first feature commit survives rebase onto main.
fn setup_feat_rebase_repo(root: &std::path::Path) -> (Repository, ObjectId, ObjectId) {
    init_repo(root);
    for i in 1..=2 {
        std::fs::write(root.join(format!("b{i}")), format!("{i}\n")).expect("write");
        git_cmd(root, &["add", "."]);
        git_cmd(root, &["commit", "-qm", &format!("base {i}")]);
    }
    git_cmd(root, &["checkout", "-q", "-b", "feat"]);
    std::fs::write(root.join("f"), "f\n").expect("write");
    git_cmd(root, &["add", "f"]);
    git_cmd(root, &["commit", "-qm", "feat 1"]);
    let feat1 = git_cmd(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("feat1");
    std::fs::write(root.join("g"), "g\n").expect("write");
    git_cmd(root, &["add", "g"]);
    git_cmd(root, &["commit", "-qm", "feat 2"]);
    let feat_tip = git_cmd(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("feat tip");
    git_cmd(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("m"), "m\n").expect("write");
    git_cmd(root, &["add", "m"]);
    git_cmd(root, &["commit", "-qm", "main adv"]);
    git_cmd(root, &["checkout", "-q", "feat"]);
    git_cmd(root, &["notes", "add", "-m", "review: approved", "HEAD~1"]);
    git_cmd(root, &["config", "notes.rewriteRef", "refs/notes/commits"]);
    git_cmd(root, &["config", "notes.rewrite.rebase", "true"]);
    git_cmd(root, &["config", "notes.rewrite.amend", "true"]);
    let repo = open_grit(root);
    (repo, feat1, feat_tip)
}

fn rebase_feat_onto_main(root: &std::path::Path, repo: &Repository, feat_tip: ObjectId) {
    let main: ObjectId = git_cmd(root, &["rev-parse", "main"])
        .trim()
        .parse()
        .expect("main");
    let outcome = start_rebase(
        repo,
        &StartRebaseOptions {
            request: RebaseRequest {
                upstream: main,
                onto: None,
                branch_tip: feat_tip,
            },
            rebase: RebaseOntoOptions::new(rewrite_options()),
            apply: apply_options(),
            pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("start rebase");
    let RebaseOutcome::Done { .. } = outcome else {
        panic!("expected clean rebase, got {outcome:?}");
    };
}

#[test]
fn rebase_copies_note_to_rewritten_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feat1_old, feat_tip) = setup_feat_rebase_repo(dir.path());
    rebase_feat_onto_main(dir.path(), &repo, feat_tip);
    let feat1_new: ObjectId = git_cmd(dir.path(), &["rev-parse", "feat~1"])
        .trim()
        .parse()
        .expect("new feat1");
    assert_ne!(feat1_old, feat1_new);
    assert_eq!(
        note_on_object(dir.path(), &feat1_new).as_deref(),
        Some("review: approved"),
        "rebased commit should show the note"
    );
    assert!(
        note_list_contains(dir.path(), &feat1_new),
        "git notes list should reference the new commit"
    );
    assert!(
        note_list_contains(dir.path(), &feat1_old),
        "original commit keeps its note blob"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn reword_copies_note_to_rewritten_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feat1_old, _) = setup_feat_rebase_repo(dir.path());
    let plan = reword(
        &repo,
        feat1_old,
        "feat 1 reworded".into(),
        &rewrite_options(),
    )
    .expect("reword plan");
    apply_rewrite(
        &repo,
        &plan,
        &apply_options(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply reword");
    let feat1_new = plan.replacement(feat1_old).expect("replacement");
    assert_eq!(
        note_on_object(dir.path(), &feat1_new).as_deref(),
        Some("review: approved")
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn squash_concatenates_notes_on_survivor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feat1_old, feat_tip) = setup_feat_rebase_repo(dir.path());
    git_cmd(
        dir.path(),
        &["notes", "add", "-m", "second note", &feat_tip.to_hex()],
    );
    let plan = squash(
        &repo,
        &["main..HEAD"],
        &SquashOptions {
            message: SquashMessage::Combine,
            rewrite: rewrite_options(),
        },
    )
    .expect("squash plan");
    apply_rewrite(
        &repo,
        &plan,
        &apply_options(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply squash");
    let survivor = plan.replacement(feat1_old).expect("survivor");
    let body = note_on_object(dir.path(), &survivor).expect("note on survivor");
    assert!(body.contains("review: approved"));
    assert!(body.contains("second note"));
    assert_fsck_strict(dir.path());
}

#[test]
fn history_drop_leaves_note_on_dropped_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feat1_old, _) = setup_feat_rebase_repo(dir.path());
    let plan = history_drop(&repo, feat1_old, &rewrite_options()).expect("drop plan");
    apply_rewrite(
        &repo,
        &plan,
        &apply_options(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("apply drop");
    assert_eq!(
        note_on_object(dir.path(), &feat1_old).as_deref(),
        Some("review: approved")
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn rewrite_ref_unset_leaves_notes_ref_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _, feat_tip) = setup_feat_rebase_repo(dir.path());
    let before = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    git_cmd(dir.path(), &["config", "--unset", "notes.rewriteRef"]);
    rebase_feat_onto_main(dir.path(), &repo, feat_tip);
    let after = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    assert_eq!(before.trim(), after.trim());
}

#[test]
fn rewrite_rebase_disabled_leaves_notes_ref_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _, feat_tip) = setup_feat_rebase_repo(dir.path());
    let before = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    git_cmd(dir.path(), &["config", "notes.rewrite.rebase", "false"]);
    rebase_feat_onto_main(dir.path(), &repo, feat_tip);
    let after = git_cmd(dir.path(), &["rev-parse", "refs/notes/commits"]);
    assert_eq!(before.trim(), after.trim());
}

#[test]
fn rewrite_ref_glob_matches_notes_namespace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feat1_old, feat_tip) = setup_feat_rebase_repo(dir.path());
    git_cmd(
        dir.path(),
        &[
            "notes",
            "--ref=refs/notes/review",
            "add",
            "-m",
            "review copy",
            "HEAD~1",
        ],
    );
    git_cmd(dir.path(), &["config", "--unset", "notes.rewriteRef"]);
    git_cmd(dir.path(), &["config", "notes.rewriteRef", "refs/notes/*"]);
    rebase_feat_onto_main(dir.path(), &repo, feat_tip);
    let feat1_new: ObjectId = git_cmd(dir.path(), &["rev-parse", "feat~1"])
        .trim()
        .parse()
        .expect("new feat1");
    assert_ne!(feat1_old, feat1_new);
    assert_eq!(
        note_on_object(dir.path(), &feat1_new).as_deref(),
        Some("review: approved")
    );
    assert_eq!(
        git_cmd(
            dir.path(),
            &["notes", "--ref=review", "show", &feat1_new.to_hex()]
        )
        .trim(),
        "review copy"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn amend_copies_note_when_post_rewrite_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "1\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let old: ObjectId = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("old");
    git_cmd(dir.path(), &["notes", "add", "-m", "amend me", "HEAD"]);
    git_cmd(
        dir.path(),
        &["config", "notes.rewriteRef", "refs/notes/commits"],
    );
    let repo = open_grit(dir.path());
    std::fs::write(dir.path().join("f"), "2\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    let request = CommitRequest {
        message: "init amended".into(),
        author: Some(IDENT.parse().expect("signature")),
        committer: Some(IDENT.parse().expect("signature")),
        amend: true,
        allow_empty: true,
        post_rewrite: PostOperationHooks::Run,
        ..Default::default()
    };
    let outcome = create_commit(
        &repo,
        &request,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("amend");
    assert!(outcome.hooks_run.contains(&HookName::PostRewrite));
    assert_eq!(
        note_on_object(dir.path(), &outcome.oid).as_deref(),
        Some("amend me")
    );
    let entries = read_notes_tree(&repo, "refs/notes/commits").expect("read notes");
    assert!(entries.iter().any(|e| {
        grit_lib::notes::note_object_name(&e.path).as_deref() == Some(outcome.oid.to_hex().as_str())
    }));
    assert_fsck_strict(dir.path());
    let _ = old;
}
