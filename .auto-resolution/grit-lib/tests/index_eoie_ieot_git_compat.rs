//! EOIE/IEOT index extensions: round-trip with system `git` and parallel grit load.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::index::{Index, IndexParseContext};
use grit_lib::objects::hash::hash_object;
use grit_lib::objects::HashAlgo;
use grit_lib::objects::ObjectKind;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::heavy_disk_serial_guard;

/// Minimum size for parallel IEOT load (`2 * INDEX_LOAD_THREAD_COST`).
const ENTRY_COUNT: usize = 20_000;

fn git_init(repo: &Path) {
    assert!(
        run_git(&["init", repo.to_str().expect("path")]).success(),
        "git init"
    );
}

fn populate_files(root: &Path, count: usize) {
    for i in 0..count {
        let dir = format!("d{:04}", i / 100);
        fs::create_dir_all(root.join(&dir)).expect("mkdir");
        fs::write(
            root.join(format!("{dir}/f{i:05}.txt")),
            format!("body {i}\n"),
        )
        .expect("write file");
    }
}

fn git_config(repo: &Path, key: &str, value: &str) {
    run_git(&["-C", repo.to_str().expect("path"), "config", key, value]);
}

fn git_add_all(repo: &Path, version: u32) {
    let status = Command::new("git")
        .args(["-C", repo.to_str().expect("path"), "add", "-A"])
        .env("GIT_INDEX_VERSION", version.to_string())
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed for index v{version}");
}

fn load_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(repo.git_dir()), true).expect("config")
}

fn grit_parse_parallel(bytes: &[u8], algo: HashAlgo, cfg: &ConfigSet, threads: usize) -> Index {
    let ctx = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(threads)));
    Index::parse_with_algo_and_config(bytes, algo, Some(cfg), ctx).expect("parallel parse")
}

fn grit_parse_serial(bytes: &[u8], algo: HashAlgo) -> Index {
    Index::parse_with_algo(bytes, algo).expect("serial parse")
}

fn assert_entries_match(a: &Index, b: &Index) {
    assert_eq!(a.entries().len(), b.entries().len());
    for (ea, eb) in a.entries().iter().zip(b.entries().iter()) {
        assert_eq!(ea.path, eb.path);
        assert_eq!(ea.oid, eb.oid);
        assert_eq!(ea.mode, eb.mode);
        assert_eq!(ea.flags, eb.flags);
        assert_eq!(ea.flags_extended, eb.flags_extended);
    }
}

fn git_writes_index_grit_reads(version: u32) {
    let _disk = heavy_disk_serial_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    populate_files(root, ENTRY_COUNT);
    git_init(root);
    git_config(root, "index.threads", "4");
    git_config(root, "index.recordOffsetTable", "true");
    git_config(root, "index.recordEndOfIndexEntries", "true");
    git_add_all(root, version);

    let index_path = root.join(".git/index");
    let bytes = fs::read(&index_path).expect("read git index");
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let cfg = load_config(&repo);
    let algo = repo.odb().hash_algo();

    let hash_len = algo.len();
    let body = &bytes[..bytes.len() - hash_len];
    assert!(
        body.windows(4).any(|w| w == b"EOIE") && body.windows(4).any(|w| w == b"IEOT"),
        "git index with index.threads=4 must contain EOIE and IEOT"
    );

    let serial = grit_parse_serial(&bytes, algo);
    let parallel = grit_parse_parallel(&bytes, algo, &cfg, 4);
    assert_entries_match(&serial, &parallel);
    assert_fsmonitor_flags_match(&serial, &parallel);
    let _ = version;
}

fn assert_fsmonitor_flags_match(a: &Index, b: &Index) {
    for (ea, eb) in a.entries().iter().zip(b.entries().iter()) {
        assert_eq!(
            ea.fsmonitor_valid(),
            eb.fsmonitor_valid(),
            "fsmonitor valid bit mismatch on {}",
            String::from_utf8_lossy(&ea.path)
        );
    }
}

#[test]
fn git_eoie_ieot_index_v2_grit_reads_match_serial() {
    git_writes_index_grit_reads(2);
}

#[test]
fn git_eoie_ieot_index_v3_grit_reads_match_serial() {
    git_writes_index_grit_reads(3);
}

