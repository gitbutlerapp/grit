//! Repository-level fsck compared to system `git fsck` (upstream t1450-style cases).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::index::Index;
use grit_lib::maintenance::repository_fsck::{self as fsck, FsckIssue, FsckOptions};
use grit_lib::objects::pack::{
    read_pack_index, verify_pack_and_collect, write_v2_pack_index_with_trailer, PackedType,
};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::{
    git_fsck, git_hash_object_literally, write_loose_object, HashAlgo as FixtureAlgo,
};
use tempfile::TempDir;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() && args.first() != Some(&"fsck") {
        panic!("git {args:?} failed: {combined}");
    }
    combined
}

fn pack_loose_object_and_remove_loose(dir: &std::path::Path, oid: ObjectId) {
    let pack_dir = dir.join(".git/objects/pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    let stem = pack_dir.join("pack-fsck-test");
    let mut pack_cmd = Command::new("git")
        .current_dir(dir)
        .args(["pack-objects", stem.to_str().expect("utf8 stem")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("pack-objects");
    if let Some(mut stdin) = pack_cmd.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(format!("{}\n", oid.to_hex()).as_bytes());
    }
    let pack_out = pack_cmd.wait_with_output().expect("pack wait");
    assert!(
        pack_out.status.success(),
        "pack-objects failed: {}",
        String::from_utf8_lossy(&pack_out.stderr)
    );
    let pack_hash = String::from_utf8_lossy(&pack_out.stdout).trim().to_string();
    let pack_file = pack_dir.join(format!("pack-fsck-test-{pack_hash}.pack"));
    assert!(pack_file.is_file(), "missing pack file {pack_file:?}");
    let index_out = Command::new("git")
        .current_dir(dir)
        .args(["index-pack", pack_file.to_str().expect("pack path")])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("index-pack");
    assert!(
        index_out.status.success(),
        "index-pack failed: {}",
        String::from_utf8_lossy(&index_out.stderr)
    );
    let loose =
        dir.join(".git/objects")
            .join(format!("{}/{}", &oid.to_hex()[..2], &oid.to_hex()[2..]));
    if loose.is_file() {
        std::fs::remove_file(loose).expect("remove loose after pack");
    }
}

fn write_ref_loose(dir: &std::path::Path, ref_name: &str, oid: ObjectId) {
    let path = dir.join(".git").join(ref_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("ref parent");
    }
    std::fs::write(path, format!("{}\n", oid.to_hex())).expect("write ref");
}

fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    (dir, repo)
}

fn one_commit(repo: &Repository) -> ObjectId {
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("f.txt"), b"hello\n").expect("write");
    stage_for_commit(repo, &mut NullProgress).expect("stage");
    let ident = "T <t@example.com> 1 +0000".to_string();
    create_commit(
        repo,
        &CommitRequest {
            message: "c1".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
        },
        &mut NullProgress,
    )
    .expect("commit")
    .oid
}

fn grit_fsck(repo: &Repository, opts: &FsckOptions) -> fsck::FsckReport {
    fsck::fsck(repo, opts, &mut NullProgress, &NeverCancel).expect("grit fsck")
}

fn git_dangling_set(root: &std::path::Path) -> Vec<(ObjectKind, ObjectId)> {
    let text = git(root, &["fsck"]);
    parse_dangling_lines(&text)
}

