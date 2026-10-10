//! Loose object store scenarios from upstream t1006/t1007/t1050/t1060 (object-level).

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::environment::Environment;
use grit_lib::error::Error;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::odb::{hash_algo_for_git_dir, hash_algo_for_objects_dir, Odb, WriteOptions};
use grit_test_support::objects::{
    flip_byte_at, git_fsck, git_supports_sha256, write_loose_object, HashAlgo as FixtureAlgo,
    RepoFixture,
};

fn fixture_algo(algo: HashAlgo) -> FixtureAlgo {
    match algo {
        HashAlgo::Sha1 => FixtureAlgo::Sha1,
        HashAlgo::Sha256 => FixtureAlgo::Sha256,
    }
}

fn algos_to_run() -> Vec<HashAlgo> {
    let mut out = vec![HashAlgo::Sha1];
    if git_supports_sha256() {
        out.push(HashAlgo::Sha256);
    }
    out
}

#[test]
fn hash_algo_for_objects_dir_reads_repository_format() {
    let repo = init_repo(HashAlgo::Sha1);
    let env = Environment::capture_process();
    assert_eq!(
        hash_algo_for_objects_dir(&env, &repo.objects_dir()),
        HashAlgo::Sha1
    );
    let odb = Odb::new(&repo.objects_dir());
    assert_eq!(odb.objects_dir(), repo.objects_dir().as_path());
    assert_eq!(
        hash_algo_for_git_dir(&env, &repo.path().join(".git")),
        HashAlgo::Sha1
    );
    odb.invalidate_packs();
    let wired = Odb::new(&repo.objects_dir()).with_config_git_dir(repo.path().join(".git"));
    assert!(wired.config_git_dir().is_some());
}

fn init_repo(algo: HashAlgo) -> RepoFixture {
    RepoFixture::init(fixture_algo(algo)).expect("git init fixture repo")
}

fn git_hex(repo: &RepoFixture, args: &[&str]) -> String {
    repo.git(args).stdout.trim().to_string()
}

fn git_cat_file_blob(repo: &RepoFixture, oid: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(repo.path())
        .args(["cat-file", "blob", oid])
        .output()
        .expect("cat-file blob");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

#[cfg(unix)]
fn make_loose_object_mutable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
}

#[cfg(not(unix))]
fn make_loose_object_mutable(_path: &std::path::Path) {}

fn blob_payloads() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        b"binary\0with\0nuls".to_vec(),
        b"line1\r\nline2\r\n".to_vec(),
        (0u8..=255).collect(),
        vec![0u8; 4 * 1024 * 1024],
    ]
}

fn assert_grit_write_passes_git_strict(
    repo: &RepoFixture,
    odb: &Odb,
    kind: ObjectKind,
    data: &[u8],
) {
    let oid = odb.write(kind, data).expect("grit write");
    let hex = oid.to_hex();
    let git_kind = git_hex(repo, &["cat-file", "-t", &hex]);
    assert_eq!(git_kind, kind.as_str());
    let git_size = git_hex(repo, &["cat-file", "-s", &hex]);
    assert_eq!(git_size, data.len().to_string());
    if kind == ObjectKind::Blob {
        let git_body = git_cat_file_blob(repo, &hex);
        assert_eq!(git_body, data);
    }
    let fsck = git_fsck(repo.path(), true);
    assert!(fsck.ok, "git fsck --strict: {:?}", fsck.msg_ids);
}

fn git_hash_object_w(repo: &RepoFixture, kind: &str, data: &[u8]) -> String {
    let object_format = match repo.algo() {
        FixtureAlgo::Sha1 => "sha1",
        FixtureAlgo::Sha256 => "sha256",
    };
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["hash-object", "-w", "-t", kind, "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_OBJECT_FORMAT", object_format)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(data)
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "hash-object: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn t1006_grit_written_loose_objects_git_cat_file_and_fsck() {
    for algo in algos_to_run() {
        let repo = init_repo(algo);
        let odb = Odb::new(&repo.objects_dir());
        for data in blob_payloads() {
            assert_grit_write_passes_git_strict(&repo, &odb, ObjectKind::Blob, &data);
        }
        let empty_tree = Vec::new();
        assert_grit_write_passes_git_strict(&repo, &odb, ObjectKind::Tree, &empty_tree);
    }
}

