//! Change ids on commits created through [`grit_lib::porcelain::commit::create_commit`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::change_id::{change_id, ChangeId, ChangeIdGenerator, ChangeIdPolicy};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::add::stage;
use grit_lib::porcelain::add::StageOptions;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

struct FixedGenerator([u8; 16]);

impl ChangeIdGenerator for FixedGenerator {
    fn random_bytes(&self) -> std::result::Result<[u8; 16], grit_lib::change_id::ChangeIdError> {
        Ok(self.0)
    }
}

struct SeqGenerator(Arc<AtomicUsize>);

impl ChangeIdGenerator for SeqGenerator {
    fn random_bytes(&self) -> std::result::Result<[u8; 16], grit_lib::change_id::ChangeIdError> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let mut bytes = [0u8; 16];
        bytes[0] = n as u8;
        bytes[1] = (n >> 8) as u8;
        Ok(bytes)
    }
}

fn init_repo_with_generator(
    root: &Path,
    config_extra: &str,
    gen: Arc<dyn ChangeIdGenerator>,
) -> Repository {
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        root.join(".git/config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n{config_extra}"
        ),
    )
    .unwrap();
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_work_tree(root)
            .with_change_id_generator(gen),
    )
    .unwrap()
}

fn ident() -> String {
    "Change Id Test <cid@example.com> 1700000000 +0000".to_owned()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: false,
        skip_pre_commit_tree_match: false,
        staged_change_count: None,
    }
}

fn init_repo(root: &Path, config_extra: &str) -> Repository {
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        root.join(".git/config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n{config_extra}"
        ),
    )
    .unwrap();
    let gen: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0xab; 16]));
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty()
            .with_work_tree(root)
            .with_change_id_generator(gen),
    )
    .unwrap()
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_fsck_strict(dir: &Path) {
    git_out(dir, &["fsck", "--strict"]);
}

fn change_id_line(cat: &str) -> Option<String> {
    cat.lines()
        .find(|l| l.starts_with("change-id "))
        .map(str::to_owned)
}

fn init_conflict_merge(root: &Path) -> Repository {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f"), b"base\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    fs::write(root.join("f"), b"side\n").unwrap();
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("f"), b"main\n").unwrap();
    git(root, &["commit", "-qam", "main"]);
    let merge_out = Command::new("git")
        .current_dir(root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("merge");
    assert!(!merge_out.status.success(), "merge should conflict");
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open")
}

fn ssh_keygen_available() -> bool {
    Command::new("ssh-keygen").arg("-V").output().is_ok()
}

#[test]
fn commit_writes_reverse_hex_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root, "");
    fs::write(root.join("a.txt"), b"1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    let line = change_id_line(&cat).expect("change-id header");
    let value = line.strip_prefix("change-id ").expect("prefix");
    assert!(
        ChangeId::parse_reverse_hex(value).is_ok(),
        "expected reverse-hex change id, got {value}"
    );
    assert_eq!(value, ChangeId::from_bytes(&[0xab; 16]).as_str());
}

#[test]
fn initial_commit_has_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root, "");
    fs::write(root.join("README"), b"hi\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("initial"), &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    assert!(change_id_line(&cat).is_some());
}

#[test]
fn amend_keeps_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root, "");
    fs::write(root.join("a.txt"), b"1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("first"), &mut NullProgress).unwrap();

    fs::write(root.join("a.txt"), b"2\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("second"), &mut NullProgress).unwrap();
    let before = change_id_line(&git_out(root, &["cat-file", "-p", "HEAD"])).expect("id");

    let mut req = commit_req("second amended");
    req.amend = true;
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let after = change_id_line(&git_out(root, &["cat-file", "-p", "HEAD"])).expect("id");
    assert_eq!(after, before);
}

#[test]
fn amend_of_headerless_commit_stores_derived_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "grit.changeIds", "false"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a.txt"), b"1\n").unwrap();
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "git-only"]);

    let tip = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("oid");
    let derived = ChangeId::derived_from_commit_id(&tip);

    git(root, &["config", "grit.changeIds", "true"]);
    let gen: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0xab; 16]));
    let repo = Repository::discover(
        Some(root),
        &RepositoryOptions::empty().with_change_id_generator(gen),
    )
    .expect("open");
    fs::write(root.join("a.txt"), b"2\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let mut req = commit_req("");
    req.amend = true;
    create_commit(&repo, &req, &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    let line = change_id_line(&cat).expect("change-id");
    let value = line.strip_prefix("change-id ").unwrap();
    assert_eq!(value, derived.as_str());
}

#[test]
fn merge_conclusion_commit_gets_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_conflict_merge(root);
    fs::write(root.join("f"), b"resolved\n").unwrap();
    git(root, &["add", "f"]);
    create_commit(&repo, &commit_req("merge side"), &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    assert!(change_id_line(&cat).is_some());
}

#[test]
fn change_ids_disabled_writes_no_header() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root, "[grit]\n\tchangeIds = false\n");
    fs::write(root.join("a.txt"), b"1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("no id"), &mut NullProgress).unwrap();

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    assert!(change_id_line(&cat).is_none());
}

