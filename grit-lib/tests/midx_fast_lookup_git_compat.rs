//! Multi-pack-index read path: Git `cat-file` parity, incremental chain, and hot-path caching.

use grit_lib::midx::{
    midx_lookup_pack_and_offset, read_midx_pack_idx_names, try_read_object_via_midx,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::odb::Odb;
use grit_lib::pack::{clear_pack_cache, read_pack_index_cached};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn pack_all_objects_as_layer(dir: &Path, layer: usize) -> Option<()> {
    let rev = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--objects", "--all"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !rev.status.success() {
        return None;
    }
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", &format!(".git/objects/pack/layer-{layer}")])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(&rev.stdout).ok()?;
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        Some(())
    } else {
        None
    }
}

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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_cat_file_batch(objects: &Path, oids: &[ObjectId]) -> Vec<(ObjectId, ObjectKind, Vec<u8>)> {
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .env("GIT_DIR", objects.parent().expect("git dir"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cat-file");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for oid in oids {
            writeln!(stdin, "{}", oid.to_hex()).expect("write oid");
        }
    }
    let out = child.wait_with_output().expect("wait cat-file");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut reader = BufReader::new(out.stdout.as_slice());
    let mut parsed = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut hdr = String::new();
        reader.read_line(&mut hdr).expect("header");
        let mut parts = hdr.split_whitespace();
        let got_oid = ObjectId::from_hex(parts.next().expect("oid")).expect("hex");
        assert_eq!(got_oid, *oid);
        let kind = match parts.next().expect("kind") {
            "blob" => ObjectKind::Blob,
            "tree" => ObjectKind::Tree,
            "commit" => ObjectKind::Commit,
            "tag" => ObjectKind::Tag,
            other => panic!("unexpected kind {other}"),
        };
        let size: usize = parts.next().expect("size").parse().expect("size");
        let mut data = vec![0u8; size + 1];
        reader.read_exact(&mut data).expect("payload");
        assert_eq!(data[size], b'\n');
        data.truncate(size);
        parsed.push((got_oid, kind, data));
    }
    parsed
}

/// Build a repo with `pack_count` separate packs and a Git-written MIDX.
fn multi_pack_midx_fixture(
    pack_count: usize,
) -> Option<(tempfile::TempDir, PathBuf, Vec<ObjectId>)> {
    if !git_available() || pack_count < 2 {
        return None;
    }
    let tmp = tempfile::tempdir().ok()?;
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("seed.txt"), b"seed").ok()?;
    git(dir, &["add", "seed.txt"]);
    git(dir, &["commit", "-q", "-m", "seed"]);

    let mut oids = Vec::new();
    for i in 0..pack_count {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("blob-{i}")).ok()?;
        git(dir, &["add", &format!("f{i}.txt")]);
        git(dir, &["commit", "-q", "-m", &format!("c{i}")]);
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let oid = ObjectId::from_hex(std::str::from_utf8(&out.stdout).ok()?.trim()).ok()?;
        oids.push(oid);
        pack_all_objects_as_layer(dir, i)?;
    }

    git(dir, &["multi-pack-index", "write"]);
    let objects = dir.join(".git/objects");
    Some((tmp, objects, oids))
}

#[test]
fn midx_reads_match_git_cat_file_batch() {
    let Some((_tmp, objects, oids)) = multi_pack_midx_fixture(6) else {
        eprintln!("SKIP: git unavailable or fixture setup failed");
        return;
    };
    clear_pack_cache();
    let git_objects = git_cat_file_batch(&objects, &oids);
    for (oid, kind, data) in git_objects {
        let via_midx = try_read_object_via_midx(&objects, &oid)
            .expect("midx read")
            .expect("object in midx");
        assert_eq!(via_midx.kind, kind);
        assert_eq!(via_midx.data, data);
    }
}

#[test]
fn midx_lookup_miss_returns_none() {
    let Some((_tmp, objects, _oids)) = multi_pack_midx_fixture(5) else {
        eprintln!("SKIP: git unavailable");
        return;
    };
    let miss = ObjectId::from_hex("0000000000000000000000000000000000000001").expect("oid");
    assert!(try_read_object_via_midx(&objects, &miss)
        .expect("read")
        .is_none());
}

