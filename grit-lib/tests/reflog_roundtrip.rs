//! Reflog read/write/expire round-trips vs system `git` (t0600/t1410/t1421 subset).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use grit_lib::config::ConfigSet;
use grit_lib::environment::Environment;
use grit_lib::objects::ObjectId;
use grit_lib::reflog::{
    all_reflog_oids, all_reflog_oids_ordered, delete_reflog_entries, expire_reflog,
    expire_reflog_git, expire_reflog_unreachable, list_reflog_refs, load_gc_reflog_expire_config,
    mark_stalefix_reachable, mirror_branch_reflog_to_head, read_reflog, read_reflog_dwim,
    reflog_exists, truncate_last_reflog_line, ReflogEntry, ReflogExpireParams,
};
use grit_lib::refs::{
    append_reflog, append_reflog_with_config, effective_log_refs_config, read_log_refs_config,
    reflog_file_path, write_ref, write_symbolic_ref,
};
use grit_lib::reftable::{reftable_create_reflog, reftable_replace_reflog};
use grit_lib::repo::Repository;

use support::{
    append_repo_config, assert_reflog_tree_matches, copy_worktree, each_backend, empty_commit_oid,
    git, git_empty_commit_oid, git_fsck_strict, git_interop_available, git_ok, git_reflog_refs,
    reflog_identity, reflog_tree_bytes, write_repo_config, Backend, TestRepo, AUTHOR_EMAIL,
    AUTHOR_NAME,
};

fn each_backend_git_oracle(f: impl Fn(Backend, &TestRepo)) {
    each_backend(|backend, repo| {
        if !git_interop_available(backend) {
            eprintln!("SKIP: git lacks reftable interop");
            return;
        }
        f(backend, repo);
    });
}

const NOW: i64 = 1_700_000_000;

fn wall_clock_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_secs(),
    )
    .expect("now fits in i64")
}

fn load_repo_config(git_dir: &Path) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("load config")
}

fn open_repo(worktree: &Path) -> Repository {
    Repository::open(&worktree.join(".git"), Some(worktree)).expect("Repository::open")
}

fn write_controlled_reflog(repo: &TestRepo, refname: &str, entries: &[ReflogEntry]) {
    let git_dir = repo.git_dir();
    if repo.backend() == Backend::Reftable {
        reftable_replace_reflog(&git_dir, refname, entries).expect("replace reftable reflog");
        return;
    }
    let body = entries
        .iter()
        .map(|entry| {
            if entry.message.is_empty() {
                format!("{} {} {}\n", entry.old_oid, entry.new_oid, entry.identity)
            } else {
                format!(
                    "{} {} {}\t{}\n",
                    entry.old_oid, entry.new_oid, entry.identity, entry.message
                )
            }
        })
        .collect::<String>();
    let path = reflog_file_path(&git_dir, refname);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir reflog parent");
    }
    fs::write(path, body).expect("write controlled reflog");
}

fn seed_three_entry_reflog(repo: &TestRepo, refname: &str) -> (ObjectId, ObjectId, ObjectId) {
    let git_dir = repo.git_dir();
    let o1 = empty_commit_oid(repo);
    let o2 = empty_commit_oid(repo);
    let o3 = empty_commit_oid(repo);
    write_ref(&git_dir, refname, &o3).expect("write tip");
    let z = ObjectId::zero();
    let entries = vec![
        ReflogEntry {
            old_oid: z,
            new_oid: o1,
            identity: reflog_identity(1_000_000),
            message: "first".to_owned(),
        },
        ReflogEntry {
            old_oid: o1,
            new_oid: o2,
            identity: reflog_identity(1_500_000_000),
            message: "second".to_owned(),
        },
        ReflogEntry {
            old_oid: o2,
            new_oid: o3,
            identity: reflog_identity(1_690_000_000),
            message: "third".to_owned(),
        },
    ];
    write_controlled_reflog(repo, refname, &entries);
    (o1, o2, o3)
}

fn git_expire(worktree: &Path, args: &[&str]) {
    git(worktree, args);
}

