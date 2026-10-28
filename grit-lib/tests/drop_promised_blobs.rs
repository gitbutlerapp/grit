//! Dropping promised large blobs from a partial clone ([`drop_promised_blobs`]), all
//! served in-process by grit: a blob-limit clone of a local repository, lazy fetches
//! of old versions of a large file, then the drop and a lazy fetch back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::maintenance::{
    drop_promised_blobs, gc, gc_auto, AutoGcOutcome, DropPromisedBlobsOptions,
    DropPromisedBlobsReport, DropPromisedBlobsSkip, GcOptions, MidxOptions,
};
use grit_lib::objects::index_pack::{install_pack_bytes, IngestPackOptions};
use grit_lib::objects::pack::{clear_pack_cache, read_pack_index, read_packed_delta_dependency};
use grit_lib::objects::pack_objects::{build_pack_from_send_list, PackBuildOptions};
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs::ref_storage::RefStorageFormat;
use grit_lib::remote::clone::{clone, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::state::InProgressOperation;
use time::OffsetDateTime;

/// The fixed "current time" of every run: far after any file these tests write.
const NOW: i64 = 4_000_000_000;
const DAY: u64 = 24 * 60 * 60;
const BIG: usize = 300 * 1024;
const LIMIT: u64 = 100 * 1024;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(NOW).unwrap()
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW as u64 - days * DAY)
}

/// Only local remotes here, so no HTTP client is ever made.
struct NoHttp;

impl HttpClientFactory for NoHttp {
    fn create(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }

    fn create_for_probe(&self, _: &ConfigSet) -> RemoteResult<Box<dyn HttpClient>> {
        Err(RemoteError::UnsupportedScheme)
    }
}

/// `len` bytes that do not compress, the same for the same `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn append_config(git_dir: &Path, text: &str) {
    let path = git_dir.join("config");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str(text);
    std::fs::write(path, config).unwrap();
}

fn tree(repo: &Repository, entries: &[(&str, ObjectId)]) -> ObjectId {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut body = Vec::new();
    for (name, oid) in sorted {
        body.extend_from_slice(format!("100644 {name}\0").as_bytes());
        body.extend_from_slice(oid.as_bytes());
    }
    repo.odb()
        .write_loose_materialize(ObjectKind::Tree, &body)
        .unwrap()
}

fn commit(repo: &Repository, tree: ObjectId, parent: Option<ObjectId>, n: usize) -> ObjectId {
    let parent = parent.map(|p| format!("parent {p}\n")).unwrap_or_default();
    let body = format!(
        "tree {tree}\n{parent}author T <t@e.com> {n}00 +0000\ncommitter T <t@e.com> {n}00 +0000\n\nv{n}\n"
    );
    repo.odb()
        .write_loose_materialize(ObjectKind::Commit, body.as_bytes())
        .unwrap()
}

