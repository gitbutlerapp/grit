//! `log --group-by author` on a custom primary object store.
//!
//! Author counts take pack, bitmap and commit-graph fast paths on the files backend.
//! A repository whose objects live in a [`MemoryStore`] has none of those on disk, so
//! its counts come from object reads through the store; they must equal what the files
//! backend reports for the same history, loose and after a repack with a bitmap and a
//! commit-graph.

use grit_lib::progress::{CancelToken, NullProgress};

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::{CommitGraphBuild, CommitGraphOptions, RepackOptions};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::{Odb, OdbBuilder};
use grit_lib::objects::{serialize_commit, CommitData, HashAlgo, ObjectId, ObjectKind};
use grit_lib::refs::store::RefName;
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::{
    AuthorGroup, GroupByAuthorOptions, RefSelection, ResolveOptions, RevWalkListOptions,
};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const AUTHORS: [&str; 4] = [
    "Ada <ada@example.com>",
    "Brook <brook@example.com>",
    "Cyd <cyd@example.com>",
    "ada <ADA@example.com>",
];

struct Scenario {
    name: &'static str,
    tokens: &'static [&'static str],
    options: RevWalkListOptions,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "HEAD",
            tokens: &["HEAD"],
            options: RevWalkListOptions::default(),
        },
        Scenario {
            name: "first-parent HEAD",
            tokens: &["HEAD"],
            options: RevWalkListOptions {
                first_parent: true,
                ..RevWalkListOptions::default()
            },
        },
        Scenario {
            name: "side..main",
            tokens: &["side..main"],
            options: RevWalkListOptions::default(),
        },
        Scenario {
            name: "--all",
            tokens: &[],
            options: RevWalkListOptions {
                refs: vec![RefSelection::All],
                ..RevWalkListOptions::default()
            },
        },
        Scenario {
            name: "limited",
            tokens: &["main"],
            options: RevWalkListOptions {
                limit: Some(7),
                ..RevWalkListOptions::default()
            },
        },
    ]
}

/// Groups for every scenario, counts-only and with commit lists.
fn all_groups(repo: &Repository) -> Vec<(String, Vec<AuthorGroup>)> {
    let mut out = Vec::new();
    for scenario in scenarios() {
        for count_only in [true, false] {
            let groups = repo
                .revwalk()
                .group_by_author_in_walk(
                    scenario.tokens,
                    &scenario.options,
                    None,
                    GroupByAuthorOptions { count_only },
                )
                .unwrap_or_else(|e| panic!("{} count_only={count_only}: {e}", scenario.name));
            out.push((format!("{} count_only={count_only}", scenario.name), groups));
        }
    }
    out
}