#[test]
fn testing_content_hash_is_deterministic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_repo(root, "[grit \"testing\"]\n\tchangeId = content-hash\n");
    fs::write(root.join("a.txt"), b"1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    let outcome = create_commit(&repo, &commit_req("hash me"), &mut NullProgress).unwrap();
    let stored = change_id(
        &parse_commit(&repo.odb().read(&outcome.oid).expect("read").data).expect("parse"),
    )
    .expect("stored");

    let policy = ChangeIdPolicy::from_config(repo.config().expect("config").as_ref());
    let data = parse_commit(&repo.odb().read(&outcome.oid).expect("read").data).expect("parse");
    let expected_once = policy
        .id_for_new_commit(
            &data,
            repo.odb().hash_algo(),
            repo.change_id_generator().as_ref(),
        )
        .expect("ok")
        .expect("some");
    let expected_twice = policy
        .id_for_new_commit(
            &data,
            repo.odb().hash_algo(),
            repo.change_id_generator().as_ref(),
        )
        .expect("ok")
        .expect("some");
    assert_eq!(expected_once, expected_twice);
    assert_eq!(stored, expected_once);

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    let header = change_id_line(&cat).expect("change-id in cat-file");
    assert_eq!(header.strip_prefix("change-id ").unwrap(), stored.as_str());
}

#[test]
fn merge_commit_gets_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("shared.txt"), b"base\n").unwrap();
    git(root, &["add", "shared.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("topic.txt"), b"topic\n").unwrap();
    git(root, &["add", "topic.txt"]);
    git(root, &["commit", "-qm", "topic"]);
    git(root, &["switch", "main"]);
    fs::write(root.join("main.txt"), b"main\n").unwrap();
    git(root, &["add", "main.txt"]);
    git(root, &["commit", "-qm", "main"]);
    let main_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("main");
    let topic_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "topic"])).expect("topic");
    let gen: Arc<dyn ChangeIdGenerator> = Arc::new(FixedGenerator([0xcd; 16]));
    let repo = Repository::discover(
        Some(root),
        &RepositoryOptions::empty().with_change_id_generator(gen),
    )
    .expect("open");
    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, topic_oid, "topic").expect("merge");
    let IntegrateBranchOutcome::Merged { oid, .. } = outcome else {
        panic!("expected merged, got {outcome:?}");
    };

    git_fsck_strict(root);
    let cat = git_out(root, &["cat-file", "-p", &oid.to_hex()]);
    let line = change_id_line(&cat).expect("change-id");
    let value = line.strip_prefix("change-id ").expect("prefix");
    assert!(ChangeId::parse_reverse_hex(value).is_ok());
    assert_eq!(value, ChangeId::from_bytes(&[0xcd; 16]).as_str());
}

#[test]
fn pick_copy_gets_new_change_id_not_source() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let gen: Arc<dyn ChangeIdGenerator> = Arc::new(SeqGenerator(Arc::new(AtomicUsize::new(0))));
    let repo = init_repo_with_generator(root, "", gen);
    fs::write(root.join("base.txt"), b"base\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("base"), &mut NullProgress).unwrap();

    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("picked.txt"), b"pick me\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("topic commit"), &mut NullProgress).unwrap();
    let source_oid = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("source");
    let source_id = change_id_line(&git_out(root, &["cat-file", "-p", &source_oid.to_hex()]))
        .expect("source change-id");

    git(root, &["switch", "main"]);
    fs::write(root.join("main.txt"), b"main\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("main commit"), &mut NullProgress).unwrap();

    let outcome = replay_commit(
        &repo,
        &ReplayRequest {
            commit: source_oid,
            direction: ReplayDirection::Pick,
            committer: ident(),
            message_override: None,
        },
    )
    .expect("pick");
    let ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected committed pick, got {outcome:?}");
    };

    git_fsck_strict(root);
    let pick_id =
        change_id_line(&git_out(root, &["cat-file", "-p", &oid.to_hex()])).expect("pick change-id");
    let source_value = source_id.strip_prefix("change-id ").expect("prefix");
    let pick_value = pick_id.strip_prefix("change-id ").expect("prefix");
    assert_ne!(source_value, pick_value);
    assert!(ChangeId::parse_reverse_hex(source_value).is_ok());
    assert!(ChangeId::parse_reverse_hex(pick_value).is_ok());
}