fn parse_dangling_lines(text: &str) -> Vec<(ObjectKind, ObjectId)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line
            .trim()
            .strip_prefix("dangling ")
            .or_else(|| line.trim().strip_prefix("notice: dangling "))
        else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let kind = match parts.next() {
            Some("commit") => ObjectKind::Commit,
            Some("blob") => ObjectKind::Blob,
            Some("tree") => ObjectKind::Tree,
            Some("tag") => ObjectKind::Tag,
            _ => continue,
        };
        let Some(hex) = parts.next() else {
            continue;
        };
        if let Ok(oid) = ObjectId::from_hex(hex) {
            out.push((kind, oid));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn grit_dangling_set(report: &fsck::FsckReport) -> Vec<(ObjectKind, ObjectId)> {
    let mut v = report.dangling.clone();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

fn has_msg_id(report: &fsck::FsckReport, id: &str) -> bool {
    report.issues.iter().any(|i| fsck::issue_msg_id(i) == id)
}

#[test]
fn clean_repo_has_no_issues_and_git_agrees() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let git_out = git_fsck(dir.path(), true);
    assert!(git_out.msg_ids.is_empty(), "{:?}", git_out.msg_ids);
    let config = repo.config().expect("config");
    let report = grit_fsck(&repo, &FsckOptions::full_repository(true));
    assert!(report.ok(&config, &FsckOptions::full_repository(true)));
    assert!(report.issues.is_empty());
}

#[test]
fn fsck_passes_on_deltified_pack_after_many_commits() {
    let (dir, repo) = init_repo();
    for i in 0..50 {
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join(format!("file-{i}.txt")), format!("payload {i}\n")).expect("write");
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: format!("commit {i}"),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
            },
            &mut NullProgress,
        )
        .expect("commit");
    }
    git(dir.path(), &["repack", "-adf"]);
    let git_out = git_fsck(dir.path(), false);
    assert!(
        git_out.msg_ids.is_empty(),
        "git fsck must pass on deltified pack: {:?}",
        git_out.msg_ids
    );
    let config = repo.config().expect("config");
    let opts = FsckOptions::full_repository(false);
    let report = grit_fsck(&repo, &opts);
    assert!(
        report.ok(&config, &opts),
        "grit fsck must verify deltified pack objects: {:?}",
        report.issues
    );
}

fn first_pack_idx(dir: &std::path::Path) -> std::path::PathBuf {
    let pack_dir = dir.join(".git/objects/pack");
    fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("pack index")
}

#[test]
fn fsck_reports_corrupt_deltified_object_and_still_checks_pack() {
    let (dir, repo) = init_repo();
    for i in 0..50 {
        let wt = repo.work_tree().expect("wt");
        std::fs::write(wt.join(format!("file-{i}.txt")), format!("payload {i}\n")).expect("write");
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: format!("commit {i}"),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
            },
            &mut NullProgress,
        )
        .expect("commit");
    }
    git(dir.path(), &["repack", "-adf"]);

    let idx_path = first_pack_idx(dir.path());
    let records = verify_pack_and_collect(&idx_path).expect("verify-pack records");
    let delta_bases: std::collections::HashSet<u64> =
        records.iter().filter_map(|r| r.base_pack_offset).collect();
    let delta = records
        .iter()
        .find(|r| r.packed_type == PackedType::OfsDelta && !delta_bases.contains(&r.offset))
        .expect("repacked repo must contain leaf ofs-delta objects");
    let bad_oid = ObjectId::from_bytes(&delta.oid).expect("delta oid");
    let pack_path = idx_path.with_extension("pack");
    let mut pack_bytes = fs::read(&pack_path).expect("read pack");
    let corrupt_at = delta.offset as usize + delta.size_in_pack as usize - 4;
    pack_bytes[corrupt_at] ^= 0x55;
    let mut perms = fs::metadata(&pack_path)
        .expect("pack metadata")
        .permissions();
    perms.set_readonly(false);
    fs::set_permissions(&pack_path, perms).expect("pack writable");
    fs::write(&pack_path, pack_bytes).expect("corrupt delta payload");

    let git_text = git(dir.path(), &["fsck", "--no-dangling"]);
    assert!(
        git_text.contains(&bad_oid.to_hex()),
        "git fsck must name corrupt object: {git_text}"
    );
    assert!(
        git_text.to_lowercase().contains("corrupt")
            || git_text.contains("cannot unpack")
            || git_text.contains("CRC mismatch")
            || git_text.contains("badObject")
            || git_text.contains("bad object"),
        "git fsck must report corruption: {git_text}"
    );

    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    let corrupt_for_oid = report.issues.iter().any(|issue| {
        matches!(
            issue,
            FsckIssue::BadObject { oid, .. }
                if *oid == bad_oid && fsck::issue_msg_id(issue) == "corruptObject"
        )
    });
    assert!(
        corrupt_for_oid,
        "grit fsck must report corruptObject for corrupt delta {}; issues: {:?}",
        bad_oid.to_hex(),
        report.issues
    );
    assert!(
        report.objects_checked > 1,
        "fsck must continue checking other pack objects"
    );
}

