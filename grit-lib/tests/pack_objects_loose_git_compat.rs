//! Building a pack from a repository whose objects are all loose (the state after
//! `git unpack-objects`, many commits, or unpacking fetches) through the pack-objects
//! path that upload-pack, push, bundle and local clone use, against system `git`:
//! the pack must index under `git index-pack --strict`, deltify trees as well as
//! blobs, and stay close to the size of `git repack -adf` on the same objects.

use std::path::{Path, PathBuf};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::pack_objects::{build_pack, PackBuildOptions};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

const DIRS: usize = 30;
const FILES_PER_DIR: usize = 8;
const LINES_PER_FILE: usize = 40;

/// A `git fast-import` stream: `commits` revisions over a `DIRS` × `FILES_PER_DIR`
/// tree, each rewriting one line in three files spread over different directories,
/// so every commit also writes new subtrees and a new root tree.
fn history_stream(commits: usize) -> String {
    let file_count = DIRS * FILES_PER_DIR;
    let mut files: Vec<Vec<String>> = (0..file_count)
        .map(|f| {
            (0..LINES_PER_FILE)
                .map(|l| format!("file {f} line {l}: the quick brown fox jumps over the lazy dog"))
                .collect()
        })
        .collect();
    let path = |f: usize| format!("dir{:02}/file{}.txt", f / FILES_PER_DIR, f % FILES_PER_DIR);
    let mut stream = String::new();
    for n in 0..commits {
        let changed: Vec<usize> = if n == 0 {
            (0..file_count).collect()
        } else {
            (0..3).map(|k| (n * 37 + k * 101) % file_count).collect()
        };
        for &f in &changed {
            if n > 0 {
                let line = (n * 13 + f) % LINES_PER_FILE;
                files[f][line] = format!("file {f} line {line}: revised in commit {n}");
            }
        }
        let message = format!("commit {n}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\ncommitter T <t@example.com> {} +0000\ndata {}\n{message}",
            1_000_000_000 + n,
            message.len()
        ));
        for &f in &changed {
            let body = files[f].join("\n") + "\n";
            stream.push_str(&format!(
                "M 100644 inline {}\ndata {}\n{body}",
                path(f),
                body.len()
            ));
        }
        stream.push('\n');
    }
    stream
}

fn only_pack(git_dir: &Path) -> PathBuf {
    let packs: Vec<PathBuf> = std::fs::read_dir(git_dir.join("objects/pack"))
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    assert_eq!(packs.len(), 1, "{packs:?}");
    packs.into_iter().next().expect("one pack")
}

/// Import `stream` into a new repository at `dir`, then explode its pack into loose
/// objects so nothing is packed.
fn loose_repo(dir: &Path, stream: &str) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "gc.auto", "0"]);
    git_cmd(&["fast-import", "--quiet"])
        .in_dir(dir)
        .stdin(stream)
        .suc();
    let pack = only_pack(&dir.join(".git"));
    let bytes = std::fs::read(&pack).expect("read pack");
    std::fs::remove_file(&pack).expect("remove pack");
    std::fs::remove_file(pack.with_extension("idx")).expect("remove idx");
    git_cmd(&["unpack-objects", "-q"])
        .in_dir(dir)
        .stdin(bytes)
        .suc();
    git(dir, &["checkout", "-q", "main"]);
}

/// Delta counts per object type from `git verify-pack -v`.
fn delta_counts(dir: &Path, pack: &Path) -> (usize, usize, usize) {
    let listing = git(dir, &["verify-pack", "-v", &pack.to_string_lossy()]);
    let (mut trees, mut tree_deltas, mut blob_deltas) = (0, 0, 0);
    for line in listing.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 || fields[0].len() != 40 {
            continue;
        }
        let is_delta = fields.len() >= 7;
        match fields[1] {
            "tree" => {
                trees += 1;
                tree_deltas += usize::from(is_delta);
            }
            "blob" => blob_deltas += usize::from(is_delta),
            _ => {}
        }
    }
    (trees, tree_deltas, blob_deltas)
}

/// Build a pack of everything reachable from `main` with grit's pack-objects path and
/// index it with `git index-pack --strict`; returns the pack path.
fn grit_pack_of_main(dir: &Path, opts: &PackBuildOptions) -> PathBuf {
    let repo = Repository::open(&dir.join(".git"), &RepositoryOptions::empty()).expect("open");
    let tip = ObjectId::from_hex(git(dir, &["rev-parse", "main"]).trim()).expect("tip");
    let bytes = build_pack(repo.odb(), &[tip], &[], opts).expect("build pack");
    let pack = dir.join("grit-out.pack");
    std::fs::write(&pack, bytes).expect("write pack");
    git(dir, &["index-pack", "--strict", &pack.to_string_lossy()]);
    pack
}

#[test]
fn loose_history_pack_finds_tree_deltas_and_matches_git_pack_size() {
    let stream = history_stream(1200);
    let grit_dir = TempDir::new().expect("tempdir");
    let git_dir = TempDir::new().expect("tempdir");
    loose_repo(grit_dir.path(), &stream);
    loose_repo(git_dir.path(), &stream);

    git(git_dir.path(), &["repack", "-adfq"]);
    let git_pack = only_pack(&git_dir.path().join(".git"));
    let grit_pack = grit_pack_of_main(grit_dir.path(), &PackBuildOptions::for_local_copy(None));

    let (trees, tree_deltas, blob_deltas) = delta_counts(grit_dir.path(), &grit_pack);
    assert!(
        tree_deltas * 2 > trees,
        "most trees should be deltas: {tree_deltas} of {trees}"
    );
    assert!(blob_deltas > 1000, "blob deltas: {blob_deltas}");

    let grit_size = std::fs::metadata(&grit_pack).expect("grit pack").len();
    let git_size = std::fs::metadata(&git_pack).expect("git pack").len();
    assert!(
        grit_size * 10 <= git_size * 12,
        "grit pack {grit_size} bytes vs git repack -adf {git_size} bytes ({:.2}x)",
        grit_size as f64 / git_size as f64
    );
}

#[test]
fn served_pack_of_a_loose_history_holds_every_object_for_git() {
    let dir = TempDir::new().expect("tempdir");
    loose_repo(dir.path(), &history_stream(150));
    let pack = grit_pack_of_main(dir.path(), &PackBuildOptions::for_serve(None, false, true));

    let listed = git(dir.path(), &["verify-pack", "-v", &pack.to_string_lossy()]);
    let expected = git(dir.path(), &["rev-list", "--objects", "main"]);
    for line in expected.lines() {
        let oid = &line[..40];
        assert!(listed.contains(oid), "{oid} missing from the pack");
    }
}