fn grit_expire_simple(git_dir: &Path, refname: &str, cutoff: Option<i64>) {
    expire_reflog(git_dir, refname, cutoff).expect("expire_reflog");
}

fn grit_expire_git(
    worktree: &Path,
    refname: &str,
    params: ReflogExpireParams,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
) {
    let repo = open_repo(worktree);
    let git_dir = worktree.join(".git");
    let cfg = load_repo_config(&git_dir);
    let gc = load_gc_reflog_expire_config(&cfg, NOW);
    expire_reflog_git(
        &repo,
        &git_dir,
        refname,
        &params,
        explicit_total,
        explicit_unreachable,
        &gc.patterns,
        gc.global_total,
        gc.global_unreachable,
        NOW,
    )
    .expect("expire_reflog_git");
}

#[test]
fn t1410_expire_matches_git_byte_for_byte() {
    struct Case {
        name: &'static str,
        config: Option<&'static str>,
        git_args: &'static [&'static str],
        grit: fn(&Path, &str),
    }

    let refname = "refs/heads/main";
    let cases = [
        Case {
            name: "expire older than cutoff",
            config: None,
            git_args: &[
                "reflog",
                "expire",
                "--expire=1600000000",
                "--expire-unreachable=never",
                "refs/heads/main",
            ],
            grit: |wt, r| {
                grit_expire_git(
                    wt,
                    r,
                    ReflogExpireParams {
                        stale_fix: false,
                        dry_run: false,
                        verbose: false,
                    },
                    Some(1_600_000_000),
                    Some(0),
                );
            },
        },
        Case {
            name: "expire all reachable",
            config: None,
            git_args: &[
                "reflog",
                "expire",
                "--expire=all",
                "--expire-unreachable=all",
                "refs/heads/main",
            ],
            grit: |wt, r| grit_expire_simple(&wt.join(".git"), r, None),
        },
        Case {
            name: "explicit unreachable window",
            config: None,
            git_args: &[
                "reflog",
                "expire",
                "--expire=never",
                "--expire-unreachable=1600000000",
                "refs/heads/main",
            ],
            grit: |wt, r| {
                grit_expire_git(
                    wt,
                    r,
                    ReflogExpireParams {
                        stale_fix: false,
                        dry_run: false,
                        verbose: false,
                    },
                    Some(0),
                    Some(1_600_000_000),
                );
            },
        },
        Case {
            name: "gc pattern for heads",
            config: None,
            git_args: &["reflog", "expire", "refs/heads/main"],
            grit: |wt, r| {
                grit_expire_git(
                    wt,
                    r,
                    ReflogExpireParams {
                        stale_fix: false,
                        dry_run: false,
                        verbose: false,
                    },
                    None,
                    None,
                );
            },
        },
    ];

    for case in cases {
        each_backend_git_oracle(|backend, repo| {
            seed_three_entry_reflog(repo, refname);
            if let Some(fragment) = case.config {
                append_repo_config(repo.worktree(), fragment);
            }
            if case.name == "gc pattern for heads" {
                git(
                    repo.worktree(),
                    &["config", "gc.refs/heads/*.reflogExpire", "1600000000"],
                );
            }

            let grit_dir = tempfile::tempdir().expect("grit copy dir");
            let git_dir = tempfile::tempdir().expect("git copy dir");
            copy_worktree(repo.worktree(), grit_dir.path());
            copy_worktree(repo.worktree(), git_dir.path());

            (case.grit)(grit_dir.path(), refname);
            git_expire(git_dir.path(), case.git_args);

            if backend == Backend::Files {
                assert_reflog_tree_matches(
                    &git_dir.path().join(".git"),
                    &grit_dir.path().join(".git"),
                );
            }
            assert!(
                git_fsck_strict(grit_dir.path()),
                "grit side fsck {}",
                case.name
            );
            assert!(
                git_fsck_strict(git_dir.path()),
                "git side fsck {}",
                case.name
            );
        });
    }
}