#[test]
fn t1007_git_hash_object_w_grit_read_and_read_info() {
    for algo in algos_to_run() {
        let repo = init_repo(algo);
        let odb = Odb::new(&repo.objects_dir());
        for data in blob_payloads() {
            let hex = git_hash_object_w(&repo, "blob", &data);
            let oid = ObjectId::from_hex(&hex).expect("git oid");
            let obj = odb.read(&oid).expect("read git blob");
            assert_eq!(obj.kind, ObjectKind::Blob);
            assert_eq!(obj.data, data);
            let info = odb.read_info(&oid).expect("read_info");
            assert_eq!(info.kind, ObjectKind::Blob);
            assert_eq!(info.size, u64::try_from(data.len()).unwrap());
        }
    }
}

#[test]
fn t1050_odb_hash_matches_git_hash_object() {
    for algo in algos_to_run() {
        let repo = init_repo(algo);
        let odb = Odb::new(&repo.objects_dir());
        for data in blob_payloads() {
            let git_oid = git_hash_object_w(&repo, "blob", &data);
            let grit_oid = odb.hash(ObjectKind::Blob, &data).to_hex();
            assert_eq!(grit_oid, git_oid, "blob len {}", data.len());
        }
        let tree = Vec::new();
        assert_eq!(
            odb.hash(ObjectKind::Tree, &tree).to_hex(),
            git_hash_object_w(&repo, "tree", &tree)
        );
        let tree_oid = git_hash_object_w(&repo, "tree", &tree);
        let commit_body = format!(
            "tree {tree_oid}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n"
        );
        let commit_oid = git_hash_object_w(&repo, "commit", commit_body.as_bytes());
        assert_eq!(
            odb.hash(ObjectKind::Commit, commit_body.as_bytes())
                .to_hex(),
            commit_oid
        );
        let tag_body = format!(
            "object {commit_oid}\ntype commit\ntag v-test\ntagger T <t@example.com> 1 +0000\n\nannotated\n"
        );
        let tag_oid = git_hash_object_w(&repo, "tag", tag_body.as_bytes());
        assert_eq!(
            odb.hash(ObjectKind::Tag, tag_body.as_bytes()).to_hex(),
            tag_oid
        );
    }
}

#[test]
fn large_blob_round_trip_grit_then_git_and_git_then_grit() {
    for algo in algos_to_run() {
        let big = vec![0xABu8; 6 * 1024 * 1024];

        let grit_repo = init_repo(algo);
        let grit_odb = Odb::new(&grit_repo.objects_dir());
        let oid = grit_odb
            .write(ObjectKind::Blob, &big)
            .expect("grit write big");
        let fsck = git_fsck(grit_repo.path(), true);
        assert!(fsck.ok, "fsck big blob: {:?}", fsck.msg_ids);
        assert_eq!(grit_odb.read(&oid).expect("read big").data, big);

        let git_repo = init_repo(algo);
        let git_odb = Odb::new(&git_repo.objects_dir());
        let hex = git_hash_object_w(&git_repo, "blob", &big);
        let git_oid = ObjectId::from_hex(&hex).expect("parse");
        assert_eq!(git_odb.read(&git_oid).unwrap().data, big);
        assert_eq!(git_oid, oid);
    }
}

#[test]
fn t1060_loose_zlib_bit_flip_returns_zlib_error() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"flip-me").expect("write");
    let path = odb.object_path(&oid);
    make_loose_object_mutable(&path);
    flip_byte_at(&path, 4).expect("flip");
    let err = odb.read(&oid).unwrap_err();
    assert!(matches!(err, Error::Zlib(_)), "{err:?}");
}

