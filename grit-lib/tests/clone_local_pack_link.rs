//! Local clone must link repacked packfiles instead of rebuilding them.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::pack::read_pack_index_no_verify;
use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

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
        &NeverCancel,
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
