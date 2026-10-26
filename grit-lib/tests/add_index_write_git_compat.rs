//! `grit add` and index write compatibility with system Git (v2/v3/v4, cache-tree).

use crate::support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use support::{null_device_for_tests, AUTHOR_EMAIL, AUTHOR_NAME};

/// Process variables that select a repository; grit's compat environment ignores these.
const REPO_SELECTION_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
];

fn apply_process_git_config_count(cmd: &mut Command) {
    let Ok(count_str) = std::env::var("GIT_CONFIG_COUNT") else {
        return;
    };
    cmd.env("GIT_CONFIG_COUNT", &count_str);
    let Ok(count) = count_str.parse::<usize>() else {
        return;
    };
    for i in 0..count {
        let key_var = format!("GIT_CONFIG_KEY_{i}");
        let value_var = format!("GIT_CONFIG_VALUE_{i}");
        if let (Ok(key), Ok(value)) = (std::env::var(&key_var), std::env::var(&value_var)) {
            cmd.env(key_var, key);
            cmd.env(value_var, value);
        }
    }
    if let Ok(params) = std::env::var("GIT_CONFIG_PARAMETERS") {
        cmd.env("GIT_CONFIG_PARAMETERS", params);
    }
}

/// Match [`git_compat_environment`] for system `git` subprocesses in this file.
fn configure_git_oracle_subprocess(cmd: &mut Command) {
    let null = null_device_for_tests();
    cmd.env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null);
    for key in REPO_SELECTION_ENV_VARS {
        cmd.env_remove(key);
    }
    apply_process_git_config_count(cmd);
}

/// Hermetic like [`configure_git_oracle_subprocess`], but still applies `GIT_CONFIG_COUNT` overrides.
fn git_compat_environment() -> Environment {
    let captured = Environment::capture_process();
    let mut env = Environment::empty();
    env.git_config_count = captured.git_config_count;
    env.git_config_pairs = captured.git_config_pairs;
    env.git_config_parameters = captured.git_config_parameters;
    let null = null_device_for_tests().to_owned();
    env.git_config_global = Some(null.clone());
    env.git_config_system = Some(null);
    env
}

fn git_oracle(dir: &Path, args: &[&str]) -> String {
    static COMMIT_TIME: AtomicU64 = AtomicU64::new(1_700_000_000);
    let secs = COMMIT_TIME.fetch_add(60, Ordering::SeqCst);
    let date = format!("{secs} +0000");
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date);
    configure_git_oracle_subprocess(&mut cmd);
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_ls_files_stage_oracle(dir: &Path) -> Vec<(String, u32, String, ObjectId)> {
    let out = git_oracle(dir, &["ls-files", "-s"]);
    let mut rows = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        let mode = parts.next().unwrap_or("").to_string();
        let oid: ObjectId = parts.next().unwrap_or("").parse().expect("oid");
        let stage: u32 = parts.next().unwrap_or("0").parse().expect("stage");
        let path = parts.collect::<Vec<_>>().join(" ");
        rows.push((path, stage, mode, oid));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    rows
}

fn assert_grit_index_matches_git_oracle(worktree: &Path, index: &Index) {
    let git_rows = git_ls_files_stage_oracle(worktree);
    let grit_map: std::collections::BTreeMap<String, (u32, ObjectId)> = index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                (e.mode, e.oid),
            )
        })
        .collect();
    for (path, stage, mode, oid) in &git_rows {
        assert_eq!(
            *stage, 0,
            "compare stage-0 only in this helper (path {path})"
        );
        let mode_num = u32::from_str_radix(mode.trim_start_matches('0'), 8).unwrap_or(0);
        let grit = grit_map
            .get(path)
            .unwrap_or_else(|| panic!("grit index missing path {path}"));
        assert_eq!(grit.0, mode_num, "mode mismatch on {path}");
        assert_eq!(grit.1, *oid, "oid mismatch on {path}");
    }
    assert_eq!(
        grit_map.len(),
        git_rows.iter().filter(|(_, s, _, _)| *s == 0).count()
    );
}

fn assert_git_fsck_strict_oracle(worktree: &Path) {
    let mut cmd = Command::new("git");
    cmd.current_dir(worktree).args(["fsck", "--strict"]);
    configure_git_oracle_subprocess(&mut cmd);
    let out = cmd.output().expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct OracleRepo {
    _root: tempfile::TempDir,
    worktree: PathBuf,
}

impl OracleRepo {
    fn init() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        git_oracle(&worktree, &["init", "-q"]);
        git_oracle(&worktree, &["branch", "-M", "main"]);
        Self {
            _root: root,
            worktree,
        }
    }

    fn worktree(&self) -> &Path {
        &self.worktree
    }
}

fn seed_multidir_repo_oracle(repo: &OracleRepo) {
    let wt = repo.worktree();
    for d in ["a", "b", "c/d"] {
        fs::create_dir_all(wt.join(d)).expect("mkdir");
    }
    for (dir, n) in [("a", 5), ("b", 5), ("c/d", 3)] {
        for i in 0..n {
            fs::write(
                wt.join(dir).join(format!("f{i}.txt")),
                format!("{dir}/{i}\n"),
            )
            .expect("write");
        }
    }
    git_oracle(wt, &["add", "-A"]);
    git_oracle(wt, &["commit", "-m", "seed"]);
}

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(
        Some(wt),
        &RepositoryOptions::with_environment(git_compat_environment()),
    )
    .expect("open grit repo")
}