/// Git `parse_expiry_date` / `match_digit`: bare values below `100_000_000` are not literal epochs.
#[test]
fn gc_reflog_expire_numeric_90000_and_100m_boundary_match_git() {
    each_backend_git_oracle(|backend, repo| {
        let refname = "refs/heads/main";
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, refname, &oid).expect("tip");
        write_controlled_reflog(
            repo,
            refname,
            &[ReflogEntry {
                old_oid: ObjectId::zero(),
                new_oid: oid,
                identity: reflog_identity(1_577_836_800),
                message: "2020".to_owned(),
            }],
        );

        for (expire_val, label) in [(90_000_i64, "90000"), (100_000_000_i64, "100000000")] {
            let grit_copy = tempfile::tempdir().expect("grit copy");
            let git_copy = tempfile::tempdir().expect("git copy");
            copy_worktree(repo.worktree(), grit_copy.path());
            copy_worktree(repo.worktree(), git_copy.path());

            append_repo_config(
                grit_copy.path(),
                &format!("[gc]\n\treflogExpire = {expire_val}\n"),
            );
            if expire_val == 90_000 {
                let cfg = load_repo_config(&grit_copy.path().join(".git"));
                let gc = load_gc_reflog_expire_config(&cfg, NOW);
                assert_ne!(
                    gc.global_total,
                    Some(90_000),
                    "90000 must not parse as a literal Unix epoch"
                );
            }
            append_repo_config(
                git_copy.path(),
                &format!("[gc]\n\treflogExpire = {expire_val}\n"),
            );

            grit_expire_git(
                grit_copy.path(),
                refname,
                ReflogExpireParams {
                    stale_fix: false,
                    dry_run: false,
                    verbose: false,
                },
                None,
                None,
            );
            git_expire(
                git_copy.path(),
                &[
                    "reflog",
                    "expire",
                    &format!("--expire={expire_val}"),
                    "--expire-unreachable=never",
                    refname,
                ],
            );

            let git_log = reflog_tree_bytes(&git_copy.path().join(".git"));
            let grit_log = reflog_tree_bytes(&grit_copy.path().join(".git"));
            assert_eq!(
                grit_log, git_log,
                "expire={label}: grit reflog bytes differ from git"
            );
            if backend == Backend::Files {
                assert_reflog_tree_matches(
                    &git_copy.path().join(".git"),
                    &grit_copy.path().join(".git"),
                );
            }
        }
    });
}

/// Git wildmatch must not treat `refs/heads/main*suffix` as matching `refs/heads/main`.
#[test]
fn gc_per_ref_pattern_non_match_wildmatch_matches_git() {
    each_backend_git_oracle(|backend, repo| {
        let refname = "refs/heads/main";
        let now = wall_clock_now();
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, refname, &oid).expect("tip");
        let ts = now - 60 * 86_400;
        write_controlled_reflog(
            repo,
            refname,
            &[ReflogEntry {
                old_oid: ObjectId::zero(),
                new_oid: oid,
                identity: reflog_identity(ts),
                message: "sixty-day".to_owned(),
            }],
        );

        let config_fragment = "\
[gc]\n\
\treflogExpire = never\n\
\treflogExpireUnreachable = never\n\
[gc \"refs/heads/main*suffix\"]\n\
\treflogExpire = now\n\
";

        let grit_copy = tempfile::tempdir().expect("grit copy");
        let git_copy = tempfile::tempdir().expect("git copy");
        copy_worktree(repo.worktree(), grit_copy.path());
        copy_worktree(repo.worktree(), git_copy.path());

        append_repo_config(grit_copy.path(), config_fragment);
        append_repo_config(git_copy.path(), config_fragment);

        let grit_git_dir = grit_copy.path().join(".git");
        let grit_repo = open_repo(grit_copy.path());
        let cfg = load_repo_config(&grit_git_dir);
        let gc = load_gc_reflog_expire_config(&cfg, now);
        let grit_result = expire_reflog_git(
            &grit_repo,
            &grit_git_dir,
            refname,
            &ReflogExpireParams {
                stale_fix: false,
                dry_run: false,
                verbose: false,
            },
            None,
            None,
            &gc.patterns,
            gc.global_total,
            gc.global_unreachable,
            now,
        )
        .expect("grit expire");
        assert_eq!(
            grit_result.pruned, 0,
            "main*suffix must not match refs/heads/main (would prune with gc now)"
        );

        git_expire(git_copy.path(), &["reflog", "expire", "refs/heads/main"]);

        if backend == Backend::Files {
            assert_reflog_tree_matches(
                &git_copy.path().join(".git"),
                &grit_copy.path().join(".git"),
            );
        }
        assert!(
            !read_reflog(&grit_git_dir, refname)
                .expect("grit read")
                .is_empty(),
            "pattern must not apply to refs/heads/main"
        );
        let _ = backend;
    });
}

