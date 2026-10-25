//! Git bundle format ([gitformat-bundle](https://git-scm.com/docs/gitformat-bundle)).
//!
//! Bundles combine a text header (signature, optional v3 capabilities, prerequisite
//! commits, ref tips) with a thin packfile. This module reads and writes that format,
//! verifies prerequisite connectivity against a local repository, and ingests the pack
//! without updating refs (callers apply [`BundleHeader::refs`] themselves).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, Read, Seek, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::commit_pretty::message_subject;
use crate::connectivity::bundle_prerequisites_connected_to_refs;
use crate::error::{Error, Result};
use crate::index_pack::{ingest_received_pack_path_with_progress, IngestPackOptions};
use crate::objects::{parse_commit, HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack_objects::{write_upload_pack_stream, PackStats};
use crate::progress::{NullProgress, ProgressPhase, ProgressSink, ProgressUnit};
use crate::repo::Repository;
use crate::rev_list::{
    FilterObjectKind, MissingAction, ObjectFilter, RevListOptions, RevListResult,
};
use crate::transfer::{build_pack_from_send_list, reachable_closure, PackBuildOptions};

const V2_SIGNATURE: &str = "# v2 git bundle\n";
const V3_SIGNATURE: &str = "# v3 git bundle\n";

/// Supported on-disk bundle header versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleVersion {
    /// Legacy header (SHA-1 repositories without filter capabilities).
    V2,
    /// Capabilities for object format and partial-clone filter.
    V3,
}

/// Wire filter specification carried in v3 `@filter=` capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec(ObjectFilter);

impl FilterSpec {
    /// Parse a filter capability value (same grammar as `--filter=` on fetch).
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::MalformedLine`] when the spec is invalid.
    pub fn parse(spec: &str) -> std::result::Result<Self, BundleError> {
        ObjectFilter::parse(spec)
            .map(Self)
            .map_err(BundleError::MalformedLine)
    }

    /// Borrow the parsed filter.
    #[must_use]
    pub fn as_filter(&self) -> &ObjectFilter {
        &self.0
    }

    /// Serialize for an `@filter=` capability line (percent-encoding when needed).
    #[must_use]
    pub fn to_capability_value(&self) -> String {
        filter_to_capability_value(&self.0)
    }
}

/// Parsed bundle header (everything before the `PACK` stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleHeader {
    /// Header version (`2` or `3`).
    pub version: BundleVersion,
    /// Object hash used in the pack and OID lines.
    pub object_format: HashAlgo,
    /// Partial-clone filter advertised by the bundle (v3 only).
    pub filter: Option<FilterSpec>,
    /// Prerequisite commits that must already exist (`-oid` lines).
    pub prerequisites: Vec<(ObjectId, Option<String>)>,
    /// Ref tips included in the pack (`oid refname` lines).
    pub refs: Vec<(String, ObjectId)>,
}

/// A single ref entry returned from [`Bundle::unbundle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnbundledRef {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: String,
    /// Tip object id.
    pub oid: ObjectId,
}

/// Result of ingesting a bundle's embedded pack into a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnbundleOutcome {
    /// Ref tips from the bundle header (refs are not updated on disk by unbundle).
    pub refs: Vec<UnbundledRef>,
    /// Objects and bytes installed from the bundle pack.
    pub transfer: crate::transfer::TransferStats,
    /// Object ids indexed from the installed pack.
    pub pack_object_ids: HashSet<ObjectId>,
}

/// Outcome of [`Bundle::verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleVerifyReport {
    /// Prerequisites missing from the object database.
    pub missing_prerequisites: Vec<(ObjectId, Option<String>)>,
    /// Prerequisites exist as objects but are not reachable from local refs.
    pub prerequisites_not_connected: bool,
}

impl BundleVerifyReport {
    /// Whether verification succeeded with no issues.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.missing_prerequisites.is_empty() && !self.prerequisites_not_connected
    }

    /// Convert validation failures into a [`BundleError`].
    ///
    /// Returns `Ok(())` when [`Self::is_ok`].
    ///
    /// # Errors
    ///
    /// Returns the first missing prerequisite, or [`BundleError::PrerequisitesNotConnected`].
    pub fn into_result(self) -> std::result::Result<(), BundleError> {
        if let Some((oid, subject)) = self.missing_prerequisites.into_iter().next() {
            return Err(BundleError::MissingPrerequisite { oid, subject });
        }
        if self.prerequisites_not_connected {
            return Err(BundleError::PrerequisitesNotConnected);
        }
        Ok(())
    }
}

