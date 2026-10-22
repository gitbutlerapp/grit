//! Repository-backed coverage for remaining public-fn allowlist entries.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::bundle::bundle_remote::fetch_from_bundle;
use grit_lib::bundle::{Bundle, BundleSpec, BundleWriter};
use grit_lib::command_runner::system_command_runner;
use grit_lib::commit::signing::verify_tag;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::gitmodules::gitdir::{
    compute_default_submodule_gitdir, connect_submodule_work_tree_and_git_dir,
    die_path_inside_submodule_when_disabled, ensure_submodule_gitdir_config, hash_blob_sha1_hex,
    init_submodule_head_from_gitlink, is_git_directory, migrate_gitdir_configs,
    path_inside_indexed_submodule, path_inside_registered_submodule,
    path_inside_registered_submodule_name, set_submodule_repo_worktree,
    submodule_gitdir_filesystem_path, submodule_gitdir_outer_conflict,
    submodule_path_config_enabled, validate_encoded_submodule_git_dir,
    validate_legacy_submodule_git_dir, write_submodule_gitfile,
};
use grit_lib::config::gitmodules::{oids_from_copied_object_paths, verify_gitmodules_for_commit};
use grit_lib::config::mailmap::{
    apply_mailmap_to_commit_or_tag_bytes, load_mailmap_raw, map_contact, read_mailmap_blob,
    MailmapTable,
};
use grit_lib::config::ConfigSet;
use grit_lib::config::{ConfigFile, ConfigScope};
use grit_lib::diff::merge_trees::index_tree_oid_matches_head;
use grit_lib::diff::rerere::rerere_forget_path;
use grit_lib::index::write_tree::build_cache_tree_from_tree;
use grit_lib::index::MODE_REGULAR;
use grit_lib::notes::remove_note_entry;
use grit_lib::notes::NotesTreeEntry;
use grit_lib::objects::index_pack::{
    ingest_received_pack_path, install_pack_bytes, install_pack_path, IngestPackOptions,
};
use grit_lib::objects::midx::{
    format_midx_bitmapped_packs, midx_sidecar_path_for_midx_file, read_midx_btmp_ranges,
};
use grit_lib::objects::odb::store::loose::enumerate_loose_objects;
use grit_lib::objects::pack::{
    clear_pack_delta_base_cache, configure_delta_base_cache_from_config,
    packed_ref_delta_reuse_slice, read_object_from_pack_at_offset, read_pack_index,
    slice_one_pack_object,
};
use grit_lib::objects::pack_objects::{
    build_pack_from_send_list_with_write_threads, reachable_closure,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::checkout::apply_index_file_mode;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::stash::worktree_bytes_for_index_mode;
use grit_lib::porcelain::status::{expand_untracked_for_staging, status_path_matches_worktree};
use grit_lib::porcelain::tag::{creator_date, get_tag_annotation, tag_contains, tag_points_at};
use grit_lib::porcelain::worktree_guard::prepare_tree_switch;
use grit_lib::progress::NullProgress;
use grit_lib::refs::ref_storage::detect_format;
use grit_lib::refs::{
    list_refs_glob_for_repository, reftable::verify_ref_transaction_batch_reftable, write_ref,
    RefBatchItem,
};
use grit_lib::remote::transfer::{build_pack, FetchOptions, PackBuildOptions, TagMode};
use grit_lib::repo::caches::RepoCaches;
use grit_lib::repo::ensure_core_bare;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::repo_handles::{StashApplyOptions, StashPushOptions};
use grit_lib::state::{read_merge_msg, resolve_head, split_commit_in_progress};
use grit_lib::RefStorageFormat;
use tempfile::tempdir;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init")
}

fn seed_main_commit(repo: &Repository, dir: &Path) -> ObjectId {
    std::fs::write(dir.join("tracked.txt"), b"payload\n").expect("write");
    stage_for_commit(repo, &mut NullProgress).expect("stage");
    let ident = "User <u@example.com> 1700000000 +0000".to_owned();
    create_commit(
        repo,
        &CommitRequest {
            message: "seed".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
        },
        &mut NullProgress,
    )
    .expect("commit");
    grit_lib::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main oid")
}

