//! Offline maintenance preserves Git's promisor/local pack boundary (#2065).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, Result};
use grit_lib::maintenance::{
    gc, prune_loose_objects, repack_all, GcOptions, PruneOptions, RepackOptions,
};
use grit_lib::objects::promisor::{promisor_pack_object_ids, PromisorFetcher};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.org")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.org")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[derive(Default)]
struct OfflineFetcher(AtomicUsize);

impl PromisorFetcher for OfflineFetcher {
    fn fetch_missing(&self, _: &Repository, oids: &[ObjectId]) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::ObjectNotFound(oids[0].to_hex()))
    }
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    repo: Repository,
    fetcher: Arc<OfflineFetcher>,
    head: ObjectId,
    missing: ObjectId,
    promised: HashSet<ObjectId>,
    refs: String,
    sidecars: Vec<(PathBuf, Vec<u8>)>,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main", "source"]);
        let source = dir.path().join("source");
        for text in ["old\n", "new\n"] {
            fs::write(source.join("a"), text).expect("file");
            git(&source, &["add", "a"]);
            git(&source, &["commit", "-qm", text.trim()]);
        }
        git(&source, &["config", "uploadpack.allowFilter", "true"]);
        let url = format!("file://{}", source.display());
        git(dir.path(), &["clone", "--filter=blob:none", &url, "client"]);
        let path = dir.path().join("client");
        fs::write(path.join("local"), "local\n").expect("local file");
        git(&path, &["add", "local"]);
        git(&path, &["commit", "-qm", "local"]);
        git(
            &path,
            &["remote", "set-url", "origin", "file:///unavailable.git"],
        );
        let head = git(&path, &["rev-parse", "HEAD"]).parse().expect("head");
        let missing = git(&path, &["rev-parse", "HEAD~2:a"])
            .parse()
            .expect("blob");
        let fetcher = Arc::new(OfflineFetcher::default());
        let repo = Repository::open(
            &path.join(".git"),
            &RepositoryOptions::empty()
                .with_work_tree(&path)
                .with_promisor_fetcher(Some(fetcher.clone())),
        )
        .expect("open");
        let promised = promisor_pack_object_ids(repo.odb().objects_dir());
        let refs = git(
            &path,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        assert!(!promised.is_empty());
        assert!(!repo.odb().exists(&missing));
        let sidecars = fs::read_dir(path.join(".git/objects/pack"))
            .expect("packs")
            .map(|e| {
                let p = e.expect("entry").path();
                let bytes = fs::read(&p).expect("bytes");
                (p, bytes)
            })
            .collect();
        git(&path, &["fsck", "--strict"]);
        Self {
            _dir: dir,
            path,
            repo,
            fetcher,
            head,
            missing,
            promised,
            refs,
            sidecars,
        }
    }

    fn check(&self) {
        assert_eq!(
            git(
                &self.path,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            self.refs
        );
        assert_eq!(
            self.fetcher.0.load(Ordering::SeqCst),
            0,
            "maintenance must not fetch"
        );
        assert_eq!(git(&self.path, &["rev-parse", "HEAD"]), self.head.to_hex());
        assert_eq!(
            self.repo.odb().read(&self.head).expect("commit").kind,
            ObjectKind::Commit
        );
        assert_eq!(git(&self.path, &["show", "HEAD:local"]), "local");
        assert!(
            !self.repo.odb().exists(&self.missing),
            "historical blob remains absent"
        );
        for (path, bytes) in &self.sidecars {
            assert_eq!(&fs::read(path).expect("retained sidecar"), bytes);
        }
        assert_eq!(
            promisor_pack_object_ids(self.repo.odb().objects_dir()),
            self.promised
        );
        for entry in fs::read_dir(self.path.join(".git/objects/pack")).expect("packs") {
            let idx = entry.expect("entry").path();
            if idx.extension().is_none_or(|e| e != "idx")
                || idx.with_extension("promisor").is_file()
            {
                continue;
            }
            let ids: HashSet<_> = grit_lib::objects::pack::read_idx_object_ids(&idx)
                .expect("ids")
                .into_iter()
                .collect();
            assert!(
                ids.is_disjoint(&self.promised),
                "local pack must exclude promised objects"
            );
            assert!(!ids.contains(&self.missing));
        }
        for oid in &self.promised {
            assert!(
                !self.repo.odb().object_path(oid).exists(),
                "do not loosen promisor objects"
            );
        }
        git(&self.path, &["fsck", "--strict"]);
    }
}