/// Bundle read/write/verify failures.
#[non_exhaustive]
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// The file does not begin with a v2/v3 bundle signature.
    #[error("not a v2 or v3 git bundle")]
    BadSignature,
    /// Only versions 2 and 3 are supported.
    #[error("unsupported bundle version")]
    UnsupportedVersion,
    /// A v3 `@` capability was not recognized.
    #[error("unknown bundle capability: {0}")]
    UnsupportedCapability(String),
    /// A header line could not be parsed.
    #[error("malformed bundle header: {0}")]
    MalformedLine(String),
    /// Header object format disagrees with the repository.
    #[error("bundle object format {bundle_name} does not match repository {repo_name}")]
    ObjectFormatMismatch {
        /// Format recorded in the bundle header.
        bundle_name: &'static str,
        /// Format configured for the open repository.
        repo_name: &'static str,
    },
    /// A prerequisite commit is absent from the ODB.
    #[error("missing prerequisite commit {oid}")]
    MissingPrerequisite {
        /// Required commit oid.
        oid: ObjectId,
        /// Optional subject from the header line.
        subject: Option<String>,
    },
    /// Prerequisite objects exist but are not connected to local refs.
    #[error("prerequisite commits are not connected to local refs")]
    PrerequisitesNotConnected,
    /// No ref tips were listed in the header.
    #[error("bundle contains no refs")]
    EmptyRefs,
    /// The pack stream did not start where expected.
    #[error("bundle pack stream missing or invalid")]
    MissingPack,
    /// The repository uses a custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary)
    /// store; ingesting the pack needs the files-backed `objects/` directory.
    #[error("{operation} requires the files-backed object store; it is not supported with a custom primary")]
    UnsupportedObjectStore {
        /// Refused bundle operation (`"unbundle"`).
        operation: &'static str,
    },
    /// Underlying I/O failure.
    #[error("{0}")]
    Io(String),
    /// The operation was cancelled before the bundle finished unpacking.
    #[error("operation cancelled")]
    Cancelled,
}

impl From<std::io::Error> for BundleError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Input for [`write_bundle`].
#[derive(Debug, Clone, Default)]
pub struct BundleSpec {
    /// Tips to include (`oid` + ref name for the header).
    pub include: Vec<(ObjectId, String)>,
    /// Commits treated as already present (incremental bundles).
    pub exclude: Vec<ObjectId>,
    /// Optional partial-clone filter (forces v3).
    pub filter: Option<FilterSpec>,
}

/// One ref tip that would appear in a bundle header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRefEntry {
    /// Full ref name (for example `refs/heads/main`).
    pub name: String,
    /// Tip object id.
    pub oid: ObjectId,
}

/// One prerequisite commit line (`-oid`) for an incremental bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePrerequisite {
    /// Required commit oid.
    pub oid: ObjectId,
    /// Optional subject from the commit message.
    pub subject: Option<String>,
}

/// Planned bundle contents without writing a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleCreatePlan {
    /// Ref tips that would be listed in the header.
    pub refs: Vec<BundleRefEntry>,
    /// Prerequisite commits the recipient must already have.
    pub prerequisites: Vec<BundlePrerequisite>,
    /// Object count encoded in the pack stream.
    pub object_count: u32,
}

/// One ref movement that would occur when applying bundle header tips locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRefUpdatePlan {
    /// Ref to update.
    pub name: String,
    /// Current tip when the ref exists locally.
    pub old_oid: Option<ObjectId>,
    /// Tip from the bundle header.
    pub new_oid: ObjectId,
}

/// Planned unbundle work: verification outcome and ref updates without ingesting the pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleUnbundlePlan {
    /// Verification against the open repository.
    pub verify: BundleVerifyReport,
    /// Ref updates implied by the bundle header (refs are not written during unbundle).
    pub ref_updates: Vec<BundleRefUpdatePlan>,
    /// Object count in the embedded pack.
    pub object_count: u32,
}

/// An opened bundle file on disk.
#[derive(Debug, Clone)]
pub struct Bundle {
    path: PathBuf,
    header: BundleHeader,
    pack_offset: u64,
}

impl Bundle {
    /// Open `path` and parse the header, leaving the pack at `pack_offset`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the signature or header lines are invalid.
    pub fn open(path: impl AsRef<Path>) -> std::result::Result<Self, BundleError> {
        let path = path.as_ref().to_path_buf();
        let (header, pack_offset) = read_header_from_file(&path)?;
        Ok(Self {
            path,
            header,
            pack_offset,
        })
    }

    /// Parsed header.
    #[must_use]
    pub fn header(&self) -> &BundleHeader {
        &self.header
    }

    /// Path passed to [`Self::open`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Object count from the embedded pack header.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::MissingPack`] when the pack stream is absent or truncated.
    pub fn pack_object_count(&self) -> std::result::Result<u32, BundleError> {
        bundle_pack_object_count(self)
    }

    /// Byte offset in [`Self::path`] where the `PACK` stream begins.
    #[must_use]
    pub fn pack_byte_offset(&self) -> u64 {
        self.pack_offset
    }

    /// Check prerequisites against `repo` and ref connectivity.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::ObjectFormatMismatch`] when hash algorithms disagree.
    pub fn verify(
        &self,
        repo: &Repository,
    ) -> std::result::Result<BundleVerifyReport, BundleError> {
        let repo_algo = repo.odb().hash_algo();
        if self.header.object_format != repo_algo {
            return Err(BundleError::ObjectFormatMismatch {
                bundle_name: self.header.object_format.name(),
                repo_name: repo_algo.name(),
            });
        }
        let mut report = BundleVerifyReport::default();
        for (oid, subject) in &self.header.prerequisites {
            if repo.odb().read(oid).is_err() {
                report.missing_prerequisites.push((*oid, subject.clone()));
            }
        }
        if report.missing_prerequisites.is_empty() {
            let prereq_oids: Vec<ObjectId> =
                self.header.prerequisites.iter().map(|(o, _)| *o).collect();
            let connected = bundle_prerequisites_connected_to_refs(repo, &prereq_oids)
                .map_err(|e| BundleError::Io(e.to_string()))?;
            report.prerequisites_not_connected = !connected;
        }
        Ok(report)
    }