fn git_fsck(dir: &Path) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn bundle_pack_odb_and_midx_paths() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let tip = seed_main_commit(&repo, dir.path());
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"payload\n")
        .expect("blob");
    let _ =
        enumerate_loose_objects(repo.odb().objects_dir(), repo.odb().hash_algo()).expect("enum");

    let pack_opts = PackBuildOptions::default();
    let pack = build_pack(repo.odb(), &[blob], &[], &pack_opts).expect("pack");
    let ingested = install_pack_bytes(pack.clone(), repo.odb(), &IngestPackOptions::default())
        .expect("install");
    let _ = install_pack_path(
        &ingested.pack_path,
        repo.odb(),
        &IngestPackOptions::default(),
    );
    let _ = ingest_received_pack_path(
        ingested.pack_path.clone(),
        repo.odb(),
        &IngestPackOptions::default(),
    );

    let pack_dir = repo.odb().objects_dir().join("pack");
    let idx_path = fs::read_dir(&pack_dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx");
    let idx = read_pack_index(&idx_path).expect("read idx");
    let pack_bytes = fs::read(&ingested.pack_path).expect("read pack");
    if let Some(off) = idx.find_offset(&blob).expect("lookup") {
        let _ = read_object_from_pack_at_offset(&idx, off).expect("read at off");
        let _ = slice_one_pack_object(&pack_bytes, off, idx.hash_bytes());
    }
    let _ = packed_ref_delta_reuse_slice(repo.odb().objects_dir(), &blob, &HashSet::new());
    clear_pack_delta_base_cache();
    configure_delta_base_cache_from_config(None);

    let stop = HashSet::new();
    let shallow = HashSet::new();
    let have = reachable_closure(repo.odb(), &[blob], &stop, true, &shallow).expect("closure");
    let _ = build_pack_from_send_list_with_write_threads(
        repo.odb(),
        &[blob],
        &have,
        &pack_opts,
        &HashMap::new(),
        Some(1),
    );

    let _ = format_midx_bitmapped_packs(repo.odb().objects_dir());
    let _ = read_midx_btmp_ranges(repo.odb().objects_dir());
    let _ = midx_sidecar_path_for_midx_file(&idx_path, "bitmap");

    let bundle_path = dir.path().join("test.bundle");
    {
        let mut f = fs::File::create(&bundle_path).expect("bundle file");
        write_bundle_header_and_pack(&repo, tip, &mut f);
    }
    let bundle = Bundle::open(&bundle_path).expect("open bundle");
    let _ = bundle.path();

    let dest = tempdir().expect("dest");
    let dest_repo = init_repo(dest.path());
    let mut fetch_opts = FetchOptions::default();
    fetch_opts.refspecs = vec!["refs/heads/main:refs/heads/main".to_owned()];
    fetch_opts.tags = TagMode::None;
    fetch_opts.dry_run = true;
    let _ = fetch_from_bundle(dest_repo.git_dir(), &bundle_path, &fetch_opts);

    git_fsck(dir.path());
    let bundle_verify = Command::new("git")
        .current_dir(dir.path())
        .args([
            "bundle",
            "verify",
            bundle_path.to_str().expect("bundle path utf8"),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git bundle verify");
    assert!(
        bundle_verify.status.success(),
        "git bundle verify: {}",
        String::from_utf8_lossy(&bundle_verify.stderr)
    );
}

fn write_bundle_header_and_pack(repo: &Repository, tip: ObjectId, w: &mut dyn Write) {
    let spec = BundleSpec {
        include: vec![(tip, "refs/heads/main".to_owned())],
        exclude: Vec::new(),
        filter: None,
    };
    let writer = BundleWriter::new(w);
    writer
        .write_header_and_pack(repo, &spec)
        .expect("bundle write");
}

#[test]
fn index_handle_begin_and_commit_update() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let handle = repo.index_handle();
    let mut lock = handle.begin_update().expect("lock");
    let mut index = handle.load().expect("index");
    handle
        .commit_update(&mut lock, &mut index, false)
        .expect("commit index");
}

#[test]
fn gitmodules_verify_for_commit() {
    let dir = tempdir().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    fs::write(
        dir.path().join(".gitmodules"),
        "[submodule \"s\"]\n\tpath = s\n\turl = ./s.git\n",
    )
    .expect("gitmodules");
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", ".gitmodules"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add");
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "add gitmodules"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git commit");
    let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let head = grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").expect("head");
    let _ = verify_gitmodules_for_commit(repo.odb(), head);
}

