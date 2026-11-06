//! Local clone must link repacked packfiles instead of rebuilding them.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::pack::read_pack_index_no_verify;
use grit_lib::progress::CancelToken;
use grit_lib::progress::{ProgressPhase, ProgressSink};
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pack_basenames(git_dir: &Path) -> Vec<String> {
    let pack_dir = git_dir.join("objects/pack");
    let mut names: Vec<String> = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".pack"))
        .collect();
    names.sort();
    names
}

fn index_object_count(idx_path: &Path) -> usize {
    read_pack_index_no_verify(idx_path).expect("read idx").len()
}

#[derive(Default)]
struct CopyProgress {
    phase: Option<ProgressPhase>,
    count: u64,
    completed: Vec<u64>,
}

impl grit_lib::progress::ProgressSink for CopyProgress {}

impl ProgressSink for CopyProgress {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.phase = Some(phase);
        self.count = 0;
    }

    fn inc(&mut self, units: u64) {
        self.count += units;
    }

    fn set(&mut self, count: u64) {
        self.count = count;
    }

    fn finish(&mut self) {
        if self.phase.take() == Some(ProgressPhase::CopyingObjects) {
            self.completed.push(self.count);
        }
    }
}

#[test]
fn clone_local_loose_source_links_blobs_and_reports_all_kinds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let source = tmp.path().join("source");
    std::fs::create_dir(&source).expect("source");
    git(&source, &["init", "-q", "-b", "main"]);
    let payload = vec![0u8; 64 * 1024];
    std::fs::write(source.join("blob.bin"), &payload).expect("blob");
    git(&source, &["add", "blob.bin"]);
    git(&source, &["commit", "-qm", "loose blob"]);
    assert!(pack_basenames(&source.join(".git")).is_empty());

    for depth in [None, Some(1)] {
        let dest = tmp.path().join(format!("dest-{depth:?}"));
        let mut options = CloneOptions::new(source.to_string_lossy(), dest.clone());
        options.depth = depth;
        let mut progress = CopyProgress::default();
        let outcome = clone_repo(options, &mut progress, &CancelToken::never(), None)
            .expect("clone loose source");
        assert_eq!(outcome.transfer.objects, 3);
        assert_eq!(progress.completed.last(), Some(&3));
        let kinds = outcome.transfer.kinds.expect("object kinds");
        assert_eq!(kinds.blobs, 1);
        assert_eq!(kinds.trees, 1);
        assert_eq!(kinds.commits, 1);
        assert_eq!(kinds.tags, 0);
        assert_eq!(
            std::fs::read(dest.join("blob.bin")).expect("checkout"),
            payload
        );
        assert!(pack_basenames(&dest.join(".git")).is_empty());
        git(&dest, &["fsck", "--strict"]);
    }
}

#[test]
fn clone_local_repacked_source_links_pack_and_reports_index_stats() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    for i in 0..48 {
        std::fs::write(remote.join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&remote, &["repack", "-a", "-d", "-f"]);

    let remote_git = remote.join(".git");
    let remote_packs = pack_basenames(&remote_git);
    assert_eq!(remote_packs.len(), 1, "fixture needs one pack");
    let remote_idx = remote_git
        .join("objects/pack")
        .join(remote_packs[0].replace(".pack", ".idx"));
    let expected_objects = index_object_count(&remote_idx);

    let dest = tmp.path().join("dest");
    let outcome = clone_repo(
        CloneOptions {
            ..CloneOptions::new(remote.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone");

    assert_eq!(
        outcome.transfer.objects,
        u64::try_from(expected_objects).expect("object count"),
        "transfer stats must come from the linked pack index, not a full verify pass"
    );

    let clone_git = dest.join(".git");
    let local_packs = pack_basenames(&clone_git);
    assert_eq!(local_packs.len(), 1, "clone must install one linked pack");
    assert_eq!(
        local_packs[0], remote_packs[0],
        "linked pack must reuse the remote pack basename"
    );

    git(&dest, &["fsck", "--strict"]);
}

#[test]
fn clone_local_repacked_plus_unreachable_loose_still_links_pack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    for i in 0..48 {
        std::fs::write(remote.join(format!("f{i}.txt")), format!("payload {i}\n")).unwrap();
        git(&remote, &["add", "."]);
        git(&remote, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&remote, &["repack", "-a", "-d", "-f"]);
    let remote_git = remote.join(".git");
    let mut hash = Command::new("git")
        .current_dir(&remote)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .spawn()
        .expect("hash-object");
    if let Some(mut stdin) = hash.stdin.take() {
        use std::io::Write;
        stdin.write_all(b"x\n").unwrap();
        drop(stdin);
    }
    let out = hash.wait_with_output().expect("hash-object wait");
    assert!(out.status.success());
    let loose_hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(loose_hex.len(), 40, "hash-object must return an oid");
    let shard = &loose_hex[..2];
    let rest = &loose_hex[2..];
    assert!(
        remote_git.join("objects").join(shard).join(rest).is_file(),
        "unreachable loose blob must exist in source"
    );

    let remote_packs = pack_basenames(&remote_git);
    assert_eq!(remote_packs.len(), 1);

    let dest = tmp.path().join("dest");
    let outcome = clone_repo(
        CloneOptions {
            ..CloneOptions::new(remote.to_string_lossy(), dest.clone())
        },
        &mut NoProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone");

    let remote_idx = remote_git
        .join("objects/pack")
        .join(remote_packs[0].replace(".pack", ".idx"));
    let expected_objects = index_object_count(&remote_idx);
    assert_eq!(
        outcome.transfer.objects,
        u64::try_from(expected_objects).expect("object count"),
        "must not walk the full object graph when pack link suffices"
    );

    let clone_git = dest.join(".git");
    let local_packs = pack_basenames(&clone_git);
    assert_eq!(local_packs.len(), 1);
    assert_eq!(local_packs[0], remote_packs[0]);

    git(&dest, &["fsck", "--strict"]);
}
