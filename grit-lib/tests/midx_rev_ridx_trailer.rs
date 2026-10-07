//! MIDX standalone `.rev` sidecars must be valid RIDX hashfiles (body + trailing checksum).

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::midx::{
    read_midx_objects, write_multi_pack_index_with_options, WriteMultiPackIndexOptions,
};
use grit_lib::objects::HashAlgo;
use grit_lib::pack_rev::hashfile_checksum_valid;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn grit_write_midx_rev(pack_dir: &Path) {
    let opts = WriteMultiPackIndexOptions {
        write_rev_placeholder: true,
        version: Some(1),
        ..WriteMultiPackIndexOptions::default()
    };
    write_multi_pack_index_with_options(pack_dir, &opts).expect("write MIDX");
}

fn find_midx_rev_sidecar(pack_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(pack_dir).ok()?.find_map(|e| {
        let e = e.ok()?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("multi-pack-index-") && name.ends_with(".rev") {
            Some(e.path())
        } else {
            None
        }
    })
}

fn assert_ridx_hashfile(rev_path: &Path, algo: HashAlgo) {
    let data = std::fs::read(rev_path).expect("read .rev");
    let hash_len = algo.len();
    assert!(
        data.len() >= 12 + hash_len + hash_len,
        "RIDX file too small: {}",
        rev_path.display()
    );
    let body_len = data.len() - hash_len;
    assert_eq!(
        (body_len - 12) % 4,
        0,
        "RIDX body length minus header must be multiple of 4"
    );
    assert!(
        hashfile_checksum_valid(&data, hash_len),
        "RIDX trailing checksum invalid: {}",
        rev_path.display()
    );
}

#[test]
fn sha1_midx_rev_sidecar_is_full_ridx_hashfile() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
    git(tmp.path(), &["add", "f.txt"]);
    git(tmp.path(), &["commit", "-q", "-m", "c"]);
    git(tmp.path(), &["repack", "-adf"]);
    let pack_dir = tmp.path().join(".git/objects/pack");
    let pack_count = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".idx"))
        .count();
    assert!(pack_count >= 1);

    grit_write_midx_rev(&pack_dir);
    let rev = find_midx_rev_sidecar(&pack_dir).expect("MIDX .rev sidecar");
    assert_ridx_hashfile(&rev, HashAlgo::Sha1);
    let _ = pack_count;
    git(tmp.path(), &["multi-pack-index", "verify"]);
}

#[test]
fn sha256_midx_rev_sidecar_is_full_ridx_hashfile() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let ok = Command::new("git")
        .current_dir(tempfile::tempdir().unwrap().path())
        .args(["init", "-q", "--object-format=sha256"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("SKIP: sha256 object format unavailable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    git(
        tmp.path(),
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    for i in 0..6 {
        std::fs::write(tmp.path().join(format!("b{i}.txt")), format!("blob {i}\n")).unwrap();
        git(tmp.path(), &["add", &format!("b{i}.txt")]);
        git(tmp.path(), &["commit", "-q", "-m", &format!("c{i}")]);
        git(tmp.path(), &["repack", "-d"]);
    }
    let pack_dir = tmp.path().join(".git/objects/pack");
    let pack_count = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".idx"))
        .count();
    assert!(
        pack_count >= 3,
        "fixture should have multiple packs, got {pack_count}"
    );

    grit_write_midx_rev(&pack_dir);
    let rev = find_midx_rev_sidecar(&pack_dir).expect("MIDX .rev sidecar");
    assert_ridx_hashfile(&rev, HashAlgo::Sha256);
    let _ = pack_count;
    git(tmp.path(), &["multi-pack-index", "verify"]);
}

#[test]
fn sha256_midx_rev_slot_count_matches_midx_object_count() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let ok = Command::new("git")
        .current_dir(tempfile::tempdir().unwrap().path())
        .args(["init", "-q", "--object-format=sha256"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("SKIP: sha256 object format unavailable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    git(
        tmp.path(),
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    std::fs::write(tmp.path().join("one.txt"), b"1").unwrap();
    git(tmp.path(), &["add", "one.txt"]);
    git(tmp.path(), &["commit", "-q", "-m", "root"]);
    for i in 0..5 {
        std::fs::write(tmp.path().join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        git(tmp.path(), &["add", &format!("f{i}.txt")]);
        git(tmp.path(), &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(tmp.path(), &["repack", "-adf"]);
    let objects = tmp.path().join(".git/objects");
    let pack_dir = objects.join("pack");
    grit_write_midx_rev(&pack_dir);
    let rev = find_midx_rev_sidecar(&pack_dir).expect("MIDX .rev sidecar");
    assert_ridx_hashfile(&rev, HashAlgo::Sha256);
    let (_, listed) = read_midx_objects(&objects).expect("midx objects");
    let data = std::fs::read(&rev).unwrap();
    let hash_len = HashAlgo::Sha256.len();
    let slots = (data.len() - 12 - hash_len - hash_len) / 4;
    assert_eq!(
        slots,
        listed.len(),
        "RIDX entry count must match MIDX object count (reviewer: n objects → 12 + n*4 + 2*hash_len)"
    );
}