/// A served repository and a `blob:limit=100k` clone of it.
struct Fixture {
    tmp: tempfile::TempDir,
    /// `big.bin` in commits 1 to 4 (each a small edit of the last).
    big: Vec<ObjectId>,
    /// Commits 1 to 4 on `main`.
    commits: Vec<ObjectId>,
    /// A large blob in the server that no ref reaches.
    stray: ObjectId,
    git_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::build(false)
    }

    /// Like [`Self::new`], with an annotated tag `refs/tags/stray` on the stray blob.
    fn with_tagged_stray() -> Self {
        Self::build(true)
    }

    fn build(tag_stray: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let server_dir = tmp.path().join("server.git");
        let server = Repository::init(
            &server_dir,
            &InitOptions::new()
                .with_bare(true)
                .with_initial_branch("main")
                .with_ref_storage(RefStorageFormat::Files),
        )
        .unwrap();
        let odb = server.odb();
        let mut data = noise(1, BIG);
        let small = odb
            .write_loose_materialize(ObjectKind::Blob, b"small\n")
            .unwrap();
        let mut big = Vec::new();
        let mut commits = Vec::new();
        for n in 1..=4usize {
            let at = n * 997;
            data[at..at + 64].copy_from_slice(&noise(100 + n as u64, 64));
            let blob = odb
                .write_loose_materialize(ObjectKind::Blob, &data)
                .unwrap();
            big.push(blob);
            let t = tree(&server, &[("big.bin", blob), ("small.txt", small)]);
            commits.push(commit(&server, t, commits.last().copied(), n));
        }
        let stray = odb
            .write_loose_materialize(ObjectKind::Blob, &noise(7, BIG))
            .unwrap();
        let tip = commits.last().copied().unwrap();
        std::fs::write(server_dir.join("refs/heads/main"), format!("{tip}\n")).unwrap();
        if tag_stray {
            let body = format!(
                "object {stray}\ntype blob\ntag stray\ntagger T <t@e.com> 100 +0000\n\nstray\n"
            );
            let tag = odb
                .write_loose_materialize(ObjectKind::Tag, body.as_bytes())
                .unwrap();
            std::fs::create_dir_all(server_dir.join("refs/tags")).unwrap();
            std::fs::write(server_dir.join("refs/tags/stray"), format!("{tag}\n")).unwrap();
        }
        append_config(
            &server_dir,
            "[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n",
        );

        let dest = tmp.path().join("client");
        let mut opts = CloneOptions::new(format!("file://{}", server_dir.display()), dest);
        opts.filter = Some(ObjectFilterSpec::BlobLimit(LIMIT));
        opts.checkout = false;
        let outcome = clone(opts, &mut NoProgress, &NeverCancel, None).expect("clone");
        let git_dir = outcome.repo.git_dir().to_path_buf();
        drop(outcome);
        clear_pack_cache();
        Self {
            tmp,
            big,
            commits,
            stray,
            git_dir,
        }
    }

    fn work_tree(&self) -> PathBuf {
        self.tmp.path().join("client")
    }

    fn repo(&self) -> Repository {
        let fetcher: Arc<dyn PromisorFetcher> =
            Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
        Repository::open(
            &self.git_dir,
            &RepositoryOptions::empty()
                .with_work_tree(self.work_tree())
                .with_promisor_fetcher(Some(fetcher)),
        )
        .unwrap()
    }

    /// The repository without a promisor fetcher: reads never reach the network.
    fn offline(&self) -> Repository {
        Repository::open(
            &self.git_dir,
            &RepositoryOptions::empty().with_work_tree(self.work_tree()),
        )
        .unwrap()
    }

    /// Lazily fetch `oids` in one request (one promisor pack).
    fn lazy_fetch(&self, oids: &[ObjectId]) {
        let repo = self.repo();
        DefaultPromisorFetcher::new(Arc::new(NoHttp))
            .fetch_missing(&repo, oids)
            .expect("lazy fetch");
        clear_pack_cache();
    }

    fn pack_dir(&self) -> PathBuf {
        self.git_dir.join("objects/pack")
    }

    fn packs(&self) -> Vec<PathBuf> {
        let mut packs: Vec<PathBuf> = std::fs::read_dir(self.pack_dir())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "pack"))
            .collect();
        packs.sort();
        packs
    }

    /// The pack holding `oid`, if any.
    fn pack_of(&self, oid: &ObjectId) -> Option<PathBuf> {
        self.packs().into_iter().find(|pack| {
            read_pack_index(&pack.with_extension("idx"))
                .unwrap()
                .find_position(oid)
                .is_some()
        })
    }

    fn set_all_pack_mtimes(&self, when: SystemTime) {
        for pack in self.packs() {
            filetime::set_file_mtime(&pack, filetime::FileTime::from_system_time(when)).unwrap();
        }
    }

    /// Lazily fetch the three old versions in one pack and age every pack a month.
    fn with_old_versions_fetched(self) -> Self {
        self.lazy_fetch(&self.big[..3]);
        self.set_all_pack_mtimes(days_ago(30));
        self
    }

    fn run(&self, opts: &DropPromisedBlobsOptions) -> DropPromisedBlobsReport {
        let report = drop_promised_blobs(&self.offline(), opts).expect("drop");
        clear_pack_cache();
        report
    }

    /// Append a `HEAD` reflog entry moving to `to`, dated `when`.
    fn reflog_checkout(&self, to: ObjectId, when: SystemTime) {
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let zero = ObjectId::from_hex(&"0".repeat(40)).unwrap();
        let line = format!("{zero} {to} T <t@e.com> {secs} +0000\tcheckout: moving to {to}\n");
        let path = self.git_dir.join("logs/HEAD");
        let mut log = std::fs::read_to_string(&path).unwrap_or_default();
        log.push_str(&line);
        std::fs::write(path, log).unwrap();
    }
}

