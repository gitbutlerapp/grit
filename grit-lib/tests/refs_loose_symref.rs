//! Loose refs and symbolic refs (t1400/t1401/t0600/t1405/t1430 subsets).

mod support;

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::reflog::read_reflog;
use grit_lib::refs::is_valid_storable_ref_name;
use grit_lib::refs::{
    append_reflog, delete_ref, list_refs, list_refs_glob, list_refs_physical,
    packed_refs_entry_exists, read_head, read_log_refs_config, read_raw_ref, read_ref_file,
    read_symbolic_ref, resolve_ref, resolve_ref_cached, resolve_ref_dwim, should_autocreate_reflog,
    verify_refname_available_for_create, write_ref, write_ref_cached, write_symbolic_ref,
    PackedRefs, RawRefLookup, Ref, RefnameUnavailable, SYMREF_MAXDEPTH,
};

use support::{
    assert_git_fsck_strict, each_backend, empty_commit_oid, git, git_check_ref_format,
    git_fsck_strict, git_interop_available, git_show_ref, git_update_ref, loose_ref_path, Backend,
};

fn assert_direct_ref_stored(git_dir: &Path, backend: Backend, refname: &str, oid: &ObjectId) {
    match backend {
        Backend::Files => {
            assert_eq!(
                read_ref_file(&loose_ref_path(git_dir, refname)).unwrap(),
                Ref::Direct(*oid)
            );
        }
        Backend::Reftable => {
            assert!(
                !loose_ref_path(git_dir, refname).exists(),
                "reftable refs are not loose files"
            );
            assert_eq!(resolve_ref(git_dir, refname).unwrap(), *oid);
        }
    }
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Files => "files",
        Backend::Reftable => "reftable",
    }
}

fn build_symref_chain(git_dir: &Path, depth: usize, tip: &ObjectId) {
    write_ref(git_dir, "refs/heads/r0", tip).expect("r0");
    for i in 1..=depth {
        write_symbolic_ref(
            git_dir,
            &format!("refs/heads/r{i}"),
            &format!("refs/heads/r{}", i - 1),
        )
        .expect("chain link");
    }
    write_symbolic_ref(
        git_dir,
        "refs/heads/chain-top",
        &format!("refs/heads/r{depth}"),
    )
    .expect("chain top");
}

#[test]
fn t1400_loose_ref_roundtrip_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let refname = "refs/heads/t1400-roundtrip";
        write_ref(&git_dir, refname, &oid).expect("write_ref");

        assert_eq!(resolve_ref(&git_dir, refname).unwrap(), oid);
        assert_eq!(
            read_raw_ref(&git_dir, refname).unwrap(),
            RawRefLookup::Exists
        );
        assert_direct_ref_stored(&git_dir, backend, refname, &oid);

        if git_interop_available(backend) {
            assert_eq!(
                git(repo.worktree(), &["rev-parse", refname]).trim(),
                oid.to_hex()
            );
            assert_eq!(git_show_ref(repo.worktree()).get(refname), Some(&oid));
            assert_git_fsck_strict(repo.worktree());
        }

        eprintln!("ok t1400_loose_ref_roundtrip ({})", backend_label(backend));
    });
}

