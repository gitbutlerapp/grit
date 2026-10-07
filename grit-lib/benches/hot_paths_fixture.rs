//! In-process L/H worktree and index fixtures for hot-path Criterion benches.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::OnceLock;

use grit_lib::error::Result;
use grit_lib::index::{entry_from_stat, Index, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::staging::stage_worktree_changes;
use grit_lib::refs;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};
use tempfile::TempDir;

const BENCH_IDENT: &str = "Bench <bench@grit-scm.test> 1700000000 +0000";

/// Large shape: 10_000 files across 100 directories.
pub const L_FILES: usize = 10_000;
pub const L_DIRS: usize = 100;

/// Heavy shape: 100_000 files across 1_000 directories.
pub const H_FILES: usize = 100_000;
pub const H_DIRS: usize = 1_000;

pub struct HotPathsFixture {
    pub _dir: TempDir,
    pub repo: Repository,
    pub head_tree: ObjectId,
    pub file_paths: Vec<String>,
}

impl HotPathsFixture {
    fn build(file_count: usize, dir_count: usize) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files")
            .expect("init hot-path bench repo");
        populate_worktree(dir.path(), file_count, dir_count);
        let file_paths = list_txt_paths(dir.path());
        let head_tree = commit_all(&repo, "initial").expect("initial commit");
        Self {
            _dir: dir,
            repo,
            head_tree,
            file_paths,
        }
    }

    pub fn large() -> &'static Self {
        static FIX: OnceLock<HotPathsFixture> = OnceLock::new();
        FIX.get_or_init(|| HotPathsFixture::build(L_FILES, L_DIRS))
    }

    pub fn heavy() -> &'static Self {
        static FIX: OnceLock<HotPathsFixture> = OnceLock::new();
        FIX.get_or_init(|| HotPathsFixture::build(H_FILES, H_DIRS))
    }

    /// Tree with `fraction` of paths changed relative to [`Self::head_tree`].
    pub fn tree_with_fraction_changed(&self, fraction: f64) -> ObjectId {
        let n = ((self.file_paths.len() as f64) * fraction).round() as usize;
        let n = n.max(1);
        let work_tree = self.repo.work_tree.as_ref().expect("work tree");
        let mut index = self.repo.load_index().expect("load index");
        for path in self.file_paths.iter().take(n) {
            let abs = work_tree.join(path);
            std::fs::write(&abs, format!("changed-{path}\n")).expect("write");
            let data = std::fs::read(&abs).expect("read");
            let oid = self.repo.odb.write(ObjectKind::Blob, &data).expect("blob");
            let entry = entry_from_stat(&abs, path.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        write_tree_update_index(&self.repo.odb, &mut index, "", WriteTreeFlags::silent())
            .expect("tree")
    }

    pub fn mutate_index_batch(&self, remove_fraction: f64) -> Index {
        let mut index = self.repo.load_index().expect("load index");
        let n = ((index.entries.len() as f64) * remove_fraction).round() as usize;
        let n = n.max(1);
        let to_touch: Vec<_> = index
            .entries
            .iter()
            .take(n)
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        for path in &to_touch {
            index.remove(path.as_bytes());
        }
        let work_tree = self.repo.work_tree.as_ref().expect("work tree");
        for path in to_touch {
            let abs = work_tree.join(&path);
            std::fs::write(&abs, b"replacement\n").expect("write");
            let oid = self
                .repo
                .odb
                .write(ObjectKind::Blob, b"replacement\n")
                .expect("blob");
            let entry = entry_from_stat(&abs, path.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        index
    }

    pub fn modify_worktree_files(&self, count: usize) {
        let work_tree = self.repo.work_tree.as_ref().expect("work tree");
        for path in self.file_paths.iter().take(count) {
            std::fs::write(work_tree.join(path), format!("modified-{path}\n")).expect("write");
        }
    }

    pub fn reset_worktree_to_head(&self) {
        checkout_between_trees(&self.repo, None, &self.head_tree).expect("reset to head");
    }
}

fn populate_worktree(root: &Path, file_count: usize, dir_count: usize) {
    let files_per_dir = file_count.div_ceil(dir_count.max(1));
    let mut created = 0usize;
    for d in 0..dir_count.max(1) {
        let sub = root.join(format!("d{d:04}"));
        std::fs::create_dir_all(&sub).expect("mkdir");
        for f in 0..files_per_dir {
            if created >= file_count {
                return;
            }
            std::fs::write(sub.join(format!("f{f:04}.txt")), format!("seed {d}/{f}\n"))
                .expect("write file");
            created += 1;
        }
    }
}

fn list_txt_paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_txt(root, root, &mut out);
    out.sort();
    out
}

fn walk_txt(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            walk_txt(base, &path, out);
        } else if path.extension().is_some_and(|e| e == "txt") {
            out.push(
                path.strip_prefix(base)
                    .expect("strip")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

fn commit_all(repo: &Repository, message: &str) -> Result<ObjectId> {
    let work_tree = repo.work_tree.as_deref().expect("work tree");
    let mut index = repo.load_index()?;
    for rel in list_txt_paths(work_tree) {
        let abs = work_tree.join(&rel);
        let data = std::fs::read(&abs)?;
        let oid = repo.odb.write(ObjectKind::Blob, &data)?;
        let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR)?;
        index.add_or_replace(entry);
    }
    index.sort();
    let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;
    repo.write_index(&mut index)?;
    let parent = refs::resolve_ref(&repo.git_dir, "HEAD").ok();
    let commit_data = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: BENCH_IDENT.to_owned(),
        committer: BENCH_IDENT.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
    };
    let bytes = serialize_commit(&commit_data);
    let oid = repo.odb.write(ObjectKind::Commit, &bytes)?;
    refs::write_ref(&repo.git_dir, "HEAD", &oid)?;
    Ok(tree)
}

pub fn bench_checkout_between_trees(repo: &Repository, from: &ObjectId, to: &ObjectId) {
    checkout_between_trees(repo, Some(from), to).expect("checkout_between_trees");
}

pub fn bench_stage_scan(repo: &Repository) {
    let _ = stage_worktree_changes(repo, &[]).expect("stage scan");
}
