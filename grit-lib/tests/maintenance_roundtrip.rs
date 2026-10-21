//! Commit-graph, MIDX, and pack-refs maintenance vs system `git`.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{
    fsck, write_commit_graph, CommitGraphOptions, FsckOptions, MidxOptions, PackRefsOptions,
};
use grit_lib::objects::commit_graph::CommitGraphChain;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use support::{git_init_reftable_repo, git_refs_verify, require_reftable_git};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const FIXED_GIT_DATE: &str = "1000000000 +0000";

fn git_cmd(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", FIXED_GIT_DATE)
        .env("GIT_COMMITTER_DATE", FIXED_GIT_DATE)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", FIXED_GIT_DATE)
        .env("GIT_COMMITTER_DATE", FIXED_GIT_DATE)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn merge_history_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main"]);
    for i in 1..=3 {
        fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/1"]);
    for i in 4..=5 {
        fs::write(root.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git_cmd(root, &["add", &format!("f{i}.txt")]);
        git_cmd(root, &["commit", "-q", "-m", &format!("c{i}")]);
        git_cmd(root, &["branch", &format!("commits/{i}")]);
    }
    git_cmd(root, &["reset", "--hard", "commits/2"]);
    git_cmd(root, &["merge", "-q", "-m", "m", "commits/4"]);
    git_cmd(root, &["repack", "-ad"]);
}

fn trace2_bloom_statistics(trace_path: &Path) -> Option<u64> {
    let data = fs::read_to_string(trace_path).ok()?;
    let mut total = 0u64;
    for line in data.lines() {
        if !line.contains("\"category\":\"bloom\"") || !line.contains("statistics") {
            continue;
        }
        for key in ["maybe", "definitely_not", "filter_not_present"] {
            if let Some(rest) = line.split(&format!("\"{key}\":")).nth(1) {
                if let Some(num) = rest.split([',', '}']).next() {
                    if let Ok(v) = num.trim().parse::<u64>() {
                        total += v;
                    }
                }
            }
        }
    }
    Some(total)
}

#[test]
fn commit_graph_git_verify_clean_without_changed_paths() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    merge_history_repo(dir.path());
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(false)).expect("write graph");
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let chain = CommitGraphChain::load(repo.odb().objects_dir()).expect("load");
    assert!(chain.top_layer_bloom_settings().is_none());
}

#[test]
fn commit_graph_git_verify_clean_with_changed_paths() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    merge_history_repo(dir.path());
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let report = write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true))
        .expect("write graph");
    assert!(report.bloom_stats.is_some());
    git_cmd(dir.path(), &["commit-graph", "verify"]);
    let chain = CommitGraphChain::load(repo.odb().objects_dir()).expect("load");
    assert!(chain.top_layer_bloom_settings().is_some());
}

#[test]
fn git_log_uses_grit_bloom_filters() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    merge_history_repo(dir.path());
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(true)).expect("write graph");
    let trace = dir.path().join("trace2.json");
    let _ = fs::remove_file(&trace);
    let out = Command::new("git")
        .current_dir(dir.path())
        .env("GIT_TRACE2_PERF", &trace)
        .env("GIT_TRACE2_EVENT", &trace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["log", "--oneline", "--full-history", "--", "f5.txt"])
        .output()
        .expect("git log");
    assert!(out.status.success());
    let bloom_events = trace2_bloom_statistics(&trace).unwrap_or(0);
    assert!(
        bloom_events > 0,
        "expected git log to consult commit-graph bloom filters (trace2 statistics)"
    );
}

