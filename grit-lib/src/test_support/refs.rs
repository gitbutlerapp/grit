//! Git-directory-level ref, reflog and reftable helpers for backend tests.
//!
//! Workspace tests that check the files and reftable layouts directly (packed-refs records,
//! reflog files, reftable tables and write options) call these with a git directory. Library
//! callers go through [`crate::refs::References`] and
//! [`crate::repo_handles::RepositoryReflogs`] instead.
#![allow(missing_docs, clippy::missing_errors_doc)]

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::objects::ObjectId;
use crate::refs::reftable::WriteOptions;
use crate::refs::store::RefNameRef;
use crate::refs::{
    BranchCommitRefUpdate, LogRefsConfig, PackedRefRecord, PackedRefs, RawRefLookup, RefBatchItem,
    RefnameUnavailable,
};

#[must_use]
pub fn packed_refs_path(common_dir: &Path) -> PathBuf {
    crate::refs::store::paths::packed_refs_path(common_dir)
}

pub fn collect_alternate_ref_oids(receiving_git_dir: &Path) -> Result<Vec<ObjectId>> {
    crate::refs::collect_alternate_ref_oids(receiving_git_dir)
}

#[must_use]
pub fn common_dir(git_dir: &Path) -> Option<PathBuf> {
    crate::refs::common_dir(git_dir)
}

#[must_use]
pub fn effective_log_refs_config(git_dir: &Path) -> LogRefsConfig {
    crate::refs::effective_log_refs_config(git_dir)
}

#[must_use]
pub fn read_log_refs_config(git_dir: &Path) -> LogRefsConfig {
    crate::refs::read_log_refs_config(git_dir)
}

#[must_use]
pub fn should_autocreate_reflog(git_dir: &Path, refname: impl AsRef<RefNameRef>) -> bool {
    crate::refs::should_autocreate_reflog(git_dir, refname)
}

pub fn list_refs_physical(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    crate::refs::list_refs_physical(git_dir, prefix)
}

pub fn pack_remote_tracking_refs_for_clone(git_dir: &Path, remote: &str) -> Result<()> {
    crate::refs::pack_remote_tracking_refs_for_clone(git_dir, remote)
}

pub fn packed_refs_entry_exists(git_dir: &Path, refname: impl AsRef<RefNameRef>) -> Result<bool> {
    crate::refs::packed_refs_entry_exists(git_dir, refname)
}

pub fn read_raw_ref(git_dir: &Path, refname: impl AsRef<RefNameRef>) -> Result<RawRefLookup> {
    crate::refs::read_raw_ref(git_dir, refname)
}

#[must_use]
pub fn reflog_file_path(git_dir: &Path, refname: impl AsRef<RefNameRef>) -> PathBuf {
    crate::refs::reflog_file_path(git_dir, refname)
}

pub fn resolve_at_n_branch(git_dir: &Path, spec: &str) -> Result<String> {
    crate::refs::resolve_at_n_branch(git_dir, spec)
}

pub fn resolve_ref_cached(
    git_dir: &Path,
    refname: impl AsRef<RefNameRef>,
    packed: &PackedRefs,
) -> Result<Option<ObjectId>> {
    crate::refs::resolve_ref_cached(git_dir, refname, packed)
}

pub fn write_ref_cached(
    git_dir: &Path,
    refname: impl AsRef<RefNameRef>,
    oid: &ObjectId,
    packed: &mut PackedRefs,
) -> Result<()> {
    crate::refs::write_ref_cached(git_dir, refname, oid, packed)
}

pub fn verify_ref_available_for_create(
    git_dir: &Path,
    refname: impl AsRef<RefNameRef>,
    extras: &BTreeSet<String>,
    skip: &HashSet<String>,
) -> std::result::Result<(), RefnameUnavailable> {
    crate::refs::verify_ref_available_for_create(git_dir, refname, extras, skip)
}

pub fn verify_ref_transaction_batch(
    git_dir: &Path,
    items: &[RefBatchItem],
) -> std::result::Result<(), RefnameUnavailable> {
    crate::refs::verify_ref_transaction_batch(git_dir, items)
}

pub fn write_packed_refs_file(git_dir: &Path, records: &[PackedRefRecord]) -> Result<()> {
    crate::refs::write_packed_refs_file(git_dir, records)
}

pub fn all_reflog_oids(git_dir: &Path) -> Result<HashSet<ObjectId>> {
    crate::reflog::all_reflog_oids(git_dir)
}

pub fn expire_reflog(git_dir: &Path, refname: &str, expire_time: Option<i64>) -> Result<usize> {
    crate::reflog::expire_reflog(git_dir, refname, expire_time)
}

#[must_use]
pub fn reflog_path(git_dir: &Path, refname: &str) -> PathBuf {
    crate::reflog::reflog_path(git_dir, refname)
}

#[must_use]
pub fn read_write_options(git_dir: &Path) -> WriteOptions {
    crate::reftable::read_write_options(git_dir)
}

#[allow(clippy::too_many_arguments)]
pub fn reftable_append_reflog(
    git_dir: &Path,
    refname: &str,
    old_oid: &ObjectId,
    new_oid: &ObjectId,
    identity: &str,
    message: &str,
    force_create: bool,
) -> Result<()> {
    crate::reftable::reftable_append_reflog(
        git_dir,
        refname,
        old_oid,
        new_oid,
        identity,
        message,
        force_create,
    )
}

pub fn reftable_delete_ref(git_dir: &Path, refname: &str) -> Result<()> {
    crate::reftable::reftable_delete_ref(git_dir, refname)
}

pub fn reftable_delete_ref_with_write_options(
    git_dir: &Path,
    refname: &str,
    opts: &WriteOptions,
) -> Result<()> {
    crate::reftable::reftable_delete_ref_with_write_options(git_dir, refname, opts)
}

pub fn reftable_list_refs(git_dir: &Path, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
    crate::reftable::reftable_list_refs(git_dir, prefix)
}

pub fn reftable_write_ref(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    log_identity: Option<&str>,
    log_message: Option<&str>,
) -> Result<()> {
    crate::reftable::reftable_write_ref(git_dir, refname, oid, log_identity, log_message)
}

pub fn reftable_write_ref_with_write_options(
    git_dir: &Path,
    refname: &str,
    oid: &ObjectId,
    log_identity: Option<&str>,
    log_message: Option<&str>,
    opts: &WriteOptions,
) -> Result<()> {
    crate::reftable::reftable_write_ref_with_write_options(
        git_dir,
        refname,
        oid,
        log_identity,
        log_message,
        opts,
    )
}

pub fn reftable_write_symref(
    git_dir: &Path,
    refname: &str,
    target: &str,
    log_identity: Option<&str>,
    log_message: Option<&str>,
) -> Result<()> {
    crate::reftable::reftable_write_symref(git_dir, refname, target, log_identity, log_message)
}

pub fn update_branch_for_commit(git_dir: &Path, update: &BranchCommitRefUpdate<'_>) -> Result<()> {
    crate::refs::update_branch_for_commit(git_dir, update)
}
