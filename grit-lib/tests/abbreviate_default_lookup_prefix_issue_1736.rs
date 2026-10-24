//! Abbreviation and short-id lookup through a store that implements only the required
//! [`ObjectStore`] methods (issue 1736), plus a round-trip against `git rev-parse --short`.

use std::collections::HashMap;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, Result};
use grit_lib::objects::odb::store::{ObjectStore, WritableObjectStore};
use grit_lib::objects::odb::{OdbBuilder, WriteOptions};
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};
use grit_lib::revwalk::rev_parse::{abbreviate_object_id, resolve_revision};

/// Implements only the required trait methods and counts full enumerations.
#[derive(Debug, Default)]
struct MinimalStore {
    objects: RwLock<HashMap<ObjectId, (ObjectKind, Vec<u8>)>>,
    scans: AtomicUsize,
}

impl MinimalStore {
    fn insert(&self, oid: ObjectId) {
        self.objects
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(oid, (ObjectKind::Blob, Vec::new()));
    }

    fn scans(&self) -> usize {
        self.scans.load(Ordering::SeqCst)
    }
}

impl ObjectStore for MinimalStore {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let guard = self.objects.read().unwrap_or_else(|e| e.into_inner());
        Ok(guard.get(oid).map(|(kind, data)| Object {
            kind: *kind,
            data: data.clone(),
        }))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let guard = self.objects.read().unwrap_or_else(|e| e.into_inner());
        Ok(guard.get(oid).map(|(kind, data)| ObjectInfo {
            kind: *kind,
            size: data.len() as u64,
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        let guard = self.objects.read().unwrap_or_else(|e| e.into_inner());
        for oid in guard.keys() {
            if f(oid).is_break() {
                break;
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for MinimalStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = grit_lib::objects::hash::hash_object(HashAlgo::Sha1, kind, data)?;
        self.objects
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .entry(oid)
            .or_insert_with(|| (kind, data.to_vec()));
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.read_info(oid)?.is_some())
    }
}

fn repo_over(store: Arc<MinimalStore>) -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::new().with_initial_branch("main")).expect("init");
    let git_dir = dir.path().join(".git");
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store)
                .alternates(false),
        )
        .open(&git_dir, Some(dir.path()))
        .expect("open over minimal store");
    (dir, repo)
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex).expect("test oid")
}

#[test]
fn abbreviation_scans_a_minimal_store_once_per_id() {
    let store = Arc::new(MinimalStore::default());
    // `target` and `shares_nine` agree on 9 hex digits; `shares_seven` on 7 with both.
    let target = oid("abcdef0123000000000000000000000000000000");
    let shares_nine = oid("abcdef0124000000000000000000000000000000");
    let shares_seven = oid("abcdef0900000000000000000000000000000000");
    let unrelated = oid("1234567890000000000000000000000000000000");
    for id in [target, shares_nine, shares_seven, unrelated] {
        store.insert(id);
    }
    let (_dir, repo) = repo_over(Arc::clone(&store));

    let cases = [
        (target, "abcdef0123"),
        (shares_nine, "abcdef0124"),
        (shares_seven, "abcdef09"),
        (unrelated, "1234567"),
    ];
    for (id, expected) in cases {
        let before = store.scans();
        assert_eq!(
            abbreviate_object_id(&repo, id, 7).expect("abbreviate"),
            expected
        );
        assert_eq!(
            store.scans() - before,
            1,
            "abbreviating {id} must enumerate the store once"
        );
    }

    assert_eq!(
        abbreviate_object_id(&repo, target, 4).expect("abbrev"),
        "abcdef0123"
    );
    assert_eq!(
        abbreviate_object_id(&repo, unrelated, 4).expect("abbrev"),
        "1234"
    );
    assert_eq!(
        abbreviate_object_id(&repo, target, 12).expect("abbrev"),
        "abcdef012300",
        "a longer minimum wins over the unique length"
    );
    assert_eq!(
        abbreviate_object_id(&repo, target, 100).expect("abbrev"),
        target.to_hex(),
        "minimum is clamped to the full id"
    );

    let missing = oid("abcdef0125000000000000000000000000000000");
    assert_eq!(
        abbreviate_object_id(&repo, missing, 7).expect("abbrev"),
        "abcdef0"
    );
}