#[test]
fn offline_partial_clone_full_forced_incremental_and_dry_run() {
    for incremental in [false, true] {
        for force in [false, true] {
            let f = Fixture::new();
            let mut opts = RepackOptions::default();
            opts.incremental = incremental;
            opts.force_recompress = force;
            opts.write_bitmap = true;
            opts.dry_run = true;
            let preview = repack_all(&f.repo, &opts).expect("offline preview");
            f.check();
            opts.dry_run = false;
            let result = repack_all(&f.repo, &opts).expect("offline repack");
            assert_eq!(preview.objects_packed, result.objects_packed);
            let pack = result
                .pack_path
                .expect("new local pack is reported with multiple packs");
            assert!(!pack.with_extension("promisor").exists());
            assert!(
                !pack.with_extension("bitmap").exists(),
                "local pack does not contain the promise closure"
            );
            assert_eq!(
                result.pack_bytes,
                Some(pack.metadata().expect("pack").len())
            );
            f.check();
            git(&f.path, &["gc"]);
            git(&f.path, &["repack", "-a", "-d"]);
            git(&f.path, &["fsck", "--strict"]);
            assert!(!f.repo.odb().exists(&f.missing));
        }
    }
}

#[test]
fn offline_partial_clone_gc_and_prune() {
    for dry_run in [true, false] {
        let f = Fixture::new();
        let garbage = f
            .repo
            .odb()
            .write(ObjectKind::Blob, b"unreachable")
            .expect("garbage");
        let opts = GcOptions::full_run(dry_run, 1_704_000_000);
        git(&f.path, &["config", "gc.pruneExpire", "now"]);
        f.repo.reload_config().expect("reload");
        let before = git(&f.path, &["reflog", "show", "--all", "--format=%H"]);
        let mut before: Vec<_> = before.lines().collect();
        before.sort_unstable();
        gc(&f.repo, &opts).expect("offline gc");
        let after = git(&f.path, &["reflog", "show", "--all", "--format=%H"]);
        let mut after: Vec<_> = after.lines().collect();
        after.sort_unstable();
        assert_eq!(
            before, after,
            "reachable reflog entries survive promised gaps"
        );
        f.check();
        let mut prune = PruneOptions::default();
        prune.dry_run = dry_run;
        prune_loose_objects(&f.repo, &prune).expect("offline prune");
        assert_eq!(f.repo.odb().object_path(&garbage).exists(), dry_run);
        f.check();
    }
}

