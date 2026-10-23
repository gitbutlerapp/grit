//! Git bundle create/verify/list/unbundle scoped to a [`Repository`](crate::repo::Repository).

use std::fs::File;
use std::path::Path;

use crate::bundle::bundle_remote::BundleCreateOptions;
use crate::bundle::{
    plan_bundle_create, plan_bundle_unbundle, write_bundle, Bundle, BundleCreatePlan, BundleError,
    BundleSpec, BundleUnbundlePlan, BundleVerifyReport, UnbundleOutcome, UnbundledRef,
};
use crate::objects::ObjectId;
use crate::progress::{NullProgress, ProgressSink};
use crate::repo::Repository;

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

    /// Plan bundle creation from CLI-style revision options.
    ///
    /// # Errors
    ///
    /// Propagates rev-list, ref resolution, and pack failures.
    pub fn plan_create_with_options(
        &self,
        options: &BundleCreateOptions,
    ) -> std::result::Result<BundleCreatePlan, BundleError> {
        let spec = crate::bundle::bundle_remote::resolve_bundle_spec(self.repo, options)
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

    /// Write a bundle to `path` using `spec`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when `spec.include` is empty or pack generation fails.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::bundle::BundleSpec;
    /// # use grit_lib::objects::ObjectId;
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let spec = BundleSpec {
    ///     include: vec![(ObjectId::zero(), "refs/heads/main".into())],
    ///     ..Default::default()
    /// };
    /// let _ = repo.bundles().create(&spec, Path::new("/tmp/x.bundle"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn create(&self, spec: &BundleSpec, path: &Path) -> std::result::Result<(), BundleError> {
        let file = File::create(path).map_err(|e| BundleError::Io(e.to_string()))?;
        let mut out = std::io::BufWriter::new(file);
        write_bundle(self.repo, spec, &mut out)
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
    pub fn list_heads(&self, path: &Path) -> std::result::Result<Vec<BundleHead>, BundleError> {
        let bundle = Bundle::open(path)?;
        Ok(bundle
            .header()
            .refs
            .iter()
            .map(|(name, oid)| BundleHead {
                name: name.clone(),
                oid: *oid,
            })
            .collect())
    }

    /// Ingest the pack stream from `path` into this repository (refs are not updated).
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] or ingest failures.
    pub fn unbundle(&self, path: &Path) -> std::result::Result<UnbundleOutcome, BundleError> {
        Bundle::open(path)?.unbundle_with_progress(self.repo, &mut NullProgress)
    }

    /// Like [`Self::unbundle`], reporting progress through `progress`.
    pub fn unbundle_with_progress(
        &self,
        path: &Path,
        progress: &mut dyn ProgressSink,
    ) -> std::result::Result<UnbundleOutcome, BundleError> {
        Bundle::open(path)?.unbundle_with_progress(self.repo, progress)
    }

    /// Open a bundle file handle for repeated operations.
    pub fn open(&self, path: impl AsRef<Path>) -> std::result::Result<Bundle, BundleError> {
        Bundle::open(path)
    }
}

fn lib_error_to_bundle_error(err: crate::error::Error) -> BundleError {
    match err {
        crate::error::Error::BundleMissingPrerequisites => BundleError::PrerequisitesNotConnected,
        other => BundleError::Io(other.to_string()),
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