    /// Stream the embedded pack into `repo`'s ODB (does not update refs).
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] or ingest failures wrapped as [`BundleError::Io`], and
    /// [`BundleError::UnsupportedObjectStore`] before reading the pack when `repo` uses a
    /// custom primary object store.
    pub fn unbundle(
        &self,
        repo: &Repository,
    ) -> std::result::Result<Vec<UnbundledRef>, BundleError> {
        Ok(self
            .unbundle_with_progress(repo, &mut NullProgress, &crate::progress::NeverCancel)?
            .refs)
    }

    /// Like [`Self::unbundle`], reporting receive and index-pack progress through `progress`.
    pub fn unbundle_with_progress(
        &self,
        repo: &Repository,
        progress: &mut dyn ProgressSink,
        cancel: &dyn crate::progress::Cancel,
    ) -> std::result::Result<UnbundleOutcome, BundleError> {
        if !repo.odb().uses_files_primary() {
            return Err(BundleError::UnsupportedObjectStore {
                operation: "unbundle",
            });
        }
        crate::progress::poll_cancel(progress, cancel).map_err(|e| match e {
            crate::error::Error::Cancelled => BundleError::Cancelled,
            other => BundleError::Io(other.to_string()),
        })?;
        if self.header.object_format != repo.odb().hash_algo() {
            return Err(BundleError::ObjectFormatMismatch {
                bundle_name: self.header.object_format.name(),
                repo_name: repo.odb().hash_algo().name(),
            });
        }
        let pack_bytes = std::fs::metadata(&self.path)
            .map(|m| m.len().saturating_sub(self.pack_offset))
            .unwrap_or(0);
        progress.start_with_unit(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            Some(pack_bytes),
        );
        let tmp = tempfile::NamedTempFile::new_in(repo.odb().objects_dir().join("pack"))
            .map_err(|e| BundleError::Io(e.to_string()))?;
        {
            let mut input = File::open(&self.path)?;
            Seek::seek(&mut input, std::io::SeekFrom::Start(self.pack_offset))
                .map_err(BundleError::from)?;
            let mut output = std::io::BufWriter::new(tmp.as_file());
            let mut copied = 0u64;
            let mut buf = [0u8; 64 * 1024];
            loop {
                crate::progress::poll_cancel(progress, cancel).map_err(|e| match e {
                    crate::error::Error::Cancelled => BundleError::Cancelled,
                    other => BundleError::Io(other.to_string()),
                })?;
                let n = input.read(&mut buf).map_err(BundleError::from)?;
                if n == 0 {
                    break;
                }
                output.write_all(&buf[..n]).map_err(BundleError::from)?;
                copied = copied.saturating_add(n as u64);
                progress.set(copied);
            }
            output.flush().map_err(BundleError::from)?;
        }
        progress.finish();
        crate::progress::poll_cancel(progress, cancel).map_err(|e| match e {
            crate::error::Error::Cancelled => BundleError::Cancelled,
            other => BundleError::Io(other.to_string()),
        })?;
        let opts = IngestPackOptions {
            fix_thin: true,
            ..Default::default()
        };
        let ingested = ingest_received_pack_path_with_progress(
            tmp.path().to_path_buf(),
            repo.odb(),
            &opts,
            Some(progress),
            Some(cancel),
        )
        .map_err(|e| match e {
            crate::error::Error::Cancelled => BundleError::Cancelled,
            other => BundleError::Io(other.to_string()),
        })?;
        if self.header.filter.is_some() {
            std::fs::write(ingested.pack_path.with_extension("promisor"), b"")
                .map_err(|e| BundleError::Io(e.to_string()))?;
            repo.odb().invalidate_packs();
        }
        let transfer = crate::transfer::TransferStats::from_ingested_pack(&ingested)
            .map_err(|e| BundleError::Io(e.to_string()))?;
        let refs = self
            .header
            .refs
            .iter()
            .map(|(name, oid)| UnbundledRef {
                name: name.clone(),
                oid: *oid,
            })
            .collect();
        Ok(UnbundleOutcome {
            refs,
            transfer,
            pack_object_ids: ingested.object_ids,
        })
    }
}

/// Parse a bundle header from `reader`, leaving the cursor at the first `PACK` byte.
///
/// # Errors
///
/// Returns [`BundleError`] when the signature or header is invalid.
pub fn read_header(reader: &mut dyn BufRead) -> std::result::Result<BundleHeader, BundleError> {
    let header = read_header_lines(reader)?;
    ensure_pack_magic(reader)?;
    Ok(header)
}

