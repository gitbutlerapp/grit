//! Issue #2508: three-way merge with many exact renames sharing basenames across directories.

use crate::support;

use std::fs;
use std::io::Write;
use std::path::Path;

use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{predict_merge, MergePrediction, MergePredictionOptions};
use grit_test_support::git;

use support::{git_merge_tree_oracle, git_version_at_least, open_grit_repo};

const DRIVERS: usize = 8;
const SUBS: usize = 4;
const FILES: usize = 25;
const MOVED_DRIVERS: usize = 4;

fn rel(driver: usize, sub: usize, file: usize) -> String {
    format!(
        "drivers{driver:03}/sub{sub:03}/impl/file{file:04}.c",
        driver = driver,
        sub = sub,
        file = file
    )
}

fn write_tree(root: &Path) {
    for d in 0..DRIVERS {
        for s in 0..SUBS {
            let dir = root.join(format!("drivers{d:03}/sub{s:03}/impl"));
            fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..FILES {
                fs::write(
                    dir.join(format!("file{f:04}.c")),
                    format!("/* {d}/{s}/{f} */\nint fn_{d}_{s}_{f}(int x) {{ return x; }}\n"),
                )
                .expect("write");
            }
        }
    }
}

/// Base commit, a branch that moves `MOVED_DRIVERS` driver trees under `arch/`, and a diverging main edit.
fn rename_fixture(root: &Path) -> (ObjectId, ObjectId, ObjectId) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);

    write_tree(root);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    let base: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("base");

    git(root, &["switch", "-c", "ren"]);
    fs::create_dir_all(root.join("arch")).expect("arch");
    for d in 0..MOVED_DRIVERS {
        let src = format!("drivers{d:03}");
        let dst = root.join("arch").join(format!("drivers{d:03}"));
        fs::rename(root.join(&src), dst).expect("mv driver");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "move"]);
    let ren: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("ren");

    git(root, &["switch", "main"]);
    let touch = rel(MOVED_DRIVERS, 0, 0);
    fs::OpenOptions::new()
        .append(true)
        .open(root.join(&touch))
        .expect("open")
        .write_all(b"// main\n")
        .expect("append");
    git(root, &["commit", "-qam", "other"]);
    let main: ObjectId = git(root, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    (base, main, ren)
}

#[test]
fn many_exact_renames_with_shared_basenames_matches_git_merge_tree() {
    if !git_version_at_least(2, 38) {
        eprintln!("SKIP: git merge-tree --write-tree requires git >= 2.38");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (base, main, ren) = rename_fixture(root);

    let oracle = git_merge_tree_oracle(root, Some(&base), &main, &ren, MergeFavor::None);
    let repo = open_grit_repo(root);
    let prediction =
        predict_merge(&repo, main, ren, &MergePredictionOptions::default()).expect("predict merge");
    let MergePrediction::Clean { tree } = prediction else {
        panic!("expected clean merge prediction, got {prediction:?}");
    };
    assert_eq!(
        tree,
        oracle.result_tree,
        "integrate tree must match git merge-tree for {} exact renames",
        MOVED_DRIVERS * SUBS * FILES
    );
}