#[test]
fn git_eoie_ieot_index_v4_grit_reads_match_serial() {
    git_writes_index_grit_reads(4);
}

fn grit_write_git_accepts(version: u32) {
    let _disk = heavy_disk_serial_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    populate_files(root, ENTRY_COUNT);
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let mut cfg_text = fs::read_to_string(repo.git_dir().join("config")).expect("config");
    cfg_text.push_str(
        "\n[index]\n\tthreads = 4\n\trecordOffsetTable = true\n\trecordEndOfIndexEntries = true\n",
    );
    fs::write(repo.git_dir().join("config"), cfg_text).expect("write config");

    let mut idx = Index::empty(version);
    for i in 0..ENTRY_COUNT {
        let path = format!("d{:04}/f{i:05}.txt", i / 100);
        let data = format!("body {i}\n");
        let oid = hash_object(repo.odb().hash_algo(), ObjectKind::Blob, data.as_bytes());
        repo.odb()
            .write_loose_materialize(ObjectKind::Blob, data.as_bytes())
            .expect("write blob");
        idx.add_or_replace(grit_lib::index::IndexEntry {
            ctime_sec: 1_700_000_000,
            ctime_nsec: 0,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
            dev: 1,
            ino: i as u32,
            mode: grit_lib::index::MODE_REGULAR,
            uid: 1000,
            gid: 1000,
            size: data.len() as u32,
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.into_bytes(),
            base_index_pos: 0,
        });
    }

    let cfg = load_config(&repo);
    let policy = ThreadPolicy::with_threads_override(Some(4));
    idx.write_with_config_and_policy(&repo.index_handle().path(), &cfg, policy)
        .expect("write");

    assert!(run_git(&["-C", root.to_str().expect("p"), "fsck"]).success());
    assert!(run_git(&["-C", root.to_str().expect("p"), "status"]).success());
    assert!(
        run_git(&["-C", root.to_str().expect("p"), "update-index", "--refresh"]).success(),
        "git update-index should rewrite EOIE/IEOT index cleanly"
    );
}

#[test]
fn grit_write_eoie_ieot_v4_git_fsck_and_update_index() {
    grit_write_git_accepts(4);
}

#[test]
fn grit_write_v4_ieot_git_reads_with_one_index_thread() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    populate_files(root, 8);
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let mut cfg_text = fs::read_to_string(repo.git_dir().join("config")).expect("config");
    cfg_text.push_str(
        "\n[index]\n\tthreads = 4\n\trecordOffsetTable = true\n\trecordEndOfIndexEntries = true\n",
    );
    fs::write(repo.git_dir().join("config"), cfg_text).expect("write config");

    let mut idx = Index::empty(4);
    for i in 0..8 {
        let path = format!("d{:04}/f{i:05}.txt", i / 100);
        let data = format!("body {i}\n");
        let oid = hash_object(repo.odb().hash_algo(), ObjectKind::Blob, data.as_bytes());
        repo.odb()
            .write_loose_materialize(ObjectKind::Blob, data.as_bytes())
            .expect("write blob");
        idx.add_or_replace(grit_lib::index::IndexEntry {
            ctime_sec: 1_700_000_000,
            ctime_nsec: 0,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
            dev: 1,
            ino: i as u32,
            mode: grit_lib::index::MODE_REGULAR,
            uid: 1000,
            gid: 1000,
            size: data.len() as u32,
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.into_bytes(),
            base_index_pos: 0,
        });
    }

    let cfg = load_config(&repo);
    let policy = ThreadPolicy::with_threads_override(Some(4));
    idx.write_with_config_and_policy(&repo.index_handle().path(), &cfg, policy)
        .expect("write v4 ieot index");

    let root_s = root.to_str().expect("path");
    assert!(
        run_git(&["-C", root_s, "-c", "index.threads=1", "ls-files"]).success(),
        "git serial index reader must accept grit-written v4 IEOT block restarts"
    );

    let bytes = fs::read(repo.index_handle().path()).expect("read index");
    let algo = repo.odb().hash_algo();
    let parsed = grit_parse_serial(&bytes, algo);
    assert_eq!(parsed.entries().len(), 8);
}