#[test]
fn refs_state_repo_caches_and_notes() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    let oid = ObjectId::from_bytes(&[7u8; 20]).expect("oid");
    write_ref(git_dir.as_path(), "refs/heads/topic/foo", &oid).expect("write ref");
    let _ = list_refs_glob_for_repository(&repo, "refs/heads/topic/*");
    let _ = detect_format(git_dir.as_path());
    ensure_core_bare(git_dir.as_path()).expect("bare");

    let caches = RepoCaches::new(system_command_runner());
    caches.set_promisor_hydrate_hook(None);

    let mut entries: Vec<NotesTreeEntry> = Vec::new();
    assert!(!remove_note_entry(&mut entries, oid.to_hex().as_str()));

    let head = resolve_head(git_dir.as_path()).expect("head");
    let _ = split_commit_in_progress(git_dir.as_path(), &head);
    let _ = read_merge_msg(git_dir.as_path());

    let rt_dir = tempdir().expect("rt");
    let rt_repo = Repository::init(
        rt_dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("reftable init");
    let items = vec![RefBatchItem {
        name: "refs/heads/new".to_owned(),
        new_oid: Some(oid),
    }];
    let _ = verify_ref_transaction_batch_reftable(rt_repo.git_dir(), &items);
}

#[test]
fn porcelain_stash_tag_status_and_worktree() {
    let dir = tempdir().expect("tempdir");
    fs::write(dir.path().join("tracked.txt"), b"hi\n").expect("file");
    let repo = init_repo(dir.path());
    let mut index = repo.index_handle().load().expect("index");
    let work_tree = dir.path();
    let _ = status_path_matches_worktree(&repo, &index, work_tree, "tracked.txt", &[]);
    let _ = expand_untracked_for_staging(&repo, &mut index, work_tree, Vec::new(), &[]);

    let file = work_tree.join("mode.txt");
    fs::write(&file, b"x\n").expect("write");
    #[cfg(unix)]
    {
        apply_index_file_mode(&file, grit_lib::index::MODE_EXECUTABLE).expect("chmod");
    }
    let _ = worktree_bytes_for_index_mode(&file, MODE_REGULAR);

    let empty_tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let _ = prepare_tree_switch(&repo, None, &empty_tree).ok();

    let tag_oid = repo
        .odb()
        .write(ObjectKind::Blob, b"not tag")
        .expect("blob");
    let _ = get_tag_annotation(&repo, &tag_oid, 1);
    let _ = creator_date(&repo, &tag_oid);
    let _ = tag_contains(&repo, &tag_oid, &tag_oid);
    let _ = tag_points_at(&repo, &tag_oid, &tag_oid);

    let gpg = grit_lib::commit::signing::GpgConfig::from_config(&ConfigSet::new()).expect("gpg");
    let _ = verify_tag(&gpg, b"not a signed tag");
    let _ = repo.verify_tag(
        tag_oid,
        &grit_lib::tree_sha256::TagVerifyOptions {
            check_signature: false,
            gpg: gpg.with_command_runner(repo.command_runner()),
        },
    );

    let identity = "User <u@example.com> 1700000000 +0000".to_owned();
    let _ = repo.stash().push(&StashPushOptions {
        message: Some("wip".into()),
        include_untracked: false,
        identity: identity.clone(),
    });
    let _ = repo.stash().pop(
        work_tree,
        &StashApplyOptions {
            index: 0,
            restore_index: false,
            identity: identity.clone(),
        },
    );
    let _ = rerere_forget_path(&repo, "tracked.txt");
}

