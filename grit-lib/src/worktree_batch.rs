//! Parallel read, filter, hash, and compress of worktree file blobs (`add` and tracked-file restage).

use std::fs;
use std::path::PathBuf;

use crate::crlf::{ConversionConfig, FileAttrs};
use crate::diff::{mode_from_metadata, worktree_blob_content, WorktreeBlobContent};
use crate::error::{Error, Result};
use crate::filter_process::FilterProcessState;
use crate::hash::{
    hash_object, try_par_hash_with, try_par_hash_with_force, ParallelHashError, Parallelism,
};
use crate::index::IndexEntry;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::{self, Odb, WriteOptions};
use crate::worktree::{io_with_path, map_worktree_io, WorktreePathOperation};

/// One worktree path to read and normalize in parallel.
pub(crate) struct WorktreeBlobReadInput {
    /// Absolute path to read.
    pub abs: PathBuf,
    /// Index-relative path (for attributes / CRLF).
    pub index_relpath: String,
    /// Stage-0 index entry when conversion may use the recorded OID blob.
    pub index_entry: Option<IndexEntry>,
    /// Attributes for `index_relpath`, resolved by the caller (attribute lookup is not
    /// thread-safe, so it happens before the parallel phase).
    pub file_attrs: FileAttrs,
    /// When set, reuse metadata from an earlier stat scan instead of calling `stat` again.
    pub cached_meta: Option<fs::Metadata>,
}

/// Blob content prepared for the repository's primary object store.
pub(crate) enum PreparedBlobPayload {
    /// Zlib-compressed canonical store bytes for [`Odb::write_loose_zlib_prehashed`]
    /// (files-backed primary).
    Zlib(Vec<u8>),
    /// Normalized blob content for a custom primary, which owns its own encoding.
    Raw(Vec<u8>),
    /// A big unconverted file (see [`WorktreeBlobContent`]), read again as a stream when
    /// stored instead of being held in memory.
    Stream(PathBuf),
}