/// Parse the signature, capability, prerequisite and ref lines up to and including the
/// blank line that ends the header, without inspecting the pack that follows.
fn read_header_lines(reader: &mut dyn BufRead) -> std::result::Result<BundleHeader, BundleError> {
    let signature = read_line(reader)?;
    let version = match signature.as_str() {
        s if s == V2_SIGNATURE.trim_end() => BundleVersion::V2,
        s if s == V3_SIGNATURE.trim_end() => BundleVersion::V3,
        _ => return Err(BundleError::BadSignature),
    };

    let mut object_format = HashAlgo::Sha1;
    let mut filter = None;
    let mut prerequisites = Vec::new();
    let mut refs = Vec::new();

    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        if version == BundleVersion::V3 && line.starts_with('@') {
            parse_capability(&line[1..], &mut object_format, &mut filter)?;
            continue;
        }
        let (is_prereq, rest) = if let Some(stripped) = line.strip_prefix('-') {
            (true, stripped)
        } else {
            (false, line.as_str())
        };
        let (oid, tail) = parse_oid_field(rest, object_format)?;
        if is_prereq {
            let subject = tail
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            prerequisites.push((oid, subject));
        } else {
            let refname = tail.ok_or_else(|| {
                BundleError::MalformedLine("ref line missing ref name".to_owned())
            })?;
            let refname = refname.trim();
            if refname.is_empty() {
                return Err(BundleError::MalformedLine(
                    "ref line missing ref name".to_owned(),
                ));
            }
            refs.push((refname.to_owned(), oid));
        }
    }

    Ok(BundleHeader {
        version,
        object_format,
        filter,
        prerequisites,
        refs,
    })
}

/// Buffer size for streaming the header; most headers fit in one read, and the pack that
/// follows (often far larger than the header) is never pulled into memory.
const HEADER_READ_BUF: usize = 64 * 1024;

fn read_header_from_file(path: &Path) -> std::result::Result<(BundleHeader, u64), BundleError> {
    let file = File::open(path).map_err(BundleError::from)?;
    let mut reader = std::io::BufReader::with_capacity(HEADER_READ_BUF, file);
    let header = read_header_lines(&mut reader)?;
    let mut pack_offset = reader.stream_position().map_err(BundleError::from)?;
    let mut magic = [0u8; 4];
    loop {
        read_exact_or_missing_pack(&mut reader, &mut magic[..1])?;
        if magic[0] != b'\n' && magic[0] != b'\r' {
            break;
        }
        pack_offset += 1;
    }
    read_exact_or_missing_pack(&mut reader, &mut magic[1..])?;
    if &magic != b"PACK" {
        return Err(BundleError::MissingPack);
    }
    Ok((header, pack_offset))
}

fn read_exact_or_missing_pack(
    reader: &mut dyn std::io::Read,
    buf: &mut [u8],
) -> std::result::Result<(), BundleError> {
    reader.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => BundleError::MissingPack,
        _ => BundleError::from(e),
    })
}

fn ensure_pack_magic(reader: &mut dyn BufRead) -> std::result::Result<(), BundleError> {
    loop {
        let buf = reader.fill_buf().map_err(BundleError::from)?;
        if buf.is_empty() {
            return Err(BundleError::MissingPack);
        }
        if buf.starts_with(b"PACK") {
            return Ok(());
        }
        if buf[0] == b'\n' || buf[0] == b'\r' {
            reader.consume(1);
            continue;
        }
        return Err(BundleError::MissingPack);
    }
}

/// Plan a bundle for `spec` without writing a file.
///
/// # Errors
///
/// Returns [`BundleError::EmptyRefs`] when `spec.include` is empty, or pack generation failures.
pub fn plan_bundle_create(
    repo: &Repository,
    spec: &BundleSpec,
) -> std::result::Result<BundleCreatePlan, BundleError> {
    if spec.include.is_empty() {
        return Err(BundleError::EmptyRefs);
    }
    let header = bundle_header_parts(repo, spec)?;
    let object_count = write_bundle_pack(repo, spec, &mut std::io::sink())?.object_count;
    Ok(BundleCreatePlan {
        refs: header
            .refs
            .into_iter()
            .map(|(oid, name)| BundleRefEntry { name, oid })
            .collect(),
        prerequisites: header
            .prerequisites
            .into_iter()
            .map(|(oid, subject)| BundlePrerequisite { oid, subject })
            .collect(),
        object_count,
    })
}

/// Plan unbundle verification and ref updates without ingesting the pack or writing refs.
///
/// # Errors
///
/// Returns [`BundleError`] when the bundle cannot be read or verified.
pub fn plan_bundle_unbundle(
    bundle: &Bundle,
    repo: &Repository,
) -> std::result::Result<BundleUnbundlePlan, BundleError> {
    let verify = bundle.verify(repo)?;
    let ref_updates = planned_ref_updates_from_header(repo, &bundle.header().refs);
    let object_count = bundle_pack_object_count(bundle)?;
    Ok(BundleUnbundlePlan {
        verify,
        ref_updates,
        object_count,
    })
}