#[test]
fn mailmap_gitmodules_submodule_and_write_tree() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let git_dir = dir.path().join(".git");
    fs::write(
        git_dir.join("mailmap"),
        "Proper Name <proper@example.com> <old@example.com>\n",
    )
    .expect("mailmap");
    repo.reload_config().expect("reload");
    let _ = load_mailmap_raw(&repo);
    let _ = read_mailmap_blob(&repo, "HEAD").ok();
    let table = MailmapTable::default();
    let _ = map_contact(Some("Old"), Some("old@example.com"), &[]);
    let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor O <o@e> 0 +0000\ncommitter C <c@e> 0 +0000\n\n\n";
    let _ = apply_mailmap_to_commit_or_tag_bytes(raw, &table);

    fs::write(
        dir.path().join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub.git\n",
    )
    .expect("gitmodules");
    let head = resolve_head(git_dir.as_path()).expect("head");
    if let Some(oid) = head.oid() {
        let _ = verify_gitmodules_for_commit(repo.odb(), *oid);
    }
    let _ = oids_from_copied_object_paths(&[PathBuf::from(
        "objects/00/00000000000000000000000000000000000000",
    )]);

    let _ = hash_blob_sha1_hex(b"data").expect("hash");
    let mut cfg_file = ConfigFile::from_path(&git_dir.join("config"), ConfigScope::Local)
        .expect("cfg path")
        .expect("cfg file");
    let _ = submodule_path_config_enabled(git_dir.as_path());
    let _ = is_git_directory(git_dir.as_path());
    let _ = submodule_gitdir_outer_conflict(git_dir.as_path(), "sub");
    let _ = validate_legacy_submodule_git_dir(git_dir.as_path(), "sub").ok();
    let modules_path = git_dir.join("modules").join("sub");
    let _ = validate_encoded_submodule_git_dir(
        dir.path(),
        &cfg_file,
        &modules_path,
        "sub",
        git_dir.as_path(),
    )
    .ok();
    let _ = compute_default_submodule_gitdir(dir.path(), git_dir.as_path(), &cfg_file, "sub");
    let _ = ensure_submodule_gitdir_config(dir.path(), git_dir.as_path(), &mut cfg_file, "sub");
    let index = repo.index_handle().load().expect("index");
    let _ = path_inside_indexed_submodule(&index, "sub/file");
    let _ = path_inside_registered_submodule(dir.path(), "sub/file");
    let _ = path_inside_registered_submodule_name(dir.path(), "sub/file");
    let _ = die_path_inside_submodule_when_disabled(
        git_dir.as_path(),
        dir.path(),
        "sub/new",
        Some(&index),
    );
    let modules = git_dir.join("modules").join("sub");
    fs::create_dir_all(&modules.join("objects")).expect("modules");
    let sub_wt = dir.path().join("sub");
    fs::create_dir_all(&sub_wt).expect("sub wt");
    let _ = write_submodule_gitfile(&sub_wt, &modules);
    let _ = set_submodule_repo_worktree(&modules, &sub_wt);
    let _ = migrate_gitdir_configs(dir.path(), git_dir.as_path());
    let _ = submodule_gitdir_filesystem_path(dir.path(), git_dir.as_path(), &cfg_file, "sub");
    let _ = connect_submodule_work_tree_and_git_dir(
        dir.path(),
        git_dir.as_path(),
        &cfg_file,
        "sub",
        &sub_wt,
    );
    let _ = init_submodule_head_from_gitlink(&modules, &ObjectId::zero().to_hex());

    let tree = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("tree");
    let _ = build_cache_tree_from_tree(repo.odb(), &tree);
    let _ = index_tree_oid_matches_head(repo.odb(), &index, &tree);
}
