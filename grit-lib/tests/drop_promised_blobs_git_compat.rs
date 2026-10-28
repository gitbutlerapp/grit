//! [`drop_promised_blobs`] on partial clones made by Git, checked with Git: after the
//! drop `git fsck` is clean, the dropped blobs are promised (`rev-list
//! --missing=print`), the rewritten packs verify, a multi-pack-index verifies, and
//! `git cat-file` fetches a dropped blob back. Needs Git 2.27 or newer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::maintenance::{
    drop_promised_blobs, DropPromisedBlobsOptions, DropPromisedBlobsReport,
};
use grit_lib::objects::pack::clear_pack_cache;
use grit_lib::objects::promisor::{DefaultPromisorFetcher, PromisorFetcher};
use grit_lib::objects::ObjectId;
use grit_lib::remote::transport::http::HttpClient;
use grit_lib::remote::{HttpClientFactory, RemoteError, RemoteResult};
use grit_lib::repo::Repository;
use grit_test_support::{git_at_least, git_cmd, Output};
use time::OffsetDateTime;

const NOW: i64 = 4_000_000_000;
const DAY: u64 = 24 * 60 * 60;
const BIG: usize = 300 * 1024;
const LIMIT: u64 = 100 * 1024;

fn skip_old_git() -> bool {
    if git_at_least(2, 27) {
        return false;
    }
    eprintln!("SKIP: needs git 2.27 or newer for blob:limit partial clones");
    true
}

fn git_out(dir: &Path, args: &[&str]) -> Output {
    git_cmd(args).in_dir(dir).exec()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_cmd(args).in_dir(dir).suc().stdout
}

/// `git` with the committer (reflog) date set to `secs`.
fn git_at(dir: &Path, secs: u64, args: &[&str]) {
    git_cmd(args)
        .in_dir(dir)
        .env("GIT_COMMITTER_DATE", format!("@{secs} +0000"))
        .suc();
}

fn days_ago(days: u64) -> u64 {
    NOW as u64 - days * DAY
}

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

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim()).unwrap()
}

/// A Git repository with four versions of a 300 KiB `big.bin` and a Git
/// `blob:limit=100k` clone of it with `main` checked out.
struct Fixture {
    tmp: tempfile::TempDir,
    big: Vec<ObjectId>,
    commits: Vec<ObjectId>,
    stray: ObjectId,
}

impl Fixture {
    fn new(bare_client: bool) -> Self {
        Self::with_clone_args(bare_client, &[])
    }

    /// [`Self::new`] with `extra` added to the `git clone` arguments.
    fn with_clone_args(bare_client: bool, extra: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        git(tmp.path(), &["init", "-q", "-b", "main", "src"]);
        let mut data = noise(1, BIG);
        std::fs::write(src.join("small.txt"), b"small\n").unwrap();
        let mut big = Vec::new();
        let mut commits = Vec::new();
        for n in 1..=4usize {
            let at = n * 997;
            data[at..at + 64].copy_from_slice(&noise(100 + n as u64, 64));
            std::fs::write(src.join("big.bin"), &data).unwrap();
            git(&src, &["add", "big.bin", "small.txt"]);
            git(&src, &["commit", "-q", "-m", &format!("v{n}")]);
            big.push(oid(&git(&src, &["rev-parse", "HEAD:big.bin"])));
            commits.push(oid(&git(&src, &["rev-parse", "HEAD"])));
        }
        std::fs::write(tmp.path().join("stray.bin"), noise(7, BIG)).unwrap();
        let stray = oid(&git(&src, &["hash-object", "-w", "../stray.bin"]));
        git(&src, &["config", "uploadpack.allowFilter", "true"]);
        git(&src, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let url = format!("file://{}", src.display());
        let filter = format!("--filter=blob:limit={LIMIT}");
        let mut args = vec!["clone", "-q"];
        args.extend_from_slice(extra);
        if bare_client {
            args.extend_from_slice(&["--bare", &filter, &url, "client.git"]);
        } else {
            args.extend_from_slice(&[&filter, &url, "client"]);
        }
        git(tmp.path(), &args);
        Self {
            tmp,
            big,
            commits,
            stray,
        }
    }

    fn client(&self) -> PathBuf {
        let bare = self.tmp.path().join("client.git");
        if bare.is_dir() {
            bare
        } else {
            self.tmp.path().join("client")
        }
    }

    fn git_dir(&self) -> PathBuf {
        let client = self.client();
        if client.join(".git").is_dir() {
            client.join(".git")
        } else {
            client
        }
    }

    fn repo(&self) -> Repository {
        self.repo_with_index_file(None)
    }

    /// The repository, with `GIT_INDEX_FILE` set to `index_file` when given.
    fn repo_with_index_file(&self, index_file: Option<&Path>) -> Repository {
        let mut env = Environment::empty();
        env.git_index_file = index_file.map(|p| p.display().to_string());
        let mut opts = RepositoryOptions::with_environment(env);
        if self.git_dir() != self.client() {
            opts = opts.with_work_tree(self.client());
        }
        Repository::open(&self.git_dir(), &opts).unwrap()
    }

    /// Lazily fetch `oids` in one grit request, so they share one (deltified) pack.
    fn lazy_fetch(&self, oids: &[ObjectId]) {
        let fetcher: Arc<dyn PromisorFetcher> =
            Arc::new(DefaultPromisorFetcher::new(Arc::new(NoHttp)));
        fetcher
            .fetch_missing(&self.repo(), oids)
            .expect("lazy fetch");
        clear_pack_cache();
    }

    fn packs(&self) -> Vec<PathBuf> {
        let mut packs: Vec<PathBuf> = std::fs::read_dir(self.git_dir().join("objects/pack"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "pack"))
            .collect();
        packs.sort();
        packs
    }

    fn age_packs(&self, days: u64) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(days_ago(days));
        for pack in self.packs() {
            filetime::set_file_mtime(&pack, filetime::FileTime::from_system_time(when)).unwrap();
        }
    }