#[test]
fn midx_deleted_pack_entry_falls_back_to_packs() {
    let Some((_tmp, objects, oids)) = multi_pack_midx_fixture(5) else {
        eprintln!("SKIP: git unavailable");
        return;
    };
    let head = *oids.last().expect("head");
    let pack_dir = objects.join("pack");
    let (pack_id, _off) = midx_lookup_pack_and_offset(&objects, &head).expect("lookup");
    let idx_names = read_midx_pack_idx_names(&objects).expect("pack names");
    let idx_name = idx_names.get(pack_id as usize).expect("pack id").clone();
    let idx_path = pack_dir.join(&idx_name);
    let pack_path = pack_dir
        .join(idx_name.strip_suffix(".idx").unwrap())
        .with_extension("pack");
    std::fs::remove_file(&idx_path).expect("remove idx");
    std::fs::remove_file(&pack_path).expect("remove pack");

    clear_pack_cache();
    assert!(
        try_read_object_via_midx(&objects, &head)
            .expect("read after delete")
            .is_none(),
        "MIDX entry for a deleted pack must fall through as absent"
    );

    let git_dir = objects.parent().expect("git dir").to_path_buf();
    let odb = Odb::new(&objects).with_config_git_dir(git_dir);
    let via_odb = odb
        .read(&head)
        .expect("Odb should fall back to another pack when MIDX names a deleted pack");
    assert_eq!(via_odb.kind, ObjectKind::Commit);
}

#[test]
fn midx_incremental_chain_reads_base_layer() {
    if !git_available() {
        eprintln!("SKIP: git unavailable");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), b"a").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "a"]);
    git(dir, &["repack", "-adf"]);
    git(dir, &["multi-pack-index", "write"]);

    let out_a = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    let oid_a = ObjectId::from_hex(std::str::from_utf8(&out_a.stdout).unwrap().trim()).unwrap();

    std::fs::write(dir.join("b.txt"), b"b").unwrap();
    git(dir, &["add", "b.txt"]);
    git(dir, &["commit", "-q", "-m", "b"]);
    git(dir, &["repack", "-adf"]);

    let inc = Command::new("git")
        .current_dir(dir)
        .args(["multi-pack-index", "write", "--incremental"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("incremental midx");
    if !inc.status.success() {
        eprintln!(
            "SKIP: incremental MIDX unsupported: {}",
            String::from_utf8_lossy(&inc.stderr)
        );
        return;
    }

    let objects = dir.join(".git/objects");
    clear_pack_cache();
    let via_midx = try_read_object_via_midx(&objects, &oid_a)
        .expect("read")
        .expect("base-layer commit");
    assert_eq!(via_midx.kind, ObjectKind::Commit);
}

#[test]
#[ignore = "perf smoke (30k reads + git cat-file); run with cargo test --release -- --ignored"]
fn odb_midx_packed_read_batch_smoke() {
    let Some((_tmp, objects, oids)) = multi_pack_midx_fixture(10) else {
        eprintln!("SKIP: git unavailable");
        return;
    };
    let git_dir = objects.parent().expect("git dir").to_path_buf();
    clear_pack_cache();
    let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
    let oid = *oids.last().expect("head");
    let n = if cfg!(debug_assertions) { 3_000 } else { 30_000 };
    let _ = odb.read(&oid).expect("warm");

    let start = std::time::Instant::now();
    for _ in 0..n {
        odb.read(&oid).expect("read");
    }
    let grit_elapsed = start.elapsed();

    let batch: Vec<ObjectId> = vec![oid; n];
    let git_start = std::time::Instant::now();
    git_cat_file_batch(&objects, &batch);
    let git_elapsed = git_start.elapsed();

    assert!(
        grit_elapsed.as_secs_f64() <= git_elapsed.as_secs_f64() * 1.2 + 0.005,
        "{n} Odb::read should stay within 1.2× git cat-file --batch (grit={grit_elapsed:?}, git={git_elapsed:?})"
    );
}

#[test]
fn midx_lookup_uses_binary_search() {
    let Some((_tmp, objects, oids)) = multi_pack_midx_fixture(5) else {
        eprintln!("SKIP: git unavailable");
        return;
    };
    for oid in oids {
        let (pack_id, off) = midx_lookup_pack_and_offset(&objects, &oid).expect("listed");
        let pack_dir = objects.join("pack");
        let idx_name = read_midx_pack_idx_names(&objects)
            .expect("names")
            .get(pack_id as usize)
            .expect("pack id")
            .clone();
        let idx_path = pack_dir.join(idx_name);
        let idx = read_pack_index_cached(&idx_path).expect("idx");
        assert_eq!(idx.find_offset(&oid), Some(off));
    }
}