#[test]
fn missing_local_objects_are_errors_without_fetching() {
    for dry_run in [true, false] {
        for spec in ["HEAD:local", "HEAD^{tree}", "HEAD"] {
            let f = Fixture::new();
            let missing: ObjectId = git(&f.path, &["rev-parse", spec]).parse().expect("oid");
            fs::remove_file(f.repo.odb().object_path(&missing)).expect("remove local object");
            let mut opts = RepackOptions::default();
            opts.dry_run = dry_run;
            opts.keep_unreachable = false;
            let err = repack_all(&f.repo, &opts).expect_err("missing local object must fail");
            assert!(matches!(err, Error::ObjectNotFound(_)), "{spec}: {err:?}");
            let mut prune = PruneOptions::default();
            prune.dry_run = dry_run;
            prune.older_than = Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_900_000_000),
            );
            assert!(matches!(
                prune_loose_objects(&f.repo, &prune),
                Err(Error::ObjectNotFound(_))
            ));
            assert_eq!(f.fetcher.0.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn missing_tree_promised_by_unreferenced_tag_stays_offline() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = TempDir::new().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "remote.origin.promisor", "true"]);
    git(
        dir.path(),
        &["config", "remote.origin.url", "file:///unavailable.git"],
    );
    let fetcher = Arc::new(OfflineFetcher::default());
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty()
            .with_work_tree(dir.path())
            .with_promisor_fetcher(Some(fetcher.clone())),
    )
    .expect("open");
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, b"server content")
        .expect("blob");
    // Use a non-empty tree so the ODB's virtual empty-tree support cannot hide the miss.
    let mut tree_data = b"100644 absent\0".to_vec();
    tree_data.extend_from_slice(blob.as_bytes());
    let tree = repo
        .odb()
        .write(ObjectKind::Tree, &tree_data)
        .expect("tree");
    let commit = repo.odb().write(ObjectKind::Commit,
        format!("tree {tree}\nauthor Tester <t@example.org> 1 +0000\ncommitter Tester <t@example.org> 1 +0000\n\nlocal\n").as_bytes()).expect("commit");
    let tag = repo.odb().write(ObjectKind::Tag,
        format!("object {tree}\ntype tree\ntag promise\ntagger Tester <t@example.org> 1 +0000\n\npromise\n").as_bytes()).expect("tag");
    let prefix = dir.path().join(".git/objects/pack/pack");
    let mut child = Command::new("git")
        .current_dir(dir.path())
        .arg("pack-objects")
        .arg(&prefix)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("pack-objects");
    writeln!(child.stdin.take().expect("stdin"), "{tag}").expect("oid");
    let packed = child.wait_with_output().expect("pack");
    assert!(packed.status.success());
    let hash = String::from_utf8(packed.stdout).expect("hash");
    let marker = prefix.with_file_name(format!("pack-{}.promisor", hash.trim()));
    fs::write(&marker, b"promise\n").expect("marker");
    fs::remove_file(repo.odb().object_path(&tree)).expect("remove tree");
    fs::remove_file(repo.odb().object_path(&tag)).expect("remove loose tag");
    git(
        dir.path(),
        &["update-ref", "refs/heads/main", &commit.to_hex()],
    );
    // A missing promise can itself be a maintenance root.
    fs::write(dir.path().join(".git/refs/tags/tree"), format!("{tree}\n")).expect("ref");
    repo.odb().invalidate_packs();
    let promises =
        grit_lib::objects::promisor::promisor_expanded_object_ids(&repo).expect("promises");
    assert!(promises.contains(&tree), "tag must promise the absent tree");
    git(dir.path(), &["fsck", "--strict"]);
    for keep in [true, false] {
        let mut opts = RepackOptions::default();
        opts.keep_unreachable = keep;
        opts.write_bitmap = true;
        repack_all(&repo, &opts).expect("offline tree boundary");
        assert!(marker.is_file());
        assert!(marker.with_extension("pack").is_file());
        assert!(!repo.odb().object_path(&tag).exists());
        assert!(!repo.odb().exists(&tree));
        assert_eq!(fetcher.0.load(Ordering::SeqCst), 0);
        git(dir.path(), &["fsck", "--strict"]);
    }
    gc(&repo, &GcOptions::full_run(false, 1_704_000_000)).expect("offline GC with missing tree");
    assert_eq!(fetcher.0.load(Ordering::SeqCst), 0);
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn all_promisor_repack_writes_no_empty_pack_and_still_prunes() {
    let f = Fixture::new();
    git(&f.path, &["repack", "-a", "-d"]);
    for entry in fs::read_dir(f.path.join(".git/objects/pack")).expect("packs") {
        let pack = entry.expect("entry").path();
        if pack.extension().is_some_and(|e| e == "pack")
            && !pack.with_extension("promisor").exists()
        {
            fs::write(pack.with_extension("promisor"), b"").expect("marker");
        }
    }
    f.repo.odb().invalidate_packs();
    let before: HashSet<_> = fs::read_dir(f.path.join(".git/objects/pack"))
        .expect("packs")
        .map(|e| e.expect("entry").file_name())
        .collect();
    let garbage = f
        .repo
        .odb()
        .write(ObjectKind::Blob, b"expire me")
        .expect("blob");
    let mut opts = RepackOptions::default();
    opts.keep_unreachable = false;
    let report = repack_all(&f.repo, &opts).expect("no local objects to pack");
    assert!(report.pack_path.is_none());
    assert!(report.objects_packed.is_none());
    assert!(!f.repo.odb().object_path(&garbage).exists());
    let after: HashSet<_> = fs::read_dir(f.path.join(".git/objects/pack"))
        .expect("packs")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(before, after, "no empty pack or staging file");
    assert_eq!(f.fetcher.0.load(Ordering::SeqCst), 0);
    git(&f.path, &["fsck", "--strict"]);
}
