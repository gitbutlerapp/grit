//! Fixtures for refs kept in the files backend while objects live in a custom primary store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::{HashAlgo, ObjectId, ObjectKind};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::refs::store::RefName;
use grit_lib::remote::clone::{clone_into, CloneIntoOptions};
use grit_lib::repo::{InitOptions, Repository};

/// Objects of [`tagged_history`]: a commit, its tree, and tags of each peel shape.
#[derive(Debug, Clone, Copy)]
pub struct TaggedHistory {
    pub tree: ObjectId,
    pub commit: ObjectId,
    /// Annotated tag `v1` naming `commit`.
    pub v1: ObjectId,
    /// Annotated tag `nested` naming the tag `v1`.
    pub nested: ObjectId,
    /// Annotated tag `treetag` naming `tree`.
    pub treetag: ObjectId,
}

impl TaggedHistory {
    /// `(ref name, peel)` for every ref [`tagged_history`] writes; `None` for refs that are not
    /// annotated tags.
    pub fn expected_peels(&self) -> BTreeMap<String, Option<ObjectId>> {
        [
            ("refs/heads/main", None),
            ("refs/tags/light", None),
            ("refs/tags/nested", Some(self.commit)),
            ("refs/tags/treetag", Some(self.tree)),
            ("refs/tags/v1", Some(self.commit)),
        ]
        .into_iter()
        .map(|(name, peel)| (name.to_owned(), peel))
        .collect()
    }
}

fn tag_body(target: ObjectId, kind: &str, name: &str) -> String {
    format!("object {target}\ntype {kind}\ntag {name}\ntagger T <t@e.com> 1 +0000\n\n{name}\n")
}

/// Write a commit, its tree, annotated tags `v1` (commit), `nested` (tag of `v1`) and
/// `treetag` (tree), and a lightweight tag `light`, through `repo`'s object and ref stores.
/// The refs are loose.
pub fn tagged_history(repo: &Repository) -> TaggedHistory {
    let odb = repo.odb();
    let tree = odb.write(ObjectKind::Tree, b"").expect("tree");
    let commit = odb
        .write(
            ObjectKind::Commit,
            format!(
                "tree {tree}\nauthor T <t@e.com> 1 +0000\ncommitter T <t@e.com> 1 +0000\n\nc\n"
            )
            .as_bytes(),
        )
        .expect("commit");
    let v1 = odb
        .write(ObjectKind::Tag, tag_body(commit, "commit", "v1").as_bytes())
        .expect("v1");
    let nested = odb
        .write(ObjectKind::Tag, tag_body(v1, "tag", "nested").as_bytes())
        .expect("nested");
    let treetag = odb
        .write(
            ObjectKind::Tag,
            tag_body(tree, "tree", "treetag").as_bytes(),
        )
        .expect("treetag");
    let refs = repo.references();
    for (name, oid) in [
        ("refs/heads/main", commit),
        ("refs/tags/light", commit),
        ("refs/tags/v1", v1),
        ("refs/tags/nested", nested),
        ("refs/tags/treetag", treetag),
    ] {
        refs.update(&RefName::try_from(name).expect("ref name"), &oid)
            .expect("update ref");
    }
    TaggedHistory {
        tree,
        commit,
        v1,
        nested,
        treetag,
    }
}

/// Initialize a non-bare files repository at `dir` (files refs, branch `main`).
pub fn init_files_repo(dir: &Path) {
    Repository::init(
        dir,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
}

/// Open the non-bare repository at `dir`, over `store` as its primary when given (its refs
/// stay in the files backend under `dir/.git`), else over `objects/`.
pub fn open_repo(dir: &Path, store: Option<Arc<MemoryStore>>) -> Repository {
    let git_dir = dir.join(".git");
    let options = RepositoryOptions::empty().with_work_tree(dir);
    let options = match store {
        Some(store) => options.with_object_store(store),
        None => options,
    };
    Repository::open(&git_dir, &options).expect("open repository")
}

/// A bare files repository holding [`tagged_history`], with `HEAD` on `main`.
pub fn tagged_upstream() -> (tempfile::TempDir, TaggedHistory) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_bare(true)
            .with_initial_branch("main")
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init upstream");
    let repo = Repository::open(dir.path(), &RepositoryOptions::empty()).expect("open upstream");
    let history = tagged_history(&repo);
    (dir, history)
}

/// Clone `upstream` with [`clone_into`] into a new non-bare files-refs repository whose
/// objects go to a [`MemoryStore`] when `memory_primary` is set, else to `objects/`.
pub fn clone_upstream(
    upstream: &Path,
    memory_primary: bool,
) -> (tempfile::TempDir, Repository, Option<Arc<MemoryStore>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    init_files_repo(dir.path());
    let store = memory_primary.then(|| Arc::new(MemoryStore::new(HashAlgo::Sha1)));
    clone_into(
        open_repo(dir.path(), store.clone()),
        CloneIntoOptions::new(upstream.to_string_lossy().into_owned()),
        &mut NullProgress,
        &CancelToken::never(),
        None,
    )
    .expect("clone_into");
    let repo = open_repo(dir.path(), store.clone());
    (dir, repo, store)
}

/// Every record of `git_dir/packed-refs`: ref name to its value and `^` peel line.
pub fn packed_refs_records(git_dir: &Path) -> BTreeMap<String, (ObjectId, Option<ObjectId>)> {
    let text = std::fs::read_to_string(git_dir.join("packed-refs")).expect("read packed-refs");
    let mut out: BTreeMap<String, (ObjectId, Option<ObjectId>)> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(peel) = line.strip_prefix('^') {
            let name = last.as_ref().expect("peel line follows a record");
            let entry = out.get_mut(name).expect("record");
            entry.1 = Some(peel.parse().expect("peel oid"));
            continue;
        }
        let (oid, name) = line.split_once(' ').expect("record line");
        out.insert(name.to_owned(), (oid.parse().expect("record oid"), None));
        last = Some(name.to_owned());
    }
    out
}