#[test]
fn t1060_loose_truncated_returns_zlib_error() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"truncate-me").expect("write");
    let path = odb.object_path(&oid);
    let len = std::fs::metadata(&path).unwrap().len();
    make_loose_object_mutable(&path);
    let new_len = len.saturating_sub(4).max(1);
    {
        use std::fs::OpenOptions;
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for truncate")
            .set_len(new_len)
            .expect("truncate");
    }
    let err = odb.read(&oid).unwrap_err();
    assert!(matches!(err, Error::Zlib(_)), "{err:?}");
}

#[test]
fn t1060_loose_header_size_mismatch_returns_corrupt_object() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let body = b"size-mismatch";
    let oid = odb.hash(ObjectKind::Blob, body);
    let path = odb.object_path(&oid);
    let mut raw = format!("blob 999999\0").into_bytes();
    raw.extend_from_slice(body);
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let zlib = enc.finish().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, zlib).unwrap();
    let err = odb.read(&oid).unwrap_err();
    assert!(
        matches!(err, Error::CorruptObject(_)),
        "expected CorruptObject, got {err:?}"
    );
}

#[test]
fn t1060_loose_header_type_mismatch_returns_loose_hash_mismatch() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let body = b"type-mismatch";
    let oid = odb.hash(ObjectKind::Blob, body);
    let path = odb.object_path(&oid);
    let mut raw = format!("tree {}\0", body.len()).into_bytes();
    raw.extend_from_slice(body);
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let zlib = enc.finish().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, zlib).unwrap();
    let err = Odb::read_loose_verify_oid(&path, &oid).unwrap_err();
    assert!(
        matches!(err, Error::LooseHashMismatch { .. }),
        "expected LooseHashMismatch, got {err:?}"
    );
}

#[test]
fn t1060_read_loose_verify_oid_wrong_path_returns_loose_hash_mismatch() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let body = b"payload-for-mismatch";
    let real_oid = odb.hash(ObjectKind::Blob, body);
    let decoy_oid = odb.hash(ObjectKind::Blob, b"decoy-name");
    let real_path = odb.object_path(&real_oid);
    write_loose_object(&repo.objects_dir(), FixtureAlgo::Sha1, "blob", body).expect("write loose");
    let decoy_path = odb.object_path(&decoy_oid);
    std::fs::create_dir_all(decoy_path.parent().unwrap()).expect("decoy prefix dir");
    std::fs::copy(&real_path, &decoy_path).expect("copy to wrong name");
    let _ = std::fs::remove_file(&real_path);
    let err = Odb::read_loose_verify_oid(&decoy_path, &decoy_oid).unwrap_err();
    assert!(matches!(err, Error::LooseHashMismatch { .. }), "{err:?}");
}

#[test]
fn t1060_missing_loose_object_returns_object_not_found() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let missing = odb.hash(ObjectKind::Blob, b"never-written");
    let err = odb.read(&missing).unwrap_err();
    assert!(matches!(err, Error::ObjectNotFound(_)), "{err:?}");
}

#[test]
fn odb_exists_local_freshen_and_write_options() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"opts").expect("write");
    assert!(odb.exists(&oid));
    assert!(odb.exists_local(&oid));

    let path = odb.object_path(&oid);
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(25));
    assert!(odb.freshen_object(&oid));
    let after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert!(after >= before);

    let again = odb
        .write_with_options(
            ObjectKind::Blob,
            b"opts",
            WriteOptions {
                silent: true,
                ..WriteOptions::default()
            },
        )
        .expect("silent rewrite");
    assert_eq!(again, oid);
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), after);

    let local = odb
        .write_local(ObjectKind::Blob, b"local-only")
        .expect("local");
    assert!(odb.read(&local).is_ok());
    assert!(odb.object_path(&local).is_file());

    let store = b"blob 3\0raw".to_vec();
    let raw_oid = odb.write_raw(&store).expect("write_raw");
    assert_eq!(odb.read(&raw_oid).unwrap().data, b"raw");

    let mat = odb
        .write_loose_materialize(ObjectKind::Blob, b"mat")
        .expect("materialize");
    assert!(odb.object_path(&mat).is_file());

    let idem = odb.write(ObjectKind::Blob, b"opts").expect("idem");
    assert_eq!(idem, oid);
}