#[test]
fn nested_annotated_tags_pass_fsck_with_cached_links() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    git(dir.path(), &["tag", "-a", "inner", "-m", "inner"]);
    git(dir.path(), &["tag", "-a", "outer", "inner", "-m", "outer"]);
    let git_out = git_fsck(dir.path(), false);
    assert!(git_out.msg_ids.is_empty(), "{:?}", git_out.msg_ids);
    let opts = FsckOptions::full_repository(false).with_check_dangling(false);
    let config = repo.config().expect("config");
    let report = grit_fsck(&repo, &opts);
    assert!(
        report.ok(&config, &opts),
        "nested tag chain must not produce brokenLink: {:?}",
        report.issues
    );
    assert!(!has_msg_id(&report, "brokenLink"));
}

#[test]
fn tree_blob_mode_pointing_at_tag_is_broken_link() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    git(dir.path(), &["tag", "-a", "t", "-m", "t"]);
    let tag_oid_hex = git(dir.path(), &["rev-parse", "refs/tags/t"]);
    let tag_oid = ObjectId::from_hex(tag_oid_hex.trim()).expect("tag oid");
    let objects = dir.path().join(".git/objects");
    let mut tree_body = b"100644 wrong-name\0".to_vec();
    tree_body.extend_from_slice(tag_oid.as_bytes());
    let tree_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("tree oid");
    let parent_hex = git(dir.path(), &["rev-parse", "HEAD"]);
    let parent = ObjectId::from_hex(parent_hex.trim()).expect("parent");
    let commit_body = format!(
        "tree {}\nparent {}\nauthor T <t@example.com> 2 +0000\ncommitter T <t@example.com> 2 +0000\n\nlink tag as blob\n",
        tree_oid.to_hex(),
        parent.to_hex()
    );
    let commit_hex = write_loose_object(
        &objects,
        FixtureAlgo::Sha1,
        "commit",
        commit_body.as_bytes(),
    )
    .expect("commit");
    let bad_commit: ObjectId = commit_hex.parse().expect("bad commit");
    write_ref_loose(dir.path(), "refs/heads/main", bad_commit);
    pack_loose_object_and_remove_loose(dir.path(), tag_oid);
    pack_loose_object_and_remove_loose(dir.path(), tree_oid);
    pack_loose_object_and_remove_loose(dir.path(), bad_commit);
    repo.odb().invalidate_packs();
    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("brokenLink") || git_text.to_lowercase().contains("tag"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        has_msg_id(&report, "brokenLink"),
        "blob mode for tag object via cached links: {:?}",
        report.issues
    );
}

#[test]
fn missing_blob_reported_as_missing_object() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("orphan.txt"), b"x").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"orphan.txt"))
        .expect("entry")
        .oid;
    let loose = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(oid, _)| *oid == staged)
        .expect("path");
    std::fs::remove_file(loose.1).expect("rm");

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("missing") && git_text.contains(&staged.to_hex()),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(has_msg_id(&report, "missingObject"), "{:?}", report.issues);
    let missing_count = report
        .issues
        .iter()
        .filter(|i| fsck::issue_msg_id(i) == "missingObject")
        .count();
    assert_eq!(missing_count, 1, "{:?}", report.issues);
}

#[test]
fn gitmodules_invalid_path_matches_git() {
    let (dir, repo) = init_repo();
    let objects = dir.path().join(".git/objects");
    let bad = b"[submodule \"x\"]\n\tpath = -bad\n\turl = https://example.com\n";
    let blob_hex = write_loose_object(&objects, FixtureAlgo::Sha1, "blob", bad).expect("blob");
    let blob_oid: ObjectId = blob_hex.parse().expect("parse");
    let mut tree_body = b"100644 .gitmodules\0".to_vec();
    tree_body.extend_from_slice(blob_oid.as_bytes());
    let tree_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("parse");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n",
        tree_oid.to_hex()
    );
    let commit_hex = write_loose_object(
        &objects,
        FixtureAlgo::Sha1,
        "commit",
        commit_body.as_bytes(),
    )
    .expect("commit");
    let commit_oid: ObjectId = commit_hex.parse().expect("parse");
    write_ref_loose(dir.path(), "refs/heads/main", commit_oid);

    let git_out = git_fsck(dir.path(), true);
    assert!(
        git_out.msg_ids.iter().any(|id| id == "gitmodulesPath"),
        "{:?}",
        git_out.msg_ids
    );
    let config = repo.config().expect("config");
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(has_msg_id(&report, "gitmodulesPath"), "{:?}", report.issues);
    assert!(!report.ok(&config, &FsckOptions::full_repository(false)));
}