#[test]
fn t1400_git_written_ref_read_by_grit_both_backends() {
    each_backend(|backend, repo| {
        if !git_interop_available(backend) {
            eprintln!("SKIP t1400_git_written: git lacks reftable interop");
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let refname = "refs/heads/t1400-from-git";
        git_update_ref(repo.worktree(), refname, &oid);

        assert_eq!(resolve_ref(&git_dir, refname).unwrap(), oid);
        assert_direct_ref_stored(&git_dir, backend, refname, &oid);
        eprintln!("ok t1400_git_written ({})", backend_label(backend));
    });
}

#[test]
fn t1400_grit_written_reftable_roundtrip() {
    each_backend(|backend, repo| {
        if backend != Backend::Reftable {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let refname = "refs/heads/t1400-reftable-only";
        write_ref(&git_dir, refname, &oid).expect("write_ref");
        assert_direct_ref_stored(&git_dir, backend, refname, &oid);
        assert_eq!(
            read_raw_ref(&git_dir, refname).unwrap(),
            RawRefLookup::Exists
        );
        eprintln!("ok t1400_grit_written_reftable_roundtrip");
    });
}

#[test]
fn t1401_symref_read_write_roundtrip_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/heads/sym-target", &oid).expect("target");
        write_symbolic_ref(&git_dir, "refs/heads/sym-alias", "refs/heads/sym-target")
            .expect("symref");

        assert_eq!(
            read_symbolic_ref(&git_dir, "refs/heads/sym-alias").unwrap(),
            Some("refs/heads/sym-target".to_owned())
        );
        assert_eq!(resolve_ref(&git_dir, "refs/heads/sym-alias").unwrap(), oid);
        if git_interop_available(backend) {
            assert_git_fsck_strict(repo.worktree());
        }
        eprintln!("ok t1401_symref_roundtrip ({})", backend_label(backend));
    });
}

#[test]
fn t1401_head_detached_vs_symbolic_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);

        assert!(
            read_head(&git_dir).unwrap().is_some(),
            "init should leave HEAD symbolic"
        );

        match backend {
            Backend::Files => write_ref(&git_dir, "HEAD", &oid).expect("detach HEAD"),
            Backend::Reftable => {
                fs::write(git_dir.join("HEAD"), format!("{oid}\n")).expect("detach HEAD file");
            }
        }
        assert!(read_head(&git_dir).unwrap().is_none());
        assert_eq!(resolve_ref(&git_dir, "HEAD").unwrap(), oid);

        match backend {
            Backend::Files => {
                write_symbolic_ref(&git_dir, "HEAD", "refs/heads/main").expect("reattach HEAD");
            }
            Backend::Reftable => {
                fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("reattach HEAD");
            }
        }
        assert_eq!(
            read_head(&git_dir).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        eprintln!("ok t1401_head ({})", backend_label(backend));
    });
}

#[test]
fn t1401_dangling_symref_resolve_and_read_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        write_symbolic_ref(&git_dir, "refs/heads/dangle", "refs/heads/no-such-target")
            .expect("dangling symref");

        assert!(resolve_ref(&git_dir, "refs/heads/dangle").is_err());
        assert_eq!(
            read_symbolic_ref(&git_dir, "refs/heads/dangle").unwrap(),
            Some("refs/heads/no-such-target".to_owned())
        );
        assert_eq!(
            read_raw_ref(&git_dir, "refs/heads/dangle").unwrap(),
            RawRefLookup::Exists
        );
        eprintln!("ok t1401_dangling_symref ({})", backend_label(backend));
    });
}

#[test]
fn t1401_symref_chain_max_depth_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);

        // `depth` intermediate segments r1..=r{depth}; resolvable when depth <= SYMREF_MAXDEPTH - 2.
        let ok_depth = SYMREF_MAXDEPTH.saturating_sub(2);
        build_symref_chain(&git_dir, ok_depth, &oid);
        assert_eq!(
            resolve_ref(&git_dir, "refs/heads/chain-top").unwrap(),
            oid,
            "chain depth {ok_depth} should resolve"
        );

        let fail_depth = ok_depth + 1;
        build_symref_chain(&git_dir, fail_depth, &oid);
        let grit_err = resolve_ref(&git_dir, "refs/heads/chain-top").unwrap_err();
        assert!(
            matches!(grit_err, Error::InvalidRef(_)),
            "expected InvalidRef, got {grit_err:?}"
        );

        if git_interop_available(backend) {
            let _git_ok = git_fsck_strict(repo.worktree());
            let git_rev = std::process::Command::new("git")
                .current_dir(repo.worktree())
                .args(["rev-parse", "refs/heads/chain-top"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert_eq!(
                git_rev, false,
                "git should fail to resolve over-long symref chain"
            );
        }
        eprintln!("ok t1401_symref_depth ({})", backend_label(backend));
    });
}