fn opts(min_size: u64, keep_newer_than: Option<OffsetDateTime>) -> DropPromisedBlobsOptions {
    DropPromisedBlobsOptions::new(now(), min_size, keep_newer_than)
}

fn two_weeks() -> Option<OffsetDateTime> {
    Some(now() - time::Duration::weeks(2))
}

fn dropped_oids(report: &DropPromisedBlobsReport) -> Vec<ObjectId> {
    report.dropped.iter().map(|d| d.oid).collect()
}

fn sorted(oids: &[ObjectId]) -> Vec<ObjectId> {
    let mut v = oids.to_vec();
    v.sort();
    v
}

#[test]
fn old_versions_go_and_the_checked_out_one_stays() {
    let fx = Fixture::new().with_old_versions_fetched();
    fx.lazy_fetch(&fx.big[3..]);
    fx.set_all_pack_mtimes(days_ago(30));

    let report = fx.run(&opts(LIMIT, two_weeks()));
    assert_eq!(report.candidates, 4);
    assert_eq!(dropped_oids(&report), sorted(&fx.big[..3]));
    assert_eq!(report.kept.checked_out, 1, "HEAD's big.bin");
    assert_eq!(
        report.kept.total() + report.dropped.len(),
        report.candidates
    );
    // Deltas: three versions of one file pack into about one version's bytes.
    assert!(report.bytes_reclaimed >= BIG as u64, "{report:?}");
    assert_eq!(
        report.packs_removed, 1,
        "the old versions' pack held nothing else"
    );
    assert!(report.dropped.iter().all(|d| d.size == BIG as u64));

    let repo = fx.offline();
    for oid in &fx.big[..3] {
        assert!(fx.pack_of(oid).is_none(), "{oid} still packed");
        assert!(!repo.odb().exists_stored(oid));
    }
    assert!(repo.odb().exists_stored(&fx.big[3]));
    for pack in fx.packs() {
        assert!(
            pack.with_extension("promisor").is_file(),
            "{}",
            pack.display()
        );
    }
    let fsck_report = fsck(
        &repo,
        &FsckOptions::full_repository(false),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("fsck");
    let config = repo.config().unwrap();
    assert!(
        fsck_report.ok(&config, &FsckOptions::full_repository(false)),
        "{:?}",
        fsck_report.issues
    );

    let again = fx.run(&opts(LIMIT, two_weeks()));
    assert!(again.dropped.is_empty());
    assert_eq!(again.candidates, 1);
}

#[test]
fn dropped_blobs_come_back_through_lazy_fetch() {
    let fx = Fixture::new().with_old_versions_fetched();
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(dropped_oids(&report), sorted(&fx.big[..3]));
    let repo = fx.repo();
    let blob = repo.odb().read(&fx.big[0]).expect("fetched again");
    assert_eq!(blob.kind, ObjectKind::Blob);
    assert_eq!(blob.data.len(), BIG);
}

#[test]
fn dry_run_changes_nothing_and_lists_what_a_run_drops() {
    let fx = Fixture::new().with_old_versions_fetched();
    let listing = |fx: &Fixture| -> Vec<(PathBuf, SystemTime, u64)> {
        std::fs::read_dir(fx.pack_dir())
            .unwrap()
            .flatten()
            .map(|e| {
                let meta = e.metadata().unwrap();
                (e.path(), meta.modified().unwrap(), meta.len())
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let before = listing(&fx);
    let mut dry = opts(LIMIT, two_weeks());
    dry.dry_run = true;
    let planned = fx.run(&dry);
    assert_eq!(listing(&fx), before);
    assert!(!fx.git_dir.join("gc.pid").exists());
    assert!(planned.bytes_reclaimed > 0);
    assert_eq!(planned.packs_removed, 0);

    let real = fx.run(&opts(LIMIT, two_weeks()));
    assert_eq!(dropped_oids(&planned), dropped_oids(&real));
    assert_eq!(planned.kept, real.kept);
}

#[test]
fn recently_fetched_blobs_stay() {
    let fx = Fixture::new().with_old_versions_fetched();
    let pack = fx.pack_of(&fx.big[0]).unwrap();
    filetime::set_file_mtime(&pack, filetime::FileTime::from_system_time(days_ago(1))).unwrap();
    let report = fx.run(&opts(LIMIT, two_weeks()));
    assert!(report.dropped.is_empty());
    assert_eq!(report.kept.recently_fetched, 3);
    // Without a recency window they go.
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.dropped.len(), 3);
}

#[test]
fn blobs_a_recent_head_used_stay() {
    let fx = Fixture::new().with_old_versions_fetched();
    fx.reflog_checkout(fx.commits[0], days_ago(40));
    fx.reflog_checkout(fx.commits[1], days_ago(3));
    let report = fx.run(&opts(LIMIT, two_weeks()));
    assert_eq!(report.kept.recently_checked_out, 1);
    assert_eq!(dropped_oids(&report), sorted(&[fx.big[0], fx.big[2]]));
}

#[test]
fn a_blob_also_stored_outside_promisor_packs_stays() {
    let fx = Fixture::new().with_old_versions_fetched();
    let repo = fx.offline();
    let data = repo.odb().read(&fx.big[0]).unwrap().data;
    // A loose copy too, as if written locally again.
    let reloose = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, &data)
        .unwrap();
    assert_eq!(reloose, fx.big[0]);
    let hex = reloose.to_hex();
    assert!(fx
        .git_dir
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..])
        .is_file());
    clear_pack_cache();
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.kept.outside_promisor_packs, 1);
    assert_eq!(dropped_oids(&report), sorted(&fx.big[1..3]));
    assert!(fx.offline().odb().exists_stored(&fx.big[0]));
}