fn bundle_pack_object_count(bundle: &Bundle) -> std::result::Result<u32, BundleError> {
    let mut input = File::open(&bundle.path).map_err(BundleError::from)?;
    Seek::seek(&mut input, std::io::SeekFrom::Start(bundle.pack_offset))
        .map_err(BundleError::from)?;
    let mut header = [0u8; 12];
    read_exact_or_missing_pack(&mut input, &mut header)?;
    Ok(crate::receive_pack::pack_object_count(&header).unwrap_or(0))
}

fn planned_ref_updates_from_header(
    repo: &Repository,
    refs: &[(String, ObjectId)],
) -> Vec<BundleRefUpdatePlan> {
    let git_dir = repo.git_dir();
    let mut updates = Vec::new();
    for (name, new_oid) in refs {
        if name.ends_with("^{}") {
            continue;
        }
        let old_oid = crate::refs::resolve_ref(git_dir, name).ok();
        if old_oid.as_ref() == Some(new_oid) {
            continue;
        }
        updates.push(BundleRefUpdatePlan {
            name: name.clone(),
            old_oid,
            new_oid: *new_oid,
        });
    }
    updates
}

struct BundleHeaderParts {
    prerequisites: Vec<(ObjectId, Option<String>)>,
    refs: Vec<(ObjectId, String)>,
}

fn bundle_header_parts(
    repo: &Repository,
    spec: &BundleSpec,
) -> std::result::Result<BundleHeaderParts, BundleError> {
    let include_oids: Vec<ObjectId> = spec.include.iter().map(|(o, _)| *o).collect();
    let prerequisites = prerequisite_commits(repo.odb(), &include_oids, &spec.exclude)?;
    Ok(BundleHeaderParts {
        prerequisites,
        refs: spec.include.clone(),
    })
}

/// Stream the bundle's pack to `out` and return its statistics.
///
/// Uses the same settings as serving a fetch ([`PackBuildOptions::for_serve`]): deltas
/// and whole objects already in local packs are copied, and only blobs not already
/// stored as deltas enter the delta search, so memory stays bounded by the pack index
/// instead of the inflated object size. Bundles with prerequisites are thin.
fn write_bundle_pack(
    repo: &Repository,
    spec: &BundleSpec,
    out: &mut dyn Write,
) -> std::result::Result<PackStats, BundleError> {
    let io_err = |e: Error| BundleError::Io(e.to_string());
    let include_oids: Vec<ObjectId> = spec.include.iter().map(|(o, _)| *o).collect();
    let mut haves: Vec<ObjectId> = spec
        .exclude
        .iter()
        .map(|oid| peel_to_commit(repo.odb(), *oid))
        .collect::<Result<Vec<_>>>()
        .map_err(io_err)?;
    haves.sort();
    haves.dedup();
    let cfg = repo.config().ok();
    let pack_opts = PackBuildOptions::for_serve(cfg.as_deref(), !haves.is_empty(), true);
    let no_shallow = HashSet::new();
    let Some(filter) = spec.filter.as_ref() else {
        return write_upload_pack_stream(
            repo,
            &include_oids,
            &haves,
            &no_shallow,
            &no_shallow,
            None,
            &pack_opts,
            &mut |_| {},
            out,
        )
        .map_err(io_err);
    };
    let have_closure = reachable_closure(repo.odb(), &haves, &HashSet::new(), true, &no_shallow)
        .map_err(io_err)?;
    let send = filtered_pack_objects(
        repo,
        &include_oids,
        &spec.exclude,
        filter.as_filter(),
        &have_closure,
    )
    .map_err(io_err)?;
    let pack = build_pack_from_send_list(
        repo.odb(),
        &send,
        &have_closure,
        &pack_opts,
        &HashMap::new(),
    )
    .map_err(io_err)?;
    out.write_all(&pack).map_err(BundleError::from)?;
    Ok(PackStats {
        object_count: crate::receive_pack::pack_object_count(&pack).unwrap_or(0),
        pack_bytes: pack.len() as u64,
        ..PackStats::default()
    })
}

/// Write a bundle to `out` from `repo` using `spec`.
///
/// The header is written first, then the pack is streamed straight into `out`; the
/// pack is never held in memory as a whole (filtered bundles excepted). Deltas and
/// whole objects already stored in local packs are copied verbatim rather than
/// recomputed, and bundles with prerequisites are thin (deltas may name a
/// prerequisite-reachable base the recipient already holds). Pass a buffered writer:
/// the pack is emitted in many small writes.
///
/// # Errors
///
/// Returns [`BundleError::EmptyRefs`] when `spec.include` is empty, or I/O / pack errors.
pub fn write_bundle(
    repo: &Repository,
    spec: &BundleSpec,
    out: &mut dyn Write,
) -> std::result::Result<(), BundleError> {
    if spec.include.is_empty() {
        return Err(BundleError::EmptyRefs);
    }
    let algo = repo.odb().hash_algo();
    let version = if algo != HashAlgo::Sha1 || spec.filter.is_some() {
        BundleVersion::V3
    } else {
        BundleVersion::V2
    };
    write_signature(out, version)?;
    if version == BundleVersion::V3 {
        write_capability_object_format(out, algo)?;
        if let Some(filter) = &spec.filter {
            write_capability_filter(out, filter)?;
        }
    }

    let header = bundle_header_parts(repo, spec)?;
    for (oid, subject) in &header.prerequisites {
        write!(out, "-{} ", oid.to_hex()).map_err(|e| BundleError::Io(e.to_string()))?;
        if let Some(s) = subject {
            write!(out, "{s}").map_err(|e| BundleError::Io(e.to_string()))?;
        }
        writeln!(out).map_err(|e| BundleError::Io(e.to_string()))?;
    }

    for (oid, name) in &header.refs {
        writeln!(out, "{} {name}", oid.to_hex()).map_err(|e| BundleError::Io(e.to_string()))?;
    }
    writeln!(out).map_err(|e| BundleError::Io(e.to_string()))?;

    write_bundle_pack(repo, spec, out)?;
    Ok(())
}

