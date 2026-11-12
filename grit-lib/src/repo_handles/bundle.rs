//! Git bundle create/verify/list/unbundle scoped to a [`Repository`](crate::repo::Repository).

use crate::error::ObjectError;
use std::io::Write;
use std::path::Path;

use crate::bundle::bundle_remote::BundleCreateOptions;
use crate::bundle::{
    plan_bundle_create, plan_bundle_unbundle, Bundle, BundleCreatePlan, BundleError, BundleSpec,
    BundleUnbundlePlan, BundleVerifyReport, UnbundleOutcome, UnbundledRef,
};
use crate::list_limit::Limited;
use crate::objects::ObjectId;
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::transfer::{FetchOptions, FetchOutcome};

/// One ref tip from a bundle header (`git bundle list-heads`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHead {
    /// Full ref name from the bundle header.
    pub name: String,
    /// Tip object id.
    pub oid: ObjectId,
}

/// Bundle file operations for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryBundles<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryBundles<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Plan bundle creation for `spec` without writing a file.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::EmptyRefs`] when `spec.include` is empty, or pack generation fails.
    pub fn plan_create(
        &self,
        spec: &BundleSpec,
    ) -> std::result::Result<BundleCreatePlan, BundleError> {
        plan_bundle_create(self.repo, spec)
    }

    /// Plan bundle creation from revision arguments ([`BundleCreateOptions`]) without writing
    /// a file.
    ///
    /// # Errors
    ///
    /// Propagates rev-list, ref resolution, and pack failures.
    pub fn plan_create_from_revs(
        &self,
        options: &BundleCreateOptions,
    ) -> std::result::Result<BundleCreatePlan, BundleError> {
        let spec = crate::bundle_remote::resolve_bundle_spec(self.repo, options)
            .map_err(lib_error_to_bundle_error)?;
        plan_bundle_create(self.repo, &spec)
    }

    /// Plan unbundle verification and ref updates without ingesting the pack.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the bundle file is invalid.
    pub fn plan_unbundle(
        &self,
        path: &Path,
    ) -> std::result::Result<BundleUnbundlePlan, BundleError> {
        let bundle = Bundle::open(path)?;
        plan_bundle_unbundle(&bundle, self.repo)
    }

    /// Write a bundle file at `path` for `spec`, reporting pack enumeration and copy progress
    /// through `progress` and stopping when `cancel` fires.
    ///
    /// Replaces an existing file at `path` only after the pack stream completes successfully
    /// (writes to a sibling `*.bundle.part` first). On failure or cancel the previous file is
    /// unchanged and the part file is removed.
    ///
    /// Returns the number of ref tips written to the bundle header.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::EmptyRefs`] when `spec.include` is empty,
    /// [`BundleError::Cancelled`] when `cancel` fires, or pack generation errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::bundle::BundleSpec;
    /// # use grit_lib::objects::ObjectId;
    /// # use grit_lib::progress::{CancelToken, NullProgress};
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let spec = BundleSpec::default()
    ///     .with_include(vec![(ObjectId::zero(), "refs/heads/main".into())]);
    /// let _ = repo.bundles().write(
    ///     &spec,
    ///     Path::new("/tmp/x.bundle"),
    ///     &mut NullProgress,
    ///     &CancelToken::never(),
    /// );
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(
        &self,
        spec: &BundleSpec,
        path: &Path,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> std::result::Result<usize, BundleError> {
        crate::bundle_remote::write_bundle_spec_to_path(self.repo, spec, path, progress, cancel)
    }

    /// Write a bundle file at `path` from revision arguments ([`BundleCreateOptions`]: tips,
    /// `A..B` ranges, exclusions, `--all`), like `git bundle create`.
    ///
    /// Same file replacement, progress, and cancellation behavior as [`Self::write`]. Returns
    /// the number of ref tips written to the bundle header.
    ///
    /// # Errors
    ///
    /// Propagates rev-list, ref resolution, and pack failures.
    pub fn write_from_revs(
        &self,
        options: &BundleCreateOptions,
        path: &Path,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> std::result::Result<usize, BundleError> {
        let spec = crate::bundle_remote::resolve_bundle_spec(self.repo, options)
            .map_err(lib_error_to_bundle_error)?;
        self.write(&spec, path, progress, cancel)
    }

    /// Stream a bundle for `spec` into `out`: the header first, then the pack.
    ///
    /// The pack is never held in memory as a whole (filtered bundles excepted). Deltas and
    /// whole objects already stored in local packs are copied verbatim, and bundles with
    /// prerequisites are thin. Pass a buffered writer: the pack is emitted in many small
    /// writes. Nothing is cleaned up on failure; use [`Self::write`] for an atomic file.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::EmptyRefs`] when `spec.include` is empty,
    /// [`BundleError::Cancelled`] when `cancel` fires, or I/O / pack errors.
    pub fn write_to(
        &self,
        spec: &BundleSpec,
        out: &mut dyn Write,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> std::result::Result<(), BundleError> {
        crate::bundle::write_bundle(self.repo, spec, out, progress, cancel)
    }

    /// Open and verify a bundle against this repository's object format and prerequisites.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the file is invalid or hash algorithms disagree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let report = repo.bundles().verify(Path::new("changes.bundle"))?;
    /// assert!(report.is_ok() || !report.missing_prerequisites.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub fn verify(&self, path: &Path) -> std::result::Result<BundleVerifyReport, BundleError> {
        Bundle::open(path)?.verify(self.repo)
    }

    /// Parse bundle header ref lines without ingesting the pack.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the signature or header is malformed.
    pub fn list_heads(
        &self,
        path: &Path,
        limit: Option<usize>,
    ) -> std::result::Result<Limited<BundleHead>, BundleError> {
        let bundle = Bundle::open(path)?;
        let heads: Vec<BundleHead> = bundle
            .header()
            .refs
            .iter()
            .map(|(name, oid)| BundleHead {
                name: name.clone(),
                oid: *oid,
            })
            .collect();
        Ok(Limited::from_vec(heads, limit))
    }

    /// Ingest the pack stream from `path` into this repository (refs are not updated),
    /// reporting receive and index-pack progress through `progress`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] (including [`BundleError::Cancelled`]) or ingest failures.
    pub fn unbundle(
        &self,
        path: &Path,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> std::result::Result<UnbundleOutcome, BundleError> {
        Bundle::open(path)?.unbundle(self.repo, progress, cancel)
    }

    /// Fetch from the bundle file at `path` into this repository, like `git fetch` from a
    /// bundle: verify prerequisites, ingest the pack (unless [`FetchOptions::dry_run`]), and
    /// update refs according to `options` (refspecs, tag mode, prune).
    ///
    /// Objects land in this repository's object store.
    ///
    /// # Errors
    ///
    /// Propagates bundle verification, pack ingest, refspec, and ref update failures, and
    /// [`Error::Cancelled`](crate::error::Error::Cancelled) when `cancel` fires.
    pub fn fetch(
        &self,
        path: &Path,
        options: &FetchOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<FetchOutcome> {
        crate::bundle_remote::fetch_from_bundle_into(self.repo, path, options, progress, cancel)
    }

    /// Open a bundle file handle for repeated operations.
    pub fn open(&self, path: impl AsRef<Path>) -> std::result::Result<Bundle, BundleError> {
        Bundle::open(path)
    }
}

fn lib_error_to_bundle_error(err: crate::error::Error) -> BundleError {
    match err {
        crate::error::Error::Object(ObjectError::NotFound { oid }) => {
            BundleError::ObjectNotFound { oid }
        }
        other => BundleError::Io {
            detail: other.to_string(),
        },
    }
}

impl From<UnbundledRef> for BundleHead {
    fn from(value: UnbundledRef) -> Self {
        Self {
            name: value.name,
            oid: value.oid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::progress::NullProgress;
    use crate::transfer::TagMode;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex).expect("oid")
    }

    /// A git repository with two commits on `main` and an annotated tag on the first.
    fn history(dir: &Path) -> (ObjectId, ObjectId) {
        git(dir, &["init", "-q", "-b", "main", "."]);
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-q", "-m", "one"]);
        git(dir, &["tag", "-a", "v1", "-m", "v1"]);
        std::fs::write(dir.join("b.txt"), "two\n").expect("write");
        git(dir, &["add", "b.txt"]);
        git(dir, &["commit", "-q", "-m", "two"]);
        (
            oid(&git(dir, &["rev-parse", "main~1"])),
            oid(&git(dir, &["rev-parse", "main"])),
        )
    }

    fn open(dir: &Path) -> Repository {
        Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open")
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn bundles_grit_writes_verify_and_fetch_with_git() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir");
        let (c1, c2) = history(&src);
        let repo = open(&src);
        let tag = oid(&git(&src, &["rev-parse", "v1"]));

        let full = tmp.path().join("full.bundle");
        let spec = BundleSpec {
            include: vec![
                (c2, "refs/heads/main".to_owned()),
                (tag, "refs/tags/v1".to_owned()),
            ],
            ..Default::default()
        };
        repo.bundles()
            .write(&spec, &full, &mut NullProgress, &CancelToken::never())
            .expect("write full");
        let incremental = tmp.path().join("incremental.bundle");
        repo.bundles()
            .write_from_revs(
                &BundleCreateOptions::from_rev_tokens(vec!["main~1..main".to_owned()]),
                &incremental,
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("write incremental");

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir");
        git(&dest, &["init", "-q", "--bare", "."]);
        git(&dest, &["bundle", "verify", full.to_str().expect("utf-8")]);
        git(
            &dest,
            &[
                "fetch",
                "-q",
                full.to_str().expect("utf-8"),
                "refs/heads/main:refs/heads/one",
                "refs/tags/v1:refs/tags/v1",
            ],
        );
        assert_eq!(oid(&git(&dest, &["rev-parse", "one"])), c2);
        assert_eq!(oid(&git(&dest, &["rev-parse", "v1^{commit}"])), c1);
        git(&dest, &["fsck", "--strict"]);

        let fresh = tmp.path().join("fresh");
        std::fs::create_dir(&fresh).expect("mkdir");
        git(&fresh, &["init", "-q", "--bare", "."]);
        let refused = Command::new("git")
            .current_dir(&fresh)
            .args(["bundle", "verify", incremental.to_str().expect("utf-8")])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("run git");
        assert!(!refused.status.success(), "the prerequisite is missing");
        git(
            &dest,
            &["bundle", "verify", incremental.to_str().expect("utf-8")],
        );
        git(
            &dest,
            &[
                "fetch",
                "-q",
                incremental.to_str().expect("utf-8"),
                "refs/heads/main:refs/heads/two",
            ],
        );
        assert_eq!(oid(&git(&dest, &["rev-parse", "two"])), c2);
        git(&dest, &["fsck", "--strict"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn bundles_git_writes_and_grit_fetches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir");
        let (c1, c2) = history(&src);
        let full = tmp.path().join("full.bundle");
        git(&src, &["branch", "first", "main~1"]);
        git(
            &src,
            &[
                "bundle",
                "create",
                full.to_str().expect("utf-8"),
                "first",
                "--tags",
            ],
        );
        let incremental = tmp.path().join("incremental.bundle");
        git(
            &src,
            &[
                "bundle",
                "create",
                incremental.to_str().expect("utf-8"),
                "main~1..main",
            ],
        );

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir");
        git(&dest, &["init", "-q", "--bare", "."]);
        let repo = open(&dest);
        let mut options = FetchOptions::default();
        options.refspecs = vec!["+refs/heads/*:refs/remotes/bundle/*".to_owned()];
        options.tags = TagMode::Following;

        let incomplete = repo.bundles().fetch(
            &incremental,
            &options,
            &mut NullProgress,
            &CancelToken::never(),
        );
        assert!(incomplete.is_err(), "the prerequisite is missing");

        let fetched = repo
            .bundles()
            .fetch(&full, &options, &mut NullProgress, &CancelToken::never())
            .expect("fetch full");
        assert!(fetched.changed_ref_count() >= 1);
        assert_eq!(
            git(&dest, &["rev-parse", "refs/remotes/bundle/first"]),
            c1.to_hex()
        );
        assert_eq!(git(&dest, &["rev-parse", "v1^{commit}"]), c1.to_hex());

        repo.bundles()
            .fetch(
                &incremental,
                &options,
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("fetch incremental");
        assert_eq!(
            git(&dest, &["rev-parse", "refs/remotes/bundle/main"]),
            c2.to_hex()
        );
        git(&dest, &["fsck", "--strict"]);
    }
}