#[test]
fn missing_author_still_reaches_tree_without_duplicate_or_false_dangling() {
    let (dir, repo) = init_repo();
    let objects = dir.path().join(".git/objects");
    let blob_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "blob", b"payload\n").expect("blob");
    let blob_oid: ObjectId = blob_hex.parse().expect("parse");
    let mut tree_body = b"100644 file\0".to_vec();
    tree_body.extend_from_slice(blob_oid.as_bytes());
    let tree_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("parse");
    let commit_body = format!(
        "tree {}\ncommitter T <t@example.com> 1 +0000\n\n\n",
        tree_oid.to_hex()
    );
    let commit_hex = write_loose_object(
        &objects,
        FixtureAlgo::Sha1,
        "commit",
        commit_body.as_bytes(),
    )
    .expect("commit");
    let commit_oid: ObjectId = commit_hex.parse().expect("parse");
    write_ref_loose(dir.path(), "refs/heads/main", commit_oid);

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("missingAuthor"),
        "git fsck should report missingAuthor: {git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    let author_count = report
        .issues
        .iter()
        .filter(|i| fsck::issue_msg_id(i) == "missingAuthor")
        .count();
    assert_eq!(author_count, 1, "{:?}", report.issues);
    assert!(
        !report
            .dangling
            .iter()
            .any(|(_, oid)| *oid == blob_oid || *oid == tree_oid),
        "tree and blob must stay reachable: {:?}",
        report.dangling
    );
}

#[test]
fn corrupt_loose_object() {
    let (dir, repo) = init_repo();
    let blob = one_commit(&repo);
    let _ = blob;
    let loose = repo
        .odb()
        .enumerate_local_loose_objects()
        .expect("loose")
        .into_iter()
        .find(|(_, p)| p.to_string_lossy().contains("/objects/"))
        .expect("any loose");
    let mut perms = std::fs::metadata(&loose.1).expect("meta").permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&loose.1, perms).expect("chmod");
    let mut bytes = std::fs::read(&loose.1).expect("read");
    if bytes.len() > 10 {
        bytes[10] ^= 0xff;
    }
    std::fs::write(&loose.1, &bytes).expect("corrupt");

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("corrupt") || git_text.contains("badObject"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report.issues.iter().any(|i| {
        matches!(
            i,
            FsckIssue::CorruptLoose { .. } | FsckIssue::BadObject { .. }
        )
    }));
}

#[test]
fn hash_mismatch() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let odb = repo.odb();
    let body = b"payload-for-mismatch";
    let real_oid = odb.hash(ObjectKind::Blob, body);
    let decoy_oid = odb.hash(ObjectKind::Blob, b"decoy");
    let real_path = odb.object_path(&real_oid);
    write_loose_object(
        &dir.path().join(".git/objects"),
        FixtureAlgo::Sha1,
        "blob",
        body,
    )
    .expect("write");
    let decoy_path = odb.object_path(&decoy_oid);
    std::fs::create_dir_all(decoy_path.parent().expect("pfx")).expect("mkdir");
    std::fs::copy(&real_path, &decoy_path).expect("copy");
    let _ = std::fs::remove_file(&real_path);

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("hasSha1")
            || git_text.to_lowercase().contains("hash")
            || git_text.to_lowercase().contains("mismatch"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i, FsckIssue::HashMismatch { .. })));
}