/// Incremental writer for bundle bytes (header + pack).
pub struct BundleWriter<W: Write> {
    inner: W,
    finished_header: bool,
}

impl<W: Write> BundleWriter<W> {
    /// Create a writer; call [`Self::write_header_and_pack`] to emit a complete bundle.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            finished_header: false,
        }
    }

    /// Write the header and pack in one step.
    ///
    /// # Errors
    ///
    /// Same as [`write_bundle`].
    pub fn write_header_and_pack(
        mut self,
        repo: &Repository,
        spec: &BundleSpec,
    ) -> std::result::Result<(), BundleError> {
        write_bundle(repo, spec, &mut self.inner)?;
        self.finished_header = true;
        Ok(())
    }
}

fn write_signature(
    out: &mut dyn Write,
    version: BundleVersion,
) -> std::result::Result<(), BundleError> {
    let sig = match version {
        BundleVersion::V2 => V2_SIGNATURE,
        BundleVersion::V3 => V3_SIGNATURE,
    };
    out.write_all(sig.as_bytes())
        .map_err(|e| BundleError::Io(e.to_string()))
}

fn write_capability_object_format(
    out: &mut dyn Write,
    algo: HashAlgo,
) -> std::result::Result<(), BundleError> {
    writeln!(out, "@object-format={}", algo.name()).map_err(|e| BundleError::Io(e.to_string()))
}

fn write_capability_filter(
    out: &mut dyn Write,
    filter: &FilterSpec,
) -> std::result::Result<(), BundleError> {
    writeln!(out, "@filter={}", filter.to_capability_value())
        .map_err(|e| BundleError::Io(e.to_string()))
}

fn parse_capability(
    cap: &str,
    object_format: &mut HashAlgo,
    filter: &mut Option<FilterSpec>,
) -> std::result::Result<(), BundleError> {
    if let Some(name) = cap.strip_prefix("object-format=") {
        *object_format = HashAlgo::from_name(name)
            .ok_or_else(|| BundleError::MalformedLine(format!("unknown object format: {name}")))?;
        return Ok(());
    }
    if let Some(spec) = cap.strip_prefix("filter=") {
        *filter = Some(FilterSpec::parse(spec)?);
        return Ok(());
    }
    Err(BundleError::UnsupportedCapability(cap.to_owned()))
}

fn read_line(reader: &mut dyn BufRead) -> std::result::Result<String, BundleError> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(BundleError::from)?;
    if n == 0 {
        return Err(BundleError::MalformedLine(
            "unexpected end of header".to_owned(),
        ));
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn parse_oid_field(
    line: &str,
    algo: HashAlgo,
) -> std::result::Result<(ObjectId, Option<&str>), BundleError> {
    let hex_len = algo.hex_len();
    if line.len() < hex_len {
        return Err(BundleError::MalformedLine("short object id".to_owned()));
    }
    let (hex, tail) = line.split_at(hex_len);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(BundleError::MalformedLine("invalid object id".to_owned()));
    }
    let oid = ObjectId::from_hex(hex)
        .map_err(|_| BundleError::MalformedLine("invalid object id".to_owned()))?;
    if hex.len() != algo.hex_len() {
        return Err(BundleError::MalformedLine(
            "object id length mismatch".to_owned(),
        ));
    }
    let tail = match tail.chars().next() {
        None => None,
        Some(c) if c.is_whitespace() => Some(tail.trim_start()),
        Some(_) => {
            return Err(BundleError::MalformedLine(
                "garbage after object id".to_owned(),
            ))
        }
    };
    Ok((oid, tail))
}

fn peel_to_commit(odb: &Odb, oid: ObjectId) -> Result<ObjectId> {
    match odb.peel_tags(oid)? {
        (commit, ObjectKind::Commit) => Ok(commit),
        (other, _) => Err(Error::CorruptObject(format!(
            "object {other} is not commit or tag"
        ))),
    }
}

fn commit_parent_set(odb: &Odb, commit: ObjectId) -> Result<Vec<ObjectId>> {
    let obj = odb.read(&commit)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit.parents)
}