#[test]
fn write_trust_new_loose_and_assume_loose_only_existence() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let data = b"trust-new";
    let oid = odb
        .write_with_options(
            ObjectKind::Blob,
            data,
            WriteOptions {
                trust_new_loose: true,
                ..WriteOptions::default()
            },
        )
        .expect("trust write");
    assert_eq!(odb.read(&oid).unwrap().data, data);

    let oid2 = odb
        .write_with_options(
            ObjectKind::Blob,
            data,
            WriteOptions {
                assume_loose_only_existence: true,
                ..WriteOptions::default()
            },
        )
        .expect("assume loose");
    assert_eq!(oid2, oid);
}

fn zlib_with_preset_dictionary() -> Vec<u8> {
    // Valid zlib CMF/FLG with FDICT set ((CMF<<8|FLG) % 31 == 0), then dictionary id.
    let mut out = vec![0x78, 0x20, 0, 0, 0, 0];
    out.extend_from_slice(b"\x03\x00");
    out
}

#[test]
fn read_loose_verify_oid_accepts_valid_object() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let oid = odb.write(ObjectKind::Blob, b"verified").expect("write");
    let path = odb.object_path(&oid);
    let obj = Odb::read_loose_verify_oid(&path, &oid).expect("verify read");
    assert_eq!(obj.data, b"verified");
}

#[test]
fn read_well_known_empty_tree_without_loose_file() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let empty = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
    let obj = odb.read(&empty).expect("empty tree");
    assert_eq!(obj.kind, ObjectKind::Tree);
    assert!(obj.data.is_empty());
    let info = odb.read_info(&empty).expect("empty tree info");
    assert_eq!(info.kind, ObjectKind::Tree);
    assert_eq!(info.size, 0);
}

#[test]
fn ensure_all_loose_prefix_dirs_creates_256_prefixes() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    odb.ensure_all_loose_prefix_dirs().expect("prefix dirs");
    for i in 0u8..=255 {
        assert!(repo.objects_dir().join(format!("{i:02x}")).is_dir());
    }
}

#[test]
fn t1060_object_header_too_long_returns_typed_error() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let long_type = "b".repeat(33);
    let raw = format!("{long_type} 0\0");
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(raw.as_bytes()).unwrap();
    let zlib = enc.finish().unwrap();
    let oid = odb.hash(ObjectKind::Blob, b"");
    let path = odb.object_path(&oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, zlib).unwrap();
    let err = odb.read(&oid).unwrap_err();
    assert!(matches!(err, Error::ObjectHeaderTooLong { .. }), "{err:?}");
}

#[test]
fn exists_via_env_alternate_dirs_at_construction() {
    let primary = init_repo(HashAlgo::Sha1);
    let alt = init_repo(HashAlgo::Sha1);
    let alt_odb = Odb::new(&alt.objects_dir());
    let oid = alt_odb.write(ObjectKind::Blob, b"env-alt").expect("write");
    let primary_odb =
        Odb::new(&primary.objects_dir()).with_env_alternate_dirs(vec![alt.objects_dir()]);
    assert!(primary_odb.exists(&oid));
    assert!(primary_odb.read(&oid).is_ok());
}