/// When several per-ref gc patterns match, Git uses the first matching rule in config order.
#[test]
fn gc_per_ref_overlapping_pattern_precedence_matches_git() {
    each_backend_git_oracle(|backend, repo| {
        let refname = "refs/heads/main";
        let now = wall_clock_now();
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, refname, &oid).expect("tip");
        let ts = now - 60 * 86_400;
        write_controlled_reflog(
            repo,
            refname,
            &[ReflogEntry {
                old_oid: ObjectId::zero(),
                new_oid: oid,
                identity: reflog_identity(ts),
                message: "sixty-day".to_owned(),
            }],
        );

        let config_fragment = "\
[gc \"refs/heads/main\"]\n\
\treflogExpire = now\n\
[gc \"refs/heads/*\"]\n\
\treflogExpire = never\n\
";

        let grit_copy = tempfile::tempdir().expect("grit copy");
        let git_copy = tempfile::tempdir().expect("git copy");
        copy_worktree(repo.worktree(), grit_copy.path());
        copy_worktree(repo.worktree(), git_copy.path());

        append_repo_config(grit_copy.path(), config_fragment);
        append_repo_config(git_copy.path(), config_fragment);

        let grit_git_dir = grit_copy.path().join(".git");
        let grit_repo = open_repo(grit_copy.path());
        let cfg = load_repo_config(&grit_git_dir);
        let gc = load_gc_reflog_expire_config(&cfg, now);
        expire_reflog_git(
            &grit_repo,
            &grit_git_dir,
            refname,
            &ReflogExpireParams {
                stale_fix: false,
                dry_run: false,
                verbose: false,
            },
            None,
            None,
            &gc.patterns,
            gc.global_total,
            gc.global_unreachable,
            now,
        )
        .expect("grit expire");

        git_expire(git_copy.path(), &["reflog", "expire", "refs/heads/main"]);

        assert_reflog_tree_matches(
            &git_copy.path().join(".git"),
            &grit_copy.path().join(".git"),
        );
        let _ = backend;
    });
}

#[test]
fn t1421_grit_written_entries_read_by_git_log_g() {
    each_backend_git_oracle(|_, repo| {
        let git_dir = repo.git_dir();
        let refname = "refs/heads/main";
        let o1 = empty_commit_oid(repo);
        let o2 = empty_commit_oid(repo);
        write_ref(&git_dir, refname, &o2).expect("tip");
        let z = ObjectId::zero();
        let identity = reflog_identity(1_650_000_000);
        append_reflog(
            &git_dir,
            refname,
            &z,
            &o1,
            &identity,
            "grit: bootstrap",
            true,
        )
        .expect("append");
        append_reflog(
            &git_dir,
            refname,
            &o1,
            &o2,
            &identity,
            "grit: advance",
            false,
        )
        .expect("append2");

        let cfg = load_repo_config(&git_dir);
        append_reflog_with_config(
            &git_dir,
            "HEAD",
            &o1,
            &o2,
            &identity,
            "grit: head",
            false,
            Some(&cfg),
        )
        .expect("head log");

        let show = git(
            repo.worktree(),
            &["reflog", "show", "--format=%H %gs", "-2", "main"],
        );
        assert!(
            show.contains(&format!("{o2} grit: advance")),
            "reflog show: {show:?}"
        );
        assert!(
            show.contains(&format!("{o1} grit: bootstrap")),
            "reflog show: {show:?}"
        );

        let log_g = git(
            repo.worktree(),
            &["log", "-g", "--format=%H %s", "-2", "main"],
        );
        assert!(
            log_g.contains(&o2.to_string()),
            "git log -g should list tip commit: {log_g:?}"
        );
    });
}

