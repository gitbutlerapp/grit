//! Rev-parse syntax oracle tests (upstream t1500/t1503/t1505–t1514/t6101 subsets).

use crate::support;

use std::fs;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse::{
    resolve_revision, resolve_revision_for_verify, resolve_upstream_symbolic_name,
    split_double_dot_range, split_triple_dot_range,
};
use grit_lib::revwalk::rev_parse_error::RevParseError;

use support::{
    build_commit_dag, commit_file, git, git_ls_files_stage, git_merge_expect_conflict,
    git_rev_parse_verify, oracle_abbrev, oracle_double_dot_range, oracle_expect_rev_parse_error,
    oracle_one, oracle_table, oracle_triple_dot_range, HistoryRepo, ResolveMode,
};

fn open_grit(wt: &Path) -> Repository {
    let mut env = Environment::empty();
    env.cwd = wt.to_path_buf();
    env.pwd = Some(wt.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    Repository::discover(Some(wt), &options).expect("discover grit repo")
}

fn setup_linear_main() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    repo
}

fn setup_t1506_worktree() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "one\n", "init");
    fs::create_dir_all(wt.join("subdir")).expect("mkdir");
    commit_file(wt, "subdir/file.txt", "two\n", "subdir");
    commit_file(wt, "subdir/file2.txt", "three\n", "file2");
    fs::write(wt.join("index-only.txt"), "four\n").expect("write");
    git(wt, &["add", "index-only.txt"]);
    fs::write(wt.join("disk-only.txt"), "five\n").expect("write");
    repo
}

fn setup_annotated_tags() -> (HistoryRepo, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("a-blob"), "blob\n").expect("write");
    let blob = git(wt, &["hash-object", "-w", "a-blob"]).trim().to_string();
    git(wt, &["tag", "-a", "-m", "blob", "blob-tag", &blob]);
    fs::create_dir_all(wt.join("a-tree")).expect("mkdir");
    fs::write(wt.join("a-tree/another-blob"), "more\n").expect("write");
    git(wt, &["add", "."]);
    let tree = git(wt, &["write-tree"]).trim().to_string();
    git(wt, &["tag", "-a", "-m", "tree", "tree-tag", &tree]);
    commit_file(wt, "a-blob", "blob\n", "Initial");
    let tip = git_rev_parse_verify(wt, "HEAD").expect("HEAD");
    git(wt, &["tag", "-a", "-m", "commit", "commit-tag"]);
    git(wt, &["branch", "ref"]);
    commit_file(wt, "a-blob", "modified\n", "Modified");
    git(wt, &["branch", "modref"]);
    (repo, tip)
}

fn setup_t1505_branch_stack() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "1", "1\n", "1");
    git(wt, &["branch", "side"]);
    commit_file(wt, "2", "2\n", "2");
    commit_file(wt, "3", "3\n", "3");
    git(wt, &["checkout", "side"]);
    commit_file(wt, "4", "4\n", "4");
    git(wt, &["merge", "main", "--no-edit"]);
    git(wt, &["checkout", "main"]);
    repo
}

fn setup_t1508_at_combinations() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "main-one.txt", "1\n", "main-one");
    commit_file(wt, "main-two.txt", "2\n", "main-two");
    git(wt, &["checkout", "-b", "upstream-branch"]);
    commit_file(wt, "upstream-one.txt", "u1\n", "upstream-one");
    commit_file(wt, "upstream-two.txt", "u2\n", "upstream-two");
    git(wt, &["checkout", "-b", "@@/at-test"]);
    git(wt, &["checkout", "-b", "@at-test"]);
    git(wt, &["checkout", "-b", "old-branch"]);
    commit_file(wt, "old-one.txt", "o1\n", "old-one");
    commit_file(wt, "old-two.txt", "o2\n", "old-two");
    git(wt, &["checkout", "-b", "new-branch"]);
    commit_file(wt, "new-one.txt", "n1\n", "new-one");
    commit_file(wt, "new-two.txt", "n2\n", "new-two");
    git(wt, &["branch", "-u", "main", "old-branch"]);
    git(wt, &["branch", "-u", "upstream-branch", "new-branch"]);
    repo
}