#[test]
fn t1401_long_ref_name_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let segment = "a".repeat(200);
        let refname = format!("refs/heads/{segment}/tip");
        write_ref(&git_dir, &refname, &oid).expect("long refname");
        assert_eq!(resolve_ref(&git_dir, &refname).unwrap(), oid);
        if git_interop_available(backend) {
            assert_git_fsck_strict(repo.worktree());
        }
        eprintln!("ok t1401_long_name ({})", backend_label(backend));
    });
}

#[test]
fn t1401_symref_invalid_target_overwrite_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_symbolic_ref(&git_dir, "refs/heads/over", "not a valid ref!!").expect("bad symref");
        write_ref(&git_dir, "refs/heads/over", &oid).expect("overwrite with direct ref");
        assert_eq!(resolve_ref(&git_dir, "refs/heads/over").unwrap(), oid);
        eprintln!("ok t1401_overwrite_bad_symref ({})", backend_label(backend));
    });
}

#[test]
fn t1400_delete_ref_semantics_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let refname = "refs/heads/t1400-delete-semantics";
        write_ref(&git_dir, refname, &oid).expect("write nested ref");

        delete_ref(&git_dir, refname).expect("delete_ref");
        assert_eq!(
            read_raw_ref(&git_dir, refname).unwrap(),
            RawRefLookup::NotFound
        );
        assert!(resolve_ref(&git_dir, refname).is_err());
        eprintln!("ok t1400_delete_semantics ({})", backend_label(backend));
    });
}

#[test]
fn t1400_delete_ref_prunes_loose_paths() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let refname = "refs/nested/t1400/delete-me";
        write_ref(&git_dir, refname, &oid).expect("write nested ref");
        let path = loose_ref_path(&git_dir, refname);
        assert!(path.is_file());

        delete_ref(&git_dir, refname).expect("delete_ref");
        assert!(!path.exists());
        assert!(
            !git_dir.join("refs/nested/t1400").exists(),
            "empty parent directory should be pruned"
        );
        eprintln!("ok t1400_delete_prune_loose_paths");
    });
}

#[test]
fn t1400_delete_missing_ref_typed_error_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let err = delete_ref(&git_dir, "refs/heads/never-created").unwrap_err();
        assert!(
            matches!(err, Error::InvalidRef(ref msg) if msg.contains("ref not found")),
            "{err:?}"
        );
        eprintln!("ok t1400_delete_missing ({})", backend_label(backend));
    });
}

#[test]
fn t1400_delete_through_symref_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/heads/delete-target", &oid).expect("target");
        write_symbolic_ref(
            &git_dir,
            "refs/heads/delete-alias",
            "refs/heads/delete-target",
        )
        .expect("alias");

        delete_ref(&git_dir, "refs/heads/delete-alias").expect("delete symref");
        assert_eq!(
            read_raw_ref(&git_dir, "refs/heads/delete-alias").unwrap(),
            RawRefLookup::NotFound
        );
        assert_eq!(
            resolve_ref(&git_dir, "refs/heads/delete-target").unwrap(),
            oid,
            "deleting symref must not delete referent"
        );
        eprintln!("ok t1400_delete_symref ({})", backend_label(backend));
    });
}

#[test]
fn t0600_empty_directory_does_not_block_create() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let parent = git_dir.join("refs/heads/empty-dir-test");
        fs::create_dir_all(&parent).expect("empty dir");
        assert!(parent.is_dir());

        write_ref(&git_dir, "refs/heads/empty-dir-test/leaf", &oid)
            .expect("create under empty dir");
        assert_eq!(
            resolve_ref(&git_dir, "refs/heads/empty-dir-test/leaf").unwrap(),
            oid
        );
        eprintln!("ok t0600_empty_directory");
    });
}

#[test]
fn t0600_nonempty_directory_blocks_create() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let sibling = git_dir.join("refs/heads/blocker/sibling");
        fs::create_dir_all(sibling.parent().unwrap()).expect("parent");
        fs::write(&sibling, format!("{oid}\n")).expect("sibling ref");

        let err = write_ref(&git_dir, "refs/heads/blocker", &oid).unwrap_err();
        assert!(
            matches!(err, Error::RefLock(_)),
            "non-empty directory should block create: {err:?}"
        );
        eprintln!("ok t0600_nonempty_directory");
    });
}