#[test]
fn idx_oid_content_mismatch_returns_fsck_report_not_error() {
    let (dir, repo) = init_repo();
    let head = one_commit(&repo);
    git(dir.path(), &["repack", "-a", "-d"]);
    let pack_dir = dir.path().join(".git/objects/pack");
    let idx_path = fs::read_dir(&pack_dir)
        .expect("rd")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "idx"))
        .expect("idx");
    let pack_path = idx_path.with_extension("pack");
    let idx = read_pack_index(&idx_path).expect("read idx");
    let pack_bytes = fs::read(&pack_path).expect("read pack");
    let trailer = &pack_bytes[pack_bytes.len().saturating_sub(20)..];
    let mut rows: Vec<(ObjectId, u64, u32)> = idx
        .iter()
        .map(|e| {
            (
                ObjectId::from_bytes(e.oid()).expect("oid"),
                e.offset(),
                e.crc32().unwrap_or(0),
            )
        })
        .collect();
    let pos = rows
        .iter()
        .position(|(oid, _, _)| *oid == head)
        .expect("head commit in pack index");
    let mut bogus_bytes: [u8; 20] = head.as_bytes().try_into().expect("sha1 oid");
    bogus_bytes[19] ^= 0x01;
    let bogus_oid = ObjectId::from_bytes(&bogus_bytes).expect("bogus oid");
    assert_eq!(bogus_bytes[0], head.as_bytes()[0]);
    rows[pos].0 = bogus_oid;
    rows.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for path in [&idx_path, &pack_path] {
        let mut perms = fs::metadata(path).expect("meta").permissions();
        perms.set_readonly(false);
        fs::set_permissions(path, perms).expect("chmod");
    }
    write_v2_pack_index_with_trailer(&idx_path, &rows, trailer, 20, None).expect("rewrite idx");
    repo.odb().invalidate_packs();

    let git_text = git(dir.path(), &["fsck", "--no-dangling"]);
    assert!(
        git_text.to_lowercase().contains("corrupt")
            || git_text.contains("badPack")
            || git_text.contains("missing"),
        "{git_text}"
    );
    let opts = FsckOptions::full_repository(false).with_check_dangling(false);
    let report = grit_fsck(&repo, &opts);
    assert!(
        has_msg_id(&report, "badPackChecksum") || has_msg_id(&report, "missingObject"),
        "expected pack or missing integrity issues: {:?}",
        report.issues
    );
    assert!(
        report.issues.iter().any(|i| matches!(
            i,
            FsckIssue::PackChecksum { .. } | FsckIssue::MissingObject { .. }
        )),
        "{:?}",
        report.issues
    );
}

#[test]
fn truncated_pack_checksum() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    git(dir.path(), &["repack", "-a", "-d"]);
    let pack_dir = dir.path().join(".git/objects/pack");
    let pack = fs::read_dir(&pack_dir)
        .expect("rd")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack");
    let mut perms = std::fs::metadata(&pack).expect("meta").permissions();
    perms.set_readonly(false);
    std::fs::set_permissions(&pack, perms).expect("chmod");
    let mut bytes = std::fs::read(&pack).expect("read pack");
    bytes.truncate(bytes.len().saturating_sub(8));
    std::fs::write(&pack, &bytes).expect("truncate");

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("checksum")
            || git_text.contains("badPack")
            || git_text.contains("does not match index"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report.issues.iter().any(|i| {
        matches!(
            i,
            FsckIssue::PackChecksum { .. } | FsckIssue::IdxMismatch { .. }
        )
    }));
}

#[test]
fn dangling_commit_and_blob() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let mut child = Command::new("git")
        .current_dir(dir.path())
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(b"dangling blob body\n");
    }
    child.wait().expect("wait");
    // Dangling commit
    let tree = git(dir.path(), &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();
    let commit_cmd = Command::new("git")
        .current_dir(dir.path())
        .args(["commit-tree", &tree, "-m", "dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("commit-tree");
    assert!(commit_cmd.status.success());

    let git_set = git_dangling_set(dir.path());
    let report = grit_fsck(
        &repo,
        &FsckOptions::full_repository(false).with_check_dangling(true),
    );
    let grit_set = grit_dangling_set(&report);
    assert_eq!(
        grit_set, git_set,
        "dangling mismatch git={git_set:?} grit={grit_set:?}"
    );
}

#[test]
fn malformed_reachable_commit_is_bad_object_not_error() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let bad_hex = git_hash_object_literally(
        dir.path(),
        FixtureAlgo::Sha1,
        "commit",
        b"this is not a commit\n",
    )
    .expect("literally write commit");
    let bad = ObjectId::from_hex(bad_hex.trim()).expect("parse oid");
    write_ref_loose(dir.path(), "refs/heads/main", bad);
    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("could not be parsed")
            || git_text.contains("bogus")
            || git_text.contains("badObject"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i, FsckIssue::BadObject { oid, .. } if oid == &bad)),
        "malformed commit must surface as BadObject: {:?}",
        report.issues
    );
}