#[test]
fn parallel_ieot_load_preserves_fsmonitor_valid_bits() {
    let _disk = heavy_disk_serial_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    populate_files(root, ENTRY_COUNT);
    let repo = Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let mut cfg_text = fs::read_to_string(repo.git_dir().join("config")).expect("config");
    cfg_text.push_str(
        "\n[index]\n\tthreads = 4\n\trecordOffsetTable = true\n\trecordEndOfIndexEntries = true\n",
    );
    fs::write(repo.git_dir().join("config"), cfg_text).expect("write config");

    let mut idx = Index::empty(2);
    for i in 0..ENTRY_COUNT {
        let path = format!("d{:04}/f{i:05}.txt", i / 100);
        let data = format!("body {i}\n");
        repo.odb()
            .write_loose_materialize(ObjectKind::Blob, data.as_bytes())
            .expect("blob");
        let oid = hash_object(repo.odb().hash_algo(), ObjectKind::Blob, data.as_bytes());
        let mut entry = grit_lib::index::IndexEntry {
            ctime_sec: 1_700_000_000,
            ctime_nsec: 0,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
            dev: 1,
            ino: i as u32,
            mode: grit_lib::index::MODE_REGULAR,
            uid: 1000,
            gid: 1000,
            size: data.len() as u32,
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.into_bytes(),
            base_index_pos: 0,
        };
        entry.set_fsmonitor_valid(i != 1_000 && i != 9_000);
        idx.add_or_replace(entry);
    }
    idx.set_fsmonitor_last_update(Some("builtin:fake".to_owned()));
    idx.sync_fsmonitor_dirty_bitmap();

    let cfg = load_config(&repo);
    let policy = ThreadPolicy::with_threads_override(Some(4));
    idx.write_with_config_and_policy(&repo.index_handle().path(), &cfg, policy)
        .expect("write ieot index");

    let bytes = fs::read(repo.index_handle().path()).expect("read index");
    let algo = repo.odb().hash_algo();
    let body = &bytes[..bytes.len() - algo.len()];
    assert!(body.windows(4).any(|w| w == b"EOIE"));

    let serial = grit_parse_serial(&bytes, algo);
    let parallel = grit_parse_parallel(&bytes, algo, &cfg, 4);
    assert_fsmonitor_flags_match(&serial, &parallel);
    assert!(!serial.entries().iter().all(|e| e.fsmonitor_valid()));
}

#[test]
fn invalid_eoie_falls_back_to_serial_parse() {
    let _disk = heavy_disk_serial_guard();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    populate_files(root, ENTRY_COUNT);
    git_init(root);
    git_config(root, "index.threads", "4");
    git_config(root, "index.recordEndOfIndexEntries", "true");
    git_add_all(root, 2);

    let index_path = root.join(".git/index");
    let mut bytes = fs::read(&index_path).expect("read");
    let algo = HashAlgo::Sha1;
    let hash_len = algo.len();
    let body_len = bytes.len() - hash_len;
    let body = &bytes[..body_len];
    assert!(
        body.windows(4).any(|w| w == b"EOIE"),
        "fixture must include EOIE before corruption"
    );
    let eoie_with_header = 8 + 4 + hash_len;
    if body.len() >= eoie_with_header {
        let eoie_start = body.len() - eoie_with_header;
        if &body[eoie_start..eoie_start + 4] == b"EOIE" {
            // Corrupt EOIE extension-header hash (not the file trailer); read_eoie must reject it.
            let hash_in_eoie = eoie_start + 8 + 4;
            bytes[hash_in_eoie] = bytes[hash_in_eoie].wrapping_add(1);
            let trailer = algo.digest(&bytes[..body_len]);
            bytes[body_len..].copy_from_slice(trailer.as_bytes());
        }
    }
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    let cfg = load_config(&repo);
    let ctx = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(4)));
    let parsed =
        Index::parse_with_algo_and_config(&bytes, algo, Some(&cfg), ctx).expect("fallback parse");
    assert_eq!(parsed.entries().len(), ENTRY_COUNT);
}

fn run_git(args: &[&str]) -> std::process::ExitStatus {
    Command::new("git").args(args).status().expect("spawn git")
}
