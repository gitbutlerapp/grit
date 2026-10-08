//! Typed operation reports (reflog expire, prune-packed, rerere) replace println status.

use std::fs;
use std::process::Command;

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::prune_packed::{prune_packed_objects, PrunePackedOptions};
use grit_lib::reflog::{
    expire_reflog_git, load_gc_reflog_expire_config, ReflogExpireActionKind, ReflogExpireParams,
};
use grit_lib::refs::{append_reflog, write_ref};
use grit_lib::repo::{init_repository, Repository};
use grit_lib::rerere::{repo_rerere, RerereAutoupdate, RerereEventKind};
use tempfile::TempDir;

fn git_fsck(repo_root: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git(repo_root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn reflog_expire_reports_actions_dry_run_and_real() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
    let git_dir = repo.git_dir.clone();

    fs::write(tmp.path().join("f.txt"), "a\n").expect("write");
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-qm", "c1"]);
    let oid1: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");
    fs::write(tmp.path().join("f.txt"), "b\n").expect("write");
    git(tmp.path(), &["commit", "-am", "c2"]);
    let oid2: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid");

    let refname = "refs/heads/main";
    let old = ObjectId::zero();
    let identity_old = "T <t@e.com> 1000000 +0000";
    let identity_new = "T <t@e.com> 2000000 +0000";
    append_reflog(&git_dir, refname, &old, &oid1, identity_old, "old", false).expect("log1");
    append_reflog(&git_dir, refname, &oid1, &oid2, identity_new, "new", false).expect("log2");
    write_ref(&git_dir, refname, &oid2).expect("tip");

    let now = 3_000_000_i64;
    let cutoff = 1_500_000_i64;
    let gc = load_gc_reflog_expire_config(
        &grit_lib::config::ConfigSet::load(
            &grit_lib::environment::Environment::capture_process(),
            Some(&git_dir),
            true,
        )
        .expect("cfg"),
        now,
    );
    let params = ReflogExpireParams {
        stale_fix: false,
        dry_run: true,
        verbose: true,
    };
    let dry = expire_reflog_git(
        &repo,
        &git_dir,
        refname,
        &params,
        Some(cutoff),
        None,
        &gc.patterns,
        gc.global_total,
        gc.global_unreachable,
        now,
    )
    .expect("dry expire");
    assert_eq!(dry.pruned, 1);
    let tagged: Vec<_> = dry
        .actions
        .iter()
        .filter(|a| a.entry.message == "old" || a.entry.message == "new")
        .collect();
    assert_eq!(tagged.len(), 2);
    assert!(tagged.iter().any(|a| {
        a.action == ReflogExpireActionKind::WouldPrune && a.entry.message == "old"
    }));
    assert!(tagged.iter().any(|a| {
        a.action == ReflogExpireActionKind::Keep && a.entry.message == "new"
    }));

    let real_params = ReflogExpireParams {
        dry_run: false,
        ..params
    };
    let real = expire_reflog_git(
        &repo,
        &git_dir,
        refname,
        &real_params,
        Some(cutoff),
        None,
        &gc.patterns,
        gc.global_total,
        gc.global_unreachable,
        now,
    )
    .expect("real expire");
    assert_eq!(real.pruned, 1);
    assert!(real
        .actions
        .iter()
        .any(|a| { a.action == ReflogExpireActionKind::Prune && a.entry.message == "old" }));

    git_fsck(tmp.path());
}

#[test]
fn prune_packed_reports_removed_paths_dry_run_and_real() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
    fs::write(tmp.path().join("x.txt"), "payload\n").expect("write");
    git(tmp.path(), &["add", "x.txt"]);
    git(tmp.path(), &["commit", "-qm", "init"]);
    git(tmp.path(), &["repack", "-a"]);

    let objects_dir = tmp.path().join(".git/objects");
    let dry = prune_packed_objects(
        &objects_dir,
        PrunePackedOptions {
            dry_run: true,
            quiet: true,
        },
    )
    .expect("dry prune");
    assert!(
        !dry.is_empty(),
        "expected would-remove paths for packed loose objects"
    );
    assert!(
        dry.iter().all(|p| p.exists()),
        "dry-run must not delete files"
    );

    let removed = prune_packed_objects(
        &objects_dir,
        PrunePackedOptions {
            dry_run: false,
            quiet: true,
        },
    )
    .expect("prune");
    assert_eq!(removed.len(), dry.len());
    git_fsck(tmp.path());
}

fn index_conflict_entry(path: &str, stage: u16, oid: ObjectId) -> IndexEntry {
    let path_b = path.as_bytes().to_vec();
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: (stage << 12) | (path_b.len().min(0x0fff) as u16),
        flags_extended: None,
        path: path_b,
        base_index_pos: 0,
    }
}

#[test]
fn rerere_records_then_reuses_resolution() {
    let tmp = TempDir::new().expect("tempdir");
    init_repository(tmp.path(), false, "main", None, "files").expect("init");
    let cfg_path = tmp.path().join(".git/config");
    let mut cfg = fs::read_to_string(&cfg_path).expect("read config");
    if !cfg.contains("[rerere]") {
        cfg.push_str("\n[rerere]\n\tenabled = true\n");
        fs::write(&cfg_path, cfg).expect("write config");
    }
    fs::create_dir_all(tmp.path().join(".git/rr-cache")).expect("rr-cache");
    let git_dir = tmp.path().join(".git");
    let repo = Repository::open(&git_dir, Some(tmp.path())).expect("reopen");

    let path = "conflict.txt";
    let base = repo.odb.write(ObjectKind::Blob, b"base\n").expect("base");
    let ours = repo.odb.write(ObjectKind::Blob, b"ours\n").expect("ours");
    let theirs = repo
        .odb
        .write(ObjectKind::Blob, b"theirs\n")
        .expect("theirs");
    let conflict_body = "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n";
    fs::write(tmp.path().join(path), conflict_body).expect("wt conflict");

    let mut index = Index::new();
    index.add_or_replace(index_conflict_entry(path, 1, base));
    index.add_or_replace(index_conflict_entry(path, 2, ours));
    index.add_or_replace(index_conflict_entry(path, 3, theirs));
    repo.write_index(&mut index).expect("write index");

    let events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere");
    assert!(
        events
            .iter()
            .any(|e| e.path == path && e.kind == RerereEventKind::RecordedPreimage),
        "expected RecordedPreimage, got {events:?}"
    );

    fs::write(tmp.path().join(path), "ours\ntheirs\n").expect("resolved");
    let events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere2");
    assert!(
        events
            .iter()
            .any(|e| { e.path == path && e.kind == RerereEventKind::RecordedResolution }),
        "expected RecordedResolution, got {events:?}"
    );

    fs::write(tmp.path().join(path), conflict_body).expect("conflict again");
    let mut index = Index::new();
    index.add_or_replace(index_conflict_entry(path, 1, base));
    index.add_or_replace(index_conflict_entry(path, 2, ours));
    index.add_or_replace(index_conflict_entry(path, 3, theirs));
    repo.write_index(&mut index).expect("write index again");

    let events = repo_rerere(&repo, RerereAutoupdate::No).expect("rerere3");
    assert!(
        events
            .iter()
            .any(|e| { e.path == path && e.kind == RerereEventKind::ResolvedFromPrevious }),
        "expected ResolvedFromPrevious, got {events:?}"
    );
}
