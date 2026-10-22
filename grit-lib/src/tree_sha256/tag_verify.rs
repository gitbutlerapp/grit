//! Annotated tag verification: signature check plus optional [`tree-sha256`] digest.
//!
//! [tree-sha256]: https://github.com/schacon/tree-sha256

use std::thread;

use crate::commit::signing::{self, GpgConfig, SignatureCheck, TrustLevel};
use crate::error::{Error, Result};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::tree_sha256::{tree_sha256, TreeSha256, TreeSha256Options, TreeSha256Report};

/// Tag header carrying the digest (`tree-sha256 <64 hex chars>`).
pub const TREE_SHA256_HEADER: &str = "tree-sha256";
/// Tag header naming one excluded submodule path (`tree-sha256-excluded <path>`).
pub const TREE_SHA256_EXCLUDED_HEADER: &str = "tree-sha256-excluded";

/// Options for [`verify_tag`].
#[derive(Debug, Clone)]
pub struct TagVerifyOptions {
    /// When false, [`SignatureVerdict::NotChecked`] is returned without running a verifier.
    pub check_signature: bool,
    /// Signing configuration and subprocess runner for signature verification.
    pub gpg: GpgConfig,
}

/// Combined signature and tree-sha256 verification outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagVerification {
    /// OpenPGP / SSH signature result.
    pub signature: SignatureVerdict,
    /// Present when the tag object carries a `tree-sha256` header.
    pub tree_sha256: Option<TreeSha256Verdict>,
}

impl TagVerification {
    /// True when the signature is good or was not checked, and any claimed digest
    /// matches a recomputation with no bogus `tree-sha256-excluded` paths.
    ///
    /// Excluded submodules listed in the tag are reported in [`TreeSha256Verdict::excluded`]
    /// but do not fail verification.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        let sig_ok = matches!(
            self.signature,
            SignatureVerdict::Good { .. } | SignatureVerdict::NotChecked
        );
        if !sig_ok {
            return false;
        }
        match &self.tree_sha256 {
            None => true,
            Some(v) => v.claimed == v.computed && v.bogus_excluded.is_empty(),
        }
    }
}

/// Signature verification outcome for a tag object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureVerdict {
    /// Signature verified and meets configured minimum trust.
    Good {
        /// Signer uid when reported by the verifier.
        signer: Option<String>,
        /// Signing key id when reported.
        key: Option<String>,
        /// Trust level from gpg status or ssh verification.
        trust: TrustLevel,
    },
    /// Signature present but invalid or untrusted.
    Bad,
    /// No signature armor was found on the tag object.
    Missing,
    /// Signature checking was disabled ([`TagVerifyOptions::check_signature`]).
    NotChecked,
}

/// Recomputed tree-sha256 compared to the tag header claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSha256Verdict {
    /// Digest recorded in the tag header.
    pub claimed: TreeSha256,
    /// Digest from hashing the tagged commit's tree with the tag's exclusions.
    pub computed: TreeSha256,
    /// Submodule paths excluded during hashing (from the tag headers).
    pub excluded: Vec<(String, ObjectId)>,
    /// Paths listed in `tree-sha256-excluded` that are not gitlinks in the tree.
    pub bogus_excluded: Vec<String>,
    /// Submodule cycle paths recorded while hashing.
    pub cycles: Vec<String>,
}

/// Parsed tree-sha256-related headers from a raw annotated tag object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSha256TagHeaders {
    /// `object` header (commit or other peeled target).
    pub target: ObjectId,
    /// `tree-sha256` claim, when present.
    pub claimed: Option<TreeSha256>,
    /// Paths from `tree-sha256-excluded` headers, in tag order.
    pub excluded: Vec<String>,
}