    fn drop(
        &self,
        dry_run: bool,
        keep_newer_than: Option<OffsetDateTime>,
    ) -> DropPromisedBlobsReport {
        self.drop_in(&self.repo(), dry_run, keep_newer_than)
    }

    fn drop_in(
        &self,
        repo: &Repository,
        dry_run: bool,
        keep_newer_than: Option<OffsetDateTime>,
    ) -> DropPromisedBlobsReport {
        let mut opts = DropPromisedBlobsOptions::new(
            OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
            LIMIT,
            keep_newer_than,
        );
        opts.dry_run = dry_run;
        let report = drop_promised_blobs(repo, &opts).expect("drop");
        clear_pack_cache();
        report
    }

    fn assert_git_fsck_clean(&self) {
        let out = git_out(&self.client(), &["fsck", "--strict", "--no-progress"]);
        assert!(out.ok(), "git fsck: {}{}", out.stdout, out.stderr);
        assert!(
            !out.stdout.contains("missing") && !out.stderr.contains("missing"),
            "git fsck: {}{}",
            out.stdout,
            out.stderr
        );
    }

    fn assert_packs_verify(&self) {
        for pack in self.packs() {
            let idx = pack.with_extension("idx");
            git(
                &self.client(),
                &["verify-pack", "-v", idx.to_str().unwrap()],
            );
        }
    }

    fn missing(&self) -> Vec<ObjectId> {
        let out = git(
            &self.client(),
            &["rev-list", "--objects", "--missing=print", "--all"],
        );
        let mut v: Vec<ObjectId> = out
            .lines()
            .filter_map(|l| l.strip_prefix('?'))
            .map(oid)
            .collect();
        v.sort();
        v
    }
}

fn two_weeks() -> Option<OffsetDateTime> {
    Some(OffsetDateTime::from_unix_timestamp(NOW).unwrap() - time::Duration::weeks(2))
}

fn sorted(oids: &[ObjectId]) -> Vec<ObjectId> {
    let mut v = oids.to_vec();
    v.sort();
    v
}

fn dropped(report: &DropPromisedBlobsReport) -> Vec<ObjectId> {
    report.dropped.iter().map(|d| d.oid).collect()
}

#[test]
fn git_sees_dropped_blobs_as_promised_and_fetches_them_back() {
    if skip_old_git() {
        return;
    }
    let fx = Fixture::new(false);
    fx.lazy_fetch(&fx.big[..3]);
    fx.age_packs(30);
    git(&fx.client(), &["multi-pack-index", "write"]);
    let before_missing = fx.missing();

    let report = fx.drop(false, two_weeks());
    assert_eq!(dropped(&report), sorted(&fx.big[..3]));
    assert_eq!(report.kept.checked_out, 1);

    fx.assert_git_fsck_clean();
    let mut expected = before_missing.clone();
    expected.extend_from_slice(&fx.big[..3]);
    expected.sort();
    assert_eq!(fx.missing(), expected);
    for pack in fx.packs() {
        assert!(pack.with_extension("promisor").is_file());
        let idx = pack.with_extension("idx");
        git(&fx.client(), &["verify-pack", "-v", idx.to_str().unwrap()]);
    }
    git(&fx.client(), &["multi-pack-index", "verify"]);

    let size = git(&fx.client(), &["cat-file", "-s", &fx.big[0].to_hex()]);
    assert_eq!(size.trim(), BIG.to_string());
}