#[test]
fn a_blob_no_promisor_tree_names_stays() {
    let fx = Fixture::new();
    fx.lazy_fetch(&[fx.stray]);
    fx.set_all_pack_mtimes(days_ago(30));
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.candidates, 1);
    assert_eq!(report.kept.not_promised_elsewhere, 1);
    assert!(report.dropped.is_empty());
    assert!(fx.offline().odb().exists_stored(&fx.stray));
}

#[test]
fn a_promisor_tree_no_commit_reaches_still_promises_its_blobs() {
    let fx = Fixture::new();
    fx.lazy_fetch(&[fx.stray]);
    // A tree naming the stray blob, in a promisor pack of its own, as a fetch of
    // that tree by id would leave it.
    let repo = fx.offline();
    let mut body = b"100644 stray.bin\0".to_vec();
    body.extend_from_slice(fx.stray.as_bytes());
    let tree = repo
        .odb()
        .write_loose_materialize(ObjectKind::Tree, &body)
        .unwrap();
    let pack = build_pack_from_send_list(
        repo.odb(),
        &[tree],
        &std::collections::HashSet::new(),
        &PackBuildOptions::default(),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    install_pack_bytes(
        pack,
        repo.odb(),
        &IngestPackOptions {
            mark_promisor_pack: true,
            ..IngestPackOptions::default()
        },
    )
    .unwrap();
    let hex = tree.to_hex();
    std::fs::remove_file(fx.git_dir.join("objects").join(&hex[..2]).join(&hex[2..])).unwrap();
    clear_pack_cache();
    fx.set_all_pack_mtimes(days_ago(30));
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(dropped_oids(&report), vec![fx.stray]);
}

#[test]
fn a_partly_dropped_pack_is_rewritten_with_its_markers_and_time() {
    let fx = Fixture::new().with_old_versions_fetched();
    let pack = fx.pack_of(&fx.big[0]).unwrap();
    let idx = read_pack_index(&pack.with_extension("idx")).unwrap();
    let bytes = std::fs::read(&pack).unwrap();
    // A kept delta whose base is dropped must be written out again in full.
    let delta = fx.big[..3]
        .iter()
        .copied()
        .find(|oid| {
            let offset = idx.find_offset(oid).unwrap().unwrap();
            read_packed_delta_dependency(&bytes, offset, idx.hash_bytes())
                .unwrap()
                .is_some()
        })
        .expect("the lazy fetch pack deltifies the old versions");
    let n = fx.big.iter().position(|oid| *oid == delta).unwrap();
    fx.reflog_checkout(fx.commits[n], days_ago(1));
    let expected = fx.offline().odb().read(&delta).unwrap().data;
    std::fs::write(pack.with_extension("promisor"), b"1234 refs/heads/x\n").unwrap();
    let old_mtime = std::fs::metadata(&pack).unwrap().modified().unwrap();

    let report = fx.run(&opts(LIMIT, two_weeks()));
    assert_eq!(report.dropped.len(), 2);
    assert_eq!(report.packs_rewritten, 1);
    assert!(!pack.exists());
    let new_pack = fx.pack_of(&delta).expect("kept");
    assert_eq!(
        std::fs::metadata(&new_pack).unwrap().modified().unwrap(),
        old_mtime
    );
    assert_eq!(
        std::fs::read(new_pack.with_extension("promisor")).unwrap(),
        b"1234 refs/heads/x\n"
    );
    assert_eq!(fx.offline().odb().read(&delta).unwrap().data, expected);
    // The kept blob's pack is still old: once the reflog entry ages out it goes too.
    let later = DropPromisedBlobsOptions::new(
        now() + time::Duration::days(30),
        LIMIT,
        Some(now() + time::Duration::days(16)),
    );
    let report = fx.run(&later);
    assert_eq!(dropped_oids(&report), vec![delta]);
}

#[test]
fn kept_packs_are_left_alone() {
    let fx = Fixture::new().with_old_versions_fetched();
    let pack = fx.pack_of(&fx.big[0]).unwrap();
    std::fs::write(pack.with_extension("keep"), b"pinned\n").unwrap();
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.kept.kept_pack, 3);
    assert!(report.dropped.is_empty());
    assert!(pack.exists());
}