fn write_commit(repo: &Repository, parents: &[ObjectId], author: &str, n: usize) -> ObjectId {
    let ident = format!("{author} {} +0000", 1_700_000_000 + n);
    let data = CommitData {
        tree: ObjectId::from_hex(EMPTY_TREE).expect("empty tree"),
        parents: parents.to_vec(),
        author: ident.clone(),
        committer: ident,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("commit {n}\n"),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    repo.odb()
        .write(ObjectKind::Commit, &serialize_commit(&data))
        .expect("write commit")
}

fn set_branch(repo: &Repository, branch: &str, oid: ObjectId) {
    let name = RefName::try_from(format!("refs/heads/{branch}").as_str()).expect("ref name");
    repo.references().update(&name, &oid).expect("update ref");
}

/// A main line with a side branch merged back every few commits, authors rotating at
/// different rates on each line, and an unmerged `side` tip.
fn build_history(repo: &Repository) {
    let mut n = 0;
    let mut next = |parents: &[ObjectId], author_slot: usize| {
        n += 1;
        write_commit(repo, parents, AUTHORS[author_slot % AUTHORS.len()], n)
    };
    let mut main = next(&[], 0);
    for round in 0..6 {
        let mut side = main;
        for step in 0..3 {
            side = next(&[side], round + step + 1);
        }
        for step in 0..2 {
            main = next(&[main], step * 2);
        }
        main = next(&[main, side], round);
        if round == 5 {
            set_branch(repo, "side", next(&[side], 2));
        }
    }
    set_branch(repo, "main", main);
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    git_dir: PathBuf,
    store: Arc<MemoryStore>,
}

fn init_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    Repository::init(
        &root,
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    let git_dir = root.join(".git");
    Fixture {
        _dir: dir,
        root,
        git_dir,
        store: Arc::new(MemoryStore::new(HashAlgo::Sha1)),
    }
}

impl Fixture {
    fn open_memory(&self, builder: impl FnOnce(OdbBuilder) -> OdbBuilder) -> Repository {
        Repository::open(
            &self.git_dir,
            &(RepositoryOptions::empty()
                .with_object_store(self.store.clone())
                .with_odb_builder(builder(OdbBuilder::new()))
                .with_work_tree(&self.root)),
        )
        .expect("open memory repo")
    }

    fn open_files(&self) -> Repository {
        Repository::open(
            &self.git_dir,
            &RepositoryOptions::empty().with_work_tree(&self.root),
        )
        .expect("open files repo")
    }

    /// Copy every object of the memory store into the on-disk `objects/`.
    fn export_to_disk(&self) {
        let disk = Odb::new(&self.git_dir.join("objects"));
        let mut oids = Vec::new();
        self.store
            .for_each_object(&mut |oid| {
                oids.push(*oid);
                ControlFlow::Continue(())
            })
            .expect("enumerate");
        for oid in oids {
            let obj = self.store.read(&oid).expect("read").expect("present");
            assert_eq!(disk.write(obj.kind, &obj.data).expect("export"), oid);
        }
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn assert_same_groups(
    label: &str,
    got: &[(String, Vec<AuthorGroup>)],
    want: &[(String, Vec<AuthorGroup>)],
) {
    assert_eq!(got.len(), want.len());
    for ((name, got), (_, want)) in got.iter().zip(want) {
        assert_eq!(got, want, "{label}: {name}");
    }
}

#[test]
fn memory_store_author_groups_match_the_files_backend() {
    let fixture = init_fixture();
    let memory = fixture.open_memory(|odb| odb);
    build_history(&memory);
    let objects = fixture.git_dir.join("objects");
    assert!(
        file_names(&objects.join("pack")).is_empty(),
        "history lives only in the memory store"
    );
    let from_memory = all_groups(&memory);

    let head = &from_memory[0].1;
    let total: usize = head.iter().map(|g| g.count).sum();
    assert_eq!(total, 1 + 6 * 6, "{head:?}");
    assert_eq!(
        head.len(),
        AUTHORS.len(),
        "case-distinct identities stay apart"
    );

    let graph = match memory
        .maintenance()
        .build_commit_graph(&CommitGraphOptions::default())
        .expect("build commit-graph")
    {
        CommitGraphBuild::Built(graph) => graph,
        CommitGraphBuild::Skipped(reason) => panic!("graph skipped: {reason:?}"),
        other => panic!("unexpected commit-graph result: {other:?}"),
    };
    let chain = Arc::new(graph.to_chain().expect("graph chain"));
    let memory_with_graph = fixture.open_memory(|odb| odb.commit_graph(chain));
    assert_same_groups(
        "memory store with attached graph",
        &all_groups(&memory_with_graph),
        &from_memory,
    );

    fixture.export_to_disk();
    let files = fixture.open_files();
    assert_same_groups("files backend, loose", &all_groups(&files), &from_memory);

    let mut repack = RepackOptions::default();
    repack.write_bitmap = true;
    repack.write_commit_graph = true;
    files
        .maintenance()
        .repack(
            &repack,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("repack");
    let files = fixture.open_files();
    let packs = file_names(&objects.join("pack"));
    assert!(
        packs.iter().any(|name| name.ends_with(".bitmap")),
        "repack wrote a bitmap: {packs:?}"
    );
    assert!(objects.join("info/commit-graph").is_file());
    assert_same_groups(
        "files backend, packed with bitmap and commit-graph",
        &all_groups(&files),
        &from_memory,
    );

    // The packs, bitmap and graph now on disk describe the files layout; the memory
    // repository keeps counting from its own store, including commits only it holds.
    let memory = fixture.open_memory(|odb| odb);
    assert_same_groups(
        "memory store beside a packed objects/",
        &all_groups(&memory),
        &from_memory,
    );
    let count_main = |repo: &Repository| {
        repo.revwalk()
            .group_by_author_in_walk(
                &["main"],
                &RevWalkListOptions::default(),
                None,
                GroupByAuthorOptions { count_only: true },
            )
            .expect("count main")
    };
    let before = count_main(&files);
    let main = memory
        .revwalk()
        .resolve("main", &ResolveOptions::default())
        .expect("main");
    set_branch(
        &memory,
        "main",
        write_commit(&memory, &[main], AUTHORS[2], 99),
    );
    let after = count_main(&memory);
    let count_of = |groups: &[AuthorGroup], email: &str| {
        groups
            .iter()
            .find(|g| g.email == email)
            .map_or(0, |g| g.count)
    };
    assert_eq!(
        count_of(&after, "cyd@example.com"),
        count_of(&before, "cyd@example.com") + 1
    );
}