fn setup_upstream_remote() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f", "1\n", "c1");
    let bare = tempfile::tempdir().expect("bare temp");
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        wt,
        &[
            "remote",
            "add",
            "origin",
            bare.path().to_str().expect("bare path utf8"),
        ],
    );
    git(wt, &["push", "-u", "origin", "main"]);
    git(wt, &["config", "branch.main.remote", "origin"]);
    git(wt, &["config", "branch.main.merge", "refs/heads/main"]);
    repo
}

#[test]
fn t1500_core_specs_match_git_rev_parse() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    let specs = [
        "HEAD",
        "main",
        "refs/heads/main",
        "main^",
        "main~1",
        "main~2",
        "HEAD~0",
        "HEAD^0",
        "HEAD^1",
        "@",
        "@^{commit}",
        "main^{commit}",
        "main^{}",
    ];
    oracle_table(&repo, wt, ResolveMode::Verify, &specs);
}

#[test]
fn t1503_verify_rejects_missing_ref() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    assert!(git_rev_parse_verify(wt, "no-such-ref").is_none());
    assert!(resolve_revision_for_verify(&repo, "no-such-ref").is_err());
}

#[test]
fn t1505_rev_parse_last_at_minus_n_matches_git() {
    let hist = setup_t1505_branch_stack();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    for spec in ["@{-1}", "@{-1}~2", "@{-1}^2", "@{-1}@{1}", "@{-2}"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    assert!(git_rev_parse_verify(wt, "@{-3}").is_none());
    assert!(resolve_revision_for_verify(&repo, "@{-3}").is_err());
}

#[test]
fn t1506_ambiguous_path_vs_rev_typed_errors() {
    let hist = setup_t1506_worktree();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    let ok_specs = [
        "HEAD:file.txt",
        "HEAD:subdir/file.txt",
        ":index-only.txt",
        ":0:file.txt",
        "HEAD:./file.txt",
    ];
    oracle_table(&repo, wt, ResolveMode::Verify, &ok_specs);
    for spec in ["file.txt", "subdir/file.txt"] {
        assert!(
            git_rev_parse_verify(wt, spec).is_none(),
            "git --verify should reject ambiguous path {spec}"
        );
        match resolve_revision(&repo, spec) {
            Err(Error::RevParse(RevParseError::AmbiguousArgument { .. })) => {}
            other => panic!("expected AmbiguousArgument for {spec}, got {other:?}"),
        }
    }
    assert!(git_rev_parse_verify(wt, "missing:path").is_none());
    match resolve_revision_for_verify(&repo, "missing:path") {
        Err(Error::RevParse(
            RevParseError::PathNotInRevision { .. } | RevParseError::InvalidObjectName { .. },
        )) => {}
        other => panic!("expected path/rev failure for missing:path, got {other:?}"),
    }
}

#[test]
fn t1507_upstream_and_push_shorthand_match_git() {
    let hist = setup_upstream_remote();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    for spec in [
        "main@{upstream}",
        "main@{u}",
        "@{upstream}",
        "@{u}",
        "main@{push}",
        "@{push}",
    ] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    let sym = resolve_upstream_symbolic_name(&repo, "main@{u}").expect("upstream sym");
    assert!(sym.contains("refs/remotes/origin"));
}

#[test]
fn t1508_at_combinations_match_git() {
    let hist = setup_t1508_at_combinations();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    let ok_specs = [
        "HEAD",
        "@{1}",
        "HEAD@{1}",
        "@{now}",
        "HEAD@{now}",
        "@{-1}",
        "@{-1}@{0}",
        "@{-1}@{1}",
        "@{u}",
        "HEAD@{u}",
        "@{u}@{1}",
        "@{-1}@{u}",
        "@{-1}@{u}@{1}",
        "@",
        "@@{u}",
        "@@/at-test",
        "@at-test",
    ];
    oracle_table(&repo, wt, ResolveMode::Verify, &ok_specs);
    for spec in [
        "@{u}@{-1}",
        "@{0}@{0}",
        "@{1}@{u}",
        "HEAD@{-1}",
        "@{-1}@{-1}",
    ] {
        oracle_expect_rev_parse_error(&repo, wt, spec, ResolveMode::Verify, |_| true)
            .unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    git(wt, &["checkout", "old-branch"]);
    let repo = open_grit(wt);
    oracle_one(&repo, wt, "HEAD@{1}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, "@{1}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    fs::write(wt.join("normal"), "content\n").expect("write");
    fs::write(wt.join("fun@ny"), "content\n").expect("write");
    git(wt, &["add", "normal", "fun@ny"]);
    git(wt, &["commit", "-m", "funny path"]);
    let repo = open_grit(wt);
    oracle_one(&repo, wt, "@:normal", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, "@:fun@ny", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

fn setup_merge_on_head() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    git(wt, &["checkout", "main"]);
    repo
}

#[test]
fn t1511_caret_peel_forms_match_git() {
    let (hist, _tip) = setup_annotated_tags();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    let specs = [
        "ref^{}",
        "ref^{commit}",
        "commit-tag^{commit}",
        "ref^{tree}",
        "tree-tag^{tree}",
        "HEAD^{tree}",
        "HEAD^{/Initial}",
        "HEAD^-1",
    ];
    for spec in specs {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    let merge_hist = setup_merge_on_head();
    let mwt = merge_hist.worktree();
    let mrepo = open_grit(mwt);
    for spec in ["HEAD^!", "HEAD^@"] {
        oracle_one(&mrepo, mwt, spec, ResolveMode::Verify)
            .unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

fn git_hash_stdin(wt: &Path, bytes: &[u8]) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .current_dir(wt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("write stdin");
    let out = child.wait_with_output().expect("hash-object");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn setup_t1512_ambiguous_blobs() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let bare = root.path().join("blob.prefix");
    git(
        &root.path(),
        &["init", "--bare", "-q", bare.to_str().expect("utf8")],
    );
    git_hash_stdin(&bare, b"brocdnra\n");
    git_hash_stdin(&bare, b"brigddsv\n");
    git_hash_stdin(&bare, b"1agllotbh\n");
    git_hash_stdin(&bare, b"1bbfctrkc\n");
    (root, bare)
}

#[test]
fn t1512_disambiguation_matches_git() {
    let (_root, bare) = setup_t1512_ambiguous_blobs();
    let mut env = Environment::empty();
    env.cwd = bare.clone();
    env.pwd = Some(bare.to_string_lossy().into_owned());
    let options = RepositoryOptions::with_environment(env);
    let bare_repo = Repository::discover(Some(&bare), &options).expect("bare repo");
    for prefix in ["dead", "beef"] {
        assert!(
            git_rev_parse_verify(&bare, prefix).is_none(),
            "git should reject ambiguous short oid {prefix}"
        );
        assert!(resolve_revision_for_verify(&bare_repo, prefix).is_err());
    }

    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    fs::write(
        wt.join("a0blgqsjc"),
        "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n\nb1rwzyc3\n",
    )
    .expect("write");
    git(wt, &["add", "a0blgqsjc"]);
    git(wt, &["write-tree"]);
    git(wt, &["commit", "-m", "a2onsxbvj"]);
    let repo = open_grit(wt);
    for spec in [
        "0000000000cdc:a0blgqsjc",
        "000000000:a0blgqsjc",
        "0000000000e4f^{commit}",
        "000000000^{commit}",
        "000000000^0",
    ] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    let full = git_rev_parse_verify(wt, "HEAD").expect("HEAD");
    oracle_abbrev(&repo, wt, full, 7);
    git(wt, &["config", "core.disambiguate", "commit"]);
    oracle_one(&repo, wt, "000000000^{commit}", ResolveMode::AsCommit)
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn t1514_rev_parse_push_default_matches_git() {
    let hist = setup_upstream_remote();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    git(wt, &["config", "push.default", "simple"]);
    oracle_one(&repo, wt, "@{push}", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn t6101_rev_parse_parents_match_git() {
    let hist = HistoryRepo::init();
    let wt = hist.worktree();
    build_commit_dag(
        wt,
        &[("b", "a"), ("c", "b"), ("m", "b"), ("m", "c")],
        &[("m", "refs/heads/main")],
    );
    let repo = open_grit(wt);
    for n in 1..=2 {
        let spec = format!("main^^{n}");
        oracle_one(&repo, wt, &spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
    oracle_one(&repo, wt, "main^1", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, "main^2", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn colon_message_search_matches_git() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    oracle_one(&repo, wt, ":/history harness c", ResolveMode::Verify)
        .unwrap_or_else(|e| panic!("{e}"));
}

fn setup_index_conflict_stages() -> HistoryRepo {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "base\n", "base");
    git(wt, &["checkout", "-b", "side"]);
    commit_file(wt, "file.txt", "side\n", "side");
    git(wt, &["checkout", "main"]);
    commit_file(wt, "file.txt", "main\n", "main");
    git_merge_expect_conflict(wt, "side");
    repo
}

#[test]
fn colon_path_and_index_stage_match_git() {
    let hist = setup_t1506_worktree();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    oracle_one(&repo, wt, ":file.txt", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    oracle_one(&repo, wt, ":0:file.txt", ResolveMode::Verify).unwrap_or_else(|e| panic!("{e}"));
    let conflict = setup_index_conflict_stages();
    let cwt = conflict.worktree();
    let crepo = open_grit(cwt);
    let stages: std::collections::BTreeMap<u32, ObjectId> = git_ls_files_stage(cwt)
        .into_iter()
        .filter(|(path, _, _, _)| path == "file.txt")
        .map(|(_, stage, _, oid)| (stage, oid))
        .collect();
    for stage in [1_u32, 2, 3] {
        assert!(
            stages.contains_key(&stage),
            "git index should have unmerged stage {stage} for file.txt: {stages:?}"
        );
    }
    assert!(
        !stages.contains_key(&0),
        "unmerged path should not have stage 0: {stages:?}"
    );
    assert!(
        git_rev_parse_verify(cwt, ":0:file.txt").is_none(),
        "git should reject stage 0 while path is unmerged"
    );
    assert!(resolve_revision_for_verify(&crepo, ":0:file.txt").is_err());
    for spec in [":1:file.txt", ":2:file.txt", ":3:file.txt"] {
        oracle_one(&crepo, cwt, spec, ResolveMode::Verify)
            .unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

#[test]
fn rev_colon_path_treeish_blob_matches_git() {
    let hist = setup_t1506_worktree();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    oracle_one(&repo, wt, "HEAD:file.txt", ResolveMode::TreeishBlobAtPath)
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn reflog_at_n_and_date_match_git() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    for spec in ["main@{0}", "main@{1}", "HEAD@{1979-01-01}"] {
        oracle_one(&repo, wt, spec, ResolveMode::Verify).unwrap_or_else(|e| panic!("{spec}: {e}"));
    }
}

#[test]
fn split_range_helpers_match_git_endpoints() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    git(wt, &["branch", "feature", "main~1"]);
    let repo = open_grit(wt);
    oracle_double_dot_range(&repo, wt, "main..feature");
    oracle_triple_dot_range(&repo, wt, "main...feature");
    assert!(split_double_dot_range("main..feature").is_some());
    assert!(split_triple_dot_range("main...feature").is_some());
}

#[test]
fn resolve_revision_and_as_commit_agree_with_git_on_main() {
    let hist = setup_linear_main();
    let wt = hist.worktree();
    let repo = open_grit(wt);
    for spec in ["main", "HEAD~1"] {
        oracle_one(&repo, wt, spec, ResolveMode::Revision).unwrap_or_else(|e| panic!("{e}"));
        oracle_one(&repo, wt, spec, ResolveMode::AsCommit).unwrap_or_else(|e| panic!("{e}"));
    }
}