#[test]
fn t0600_broken_ref_file_blocks_create() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let broken = git_dir.join("refs/heads/broken-file");
        fs::create_dir_all(broken.parent().unwrap()).expect("parent");
        fs::write(&broken, "not-a-ref\n").expect("garbage ref");

        let read_err = read_ref_file(&broken).unwrap_err();
        assert!(matches!(read_err, Error::InvalidRef(_)));

        let err = write_ref(&git_dir, "refs/heads/broken-file", &oid).unwrap_err();
        assert!(
            matches!(err, Error::InvalidRef(ref m) if m.contains("reference broken")),
            "updating broken ref should fail: {err:?}"
        );

        assert!(
            write_ref(&git_dir, "refs/heads/broken-file/child", &oid).is_err(),
            "nested create under broken ref file must fail"
        );
        assert!(
            !std::process::Command::new("git")
                .current_dir(repo.worktree())
                .args(["update-ref", "refs/heads/broken-file/child", &oid.to_hex()])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            "git must reject nested ref under broken file"
        );
        eprintln!("ok t0600_broken_ref");
    });
}

#[test]
fn t0600_short_hash_in_ref_rejected() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let short_path = git_dir.join("refs/heads/short-hash");
        fs::create_dir_all(short_path.parent().unwrap()).expect("parent");
        fs::write(&short_path, "193006e\n").expect("short oid");

        let grit_err = read_ref_file(&short_path).unwrap_err();
        assert!(matches!(grit_err, Error::InvalidRef(_)));

        let git_ok = std::process::Command::new("git")
            .current_dir(repo.worktree())
            .args(["rev-parse", "refs/heads/short-hash"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!git_ok, "git should not resolve short hash ref");
        eprintln!("ok t0600_short_hash");
    });
}

#[test]
fn t0600_trailing_garbage_direct_ref_parses_like_git() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        let path = git_dir.join("refs/heads/trail-garbage");
        fs::write(&path, format!("{} extra-token\n", oid.to_hex())).expect("ref body");

        let parsed = read_ref_file(&path).expect("parse like git");
        assert_eq!(parsed, Ref::Direct(oid));
        assert_eq!(
            git(repo.worktree(), &["rev-parse", "refs/heads/trail-garbage"]).trim(),
            oid.to_hex()
        );
        eprintln!("ok t0600_trailing_garbage");
    });
}

#[test]
fn t1405_verify_refname_df_prefix_single_ref() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/df/parent", &oid).expect("parent ref");

        let extras = BTreeSet::new();
        let skip = HashSet::new();
        let err =
            verify_refname_available_for_create(&git_dir, "refs/df/parent/child", &extras, &skip)
                .unwrap_err();
        assert!(matches!(err, RefnameUnavailable::AncestorExists { .. }));

        delete_ref(&git_dir, "refs/df/parent").expect("remove parent");
        write_ref(&git_dir, "refs/df/parent/child", &oid).expect("child ref");
        let err = verify_refname_available_for_create(&git_dir, "refs/df/parent", &extras, &skip)
            .unwrap_err();
        assert!(matches!(err, RefnameUnavailable::DescendantExists { .. }));
        eprintln!("ok t1405_verify_df ({})", backend_label(backend));
    });
}

#[test]
fn t1430_invalid_names_match_git_check_ref_format() {
    let mut names: Vec<String> = Vec::new();
    for short in [
        "main",
        "feature/x",
        "bad name",
        "a..b",
        "a~b",
        "a.lock",
        "HEAD",
        "@",
        "-",
        "x..y",
        "a:b",
        "a*b",
        ".",
        "..",
        "foo/",
        "/bar",
    ] {
        names.push(format!("refs/heads/{short}"));
    }
    for name in names {
        let grit_ok = is_valid_storable_ref_name(&name);
        let git_ok = git_check_ref_format(&name);
        assert_eq!(grit_ok, git_ok, "refname {name:?}");
    }
    eprintln!("ok t1430_invalid_names_match_git_check_ref_format");
}