#[test]
fn malformed_reachable_tree_is_bad_object_not_error() {
    let (dir, repo) = init_repo();
    let head = one_commit(&repo);
    let bad_tree = repo
        .odb()
        .write(ObjectKind::Tree, b"not a valid tree")
        .expect("tree");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
        bad_tree.to_hex()
    );
    let bad_commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    let _ = head;
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &bad_commit.to_hex()],
    );
    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("could not be parsed")
            || git_text.contains("badTree")
            || git_text.contains("bogus"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        report
            .issues
            .iter()
            .any(|i| { matches!(i, FsckIssue::BadObject { oid, .. } if oid == &bad_tree) }),
        "malformed tree must surface as BadObject: {:?}",
        report.issues
    );
}

#[test]
fn strict_fsck_honors_ignore_config_for_object_diagnostics() {
    use std::io::Write;
    let (dir, repo) = init_repo();
    let head = one_commit(&repo);
    let tag_body = format!(
        "object {}\ntype commit\ntag missing-tagger\n\n",
        head.to_hex()
    );
    let tag_oid = repo
        .odb()
        .write(ObjectKind::Tag, tag_body.as_bytes())
        .expect("tag");
    write_ref_loose(dir.path(), "refs/tags/missing-tagger", tag_oid);
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join(".git/config"))
        .expect("open config")
        .write_all(b"\n[fsck]\n\tmissingTaggerEntry = ignore\n")
        .expect("config");

    let git_text = git(
        dir.path(),
        &["-c", "fsck.missingTaggerEntry=ignore", "fsck", "--strict"],
    );
    assert!(
        !git_text.contains("missingTaggerEntry"),
        "git must suppress ignored msg-id even in strict mode: {git_text}"
    );

    let report = grit_fsck(&repo, &FsckOptions::full_repository(true));
    assert!(
        !has_msg_id(&report, "missingTaggerEntry"),
        "ignored msg-id must not appear: {:?}",
        report.issues
    );
}

#[test]
fn malformed_reachable_tag_is_bad_object_not_error() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let bad_hex =
        git_hash_object_literally(dir.path(), FixtureAlgo::Sha1, "tag", b"not a tag object\n")
            .expect("literally write tag");
    let bad_tag = ObjectId::from_hex(bad_hex.trim()).expect("parse oid");
    write_ref_loose(dir.path(), "refs/tags/bad-tag", bad_tag);
    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("could not be parsed")
            || git_text.contains("badTag")
            || git_text.contains("bogus"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i, FsckIssue::BadObject { oid, .. } if oid == &bad_tag)),
        "malformed tag must surface as BadObject: {:?}",
        report.issues
    );
}

#[test]
fn malformed_unreachable_commit_in_pack_is_bad_object() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let bad_hex = git_hash_object_literally(
        dir.path(),
        FixtureAlgo::Sha1,
        "commit",
        b"this is not a commit\n",
    )
    .expect("literally write commit");
    let bad = ObjectId::from_hex(bad_hex.trim()).expect("parse oid");
    pack_loose_object_and_remove_loose(dir.path(), bad);
    repo.odb().invalidate_packs();
    assert!(
        !repo
            .odb()
            .enumerate_local_loose_objects()
            .expect("loose")
            .iter()
            .any(|(oid, _)| *oid == bad),
        "malformed commit should exist only in pack"
    );
    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.to_lowercase().contains("could not be parsed")
            || git_text.contains("bogus")
            || git_text.contains("badObject"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        report
            .issues
            .iter()
            .any(|i| matches!(i, FsckIssue::BadObject { oid, .. } if oid == &bad)),
        "packed unreachable malformed commit must be reported: {:?}",
        report.issues
    );
}