#[test]
fn a_rewritten_pack_verifies_and_checkouts_keep_their_blobs() {
    if skip_old_git() {
        return;
    }
    let fx = Fixture::new(false);
    fx.lazy_fetch(&fx.big[..3]);
    let client = fx.client();
    // v1 in a linked worktree, v2 staged in the main index under another name.
    git(
        &client,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            "../wt",
            &fx.commits[0].to_hex(),
        ],
    );
    git(
        &client,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},old.bin", fx.big[1]),
        ],
    );
    fx.age_packs(30);
    let report = fx.drop(false, two_weeks());
    assert_eq!(report.kept.checked_out, 3, "{report:?}");
    assert_eq!(dropped(&report), vec![fx.big[2]]);
    assert_eq!(report.packs_rewritten, 1);
    fx.assert_git_fsck_clean();
    fx.assert_packs_verify();
    let shown = git(&client, &["cat-file", "-s", &fx.big[1].to_hex()]);
    assert_eq!(shown.trim(), BIG.to_string());
}

#[test]
fn blobs_of_recently_checked_out_commits_stay() {
    if skip_old_git() {
        return;
    }
    let fx = Fixture::new(false);
    fx.lazy_fetch(&fx.big[..3]);
    let client = fx.client();
    // An entry names the commit left as well as the one checked out: v1 was in use
    // from day 40 to day 39 only, v2 three days ago.
    git_at(
        &client,
        days_ago(40),
        &["checkout", "-q", &fx.commits[0].to_hex()],
    );
    git_at(&client, days_ago(39), &["checkout", "-q", "main"]);
    git_at(
        &client,
        days_ago(3),
        &["checkout", "-q", &fx.commits[1].to_hex()],
    );
    git_at(&client, days_ago(3), &["checkout", "-q", "main"]);
    fx.age_packs(30);
    let report = fx.drop(false, two_weeks());
    assert_eq!(report.kept.recently_checked_out, 1);
    assert_eq!(dropped(&report), sorted(&[fx.big[0], fx.big[2]]));
    fx.assert_git_fsck_clean();
}

#[test]
fn a_blob_only_a_local_tree_names_stays_promised_by_nothing() {
    if skip_old_git() {
        return;
    }
    // Git's fsck reports a missing blob if a promisor-pack blob that only a local
    // (non-promisor) tree names is deleted, so it must stay.
    let fx = Fixture::new(false);
    let client = fx.client();
    let size = git(&client, &["cat-file", "-s", &fx.stray.to_hex()]);
    assert_eq!(size.trim(), BIG.to_string());
    git(
        &client,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},stray.bin", fx.stray),
        ],
    );
    git(&client, &["commit", "-q", "-m", "add stray"]);
    git(&client, &["rm", "-q", "--cached", "stray.bin"]);
    git(&client, &["commit", "-q", "-m", "drop stray"]);
    git(&client, &["gc", "-q"]);
    fx.age_packs(30);
    let report = fx.drop(false, two_weeks());
    assert_eq!(report.kept.not_promised_elsewhere, 1, "{report:?}");
    assert!(report.dropped.is_empty());
    fx.assert_git_fsck_clean();
}

#[test]
fn dry_run_matches_git_repack_drop_filtered() {
    if !git_at_least(2, 56) {
        eprintln!("SKIP: needs git 2.56 or newer for repack --drop-filtered");
        return;
    }
    let fx = Fixture::new(true);
    fx.lazy_fetch(&fx.big);
    let report = fx.drop(true, None);
    let out = git(
        &fx.client(),
        &[
            "repack",
            "-a",
            "-d",
            &format!("--filter=blob:limit={LIMIT}"),
            "--drop-filtered",
            "--dry-run",
        ],
    );
    let mut git_oids: Vec<ObjectId> = out.lines().filter(|l| !l.is_empty()).map(oid).collect();
    git_oids.sort();
    assert_eq!(dropped(&report), git_oids);
    assert_eq!(dropped(&report), sorted(&fx.big));
}