#[test]
fn log_all_ref_updates_modes_match_git() {
    struct Mode {
        key: &'static str,
        tag_expect: bool,
        branch_expect: bool,
    }
    let modes = [
        Mode {
            key: "false",
            tag_expect: false,
            branch_expect: false,
        },
        Mode {
            key: "true",
            tag_expect: false,
            branch_expect: true,
        },
        Mode {
            key: "always",
            tag_expect: true,
            branch_expect: true,
        },
    ];

    for mode in modes {
        each_backend_git_oracle(|_, repo| {
            write_repo_config(
                repo.worktree(),
                &format!(
                    "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = {}\n",
                    mode.key
                ),
            );
            let git_dir = repo.git_dir();
            let oid = empty_commit_oid(repo);
            let tag = "refs/tags/rlu-tag";
            let branch = "refs/heads/rlu-branch";
            git(repo.worktree(), &["update-ref", tag, &oid.to_string()]);
            git(repo.worktree(), &["update-ref", branch, &oid.to_string()]);

            let git_tag = git_ok(repo.worktree(), &["reflog", "exists", tag]);
            let git_branch = git_ok(repo.worktree(), &["reflog", "exists", branch]);
            assert_eq!(git_tag, mode.tag_expect, "git tag reflog mode {}", mode.key);
            assert_eq!(
                git_branch, mode.branch_expect,
                "git branch reflog mode {}",
                mode.key
            );

            let cfg = load_repo_config(&git_dir);
            let effective = effective_log_refs_config(&git_dir);
            let _ = read_log_refs_config(&git_dir);
            assert_eq!(
                grit_lib::refs::should_autocreate_reflog_for_mode(tag, effective),
                mode.tag_expect,
                "grit tag policy {}",
                mode.key
            );
            assert_eq!(
                grit_lib::refs::should_autocreate_reflog_for_mode(branch, effective),
                mode.branch_expect,
                "grit branch policy {}",
                mode.key
            );

            let tag_log_before = reflog_exists(&git_dir, tag);
            append_reflog_with_config(
                &git_dir,
                tag,
                &oid,
                &oid,
                &reflog_identity(1_660_000_000),
                "grit tag touch",
                false,
                Some(&cfg),
            )
            .expect("tag append");
            let branch_log_before = reflog_exists(&git_dir, branch);
            append_reflog_with_config(
                &git_dir,
                branch,
                &oid,
                &oid,
                &reflog_identity(1_660_000_000),
                "grit branch touch",
                false,
                Some(&cfg),
            )
            .expect("branch append");

            assert_eq!(
                reflog_exists(&git_dir, tag),
                tag_log_before || mode.tag_expect,
                "grit tag reflog create {}",
                mode.key
            );
            assert_eq!(
                reflog_exists(&git_dir, branch),
                branch_log_before || mode.branch_expect,
                "grit branch reflog create {}",
                mode.key
            );
        });
    }
}

