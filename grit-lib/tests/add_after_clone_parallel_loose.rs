//! `grit add` on a clean tree after `grit clone` must not race on parallel loose writes (issue #1677).

use std::path::Path;

use grit_test_support::{git, grit_cmd, unique_tmp};

fn seed_medium_repo(root: &Path, file_count: usize) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..file_count {
        let dir = format!("d{:02}", i % 32);
        std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
        let name = format!("{dir}/file-{i:05}.txt");
        std::fs::write(root.join(&name), format!("payload {i}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "seed"]);
}

fn loose_object_count(repo: &Path) -> usize {
    let out = git(repo, &["count-objects", "-v"]);
    out.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? == "count:" {
                return parts.next()?.parse().ok();
            }
            None
        })
        .unwrap_or(0)
}

#[test]
fn add_after_grit_clone_clean_tree_no_loose_writes() {
    if !cfg!(unix) {
        return;
    }
    let upstream = unique_tmp("add-clone", "upstream");
    const FILES: usize = 2000;
    seed_medium_repo(&upstream, FILES);

    let source = upstream.to_str().expect("utf-8");
    for attempt in 1..=8 {
        let dest_path = unique_tmp("add-clone", &format!("dest-{attempt}"));
        grit_cmd(&[
            "clone",
            &format!("file://{source}"),
            dest_path.to_str().expect("utf-8"),
        ])
        .in_dir(&upstream)
        .suc();
        // A local clone hard-links the upstream's loose objects, so the clone starts with
        // as many loose objects as the upstream; `grit add` must not add any.
        let loose_after_clone = loose_object_count(&dest_path);
        assert_eq!(loose_after_clone, loose_object_count(&upstream));
        let add = grit_cmd(&["add"]).in_dir(&dest_path).exec();
        assert!(
            add.ok(),
            "grit add must succeed on clean clone (attempt {attempt}): {}",
            add.stderr
        );
        let loose = loose_object_count(&dest_path);
        assert_eq!(
            loose, loose_after_clone,
            "clean grit add after grit clone must not write loose objects (attempt {attempt})"
        );
    }
}