#[test]
fn t1400_list_refs_glob_and_dwim_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/heads/topic/one", &oid).expect("one");
        write_ref(&git_dir, "refs/heads/topic/two", &oid).expect("two");
        write_ref(&git_dir, "refs/tags/v1", &oid).expect("tag");

        let listed = list_refs(&git_dir, "refs/heads/topic/").expect("list_refs");
        assert_eq!(listed.len(), 2);

        let globbed = list_refs_glob(&git_dir, "refs/heads/topic/*").expect("glob");
        assert_eq!(globbed.len(), 2);

        let physical = list_refs_physical(&git_dir, "refs/heads/topic/").expect("physical");
        assert_eq!(physical.len(), 2);

        let (_, dwim_oid) = resolve_ref_dwim(&git_dir, "topic/one");
        assert_eq!(dwim_oid, Some(oid));

        eprintln!("ok t1400_list_glob_dwim ({})", backend_label(backend));
    });
}

#[test]
fn t1400_packed_refs_resolve_delete_and_cached() {
    each_backend(|backend, repo| {
        if backend != Backend::Files {
            return;
        }
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/heads/packed-a", &oid).expect("a");
        write_ref(&git_dir, "refs/heads/packed-b", &oid).expect("b");
        git(repo.worktree(), &["pack-refs", "--all", "--prune"]);

        assert!(packed_refs_entry_exists(&git_dir, "refs/heads/packed-a").unwrap());
        assert_eq!(resolve_ref(&git_dir, "refs/heads/packed-a").unwrap(), oid);

        let packed = PackedRefs::load(&git_dir).expect("load packed");
        assert_eq!(
            resolve_ref_cached(&git_dir, "refs/heads/packed-b", &packed)
                .unwrap()
                .unwrap(),
            oid
        );

        write_ref_cached(&git_dir, "refs/heads/packed-c", &oid, &packed).expect("cached write");
        delete_ref(&git_dir, "refs/heads/packed-a").expect("delete packed entry");
        assert!(!packed_refs_entry_exists(&git_dir, "refs/heads/packed-a").unwrap());
        assert_git_fsck_strict(repo.worktree());
        eprintln!("ok t1400_packed_refs");
    });
}

#[test]
fn t1400_reflog_append_and_log_refs_config_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        write_ref(&git_dir, "refs/heads/reflog-branch", &oid).expect("write");
        let _cfg = read_log_refs_config(&git_dir);
        assert!(should_autocreate_reflog(
            &git_dir,
            "refs/heads/reflog-branch"
        ));
        let zero_oid: ObjectId = "0000000000000000000000000000000000000000"
            .parse()
            .expect("zero oid");
        append_reflog(
            &git_dir,
            "refs/heads/reflog-branch",
            &zero_oid,
            &oid,
            "Refs Harness <refs-harness@example.com>",
            "refs harness reflog",
            true,
        )
        .expect("append_reflog");
        let entries = read_reflog(&git_dir, "refs/heads/reflog-branch").expect("read_reflog");
        assert!(
            !entries.is_empty(),
            "reflog should contain at least one entry"
        );
        eprintln!("ok t1400_reflog ({})", backend_label(backend));
    });
}

#[test]
fn t1430_write_ref_rejects_invalid_names_both_backends() {
    each_backend(|backend, repo| {
        let git_dir = repo.git_dir();
        let oid = empty_commit_oid(repo);
        for refname in [
            "refs/heads/bad name",
            "refs/heads/x..y",
            "refs/heads/a.lock",
        ] {
            assert!(
                write_ref(&git_dir, refname, &oid).is_err(),
                "write_ref must reject {refname}"
            );
            assert!(
                !loose_ref_path(&git_dir, refname).exists(),
                "must not create {refname}"
            );
        }
        eprintln!("ok t1430_write_rejects ({})", backend_label(backend));
    });
}