#[test]
fn git_oracle_and_grit_ignore_hostile_git_dir_on_subprocess() {
    let hostile_root = tempfile::tempdir().expect("tempdir");
    let hostile_git = hostile_root.path().join("hostile.git");
    Command::new("git")
        .args(["init", "--bare"])
        .arg(&hostile_git)
        .status()
        .expect("bare init");

    let repo = OracleRepo::init();
    seed_multidir_repo_oracle(&repo);
    let wt = repo.worktree();

    let mut cmd = Command::new("git");
    cmd.current_dir(wt)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_DIR", &hostile_git);
    configure_git_oracle_subprocess(&mut cmd);
    let out = cmd.output().expect("git rev-parse");
    assert!(
        out.status.success(),
        "oracle git must use worktree, not hostile GIT_DIR: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert_eq!(
        top.canonicalize().expect("canonical wt"),
        wt.canonicalize().expect("canonical wt")
    );

    git_oracle(wt, &["add", "-A"]);
    open_grit(wt)
        .index_handle()
        .load()
        .expect("grit opens fixture");
    assert_git_fsck_strict_oracle(wt);
}

#[test]
fn grit_written_index_v2_v3_v4_accepted_by_git() {
    for version in [2u32, 3, 4] {
        let repo = OracleRepo::init();
        seed_multidir_repo_oracle(&repo);
        let wt = repo.worktree();

        let grit = open_grit(wt);
        let mut index = grit.index_handle().load().expect("load");
        index.version = version;
        if version == 3 {
            if let Some(e) = index.entries_mut().first_mut() {
                e.set_skip_worktree(true);
            }
        }
        grit.index_handle().write(&mut index).expect("grit write");
        assert_git_fsck_strict_oracle(wt);
        assert_grit_index_matches_git_oracle(wt, &index);
    }
}

#[test]
fn git_written_index_reload_byte_identical_when_unchanged() {
    let repo = OracleRepo::init();
    seed_multidir_repo_oracle(&repo);
    let wt = repo.worktree();
    git_oracle(wt, &["add", "-A"]);
    let bytes = fs::read(wt.join(".git/index")).expect("git index");

    let grit = open_grit(wt);
    let mut parsed = Index::parse_with_algo(&bytes, grit.odb().hash_algo()).expect("parse");
    grit.index_handle().write(&mut parsed).expect("rewrite");
    let round = fs::read(wt.join(".git/index")).expect("roundtrip index");
    assert_eq!(
        bytes, round,
        "unchanged logical index should round-trip bytes"
    );
}

#[test]
fn cache_tree_after_add_matches_git_write_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git_oracle(&repo_dir, &["init", "-q", "-b", "main", "."]);

    for d in 0..3 {
        let dir = repo_dir.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..20 {
            fs::write(dir.join(format!("f{f}.txt")), format!("{d}/{f}\n")).unwrap();
        }
    }
    git_oracle(&repo_dir, &["add", "-A"]);
    git_oracle(&repo_dir, &["commit", "-q", "-m", "initial"]);

    fs::write(repo_dir.join("d1/f5.txt"), "changed\n").unwrap();

    let grit = open_grit(&repo_dir);
    let opts = StageOptions::default();
    stage(&grit, &opts, &mut NullProgress).expect("grit add");

    let mut write_tree_cmd = Command::new("git");
    write_tree_cmd.current_dir(&repo_dir).args(["write-tree"]);
    configure_git_oracle_subprocess(&mut write_tree_cmd);
    let git_tree_hex = String::from_utf8(write_tree_cmd.output().expect("git write-tree").stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let git_tree = ObjectId::from_hex(&git_tree_hex).expect("parse git tree");

    let mut index = grit.index_handle().load().expect("load after add");
    let grit_tree = write_tree_update_index(grit.odb(), &mut index, "", WriteTreeFlags::default())
        .expect("grit write-tree from index");
    assert_eq!(grit_tree, git_tree);

    let grit_stages = git_ls_files_stage_oracle(&repo_dir);
    assert_grit_index_matches_git_oracle(&repo_dir, &index);
    assert!(!grit_stages.is_empty());
    assert_git_fsck_strict_oracle(&repo_dir);
}

#[test]
fn add_untracked_path_keeps_index_entries_canonical() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git_oracle(&repo_dir, &["init", "-q", "-b", "main", "."]);
    fs::write(repo_dir.join("zzz.txt"), "z\n").unwrap();
    git_oracle(&repo_dir, &["add", "zzz.txt"]);
    git_oracle(&repo_dir, &["commit", "-q", "-m", "initial"]);

    fs::write(repo_dir.join("aaa.txt"), "a\n").unwrap();

    let grit = open_grit(&repo_dir);
    let opts = StageOptions::default();
    stage(&grit, &opts, &mut NullProgress).expect("grit add");

    assert_git_fsck_strict_oracle(&repo_dir);
    let index = grit.index_handle().load().expect("load index");
    assert!(
        index.entries_are_sorted(),
        "index paths must stay sorted for git fsck"
    );
    assert_eq!(
        index.entries().first().map(|e| e.path.as_slice()),
        Some(b"aaa.txt" as &[u8])
    );
}
