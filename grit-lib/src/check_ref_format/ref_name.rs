//! Owned and borrowed ref names with policy-driven validation.

use std::borrow::{Borrow, Cow};
use std::fmt;
use std::ops::Deref;
use std::str;

use super::{check_refname_format_borrowed, refname_is_safe, RefFormatOptions, RefNameError};

/// Options controlling how a ref name is validated when parsed.
///
/// The default matches Git's normal full-ref rules (`git check-ref-format` with no flags).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RefNamePolicy {
    format: RefFormatOptions,
    storable: bool,
    safe_for_fs: bool,
    receive_pack_delete: Option<bool>,
    advertised_fetch: bool,
    advertised_symref: bool,
}

impl RefNamePolicy {
    /// Git's default full-ref format rules.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow a single-level ref name (no `/` separator required).
    #[must_use]
    pub fn allow_onelevel(mut self) -> Self {
        self.format.allow_onelevel = true;
        self
    }

    /// Allow exactly one `*` wildcard anywhere in the name.
    #[must_use]
    pub fn refspec_pattern(mut self) -> Self {
        self.format.refspec_pattern = true;
        self
    }

    /// Collapse consecutive slashes and strip a leading `/` before validating.
    ///
    /// When the name changes, [`RefName::parse`] returns the normalized owned string.
    /// [`RefNameRef::parse`] rejects names that would require normalization.
    #[must_use]
    pub fn normalize(mut self) -> Self {
        self.format.normalize = true;
        self
    }

    /// Require the name to be safe to store (HEAD, pseudo-refs, and valid `refs/…` names).
    #[must_use]
    pub fn storable(mut self) -> Self {
        self.storable = true;
        self
    }

    /// Require the name to be safe to map to a path under ref storage (`refname_is_safe`).
    #[must_use]
    pub fn safe_for_fs(mut self) -> Self {
        self.safe_for_fs = true;
        self
    }

    /// Require a receive-pack wire ref name (`refs/…` with delete-aware one-level rules).
    #[must_use]
    pub fn receive_pack(mut self, is_delete: bool) -> Self {
        self.receive_pack_delete = Some(is_delete);
        self
    }

    /// Require a fetch/upload-pack advertised ref (`refs/…` plus default format rules).
    #[must_use]
    pub fn advertised_fetch(mut self) -> Self {
        self.advertised_fetch = true;
        self
    }

    /// Require a symref advertisement target (format with `--allow-onelevel`).
    #[must_use]
    pub fn advertised_symref(mut self) -> Self {
        self.advertised_symref = true;
        self
    }

    pub(crate) fn format_options(&self) -> &RefFormatOptions {
        &self.format
    }

    pub(crate) fn receive_pack_delete(&self) -> Option<bool> {
        self.receive_pack_delete
    }

    pub(crate) fn requires_storable(&self) -> bool {
        self.storable
    }

    pub(crate) fn requires_safe_for_fs(&self) -> bool {
        self.safe_for_fs
    }

    pub(crate) fn requires_advertised_fetch(&self) -> bool {
        self.advertised_fetch
    }

    pub(crate) fn requires_advertised_symref(&self) -> bool {
        self.advertised_symref
    }

    /// Policy for symref advertisement targets (`--allow-onelevel` format rules).
    #[must_use]
    pub fn symref_target() -> Self {
        Self::new().allow_onelevel()
    }
}

/// Validated full ref name (for example `refs/heads/main`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

/// Borrowed validated ref name (like [`std::path::Path`] to [`std::path::PathBuf`]).
#[repr(transparent)]
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct RefNameRef(str);

impl RefName {
    /// Parse and validate `name` according to `policy`.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError`] when the name fails any enabled rule.
    pub fn parse(name: &str, policy: &RefNamePolicy) -> Result<Self, RefNameError> {
        let format = effective_format_options(name, policy);
        let normalized = check_refname_format_borrowed(name, &format)?;
        let stored: Cow<'_, str> = normalized;
        apply_policy_checks(stored.as_ref(), policy)?;
        Ok(Self(stored.into_owned()))
    }

    /// Parse a branch short name into `refs/heads/<name>`.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError`] when the short name or full ref would be invalid.
    pub fn branch(short: &str) -> Result<Self, RefNameError> {
        branch_short_checks(short)?;
        Self::parse(&format!("refs/heads/{short}"), &RefNamePolicy::default())
    }

    /// Parse a tag short name into `refs/tags/<name>`.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError`] when the short name or full ref would be invalid.
    pub fn tag(short: &str) -> Result<Self, RefNameError> {
        if short.is_empty() {
            return Err(RefNameError::Empty);
        }
        Self::parse(&format!("refs/tags/{short}"), &RefNamePolicy::default())
    }

    /// Construct a ref name read from storage without re-validating.
    #[must_use]
    pub fn from_trusted(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Parse and validate using default Git full-ref format rules, plus `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError`] when the name is empty or otherwise invalid.
    pub fn try_from(name: &str) -> Result<Self, RefNameError> {
        if name == "HEAD" {
            return Ok(Self(name.to_owned()));
        }
        Self::parse(name, &RefNamePolicy::default())
    }