impl PreparedBlobPayload {
    /// Write this payload as blob `oid` through the primary store.
    ///
    /// # Errors
    ///
    /// Propagates write failures from the loose store or the custom primary.
    /// [`Error::WorktreeFileChanged`] when a streamed file no longer hashes to `oid`.
    pub(crate) fn store(
        &self,
        odb: &Odb,
        index_relpath: &str,
        oid: &ObjectId,
        options: WriteOptions,
    ) -> Result<()> {
        match self {
            Self::Zlib(zlib_store) => {
                odb.write_loose_zlib_prehashed(oid, zlib_store, options)?;
            }
            Self::Raw(data) => {
                let written = odb.write_with_options(ObjectKind::Blob, data, options)?;
                debug_assert_eq!(written, *oid);
            }
            Self::Stream(path) => {
                let mut file = fs::File::open(path).map_err(|source| {
                    map_worktree_io(index_relpath, WorktreePathOperation::Read, source)
                })?;
                let size = file.metadata()?.len();
                let written = odb.write_stream(ObjectKind::Blob, size, &mut file, options)?;
                if written != *oid {
                    return Err(Error::WorktreeFileChanged {
                        path: index_relpath.to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// A worktree blob that was hashed (and possibly stored) by a parallel batch.
pub(crate) struct WorktreeBlob {
    /// Index path spelling used for staging.
    pub index_relpath: String,
    /// Object id of the normalized blob (computed in the parallel phase).
    pub oid: ObjectId,
    /// Worktree metadata captured during the read pass.
    pub meta: fs::Metadata,
    /// Git mode derived from `meta`.
    pub mode: u32,
}

impl WorktreeBlob {
    fn new(item: &WorktreeBlobReadInput, oid: ObjectId, meta: fs::Metadata) -> Self {
        Self {
            index_relpath: item.index_relpath.clone(),
            oid,
            mode: mode_from_metadata(&meta),
            meta,
        }
    }
}

/// Worktree blob after parallel read and hash, with its content ready to store.
pub(crate) struct PreparedWorktreeBlob {
    /// Path, id, and metadata of the blob.
    pub blob: WorktreeBlob,
    /// Bytes to hand to the primary store (see [`PreparedBlobPayload::store`]).
    pub payload: PreparedBlobPayload,
}

struct PrepareBlobContext {
    algo: HashAlgo,
    /// Loose zlib level, or `None` when the primary is a custom store (no loose encoding).
    compression: Option<flate2::Compression>,
}

impl PrepareBlobContext {
    fn new(odb: &Odb) -> Result<Self> {
        let compression = if odb.uses_files_primary() {
            Some(odb.loose_compression()?)
        } else {
            None
        };
        Ok(Self {
            algo: odb.hash_algo(),
            compression,
        })
    }

    fn read(
        odb: &Odb,
        item: &WorktreeBlobReadInput,
        conv: &ConversionConfig,
        filter_process: Option<&FilterProcessState>,
    ) -> Result<(fs::Metadata, WorktreeBlobContent)> {
        let meta = match &item.cached_meta {
            Some(m) => m.clone(),
            None => io_with_path(
                &item.index_relpath,
                WorktreePathOperation::Metadata,
                fs::symlink_metadata(&item.abs),
            )?,
        };
        let content = worktree_blob_content(
            odb,
            &item.abs,
            &meta,
            conv,
            &item.file_attrs,
            &item.index_relpath,
            item.index_entry.as_ref(),
            filter_process,
        )?;
        Ok((meta, content))
    }

    /// Hash `data` and encode it for the primary store.
    fn encode(&self, data: Vec<u8>) -> Result<(ObjectId, PreparedBlobPayload)> {
        let oid = hash_object(self.algo, ObjectKind::Blob, &data)?;
        let payload = match self.compression {
            Some(compression) => {
                PreparedBlobPayload::Zlib(odb::zlib_compress_blob_loose(&data, compression)?)
            }
            None => PreparedBlobPayload::Raw(data),
        };
        Ok((oid, payload))
    }

    /// Read, hash, and encode one worktree blob for the primary store.
    fn prepare(
        &self,
        odb: &Odb,
        item: &WorktreeBlobReadInput,
        conv: &ConversionConfig,
        filter_process: Option<&FilterProcessState>,
    ) -> Result<PreparedWorktreeBlob> {
        let (meta, content) = Self::read(odb, item, conv, filter_process)?;
        let (oid, payload) = match content {
            WorktreeBlobContent::Bytes(data) => self.encode(data)?,
            stream @ WorktreeBlobContent::Stream { .. } => (
                stream.hash(odb)?,
                PreparedBlobPayload::Stream(item.abs.clone()),
            ),
        };
        Ok(PreparedWorktreeBlob {
            blob: WorktreeBlob::new(item, oid, meta),
            payload,
        })
    }

    /// Read, hash, and store one worktree blob, keeping no content in memory afterwards.
    fn prepare_and_store(
        &self,
        odb: &Odb,
        item: &WorktreeBlobReadInput,
        conv: &ConversionConfig,
        filter_process: Option<&FilterProcessState>,
        write_opts: WriteOptions,
    ) -> Result<WorktreeBlob> {
        let (meta, content) = Self::read(odb, item, conv, filter_process)?;
        match content {
            WorktreeBlobContent::Bytes(data) => {
                let oid = hash_object(self.algo, ObjectKind::Blob, &data)?;
                odb.write_blob_prehashed(&oid, &data, write_opts)
                    .map_err(|e| with_path_context(&item.index_relpath, e))?;
                Ok(WorktreeBlob::new(item, oid, meta))
            }
            stream @ WorktreeBlobContent::Stream { .. } => {
                let oid = stream
                    .store(odb, write_opts)
                    .map_err(|e| with_path_context(&item.index_relpath, e))?;
                Ok(WorktreeBlob::new(item, oid, meta))
            }
        }
    }
}

/// Name the index path in I/O errors from storing a blob.
fn with_path_context(index_relpath: &str, err: Error) -> Error {
    match err {
        Error::Io(source) => Error::LooseObjectWrite {
            path: index_relpath.to_owned(),
            source,
        },
        Error::WorktreePathIo { .. } => err,
        other => other,
    }
}

/// Read, CRLF-filter, hash, and compress many worktree blobs in parallel (input order preserved).
///
/// On a custom primary the blobs are hashed but not compressed (see [`PreparedBlobPayload`]).
/// Big unconverted files are only hashed, as a stream; their payload rereads the file.
///
/// # Errors
///
/// Propagates I/O, conversion, hashing, or compression failures from any worker.
pub(crate) fn prepare_worktree_blobs_parallel(
    odb: &Odb,
    items: &[WorktreeBlobReadInput],
    conv: &ConversionConfig,
    parallelism: Parallelism,
    filter_process: Option<&FilterProcessState>,
) -> Result<Vec<PreparedWorktreeBlob>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = PrepareBlobContext::new(odb)?;
    let total_bytes = items.len().saturating_mul(4096);
    let threads = parallelism.threads();
    try_par_hash_with(items, threads, total_bytes, |item| {
        ctx.prepare(odb, item, conv, filter_process)
    })
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })
}

/// Like [`prepare_worktree_blobs_parallel`], but writes each blob through the primary store
/// in the same worker pass and drops its content right away.
///
/// Avoids a second parallel wave over the same inputs during bulk `git add` restage. Big
/// unconverted files are hashed and stored in one streaming pass.
///
/// # Errors
///
/// Propagates I/O, conversion, hashing, compression, or object-write failures from any worker.
pub(crate) fn prepare_and_store_worktree_blobs_parallel(
    odb: &Odb,
    items: &[WorktreeBlobReadInput],
    conv: &ConversionConfig,
    parallelism: Parallelism,
    filter_process: Option<&FilterProcessState>,
    write_opts: WriteOptions,
) -> Result<Vec<WorktreeBlob>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let ctx = PrepareBlobContext::new(odb)?;
    let threads = parallelism.threads();
    let map_item = |item: &WorktreeBlobReadInput| {
        ctx.prepare_and_store(odb, item, conv, filter_process, write_opts)
    };
    if items.len() >= crate::hash::PAR_HASH_MIN_ITEMS {
        try_par_hash_with_force(items, threads, map_item)
    } else {
        let total_bytes = items.len().saturating_mul(4096);
        try_par_hash_with(items, threads, total_bytes, map_item)
    }
    .map_err(|e: ParallelHashError<Error>| match e {
        ParallelHashError::Task(err) => err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigSet;
    use crate::crlf::FileAttrs;
    use std::fs;
    use std::path::PathBuf;

    fn shattered_prefix() -> Vec<u8> {
        fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/shattered-1-prefix.bin"),
        )
        .expect("fixture")
    }

    #[test]
    fn worktree_add_accepts_shattered_file_like_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("shattered.bin");
        let data = shattered_prefix();
        fs::write(&file, &data).expect("write");
        let meta = fs::symlink_metadata(&file).expect("stat");
        let odb = Odb::new(&dir.path().join("objects"));
        fs::create_dir_all(odb.objects_dir()).expect("objects");
        let conv = ConversionConfig::from_config(&ConfigSet::new());
        let input = WorktreeBlobReadInput {
            abs: file,
            index_relpath: "shattered.bin".to_owned(),
            index_entry: None,
            file_attrs: FileAttrs::default(),
            cached_meta: Some(meta),
        };
        let prepared = prepare_worktree_blobs_parallel(
            &odb,
            &[input],
            &conv,
            Parallelism::resolve(Some(1)),
            None,
        )
        .expect("prepare shattered blob");
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            prepared[0].blob.oid,
            hash_object(HashAlgo::Sha1, ObjectKind::Blob, &data).expect("blob id")
        );
    }
}