#[test]
fn revert_gets_new_change_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let gen: Arc<dyn ChangeIdGenerator> = Arc::new(SeqGenerator(Arc::new(AtomicUsize::new(0))));
    let repo = init_repo_with_generator(root, "", gen);
    fs::write(root.join("a.txt"), b"v1\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("add a"), &mut NullProgress).unwrap();
    fs::write(root.join("a.txt"), b"v2\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("change a"), &mut NullProgress).unwrap();
    let reverted = ObjectId::from_hex(&git_out(root, &["rev-parse", "HEAD"])).expect("head");
    let before_id = change_id_line(&git_out(root, &["cat-file", "-p", &reverted.to_hex()]))
        .expect("reverted change-id");

    let outcome = replay_commit(
        &repo,
        &ReplayRequest {
            commit: reverted,
            direction: ReplayDirection::Revert,
            committer: ident(),
            message_override: None,
        },
    )
    .expect("revert");
    let ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected committed revert, got {outcome:?}");
    };

    git_fsck_strict(root);
    let revert_id = change_id_line(&git_out(root, &["cat-file", "-p", &oid.to_hex()]))
        .expect("revert change-id");
    let before_value = before_id.strip_prefix("change-id ").expect("prefix");
    let revert_value = revert_id.strip_prefix("change-id ").expect("prefix");
    assert_ne!(before_value, revert_value);
    assert!(ChangeId::parse_reverse_hex(revert_value).is_ok());
}

#[test]
fn signed_commit_with_change_id_verifies() {
    if !ssh_keygen_available() {
        eprintln!("skipping: ssh-keygen not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let key = root.join("key");
    let allowed = root.join("allowed_signers");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");

    let repo = init_repo(root, "");
    let pub_key = fs::read_to_string(key.with_extension("pub")).expect("pub");
    fs::write(&allowed, format!("t@example.com {pub_key}")).expect("allowed");
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(
        root,
        &[
            "config",
            "user.signingkey",
            &key.with_extension("pub").to_string_lossy(),
        ],
    );
    git(root, &["config", "gpg.format", "ssh"]);
    git(
        root,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            &allowed.to_string_lossy(),
        ],
    );
    git(root, &["config", "commit.gpgsign", "true"]);

    fs::write(root.join("a.txt"), b"signed\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("signed"), &mut NullProgress).unwrap();

    git_fsck_strict(root);
    assert_eq!(git_out(root, &["verify-commit", "HEAD"]), "");
    let cat = git_out(root, &["cat-file", "-p", "HEAD"]);
    assert!(change_id_line(&cat).is_some());
    let change_pos = cat.find("change-id ").expect("change-id");
    let sig_pos = cat.find("gpgsig ").expect("gpgsig");
    assert!(change_pos < sig_pos);
}

#[test]
fn rewritten_commit_records_change_id_on_reword() {
    use grit_lib::revwalk::history::{reword, EmptyPolicy, RefScope, RewriteOptions};

    let root = grit_test_support::unique_tmp("rewrite-change-id", "lib");
    let repo = init_repo(&root, "");
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("first\n"), &mut NullProgress).unwrap();
    fs::write(root.join("b.txt"), b"b\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("second\n"), &mut NullProgress).unwrap();
    let target = ObjectId::from_hex(git_out(&root, &["rev-parse", "HEAD~1"]).trim()).unwrap();

    let plan = reword(
        &repo,
        target,
        "first reworded\n".into(),
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: ident(),
            preserve_committer: false,
        },
    )
    .expect("reword plan");
    let row = plan
        .commits
        .iter()
        .find(|c| c.old == target)
        .expect("rewritten row");
    let new_oid = row.new.expect("new oid");
    let stored = row.change_id.as_ref().expect("plan change_id").as_str();
    let cat = git_out(&root, &["cat-file", "-p", &new_oid.to_hex()]);
    let header =
        change_id_line(&cat).and_then(|line| line.strip_prefix("change-id ").map(str::to_owned));
    assert_eq!(header.as_deref(), Some(stored));
}

#[test]
fn squash_fold_preserves_oldest_change_id() {
    use grit_lib::revwalk::history::{
        squash, EmptyPolicy, RefScope, RewriteOptions, SquashMessage, SquashOptions,
    };

    let root = grit_test_support::unique_tmp("squash-change-id", "lib");
    let repo = init_repo(&root, "");
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("first\n"), &mut NullProgress).unwrap();
    let oldest = ObjectId::from_hex(git_out(&root, &["rev-parse", "HEAD"]).trim()).unwrap();
    let oldest_id = change_id_line(&git_out(&root, &["cat-file", "-p", &oldest.to_hex()]))
        .and_then(|l| l.strip_prefix("change-id ").map(str::to_owned))
        .expect("oldest header");
    fs::write(root.join("b.txt"), b"b\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("second\n"), &mut NullProgress).unwrap();
    fs::write(root.join("c.txt"), b"c\n").unwrap();
    stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
    create_commit(&repo, &commit_req("third\n"), &mut NullProgress).unwrap();
    let tip = ObjectId::from_hex(git_out(&root, &["rev-parse", "HEAD"]).trim()).unwrap();
    let range = format!("{}..{}", oldest.to_hex(), tip.to_hex());

    let plan = squash(
        &repo,
        &[range.as_str()],
        &SquashOptions {
            message: SquashMessage::Custom("squashed\n".into()),
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: ident(),
                preserve_committer: false,
            },
        },
    )
    .expect("squash plan");
    let row = plan
        .commits
        .iter()
        .find(|r| r.new.is_some())
        .expect("replacement row");
    assert_eq!(
        row.change_id.as_ref().map(|id| id.as_str()),
        Some(oldest_id.as_str())
    );
}