fn commit_closure_roots(odb: &Odb, roots: &[ObjectId]) -> Result<HashSet<ObjectId>> {
    let mut seen = HashSet::new();
    let mut q: VecDeque<ObjectId> = VecDeque::new();
    for &root in roots {
        let c = peel_to_commit(odb, root)?;
        if seen.insert(c) {
            q.push_back(c);
        }
    }
    while let Some(c) = q.pop_front() {
        for p in commit_parent_set(odb, c)? {
            if seen.insert(p) {
                q.push_back(p);
            }
        }
    }
    Ok(seen)
}

fn prerequisite_commits(
    odb: &Odb,
    includes: &[ObjectId],
    excludes: &[ObjectId],
) -> std::result::Result<Vec<(ObjectId, Option<String>)>, BundleError> {
    if excludes.is_empty() {
        return Ok(Vec::new());
    }
    let exclude_set =
        commit_closure_roots(odb, excludes).map_err(|e| BundleError::Io(e.to_string()))?;
    let mut prerequisites = Vec::new();
    let mut seen_prereq = HashSet::new();
    let mut walk_seen = HashSet::new();
    let mut q: VecDeque<ObjectId> = VecDeque::new();
    for &root in includes {
        let c = peel_to_commit(odb, root).map_err(|e| BundleError::Io(e.to_string()))?;
        if exclude_set.contains(&c) {
            continue;
        }
        if walk_seen.insert(c) {
            q.push_back(c);
        }
    }
    while let Some(c) = q.pop_front() {
        let parents = commit_parent_set(odb, c).map_err(|e| BundleError::Io(e.to_string()))?;
        for p in parents {
            if exclude_set.contains(&p) && seen_prereq.insert(p) {
                let subject = commit_subject(odb, p).map_err(|e| BundleError::Io(e.to_string()))?;
                prerequisites.push((p, subject));
            }
            if !exclude_set.contains(&p) && walk_seen.insert(p) {
                q.push_back(p);
            }
        }
    }
    Ok(prerequisites)
}

fn commit_subject(odb: &Odb, commit: ObjectId) -> Result<Option<String>> {
    let obj = odb.read(&commit)?;
    let parsed = parse_commit(&obj.data)?;
    let subj = message_subject(&parsed.message);
    if subj.is_empty() {
        Ok(None)
    } else {
        Ok(Some(subj))
    }
}

fn filter_to_capability_value(filter: &ObjectFilter) -> String {
    match filter {
        ObjectFilter::BlobNone => "blob:none".to_owned(),
        ObjectFilter::BlobLimit(n) => format!("blob:limit={n}"),
        ObjectFilter::TreeDepth(d) => format!("tree:{d}"),
        ObjectFilter::SparseOid(s) => format!("sparse:oid={s}"),
        ObjectFilter::ObjectType(k) => format!("object:type={}", filter_object_kind_name(*k)),
        ObjectFilter::Combine(parts) => {
            let encoded: Vec<String> = parts
                .iter()
                .map(|p| {
                    crate::rev_list::url_encode_object_filter_subspec(&filter_to_capability_value(
                        p,
                    ))
                })
                .collect();
            format!("combine:{}", encoded.join("+"))
        }
    }
}

fn filter_object_kind_name(kind: FilterObjectKind) -> &'static str {
    match kind {
        FilterObjectKind::Blob => "blob",
        FilterObjectKind::Tree => "tree",
        FilterObjectKind::Commit => "commit",
        FilterObjectKind::Tag => "tag",
    }
}

/// Object list for a filtered bundle pack (`rev-list --objects --filter`).
fn filtered_pack_objects(
    repo: &Repository,
    include: &[ObjectId],
    exclude: &[ObjectId],
    filter: &ObjectFilter,
    have_closure: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let positive: Vec<String> = include.iter().map(ObjectId::to_hex).collect();
    let negative: Vec<String> = exclude
        .iter()
        .map(|oid| format!("^{}", oid.to_hex()))
        .collect();
    let options = RevListOptions {
        objects: true,
        no_object_names: true,
        quiet: true,
        filter: Some(filter.clone()),
        missing_action: MissingAction::Error,
        ..Default::default()
    };
    let walk = crate::rev_list::rev_list(repo, &positive, &negative, &options)?;
    Ok(collect_filtered_send_list(&walk, have_closure))
}