#[test]
fn gitlink_to_absent_commit_not_missing_object() {
    let (dir, repo) = init_repo();
    let head = one_commit(&repo);
    let absent = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("hex");
    let mut tree_body = Vec::from(b"160000 sub\0" as &[u8]);
    tree_body.extend_from_slice(absent.as_bytes());
    let tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_body)
        .expect("tree");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
        tree.to_hex()
    );
    let commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    let _ = head;
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &commit.to_hex()],
    );

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        !git_text.contains(&absent.to_hex()),
        "git must not report absent gitlink oid: {git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        !report
            .issues
            .iter()
            .any(|i| matches!(i, FsckIssue::MissingObject { oid, .. } if oid == &absent)),
        "gitlink must not require submodule commit in superproject ODB: {:?}",
        report.issues
    );
}

#[test]
fn commit_tree_wrong_kind_matches_git() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"not-a-tree")
        .expect("blob");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
        blob.to_hex()
    );
    let bad_commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &bad_commit.to_hex()],
    );

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("not a tree") || git_text.contains("bad tree"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        report.issues.iter().any(|i| {
            matches!(
                i,
                FsckIssue::BrokenLink { target, .. } if target == &blob
            ) || matches!(i, FsckIssue::BadObject { .. })
        }),
        "wrong tree kind must be reported: {:?}",
        report.issues
    );
}

#[test]
fn broken_link_from_tree() {
    let (dir, repo) = init_repo();
    let head = one_commit(&repo);
    let mut tree_body = Vec::from(b"100644 link\0" as &[u8]);
    tree_body.extend_from_slice(head.as_bytes());
    let new_tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_body)
        .expect("tree");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\n",
        new_tree.to_hex()
    );
    let bad_commit = repo
        .odb()
        .write(ObjectKind::Commit, commit_body.as_bytes())
        .expect("commit");
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &bad_commit.to_hex()],
    );

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("brokenLink")
            || git_text.contains("missing")
            || git_text.to_lowercase().contains("broken link"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report.issues.iter().any(|i| matches!(
        i,
        FsckIssue::BrokenLink { .. } | FsckIssue::MissingObject { .. }
    )));
}

#[test]
fn bad_ref_name() {
    let (dir, repo) = init_repo();
    let tip = one_commit(&repo);
    std::fs::write(
        dir.path().join(".git/refs/heads/bad ref"),
        format!("{}\n", tip.to_hex()).as_bytes(),
    )
    .expect("write ref");

    let git_text = git(dir.path(), &["fsck"]);
    assert!(
        git_text.contains("badRefName")
            || git_text.contains("badRefContent")
            || git_text.contains("invalid sha1 pointer")
            || git_text.contains("bad ref"),
        "{git_text}"
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i, FsckIssue::BadRef(_))));
}

#[test]
fn reflog_and_index_roots_are_not_dangling() {
    let (dir, repo) = init_repo();
    one_commit(&repo);
    git(dir.path(), &["commit", "--allow-empty", "-m", "e2"]);
    git(dir.path(), &["reset", "--hard", "HEAD~1"]);
    std::fs::write(repo.work_tree().expect("wt").join("idx-only"), b"z").expect("write");
    stage_for_commit(&repo, &mut NullProgress).expect("stage");

    let git_set = git_dangling_set(dir.path());
    let report = grit_fsck(
        &repo,
        &FsckOptions::full_repository(false).with_check_dangling(true),
    );
    let index = Index::load(&repo.git_dir().join("index")).expect("index");
    let staged = index
        .entries()
        .iter()
        .find(|e| e.path.ends_with(b"idx-only"))
        .expect("entry")
        .oid;
    assert!(
        !report.dangling.iter().any(|(_, oid)| *oid == staged),
        "index-only blob must not be dangling"
    );
    let reflog_out = git(dir.path(), &["rev-parse", "HEAD@{1}"])
        .trim()
        .to_string();
    if let Ok(reflog_oid) = ObjectId::from_hex(&reflog_out) {
        assert!(
            !report.dangling.iter().any(|(_, oid)| *oid == reflog_oid),
            "reflog commit must not be dangling"
        );
    }
    let grit_set = grit_dangling_set(&report);
    assert_eq!(grit_set, git_set);
}