#[test]
fn default_lookup_prefix_handles_odd_even_empty_full_and_invalid_prefixes() {
    let store = MinimalStore::default();
    let a = oid("abcdef0123000000000000000000000000000000");
    let b = oid("abcdef0124000000000000000000000000000000");
    let c = oid("abd0000000000000000000000000000000000000");
    for id in [a, b, c] {
        store.insert(id);
    }
    let lookup = |prefix: &str, limit: usize| -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        store.lookup_prefix(prefix, limit, &mut out)?;
        out.sort();
        Ok(out)
    };

    assert_eq!(lookup("", 0).expect("empty"), vec![a, b, c]);
    assert_eq!(lookup("", 2).expect("limited").len(), 2);
    assert_eq!(lookup("a", 0).expect("one nibble"), vec![a, b, c]);
    assert_eq!(lookup("ab", 0).expect("one byte"), vec![a, b, c]);
    assert_eq!(lookup("abc", 0).expect("odd"), vec![a, b]);
    assert_eq!(lookup("abd", 0).expect("odd"), vec![c]);
    assert_eq!(lookup("abcdef012", 0).expect("odd"), vec![a, b]);
    assert_eq!(lookup("abcdef0123", 0).expect("even"), vec![a]);
    assert_eq!(lookup("ABCDEF0124", 0).expect("uppercase"), vec![b]);
    assert_eq!(lookup(&a.to_hex(), 0).expect("full"), vec![a]);
    assert!(lookup("abe", 0).expect("miss").is_empty());

    assert!(matches!(lookup("abg", 0), Err(Error::InvalidObjectId(_))));
    assert!(matches!(
        lookup(&format!("{}0", a.to_hex()), 0),
        Err(Error::InvalidObjectId(_))
    ));
}

#[test]
fn short_ids_resolve_through_a_minimal_store() {
    let store = Arc::new(MinimalStore::default());
    let (_dir, repo) = repo_over(Arc::clone(&store));
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"short\n")
        .expect("write");
    let short = abbreviate_object_id(&repo, blob, 7).expect("abbrev");
    assert_eq!(resolve_revision(&repo, &short).expect("resolve"), blob);
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

#[test]
fn abbreviation_matches_git_rev_parse_short_over_pack_and_loose() {
    let dir = tempfile::tempdir().expect("tempdir");
    Repository::init(dir.path(), &InitOptions::new().with_initial_branch("main")).expect("init");

    // Enough blobs that many 4-digit prefixes collide (some need 5 or 6 digits).
    let mut child = git(dir.path())
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git fast-import");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for i in 0..3000 {
            let body = format!("packed {i}\n");
            write!(stdin, "blob\ndata {}\n{body}\n", body.len()).expect("fast-import");
        }
    }
    assert!(child.wait().expect("fast-import").success());
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let mut ids = Vec::new();
    for i in 0..300 {
        ids.push(
            repo.odb()
                .write(ObjectKind::Blob, format!("loose {i}\n").as_bytes())
                .expect("loose blob"),
        );
    }
    for i in (0..3000).step_by(7) {
        ids.push(
            grit_lib::objects::hash::hash_object(
                HashAlgo::Sha1,
                ObjectKind::Blob,
                format!("packed {i}\n").as_bytes(),
            )
            .expect("hash"),
        );
    }

    let sample: Vec<ObjectId> = ids.into_iter().step_by(3).collect();
    for min_len in [4, 7] {
        let expected: Vec<String> = sample
            .iter()
            .map(|id| {
                let out = git(dir.path())
                    .arg("rev-parse")
                    .arg(format!("--short={min_len}"))
                    .arg(id.to_hex())
                    .output()
                    .expect("git rev-parse --short");
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            })
            .collect();
        let actual: Vec<String> = sample
            .iter()
            .map(|id| abbreviate_object_id(&repo, *id, min_len).expect("abbreviate"))
            .collect();
        assert_eq!(actual, expected, "min_len {min_len}");
        if min_len == 4 {
            assert!(
                actual.iter().any(|a| a.len() > 4),
                "fixture must exercise colliding prefixes"
            );
        }
    }
}