#[test]
fn commit_graph_write_is_atomic_and_replaces_chain() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    merge_history_repo(dir.path());
    git_cmd(
        dir.path(),
        &[
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
        ],
    );
    let graphs = dir.path().join(".git/objects/info/commit-graphs");
    assert!(graphs.is_dir(), "expected split chain from git");

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let report = repo
        .write_commit_graph(&CommitGraphOptions::with_changed_paths(true))
        .expect("grit write");
    assert!(report.removed_chain);
    assert!(!graphs.exists(), "split chain directory should be removed");
    assert!(
        report.path.ends_with("commit-graph"),
        "monolithic graph path"
    );
    assert!(!dir
        .path()
        .join(".git/objects/info/commit-graph.lock")
        .exists());

    let lock = dir.path().join(".git/objects/info/commit-graph.lock");
    fs::write(&lock, b"held").expect("lock");
    assert!(repo
        .write_commit_graph(&CommitGraphOptions::with_changed_paths(false))
        .is_err());
    fs::remove_file(&lock).expect("unlock");

    git_cmd(dir.path(), &["commit-graph", "verify"]);
}

#[test]
fn midx_git_verify_clean() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    merge_history_repo(dir.path());
    git_cmd(dir.path(), &["repack", "-d"]);
    fs::write(dir.path().join("extra.txt"), b"x\n").unwrap();
    git_cmd(dir.path(), &["add", "extra.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "extra"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let report = repo
        .write_midx(&MidxOptions::with_bitmap(false))
        .expect("write midx");
    assert!(report.path.is_file());
    assert!(report.pack_count >= 1);
    let report_fsck = fsck(&repo, &FsckOptions::midx_only(true)).expect("grit fsck midx");
    assert!(report_fsck.ok, "{:?}", report_fsck.issues);
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["multi-pack-index", "verify", "--object-dir", ".git/objects"])
        .output()
        .expect("git midx verify");
    if out.status.success() {
        return;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not recognized") || stderr.contains("unknown option") {
        eprintln!("SKIP: system git does not verify this MIDX version: {stderr}");
        return;
    }
    panic!("git multi-pack-index verify: {stderr}");
}

fn seed_branched_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    git_cmd(root, &["add", "a.txt"]);
    git_cmd(root, &["commit", "-q", "-m", "init"]);
    git_cmd(root, &["branch", "topic"]);
    git_cmd(root, &["branch", "other"]);
}

#[test]
fn pack_refs_files_backend_git_show_ref_identical() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    seed_branched_repo(grit_dir.path());
    seed_branched_repo(git_dir.path());

    let grit_repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    let report = grit_repo
        .references()
        .pack(&PackRefsOptions::with_all(true))
        .expect("pack");
    assert!(report.refs_packed >= 2);
    assert!(report.path.is_file());

    git_cmd(git_dir.path(), &["pack-refs", "--all"]);
    assert_eq!(
        git_out(grit_dir.path(), &["show-ref"]).trim(),
        git_out(git_dir.path(), &["show-ref"]).trim()
    );
}

#[test]
fn pack_refs_peeled_tags() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    git_cmd(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "init"]);
    git_cmd(dir.path(), &["tag", "-a", "v1", "-m", "annotated"]);
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    repo.references()
        .pack(&PackRefsOptions::with_all(false))
        .expect("pack tags");
    let body = fs::read_to_string(dir.path().join(".git/packed-refs")).expect("packed-refs");
    assert!(body.contains("refs/tags/v1"));
    assert!(
        body.lines().any(|l| l.starts_with('^')),
        "expected peeled tag line"
    );
    git_cmd(dir.path(), &["show-ref", "--tags"]);
}

#[test]
fn commit_graph_write_reftable_includes_reachable_commits() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    if !require_reftable_git() {
        eprintln!("SKIP: reftable-capable git unavailable");
        return;
    }
    let dir = git_init_reftable_repo("main");
    fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "one"]);

    let expected_count: usize = git_out(dir.path(), &["rev-list", "--count", "--all"])
        .trim()
        .parse()
        .expect("count");
    assert!(expected_count >= 1, "fixture must have reachable commits");

    let head_oid =
        ObjectId::from_hex(git_out(dir.path(), &["rev-parse", "HEAD"]).trim()).expect("head");
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let report = write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(false))
        .expect("write graph");
    assert_eq!(
        report.commit_count, expected_count,
        "reftable refs must drive commit-graph reachability"
    );

    let chain = CommitGraphChain::load(repo.odb().objects_dir()).expect("load");
    assert!(
        chain.find_commit(&head_oid).is_some(),
        "HEAD commit must appear in written commit-graph"
    );
    git_cmd(dir.path(), &["commit-graph", "verify"]);
}