#[test]
fn small_blobs_are_not_candidates() {
    let fx = Fixture::new().with_old_versions_fetched();
    let report = fx.run(&opts(BIG as u64 + 1, None));
    assert_eq!(report.candidates, 0);
    assert!(report.dropped.is_empty());
}

#[test]
fn refusals_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = Repository::init(tmp.path(), &InitOptions::default()).unwrap();
    assert!(matches!(
        drop_promised_blobs(&plain, &opts(LIMIT, None)),
        Err(Error::NoPromisorRemote { .. })
    ));

    let fx = Fixture::new().with_old_versions_fetched();
    std::fs::write(
        fx.git_dir.join("MERGE_HEAD"),
        format!("{}\n", fx.commits[0]),
    )
    .unwrap();
    assert!(matches!(
        drop_promised_blobs(&fx.offline(), &opts(LIMIT, None)),
        Err(Error::OperationInProgress(InProgressOperation::Merge))
    ));
    std::fs::remove_file(fx.git_dir.join("MERGE_HEAD")).unwrap();

    // A lock taken just now by a run on another machine.
    std::fs::write(fx.git_dir.join("gc.pid"), "1 some-other-host").unwrap();
    filetime::set_file_mtime(
        fx.git_dir.join("gc.pid"),
        filetime::FileTime::from_system_time(days_ago(0)),
    )
    .unwrap();
    assert!(matches!(
        drop_promised_blobs(&fx.offline(), &opts(LIMIT, None)),
        Err(Error::MaintenanceRunning { .. })
    ));
    std::fs::remove_file(fx.git_dir.join("gc.pid")).unwrap();

    append_config(&fx.git_dir, "[extensions]\n\tpreciousObjects = true\n");
    assert!(matches!(
        drop_promised_blobs(&fx.offline(), &opts(LIMIT, None)),
        Err(Error::PreciousObjectsRepository { .. })
    ));
    assert!(fx.offline().odb().exists_stored(&fx.big[0]));
}

#[test]
fn a_multi_pack_index_is_rewritten_for_the_remaining_packs() {
    let fx = Fixture::new().with_old_versions_fetched();
    fx.offline().write_midx(&MidxOptions::default()).unwrap();
    clear_pack_cache();
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.dropped.len(), 3);
    let midx = std::fs::read(fx.pack_dir().join("multi-pack-index")).unwrap();
    let packs = fx.packs();
    assert_eq!(packs.len(), 1);
    let name = packs[0]
        .with_extension("idx")
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        midx.windows(name.len()).any(|w| w == name.as_bytes()),
        "midx names the remaining pack"
    );
    let repo = fx.offline();
    assert_eq!(
        repo.odb().read(&fx.commits[0]).unwrap().kind,
        ObjectKind::Commit
    );
}