fn collect_filtered_send_list(
    walk: &RevListResult,
    have_closure: &HashSet<ObjectId>,
) -> Vec<ObjectId> {
    let mut send = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |oid: ObjectId| {
        if have_closure.contains(&oid) {
            return;
        }
        if seen.insert(oid) {
            send.push(oid);
        }
    };
    for oid in &walk.commits {
        push(*oid);
    }
    for (oid, _) in &walk.objects {
        push(*oid);
    }
    for tag_oid in walk.tip_annotated_tag_by_commit.values().flatten() {
        push(*tag_oid);
    }
    send
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_v2_header_and_refs() {
        let oid = "a".repeat(40);
        let data = format!("# v2 git bundle\n{oid} refs/heads/main\n\nPACK");
        let mut cur = Cursor::new(data.into_bytes());
        let header = read_header(&mut cur).expect("header");
        assert_eq!(header.version, BundleVersion::V2);
        assert_eq!(header.object_format, HashAlgo::Sha1);
        assert_eq!(header.prerequisites.len(), 0);
        assert_eq!(header.refs.len(), 1);
        assert_eq!(header.refs[0].0, "refs/heads/main");
    }

    #[test]
    fn bad_signature_yields_error() {
        let mut cur = Cursor::new(b"# v4 git bundle\n\nPACK");
        let err = read_header(&mut cur).unwrap_err();
        assert_eq!(err, BundleError::BadSignature);
    }

    #[test]
    fn unknown_capability() {
        let mut cur = Cursor::new(b"# v3 git bundle\n@unknown=yes\n\nPACK");
        let err = read_header(&mut cur).unwrap_err();
        assert!(matches!(err, BundleError::UnsupportedCapability(_)));
    }

    #[test]
    fn truncated_ref_line() {
        let oid = "a".repeat(40);
        let data = format!("# v2 git bundle\n{oid}\n\nPACK");
        let mut cur = Cursor::new(data.into_bytes());
        let err = read_header(&mut cur).unwrap_err();
        assert!(matches!(err, BundleError::MalformedLine(_)));
    }

    #[test]
    fn filter_spec_roundtrip_blob_none() {
        let f = FilterSpec::parse("blob:none").expect("parse");
        assert_eq!(f.to_capability_value(), "blob:none");
    }

    #[test]
    fn verify_report_missing_prerequisite_error() {
        let oid = ObjectId::from_hex(&"b".repeat(40)).expect("oid");
        let report = BundleVerifyReport {
            missing_prerequisites: vec![(oid, Some("subj".to_owned()))],
            prerequisites_not_connected: false,
        };
        let err = report.into_result().unwrap_err();
        assert!(matches!(err, BundleError::MissingPrerequisite { .. }));
    }

    fn write_bundle_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("tempfile");
        file.write_all(bytes).expect("write bundle");
        file.flush().expect("flush bundle");
        file
    }

    #[test]
    fn open_reports_pack_offset_after_header() {
        let oid = "a".repeat(40);
        let header = format!("# v2 git bundle\n{oid} refs/heads/main\n\n");
        let mut data = header.clone().into_bytes();
        data.extend_from_slice(b"PACK\0\0\0\x02\0\0\0\0");
        let file = write_bundle_file(&data);
        let bundle = Bundle::open(file.path()).expect("open");
        assert_eq!(bundle.pack_byte_offset(), header.len() as u64);
        assert_eq!(bundle.header().refs.len(), 1);
    }

    #[test]
    fn open_skips_blank_lines_before_pack() {
        let oid = "a".repeat(40);
        let header = format!("# v2 git bundle\n{oid} HEAD\n\n\r\n\n");
        let mut data = header.clone().into_bytes();
        data.extend_from_slice(b"PACK");
        let file = write_bundle_file(&data);
        let bundle = Bundle::open(file.path()).expect("open");
        assert_eq!(bundle.pack_byte_offset(), header.len() as u64);
    }

    #[test]
    fn open_handles_pack_magic_across_read_buffer_boundary() {
        let oid = "c".repeat(40);
        let signature = "# v2 git bundle\n";
        let ref_line = |i: usize| format!("{oid} refs/tags/t{i:06}\n");
        let line_len = ref_line(0).len();
        // Grow the header until the blank terminator lands two bytes before the first
        // buffer refill, so "PACK" straddles the boundary.
        let mut refs = 0usize;
        while signature.len() + (refs + 1) * line_len + 1 + 2 <= HEADER_READ_BUF {
            refs += 1;
        }
        let mut header = String::from(signature);
        for i in 0..refs {
            header.push_str(&ref_line(i));
        }
        header.push('\n');
        let pad = HEADER_READ_BUF - 2 - header.len();
        let mut data = header.clone().into_bytes();
        data.extend(std::iter::repeat_n(b'\n', pad));
        data.extend_from_slice(b"PACK\0\0\0\x02");
        assert_eq!(&data[HEADER_READ_BUF - 2..HEADER_READ_BUF + 2], b"PACK");
        let file = write_bundle_file(&data);
        let bundle = Bundle::open(file.path()).expect("open");
        assert_eq!(bundle.header().refs.len(), refs);
        assert_eq!(bundle.pack_byte_offset(), (HEADER_READ_BUF - 2) as u64);
    }

    #[test]
    fn open_without_pack_is_missing_pack() {
        let oid = "a".repeat(40);
        let truncated = format!("# v2 git bundle\n{oid} HEAD\n\n");
        let file = write_bundle_file(truncated.as_bytes());
        assert_eq!(
            Bundle::open(file.path()).unwrap_err(),
            BundleError::MissingPack
        );

        let short = format!("# v2 git bundle\n{oid} HEAD\n\nPA");
        let file = write_bundle_file(short.as_bytes());
        assert_eq!(
            Bundle::open(file.path()).unwrap_err(),
            BundleError::MissingPack
        );

        let garbage = format!("# v2 git bundle\n{oid} HEAD\n\nJUNK");
        let file = write_bundle_file(garbage.as_bytes());
        assert_eq!(
            Bundle::open(file.path()).unwrap_err(),
            BundleError::MissingPack
        );
    }
}