/// Read tree-sha256 headers from a raw tag object (header block through the first blank line).
///
/// # Errors
///
/// Returns [`Error::TagVerifyDuplicateTreeSha256Header`] or parse failures for malformed headers.
pub fn parse_tree_sha256_tag_headers(data: &[u8]) -> Result<TreeSha256TagHeaders> {
    let mut object = None;
    let mut claimed = None;
    let mut excluded = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            break;
        }
        let line_str = std::str::from_utf8(line)
            .map_err(|_| Error::CorruptObject("tag header is not valid UTF-8".to_owned()))?;
        let Some((key, value)) = line_str.split_once(' ') else {
            continue;
        };
        match key {
            "object" => {
                object = Some(value.parse()?);
            }
            TREE_SHA256_HEADER => {
                if claimed.is_some() {
                    return Err(Error::TagVerifyDuplicateTreeSha256Header);
                }
                claimed = Some(value.parse()?);
            }
            TREE_SHA256_EXCLUDED_HEADER => excluded.push(value.to_owned()),
            _ => {}
        }
    }
    let target = object.ok_or(Error::TagVerifyMissingObjectHeader)?;
    Ok(TreeSha256TagHeaders {
        target,
        claimed,
        excluded,
    })
}

/// Verify an annotated tag object by id (not via a ref).
///
/// Reads the object from the ODB, rejects non-tag kinds, then runs signature
/// verification and tree-sha256 recomputation concurrently on separate threads.
///
/// # Errors
///
/// Returns [`Error::TagVerifyNotTag`] when `oid` is not a tag object, header parse
/// failures, signing subprocess errors, or [`Error::TreeSha256MissingSubmodules`]
/// when digest recomputation requires missing submodules.
pub fn verify_tag(
    repo: &Repository,
    oid: ObjectId,
    options: &TagVerifyOptions,
) -> Result<TagVerification> {
    let obj = repo.odb().read(&oid)?;
    if obj.kind != ObjectKind::Tag {
        return Err(Error::TagVerifyNotTag {
            oid,
            found: obj.kind,
        });
    }
    let raw = obj.data;
    let headers = parse_tree_sha256_tag_headers(&raw)?;
    let min_trust = options.gpg.min_trust_level;

    thread::scope(|scope| {
        let raw_for_sig = raw.clone();
        let gpg = options.gpg.clone();
        let check_signature = options.check_signature;
        let sig_handle = scope.spawn(move || -> Result<SignatureVerdict> {
            if !check_signature {
                return Ok(SignatureVerdict::NotChecked);
            }
            let check = signing::verify_tag(&gpg, &raw_for_sig)?;
            Ok(signature_verdict_from_check(&check, min_trust))
        });

        let hash_handle = scope.spawn(|| -> Result<Option<TreeSha256Verdict>> {
            let Some(claimed) = headers.claimed else {
                return Ok(None);
            };
            let opts = TreeSha256Options {
                exclude_globs: Vec::new(),
                exclude_exact: headers.excluded.clone(),
            };
            let report = tree_sha256(repo, headers.target, &opts)?;
            Ok(Some(tree_sha256_verdict(
                claimed,
                &headers.excluded,
                &report,
            )))
        });

        let signature = sig_handle
            .join()
            .map_err(|_| Error::Message("signature verification thread panicked".into()))??;
        let tree_sha256 = hash_handle
            .join()
            .map_err(|_| Error::Message("tree-sha256 verification thread panicked".into()))??;

        Ok(TagVerification {
            signature,
            tree_sha256,
        })
    })
}

fn tree_sha256_verdict(
    claimed: TreeSha256,
    header_excluded: &[String],
    report: &TreeSha256Report,
) -> TreeSha256Verdict {
    let found: Vec<&str> = report.excluded.iter().map(|(p, _)| p.as_str()).collect();
    let bogus_excluded: Vec<String> = header_excluded
        .iter()
        .filter(|p| !found.contains(&p.as_str()))
        .cloned()
        .collect();
    TreeSha256Verdict {
        claimed,
        computed: report.digest,
        excluded: report.excluded.clone(),
        bogus_excluded,
        cycles: report.cycles.clone(),
    }
}

fn signature_verdict_from_check(
    check: &SignatureCheck,
    min_trust: Option<TrustLevel>,
) -> SignatureVerdict {
    if check.result == 'N' {
        return SignatureVerdict::Missing;
    }
    if check.verify_status(min_trust) {
        SignatureVerdict::Good {
            signer: check.signer.clone(),
            key: check.key.clone(),
            trust: check.trust_level,
        }
    } else {
        SignatureVerdict::Bad
    }
}