#[test]
fn read_and_exists_via_file_alternate() {
    let primary = init_repo(HashAlgo::Sha1);
    let alt = init_repo(HashAlgo::Sha1);
    let alt_odb = Odb::new(&alt.objects_dir());
    let oid = alt_odb
        .write(ObjectKind::Blob, b"from-alternate")
        .expect("alt write");
    let primary_odb = Odb::new(&primary.objects_dir());
    assert!(!primary_odb.exists(&oid));
    std::fs::create_dir_all(primary.objects_dir().join("info")).expect("info");
    primary_odb
        .append_file_alternate(&alt.objects_dir())
        .expect("register alternate");
    assert!(primary_odb.exists(&oid));
    assert_eq!(
        primary_odb.read(&oid).expect("read via alt").data,
        b"from-alternate"
    );
}

#[test]
fn write_raw_local_stays_in_primary_objects_dir() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let store = b"blob 4\0only";
    let oid = odb.write_raw_local(store).expect("write_raw_local");
    assert!(odb.object_path(&oid).is_file());
    assert_eq!(odb.read(&oid).unwrap().data, b"only");
}

#[test]
fn write_loose_zlib_prehashed_round_trip() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let payload = b"prehashed zlib path";
    let store = format!("blob {}\0", payload.len());
    let mut store_bytes = store.into_bytes();
    store_bytes.extend_from_slice(payload);
    let zlib = odb
        .zlib_compress_loose_store(&store_bytes)
        .expect("compress");
    let oid = odb.hash(ObjectKind::Blob, payload);
    odb.write_loose_zlib_prehashed(&oid, &zlib, WriteOptions::default())
        .expect("prehashed write");
    let again = odb
        .write_loose_zlib_prehashed(&oid, &zlib, WriteOptions::default())
        .expect("idempotent");
    assert_eq!(again, oid);
    assert_eq!(odb.read(&oid).unwrap().data, payload);
}

#[test]
fn sha256_exists_and_hash_when_git_supports() {
    if !git_supports_sha256() {
        return;
    }
    let repo = init_repo(HashAlgo::Sha256);
    let odb = Odb::new(&repo.objects_dir());
    assert_eq!(odb.hash_algo(), HashAlgo::Sha256);
    let oid = odb.write(ObjectKind::Blob, b"sha256-loose").expect("write");
    assert!(odb.exists(&oid));
    assert!(odb.exists_local(&oid));
}

#[test]
fn write_local_materializes_loose_when_object_only_in_cruft_pack() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let data = b"cruft-pack write_local rescue\n";
    let oid = odb.write(ObjectKind::Blob, data).expect("write loose blob");
    assert!(
        odb.object_path(&oid).is_file(),
        "fixture starts as loose object"
    );

    let repack = Command::new("git")
        .current_dir(repo.path())
        .args(["repack", "--cruft", "-d"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git repack --cruft");
    assert!(
        repack.status.success(),
        "git repack --cruft: {}",
        String::from_utf8_lossy(&repack.stderr)
    );
    odb.invalidate_packs();

    assert!(
        !odb.object_path(&oid).is_file(),
        "cruft repack should remove the loose copy"
    );
    assert!(
        odb.exists_local(&oid),
        "object must remain reachable from local cruft pack"
    );

    let again = odb
        .write_local(ObjectKind::Blob, data)
        .expect("write_local after cruft repack");
    assert_eq!(again, oid);
    assert!(
        odb.object_path(&oid).is_file(),
        "write_local must materialize a loose copy when cruft pack cannot be freshened"
    );
    assert_eq!(odb.read(&oid).unwrap().data, data);
}

#[test]
fn t1006_zlib_preset_dictionary_returns_needs_dictionary() {
    let repo = init_repo(HashAlgo::Sha1);
    let odb = Odb::new(&repo.objects_dir());
    let oid = odb.hash(ObjectKind::Blob, b"dict");
    let path = odb.object_path(&oid);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, zlib_with_preset_dictionary()).unwrap();
    let err = odb.read(&oid).unwrap_err();
    match err {
        Error::Zlib(msg) => assert_eq!(msg, "needs dictionary"),
        other => panic!("expected needs dictionary, got {other:?}"),
    }
}