#[test]
fn t0600_expire_on_symref_not_referent() {
    let root = tempfile::tempdir().expect("tempdir");
    let worktree = root.path();
    grit_lib::repo::init_repository(worktree, false, "main", None, "files").expect("init");
    let git_dir = worktree.join(".git");
    let refname = "refs/heads/main";
    let sym = "refs/heads/sym-main";
    let seed_main_reflog = |wt: &Path| {
        let gd = wt.join(".git");
        let o1 = git_empty_commit_oid(wt);
        let o2 = git_empty_commit_oid(wt);
        let o3 = git_empty_commit_oid(wt);
        write_ref(&gd, refname, &o3).expect("write tip");
        let z = ObjectId::zero();
        append_reflog(
            &gd,
            refname,
            &z,
            &o1,
            &reflog_identity(1_000_000),
            "first",
            true,
        )
        .expect("log1");
        append_reflog(
            &gd,
            refname,
            &o1,
            &o2,
            &reflog_identity(1_500_000_000),
            "second",
            false,
        )
        .expect("log2");
        append_reflog(
            &gd,
            refname,
            &o2,
            &o3,
            &reflog_identity(1_690_000_000),
            "third",
            false,
        )
        .expect("log3");
    };
    seed_main_reflog(worktree);
    write_symbolic_ref(&git_dir, sym, refname).expect("symref");
    append_reflog(
        &git_dir,
        sym,
        &ObjectId::zero(),
        &git_empty_commit_oid(worktree), // files-only repo; system git seed
        &reflog_identity(1_000_000),
        "sym only",
        true,
    )
    .expect("sym log");

    let grit_copy = tempfile::tempdir().expect("grit copy");
    let git_copy = tempfile::tempdir().expect("git copy");
    copy_worktree(worktree, grit_copy.path());
    copy_worktree(worktree, git_copy.path());

    git_expire(git_copy.path(), &["reflog", "expire", "--expire=now", sym]);
    grit_expire_simple(&grit_copy.path().join(".git"), sym, None);

    assert_reflog_tree_matches(
        &git_copy.path().join(".git"),
        &grit_copy.path().join(".git"),
    );

    let main_git =
        fs::read(reflog_file_path(&git_copy.path().join(".git"), refname)).expect("main");
    let main_grit =
        fs::read(reflog_file_path(&grit_copy.path().join(".git"), refname)).expect("main");
    assert_eq!(
        main_git, main_grit,
        "referent reflog must survive symref expire"
    );
    assert!(git_fsck_strict(git_copy.path()));
    assert!(git_fsck_strict(grit_copy.path()));
}

#[test]
fn git_written_reflog_lines_parse_like_grit() {
    each_backend_git_oracle(|backend, repo| {
        if backend == Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let refname = "refs/heads/parse-me";
        let o1 = empty_commit_oid(repo);
        let o2 = empty_commit_oid(repo);
        write_ref(&git_dir, refname, &o2).expect("tip");
        let path = reflog_file_path(&git_dir, refname);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let identity = format!("{AUTHOR_NAME} <{AUTHOR_EMAIL}> 1234567890 +0000");
        let raw = format!(
            "{} {} {}\tmsg\twith\ttabs\n{} {} {}\n{} {} {}\r\n",
            ObjectId::zero(),
            o1,
            identity,
            o1,
            o2,
            identity,
            o2,
            o2,
            identity,
        );
        fs::write(&path, raw).expect("write raw log");

        let want = vec![
            ReflogEntry {
                old_oid: ObjectId::zero(),
                new_oid: o1,
                identity: identity.clone(),
                message: "msg\twith\ttabs".to_owned(),
            },
            ReflogEntry {
                old_oid: o1,
                new_oid: o2,
                identity: identity.clone(),
                message: String::new(),
            },
            ReflogEntry {
                old_oid: o2,
                new_oid: o2,
                identity: identity.clone(),
                message: String::new(),
            },
        ];
        assert_eq!(read_reflog(&git_dir, refname).expect("read"), want);
        assert_eq!(read_reflog_dwim(&git_dir, "parse-me").expect("dwim"), want);
    });
}

#[test]
fn reflog_exists_list_and_path_match_git() {
    each_backend_git_oracle(|backend, repo| {
        let git_dir = repo.git_dir();
        let nested = "refs/heads/group/deep/ref";
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, nested, &oid).expect("write nested");
        append_reflog(
            &git_dir,
            nested,
            &ObjectId::zero(),
            &oid,
            &reflog_identity(1_640_000_000),
            "nested",
            true,
        )
        .expect("nested log");

        let empty_path = reflog_file_path(&git_dir, nested);
        if empty_path.is_file() {
            // touch empty trailing file for a sibling ref
        }
        let sibling = "refs/heads/group/deep/empty";
        write_ref(&git_dir, sibling, &oid).expect("sibling");
        let sib_log = reflog_file_path(&git_dir, sibling);
        if backend == Backend::Reftable {
            reftable_create_reflog(&git_dir, sibling).expect("empty reftable reflog");
        } else {
            if let Some(p) = sib_log.parent() {
                fs::create_dir_all(p).expect("mkdir");
            }
            fs::write(&sib_log, "").expect("empty reflog file");
        }

        assert!(reflog_exists(&git_dir, nested));
        assert!(reflog_exists(&git_dir, sibling));
        assert_eq!(
            reflog_file_path(&git_dir, nested),
            grit_lib::reflog::reflog_path(&git_dir, nested)
        );

        let mut grit_refs = list_reflog_refs(&git_dir).expect("list");
        grit_refs.sort();
        let mut git_refs = git_reflog_refs(repo.worktree());
        git_refs.sort();
        for r in &git_refs {
            assert!(grit_refs.contains(r), "grit missing reflog ref {r}");
        }
    });
}

