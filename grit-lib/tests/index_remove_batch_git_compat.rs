//! Index batch removal and replacement round-trip with system `git ls-files` / `fsck`.

use std::process::Command;

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{init_repository, Repository};
use grit_test_support::git;

fn ls_files_stage_lines(repo_dir: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["ls-files", "-s"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git ls-files -s");
    assert!(
        out.status.success(),
        "git ls-files -s failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn expected_ls_files_lines(entries: &[IndexEntry]) -> Vec<String> {
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
    sorted
        .into_iter()
        .map(|e| {
            format!(
                "{:o} {} {}\t{}",
                e.mode,
                e.oid.to_hex(),
                e.stage(),
                String::from_utf8_lossy(&e.path)
            )
        })
        .collect()
}

fn fsck_clean(repo_dir: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn entry_for_path(repo: &Repository, path: &str, stage: u8, payload: &[u8]) -> IndexEntry {
    let oid = repo
        .odb
        .write(ObjectKind::Blob, payload)
        .expect("write blob");
    let mut e = IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: payload.len() as u32,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    };
    e.flags = (e.flags & 0x0FFF) | ((stage as u16 & 0x3) << 12);
    e
}

#[test]
fn grit_index_batch_remove_replace_matches_git_ls_files_and_fsck() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
    let payload = b"index-compat-payload\n";

    let mut index = Index::new();
    const TOTAL: usize = 5_000;
    for i in 0..TOTAL {
        let path = format!("f/{i:04}.txt");
        if i % 37 == 0 {
            for stage in 1u8..=3 {
                index.add_or_replace(entry_for_path(&repo, &path, stage, payload));
            }
        } else {
            index.add_or_replace(entry_for_path(&repo, &path, 0, payload));
        }
    }

    let touch_count = TOTAL / 10;
    let mut remove_paths: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .take(touch_count)
        .map(|e| e.path.clone())
        .collect();
    remove_paths.sort();
    remove_paths.dedup();

    index.remove_paths(remove_paths.iter().map(|p| p.as_slice()));
    let replacement_oid: ObjectId = repo
        .odb
        .write(ObjectKind::Blob, b"replacement\n")
        .expect("replacement blob");
    for path in &remove_paths {
        let mut e = entry_for_path(
            &repo,
            std::str::from_utf8(path).unwrap(),
            0,
            b"replacement\n",
        );
        e.oid = replacement_oid;
        index.add_or_replace(e);
    }

    repo.write_index(&mut index).expect("write index");

    let git_lines = ls_files_stage_lines(tmp.path());
    let expected = expected_ls_files_lines(&index.entries);
    assert_eq!(git_lines, expected);

    git(tmp.path(), &["add", "-A"]);
    fsck_clean(tmp.path());
}