#[test]
fn the_worktree_index_counts_even_with_git_index_file_set() {
    if skip_old_git() {
        return;
    }
    let fx = Fixture::new(false);
    fx.lazy_fetch(&fx.big[..3]);
    let client = fx.client();
    let alt = fx.tmp.path().join("alt-index");
    std::fs::copy(fx.git_dir().join("index"), &alt).unwrap();
    // v2 only in the real index, v1 only in the GIT_INDEX_FILE one.
    git(
        &client,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{},old.bin", fx.big[1]),
        ],
    );
    git_cmd(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("100644,{},older.bin", fx.big[0]),
    ])
    .in_dir(&client)
    .env("GIT_INDEX_FILE", alt.to_str().unwrap())
    .suc();
    fx.age_packs(30);
    let report = fx.drop_in(&fx.repo_with_index_file(Some(&alt)), true, None);
    assert_eq!(report.kept.checked_out, 3, "{report:?}");
    assert_eq!(dropped(&report), vec![fx.big[2]]);
}

#[test]
fn sparse_index_directories_protect_their_blobs() {
    if !git_at_least(2, 37) {
        eprintln!("SKIP: needs git 2.37 or newer for sparse-checkout --sparse-index");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    git(tmp.path(), &["init", "-q", "-b", "main", "src"]);
    std::fs::create_dir_all(src.join("keep")).unwrap();
    std::fs::create_dir_all(src.join("assets")).unwrap();
    std::fs::write(src.join("keep/a.txt"), b"a\n").unwrap();
    let mut blobs = Vec::new();
    for n in 1..=2u64 {
        std::fs::write(src.join("assets/big.bin"), noise(n, BIG)).unwrap();
        git(&src, &["add", "."]);
        git(&src, &["commit", "-q", "-m", &format!("v{n}")]);
        blobs.push(oid(&git(&src, &["rev-parse", "HEAD:assets/big.bin"])));
    }
    git(&src, &["config", "uploadpack.allowFilter", "true"]);
    git(&src, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let url = format!("file://{}", src.display());
    let filter = format!("--filter=blob:limit={LIMIT}");
    git(tmp.path(), &["clone", "-q", &filter, &url, "client"]);
    let client = tmp.path().join("client");
    git(
        &client,
        &["sparse-checkout", "set", "--cone", "--sparse-index", "keep"],
    );
    // HEAD moves back to v1 (never fetched); the index keeps v2's `assets/`.
    git(&client, &["reset", "-q", "--soft", "HEAD~1"]);
    let listed = git(&client, &["ls-files", "--sparse"]);
    assert!(listed.lines().any(|l| l == "assets/"), "{listed}");

    let repo = Repository::open(
        &client.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&client),
    )
    .unwrap();
    let opts = DropPromisedBlobsOptions::new(
        OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        LIMIT,
        None,
    );
    let report = drop_promised_blobs(&repo, &opts).expect("drop");
    clear_pack_cache();
    assert_eq!(report.candidates, 1, "{report:?}");
    assert_eq!(report.kept.checked_out, 1, "{report:?}");
    assert!(report.dropped.is_empty());
    assert!(repo.odb().exists_stored(&blobs[1]));
}

#[test]
fn a_linked_worktrees_recent_head_reflog_protects_its_commits() {
    if skip_old_git() {
        return;
    }
    let mut formats = vec!["files"];
    if git_at_least(2, 45) {
        formats.push("reftable");
    } else {
        eprintln!("SKIP reftable: needs git 2.45 or newer");
    }
    for format in formats {
        let ref_format = format!("--ref-format={format}");
        let extra: &[&str] = if format == "files" {
            &[]
        } else {
            &[&ref_format]
        };
        let fx = Fixture::with_clone_args(false, extra);
        fx.lazy_fetch(&fx.big[..3]);
        let client = fx.client();
        // The linked worktree was at v2 three days ago and is at v4 now.
        git_at(
            &client,
            days_ago(3),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                "../wt",
                &fx.commits[1].to_hex(),
            ],
        );
        let wt = fx.tmp.path().join("wt");
        git_at(
            &wt,
            days_ago(3),
            &["checkout", "-q", "--detach", &fx.commits[3].to_hex()],
        );
        fx.age_packs(30);
        let report = fx.drop(false, two_weeks());
        assert_eq!(report.kept.recently_checked_out, 1, "{format}: {report:?}");
        assert_eq!(
            dropped(&report),
            sorted(&[fx.big[0], fx.big[2]]),
            "{format}"
        );
        fx.assert_git_fsck_clean();
    }
}