#[test]
fn commit_graph_write_linked_worktree_includes_common_refs() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    fs::create_dir_all(&main).unwrap();
    git_cmd(&main, &["init", "-q", "-b", "main"]);
    fs::write(main.join("tracked.txt"), b"1\n").unwrap();
    git_cmd(&main, &["add", "tracked.txt"]);
    git_cmd(&main, &["commit", "-q", "-m", "seed"]);

    let linked = root.join("linked");
    git_cmd(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf8 path"),
        ],
    );

    let expected_count: usize = git_out(&linked, &["rev-list", "--count", "--all"])
        .trim()
        .parse()
        .expect("count");
    assert!(expected_count >= 1);

    let repo =
        Repository::discover(Some(&linked), &RepositoryOptions::empty()).expect("open linked");
    let report = write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(false))
        .expect("write graph");
    assert_eq!(
        report.commit_count, expected_count,
        "linked worktree must enumerate refs from the common git dir"
    );
    git_cmd(&linked, &["commit-graph", "verify"]);
}

#[test]
fn commit_graph_write_injected_ref_store_head_when_disk_unborn() {
    use std::sync::Arc;

    use grit_lib::porcelain::add::stage_for_commit;
    use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    use grit_lib::progress::NullProgress;
    use grit_lib::refs;
    use grit_lib::refs::store::{MemoryRefStore, RawRef};
    use grit_lib::repo::{InitOptions, Repository};

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = repo.work_tree().expect("worktree");
    fs::write(wt.join("a.txt"), b"a\n").unwrap();
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let ident = "T <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "one".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            hooks: Default::default(),
        },
        &mut NullProgress,
    )
    .expect("commit");

    let tip = refs::resolve_ref(repo.git_dir(), "HEAD").expect("tip");
    let git_dir = repo.git_dir();
    let branch_ref = git_dir.join("refs/heads/main");
    if branch_ref.is_file() {
        fs::remove_file(&branch_ref).expect("remove branch");
    }
    fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("unborn head");
    assert!(
        refs::resolve_ref(git_dir, "HEAD").is_err(),
        "on-disk HEAD should be unborn after removing the branch ref"
    );

    let store = Arc::new(MemoryRefStore::new());
    store
        .set_ref("HEAD", RawRef::Direct(tip))
        .expect("inject detached HEAD");
    let repo = repo.with_ref_store(store);
    assert_eq!(repo.refs().resolve("HEAD").expect("injected HEAD"), tip);

    let report = write_commit_graph(&repo, &CommitGraphOptions::with_changed_paths(false))
        .expect("write graph");
    assert_eq!(
        report.commit_count, 1,
        "reachable commits must come from the injected RefStore, not on-disk HEAD"
    );
}

#[test]
fn pack_refs_nested_annotated_tags_match_git_dereference() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    for root in [grit_dir.path(), git_dir.path()] {
        git_cmd(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), b"a\n").unwrap();
        git_cmd(root, &["add", "a.txt"]);
        git_cmd(root, &["commit", "-q", "-m", "init"]);
        git_cmd(root, &["tag", "-a", "inner", "-m", "inner"]);
        let inner_oid = git_out(root, &["rev-parse", "refs/tags/inner"])
            .trim()
            .to_owned();
        git_cmd(
            root,
            &["tag", "-a", "outer", "-m", "outer", inner_oid.as_str()],
        );
    }

    let grit_repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    grit_repo
        .references()
        .pack(&PackRefsOptions::with_all(false))
        .expect("pack");

    git_cmd(git_dir.path(), &["pack-refs"]);
    assert_eq!(
        git_out(grit_dir.path(), &["show-ref", "--dereference", "--tags"]).trim(),
        git_out(git_dir.path(), &["show-ref", "--dereference", "--tags"]).trim()
    );
}