#[test]
fn linked_worktree_head_reflog_roots_match_git_fsck() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let main = root.join("main");
    std::fs::create_dir_all(&main).expect("main dir");
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("f.txt"), b"seed\n").expect("write");
    git(&main, &["add", "f.txt"]);
    git(&main, &["commit", "-qm", "seed"]);
    let base = git(&main, &["rev-parse", "HEAD"]).trim().to_string();

    let linked = root.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().expect("linked path"),
            "HEAD",
        ],
    );
    std::fs::write(linked.join("only-linked.txt"), b"x\n").expect("write");
    git(&linked, &["add", "only-linked.txt"]);
    git(&linked, &["commit", "-qm", "linked-only"]);
    let detached_commit = git(&linked, &["rev-parse", "HEAD"]).trim().to_string();
    git(&linked, &["checkout", "--detach", &base]);

    let git_set = git_dangling_set(&main);
    assert!(
        !git_set
            .iter()
            .any(|(_, oid)| oid.to_hex() == detached_commit),
        "system git must retain linked-worktree reflog commit: {git_set:?}"
    );
    let git_no_reflog = parse_dangling_lines(&git(&main, &["fsck", "--no-reflogs"]));
    assert!(
        git_no_reflog
            .iter()
            .any(|(_, oid)| oid.to_hex() == detached_commit),
        "git --no-reflogs should report detached commit as dangling"
    );

    let repo = Repository::discover(
        Some(main.as_path()),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )
    .expect("discover");
    let report = grit_fsck(
        &repo,
        &FsckOptions::full_repository(false).with_check_dangling(true),
    );
    assert!(
        !report
            .dangling
            .iter()
            .any(|(_, oid)| oid.to_hex() == detached_commit),
        "linked worktree HEAD reflog must anchor reachability"
    );
    let grit_set = grit_dangling_set(&report);
    assert_eq!(
        grit_set, git_set,
        "dangling mismatch git={git_set:?} grit={grit_set:?}"
    );
}

/// Regression for issue #1037: tree dot-special checks must not panic on UTF-8 names
/// whose multibyte character starts at byte index 6.
#[test]
fn fsck_commit_with_multibyte_name_at_byte_six() {
    let (dir, repo) = init_repo();
    let wt = repo.work_tree().expect("wt");
    std::fs::write(wt.join("abcde日.txt"), b"x\n").expect("write");
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
        },
        &mut NullProgress,
    )
    .expect("commit");

    let git_out = git_fsck(dir.path(), true);
    assert!(
        git_out.msg_ids.is_empty(),
        "git fsck --strict: {:?}",
        git_out.msg_ids
    );

    let report = grit_fsck(&repo, &FsckOptions::full_repository(true));
    assert!(
        report.issues.is_empty(),
        "grit fsck issues: {:?}",
        report.issues
    );
}

/// NTFS DOS 8.3 alias `gitmod~1` must be treated like `.gitmodules` (Git `fsck`).
#[test]
fn gitmod_dos83_symlink_matches_git() {
    let (dir, repo) = init_repo();
    let objects = dir.path().join(".git/objects");
    let blob_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "blob", b"target\n").expect("blob");
    let blob_oid: ObjectId = blob_hex.parse().expect("parse");
    let mut tree_body = b"120000 gitmod~1\0".to_vec();
    tree_body.extend_from_slice(blob_oid.as_bytes());
    let tree_hex =
        write_loose_object(&objects, FixtureAlgo::Sha1, "tree", &tree_body).expect("tree");
    let tree_oid: ObjectId = tree_hex.parse().expect("parse");
    let commit_body = format!(
        "tree {}\nauthor T <t@example.com> 1 +0000\ncommitter T <t@example.com> 1 +0000\n\nm\n",
        tree_oid.to_hex()
    );
    let commit_hex = write_loose_object(
        &objects,
        FixtureAlgo::Sha1,
        "commit",
        commit_body.as_bytes(),
    )
    .expect("commit");
    let commit_oid: ObjectId = commit_hex.parse().expect("parse");
    write_ref_loose(dir.path(), "refs/heads/main", commit_oid);

    let git_out = git_fsck(dir.path(), true);
    assert!(
        git_out.msg_ids.iter().any(|id| id == "gitmodulesSymlink"),
        "git fsck: {:?}",
        git_out.msg_ids
    );
    let report = grit_fsck(&repo, &FsckOptions::full_repository(false));
    assert!(
        has_msg_id(&report, "gitmodulesSymlink"),
        "grit fsck: {:?}",
        report.issues
    );
}