#[test]
fn delete_reflog_and_truncate_match_git() {
    each_backend_git_oracle(|backend, repo| {
        let refname = "refs/heads/main";
        seed_three_entry_reflog(repo, refname);

        let grit_copy = tempfile::tempdir().expect("grit copy");
        let git_copy = tempfile::tempdir().expect("git copy");
        copy_worktree(repo.worktree(), grit_copy.path());
        copy_worktree(repo.worktree(), git_copy.path());

        git(git_copy.path(), &["reflog", "delete", "main@{1}"]);
        delete_reflog_entries(&grit_copy.path().join(".git"), refname, &[1]).expect("delete idx");

        if backend == Backend::Files {
            assert_reflog_tree_matches(
                &git_copy.path().join(".git"),
                &grit_copy.path().join(".git"),
            );
        }
        assert!(git_fsck_strict(git_copy.path()));
        assert!(git_fsck_strict(grit_copy.path()));

        truncate_last_reflog_line(&grit_copy.path().join(".git"), refname).expect("truncate");
        git(git_copy.path(), &["reflog", "delete", "main@{0}"]);
        if backend == Backend::Files {
            assert_reflog_tree_matches(
                &git_copy.path().join(".git"),
                &grit_copy.path().join(".git"),
            );
        }
    });
}

#[test]
fn expire_unreachable_stalefix_and_mark_reachable() {
    each_backend_git_oracle(|backend, repo| {
        let git_dir = repo.git_dir();
        let refname = "refs/heads/main";
        let (o1, o2, o3) = seed_three_entry_reflog(repo, refname);

        let grit_copy = tempfile::tempdir().expect("grit copy");
        let git_copy = tempfile::tempdir().expect("git copy");
        copy_worktree(repo.worktree(), grit_copy.path());
        copy_worktree(repo.worktree(), git_copy.path());

        let repo_obj = open_repo(grit_copy.path());
        expire_reflog_unreachable(
            &repo_obj,
            &grit_copy.path().join(".git"),
            refname,
            Some(1_600_000_000),
        )
        .expect("unreachable expire");
        git_expire(
            git_copy.path(),
            &[
                "reflog",
                "expire",
                "--expire=never",
                "--expire-unreachable=1600000000",
                refname,
            ],
        );
        if backend == Backend::Files {
            assert_reflog_tree_matches(
                &git_copy.path().join(".git"),
                &grit_copy.path().join(".git"),
            );
        }

        append_reflog(
            &git_dir,
            refname,
            &o3,
            &ObjectId::from_hex("1111111111111111111111111111111111111111").expect("bad oid"),
            &reflog_identity(1_000_000),
            "stale",
            false,
        )
        .expect("stale append");
        if backend == Backend::Files {
            write_repo_config(repo.worktree(), "[core]\nrepositoryformatversion = 0\n");
        }
        append_repo_config(repo.worktree(), "[gc]\n\treflogExpire = now\n");
        let stale_grit = tempfile::tempdir().expect("stale grit");
        let stale_git = tempfile::tempdir().expect("stale git");
        copy_worktree(repo.worktree(), stale_grit.path());
        copy_worktree(repo.worktree(), stale_git.path());
        grit_expire_git(
            stale_grit.path(),
            refname,
            ReflogExpireParams {
                stale_fix: true,
                dry_run: false,
                verbose: false,
            },
            None,
            None,
        );
        git_expire(
            stale_git.path(),
            &["reflog", "expire", "--stale-fix", "--expire=now", refname],
        );
        if backend == Backend::Files {
            assert_reflog_tree_matches(
                &stale_git.path().join(".git"),
                &stale_grit.path().join(".git"),
            );
        }

        let reachable = mark_stalefix_reachable(&repo_obj, &git_dir).expect("mark");
        assert!(reachable.contains(&o1));
        assert!(reachable.contains(&o2));
        assert!(reachable.contains(&o3));
        let _ = (o1, o2);
    });
}