#[test]
fn gc_runs_the_phase_only_when_enabled() {
    let fx = Fixture::new().with_old_versions_fetched();
    let gc_opts = GcOptions::full_run(false, NOW);
    let stats = gc(&fx.offline(), &gc_opts).expect("gc");
    assert_eq!(
        stats.drop_large_blobs.as_ref().and_then(|r| r.skipped),
        Some(DropPromisedBlobsSkip::Disabled)
    );
    clear_pack_cache();
    assert!(fx.offline().odb().exists_stored(&fx.big[0]));

    append_config(
        &fx.git_dir,
        "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\tminSize = 100k\n",
    );
    fx.set_all_pack_mtimes(days_ago(30));
    let stats = gc(&fx.offline(), &gc_opts).expect("gc");
    clear_pack_cache();
    let report = stats.drop_large_blobs.expect("phase ran");
    assert_eq!(report.skipped, None);
    assert_eq!(dropped_oids(&report), sorted(&fx.big[..3]));
    assert!(!fx.offline().odb().exists_stored(&fx.big[0]));
}

#[test]
fn gc_skips_the_phase_during_a_merge() {
    let fx = Fixture::new().with_old_versions_fetched();
    append_config(
        &fx.git_dir,
        "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\texpire = now\n",
    );
    std::fs::write(
        fx.git_dir.join("MERGE_HEAD"),
        format!("{}\n", fx.commits[0]),
    )
    .unwrap();
    let stats = gc(&fx.offline(), &GcOptions::full_run(false, NOW)).expect("gc");
    assert_eq!(
        stats.drop_large_blobs.and_then(|r| r.skipped),
        Some(DropPromisedBlobsSkip::OperationInProgress(
            InProgressOperation::Merge
        ))
    );
}

#[test]
fn auto_gc_dry_run_reports_the_phase_and_changes_nothing() {
    let fx = Fixture::new().with_old_versions_fetched();
    append_config(
        &fx.git_dir,
        "[gc]\n\tautoPackLimit = 1\n\
         [maintenance \"drop-large-blobs\"]\n\tenabled = true\n\tminSize = 100k\n",
    );
    let before = fx.packs();
    let outcome = gc_auto(&fx.offline(), &GcOptions::full_run(true, NOW)).expect("gc --auto");
    let AutoGcOutcome::Ran { stats, .. } = outcome else {
        panic!("auto gc should run with two packs and autoPackLimit = 1");
    };
    let report = stats.drop_large_blobs.clone().expect("phase");
    assert_eq!(dropped_oids(&report), sorted(&fx.big[..3]));
    assert_eq!(fx.packs(), before);
}

#[test]
fn a_tag_in_a_promisor_pack_promises_its_blob() {
    let fx = Fixture::with_tagged_stray();
    fx.lazy_fetch(&[fx.stray]);
    fx.set_all_pack_mtimes(days_ago(30));
    let report = fx.run(&opts(LIMIT, None));
    assert_eq!(report.kept.not_promised_elsewhere, 0, "{report:?}");
    assert!(dropped_oids(&report).contains(&fx.stray), "{report:?}");
    assert!(!fx.offline().odb().exists_stored(&fx.stray));
    let blob = fx.repo().odb().read(&fx.stray).expect("fetched again");
    assert_eq!(blob.data.len(), BIG);
}

#[test]
fn gc_without_a_time_skips_the_phase() {
    let fx = Fixture::new().with_old_versions_fetched();
    append_config(
        &fx.git_dir,
        "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\texpire = now\n",
    );
    let stats = gc(&fx.offline(), &GcOptions::default()).expect("gc");
    assert_eq!(
        stats.drop_large_blobs.and_then(|r| r.skipped),
        Some(DropPromisedBlobsSkip::NoCurrentTime)
    );
    clear_pack_cache();
    assert!(fx.offline().odb().exists_stored(&fx.big[0]));
}

#[test]
fn gc_with_an_invalid_expire_fails_before_repacking() {
    let fx = Fixture::new().with_old_versions_fetched();
    append_config(
        &fx.git_dir,
        "[maintenance \"drop-large-blobs\"]\n\tenabled = true\n\texpire = bogus\n",
    );
    let repo = fx.offline();
    let loose = repo
        .odb()
        .write_loose_materialize(ObjectKind::Blob, b"loose before gc\n")
        .unwrap();
    let packs = fx.packs();
    let err = gc(&repo, &GcOptions::full_run(false, NOW)).expect_err("bad expire");
    assert!(
        matches!(
            &err,
            Error::Config(grit_lib::error::ConfigError::InvalidDate { value, .. }) if value == "bogus"
        ),
        "{err:?}"
    );
    assert_eq!(fx.packs(), packs);
    let hex = loose.to_hex();
    assert!(
        fx.git_dir
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
            .is_file(),
        "repack ran"
    );
}