#[test]
fn pack_refs_tree_target_annotated_tag_matches_git() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    for root in [grit_dir.path(), git_dir.path()] {
        git_cmd(root, &["init", "-q", "-b", "main"]);
        git_cmd(
            root,
            &["tag", "-a", "tree-tag", "-m", "on empty tree", EMPTY_TREE],
        );
    }

    let grit_repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    grit_repo
        .references()
        .pack(&PackRefsOptions::with_all(true))
        .expect("pack");

    git_cmd(git_dir.path(), &["pack-refs", "--all"]);

    let grit_packed =
        fs::read_to_string(grit_dir.path().join(".git/packed-refs")).expect("grit packed");
    assert!(
        grit_packed.contains("refs/tags/tree-tag"),
        "expected tag in packed-refs: {grit_packed}"
    );
    assert!(
        grit_packed.contains(&format!("^{EMPTY_TREE}")),
        "expected fully-peeled empty tree line: {grit_packed}"
    );
    assert_eq!(
        git_out(grit_dir.path(), &["show-ref", "--dereference", "--tags"]).trim(),
        git_out(git_dir.path(), &["show-ref", "--dereference", "--tags"]).trim()
    );
}

fn write_loose_ref(root: &Path, refname: &str, oid_hex: &str) {
    let rel = refname.strip_prefix("refs/").expect("refs prefix");
    let path = root.join(".git").join("refs").join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, format!("{oid_hex}\n")).expect("ref");
}

#[test]
fn pack_refs_all_leaves_per_worktree_namespaces_loose_like_git() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    for root in [grit_dir.path(), git_dir.path()] {
        git_cmd(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), b"a\n").unwrap();
        git_cmd(root, &["add", "a.txt"]);
        git_cmd(root, &["commit", "-q", "-m", "init"]);
        let head = git_out(root, &["rev-parse", "HEAD"]).trim().to_owned();
        write_loose_ref(root, "refs/rewritten/topic", &head);
        write_loose_ref(root, "refs/worktree/mark", &head);
        git_cmd(root, &["branch", "shared"]);
    }

    let grit_repo =
        Repository::discover(Some(grit_dir.path()), &RepositoryOptions::empty()).expect("open");
    grit_repo
        .references()
        .pack(&PackRefsOptions::with_all(true))
        .expect("pack");
    git_cmd(git_dir.path(), &["pack-refs", "--all"]);

    for root in [grit_dir.path(), git_dir.path()] {
        assert!(
            root.join(".git/refs/rewritten/topic").is_file(),
            "refs/rewritten must stay loose"
        );
        assert!(
            root.join(".git/refs/worktree/mark").is_file(),
            "refs/worktree must stay loose"
        );
    }
    assert_eq!(
        git_out(grit_dir.path(), &["show-ref"]).trim(),
        git_out(git_dir.path(), &["show-ref"]).trim()
    );
}

#[test]
fn reftable_compaction_git_refs_verify() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    if !require_reftable_git() {
        eprintln!("SKIP: reftable-capable git unavailable");
        return;
    }
    let dir = git_init_reftable_repo("main");
    fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "one"]);
    git_cmd(dir.path(), &["branch", "topic"]);
    git_cmd(dir.path(), &["checkout", "-q", "main"]);
    fs::write(dir.path().join("b.txt"), b"b\n").unwrap();
    git_cmd(dir.path(), &["add", "b.txt"]);
    git_cmd(dir.path(), &["commit", "-q", "-m", "two"]);

    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let rt = dir.path().join(".git/reftable");
    let tables_before = fs::read_to_string(rt.join("tables.list"))
        .expect("list")
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    let report = repo
        .references()
        .pack(&PackRefsOptions::with_all(true))
        .expect("compact");
    let tables_after = fs::read_to_string(rt.join("tables.list"))
        .expect("list")
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert!(tables_after <= tables_before);
    assert!(tables_after >= 1);
    let _ = report;

    git_refs_verify(dir.path());
}