#[test]
fn mirror_branch_reflog_to_head_and_detached() {
    each_backend_git_oracle(|backend, repo| {
        let git_dir = repo.git_dir();
        let branch = "refs/heads/main";
        seed_three_entry_reflog(repo, branch);
        mirror_branch_reflog_to_head(&git_dir, branch).expect("mirror");
        if backend == Backend::Files {
            let head = fs::read(reflog_file_path(&git_dir, "HEAD")).expect("head log");
            let branch_log = fs::read(reflog_file_path(&git_dir, branch)).expect("branch log");
            assert_eq!(head, branch_log);
        }

        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "HEAD", &oid).expect("detach head");
        append_reflog(
            &git_dir,
            "HEAD",
            &oid,
            &oid,
            &reflog_identity(1_680_000_000),
            "detached",
            true,
        )
        .expect("detached head log");
        assert!(reflog_exists(&git_dir, "HEAD"));
        let head_entries = read_reflog(&git_dir, "HEAD").expect("head read");
        assert!(
            head_entries.iter().any(|e| e.message.contains("detached")),
            "HEAD reflog should contain detached entry"
        );
    });
}

#[test]
fn all_reflog_oids_and_ordered_match_git_walk() {
    fn git_reflog_oid_set(worktree: &Path) -> HashSet<ObjectId> {
        let mut out = HashSet::new();
        for rel in reflog_tree_bytes(&worktree.join(".git")).into_keys() {
            let content = fs::read(worktree.join(".git/logs").join(&rel)).expect("read");
            let text = String::from_utf8_lossy(&content);
            for line in text.lines() {
                let Some((old, rest)) = line.split_once(' ') else {
                    continue;
                };
                let Some((new, _)) = rest.split_once(' ') else {
                    continue;
                };
                if let Ok(o) = old.parse::<ObjectId>() {
                    if !o.is_zero() {
                        out.insert(o);
                    }
                }
                if let Ok(o) = new.parse::<ObjectId>() {
                    if !o.is_zero() {
                        out.insert(o);
                    }
                }
            }
        }
        out
    }

    each_backend_git_oracle(|backend, repo| {
        seed_three_entry_reflog(repo, "refs/heads/main");
        append_reflog(
            &repo.git_dir(),
            "refs/heads/extra",
            &ObjectId::zero(),
            &empty_commit_oid(repo),
            &reflog_identity(1_610_000_000),
            "extra",
            true,
        )
        .expect("extra");
        let git_dir = repo.git_dir();
        let grit_set = all_reflog_oids(&git_dir).expect("oids");
        if backend == Backend::Files {
            assert_eq!(grit_set, git_reflog_oid_set(repo.worktree()));
        }
        let ordered = all_reflog_oids_ordered(&git_dir).expect("ordered");
        let mut names = list_reflog_refs(&git_dir).expect("names");
        names.sort();
        let mut manual = Vec::new();
        let mut seen = HashSet::new();
        let z = ObjectId::zero();
        for name in names {
            for entry in read_reflog(&git_dir, &name).expect("read") {
                for oid in [entry.old_oid, entry.new_oid] {
                    if oid != z && seen.insert(oid) {
                        manual.push(oid);
                    }
                }
            }
        }
        assert_eq!(ordered, manual);
        let mut manual_set = BTreeSet::new();
        manual_set.extend(ordered.iter().copied());
        assert_eq!(grit_set, manual_set.into_iter().collect());
    });
}