    /// Borrow the stored ref name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl RefNameRef {
    /// Parse `name` in place when validation does not require owning a normalized copy.
    ///
    /// # Errors
    ///
    /// Returns [`RefNameError::NormalizationRequired`] when `policy` enables normalization and
    /// the normalized form differs from `name`. Use [`RefName::parse`] in that case.
    pub fn parse<'a>(name: &'a str, policy: &RefNamePolicy) -> Result<&'a Self, RefNameError> {
        let format = effective_format_options(name, policy);
        if format.normalize {
            let normalized = check_refname_format_borrowed(name, &format)?;
            if let Cow::Owned(_) = normalized {
                return Err(RefNameError::NormalizationRequired);
            }
        } else {
            check_refname_format_borrowed(name, &format)?;
        }
        apply_policy_checks(name, policy)?;
        Ok(Self::new(name))
    }

    /// Borrow the validated ref name as UTF-8.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow a name read from ref storage without re-validating.
    #[must_use]
    pub fn from_trusted(s: &str) -> &Self {
        Self::new(s)
    }

    #[must_use]
    fn new(s: &str) -> &Self {
        // SAFETY: RefNameRef is repr(transparent) over str.
        unsafe { &*(s as *const str as *const RefNameRef) }
    }
}

impl AsRef<RefNameRef> for str {
    fn as_ref(&self) -> &RefNameRef {
        RefNameRef::from_trusted(self)
    }
}

impl AsRef<RefNameRef> for String {
    fn as_ref(&self) -> &RefNameRef {
        RefNameRef::from_trusted(self.as_str())
    }
}

impl AsRef<RefNameRef> for RefNameRef {
    fn as_ref(&self) -> &RefNameRef {
        self
    }
}

/// Format flags for the first validation pass: pseudo-refs and HEAD need
/// `--allow-onelevel` when `storable()` or `advertised_symref()` is enabled.
fn effective_format_options(name: &str, policy: &RefNamePolicy) -> RefFormatOptions {
    let mut format = policy.format_options().clone();
    if !name.starts_with("refs/")
        && (policy.requires_storable() || policy.requires_advertised_symref())
    {
        format.allow_onelevel = true;
    }
    format
}

fn branch_short_checks(name: &str) -> Result<(), RefNameError> {
    if name.is_empty() {
        return Err(RefNameError::Empty);
    }
    if name.starts_with('-') {
        return Err(RefNameError::BranchStartsWithDash);
    }
    if name == "HEAD" {
        return Err(RefNameError::ReservedBranchName);
    }
    Ok(())
}

fn apply_policy_checks(name: &str, policy: &RefNamePolicy) -> Result<(), RefNameError> {
    if policy.requires_advertised_symref() {
        RefNameRef::parse(name, &RefNamePolicy::symref_target())?;
    }

    if policy.requires_advertised_fetch() && !is_valid_fetch_advertised_ref(name) {
        return Err(RefNameError::NotAdvertisedFetchRef);
    }

    if let Some(is_delete) = policy.receive_pack_delete() {
        if !receive_pack_refname_ok(name, is_delete) {
            return Err(RefNameError::ReceivePackRefused);
        }
    }

    if policy.requires_safe_for_fs() && !refname_is_safe(name) {
        return Err(RefNameError::UnsafeForFilesystem);
    }

    if policy.requires_storable() && !is_storable_ref_name(name) {
        return Err(RefNameError::NotStorable);
    }

    Ok(())
}

pub(crate) fn is_storable_ref_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("refs/") {
        !rest.is_empty() && is_valid_fetch_advertised_ref(name)
    } else {
        refname_is_safe(name)
    }
}

pub(crate) fn is_valid_fetch_advertised_ref(name: &str) -> bool {
    if !name.starts_with("refs/") {
        return false;
    }
    check_refname_format_borrowed(name, &RefFormatOptions::default()).is_ok()
}

pub(crate) fn receive_pack_refname_ok(refname: &str, is_delete: bool) -> bool {
    if !refname.starts_with("refs/") {
        return false;
    }
    check_refname_format_borrowed(
        &refname[5..],
        &RefFormatOptions {
            allow_onelevel: is_delete,
            ..RefFormatOptions::default()
        },
    )
    .is_ok()
}

impl AsRef<str> for RefName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<RefNameRef> for RefName {
    fn as_ref(&self) -> &RefNameRef {
        RefNameRef::new(self.as_str())
    }
}

impl Borrow<RefNameRef> for RefName {
    fn borrow(&self) -> &RefNameRef {
        RefNameRef::new(self.as_str())
    }
}

impl Deref for RefName {
    type Target = RefNameRef;

    fn deref(&self) -> &RefNameRef {
        RefNameRef::new(self.as_str())
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for RefNameRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(feature = "serde-models")]
impl serde::Serialize for RefName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
