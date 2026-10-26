//! Typed parsing for reflog references and newest-first selectors.

use std::str::FromStr;

use crate::error::{Error, Result};
use crate::refs::store::RefName;

/// One entry in a reference log, indexed from its newest entry.
///
/// Parse `ref@{n}` with [`FromStr`]. Surrounding whitespace and whitespace after
/// the reference are ignored. The reference is validated by [`parse_reflog_ref`];
/// parsing does not check whether the reference or entry exists.
///
/// ```rust
/// use grit_lib::refs::reflog::ReflogSelector;
/// # fn main() -> grit_lib::error::Result<()> {
/// let entry: ReflogSelector = "main@{2}".parse()?;
/// assert_eq!(entry.ref_name.as_str(), "refs/heads/main");
/// assert_eq!(entry.index, 2);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogSelector {
    /// Validated reference to pass to [`RepositoryReflogs`](crate::repo_handles::RepositoryReflogs).
    pub ref_name: RefName,
    /// Newest-first entry index (`0` is the latest entry).
    pub index: usize,
}

impl FromStr for ReflogSelector {
    type Err = Error;

    /// Parse `spec` in `ref@{n}` form, returning a validated reference and index.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReflogInvalidSelector`] for missing delimiters or an empty
    /// reference, [`Error::ReflogInvalidIndex`] for an invalid or overflowing index,
    /// and [`Error::ReflogUnknownRef`] for an invalid reference name.
    fn from_str(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let invalid_selector = || Error::ReflogInvalidSelector {
            spec: spec.to_owned(),
        };
        let (ref_part, rest) = spec.split_once("@{").ok_or_else(invalid_selector)?;
        let ref_part = ref_part.trim();
        let rest = rest.strip_suffix('}').ok_or_else(invalid_selector)?;
        let index = rest.parse().map_err(|source| Error::ReflogInvalidIndex {
            spec: spec.to_owned(),
            source,
        })?;
        if ref_part.is_empty() {
            return Err(invalid_selector());
        }
        Ok(Self {
            ref_name: parse_reflog_ref(ref_part)?,
            index,
        })
    }
}

/// Validate `spec` as a reflog reference, accepting `HEAD` and branch shorthand.
///
/// First tries `spec` unchanged using [`RefName`] validation. If that fails and
/// `spec` is neither `HEAD` nor prefixed with `refs/`, tries `refs/heads/{spec}`.
/// Thus `main` becomes `refs/heads/main`, while a valid multi-component name such
/// as `topic/nested` is retained as supplied. No whitespace is trimmed and no
/// repository is read; a valid name may refer to a missing reference or log.
///
/// # Errors
///
/// Returns [`Error::ReflogUnknownRef`] with the supplied `spec` when neither form
/// is valid.
pub fn parse_reflog_ref(spec: &str) -> Result<RefName> {
    if let Ok(name) = RefName::try_from(spec) {
        return Ok(name);
    }
    if spec != "HEAD" && !spec.starts_with("refs/") {
        if let Ok(name) = RefName::try_from(&format!("refs/heads/{spec}")) {
            return Ok(name);
        }
    }
    Err(Error::ReflogUnknownRef {
        refname: spec.to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn reference_validation_preserves_existing_shorthand_rules() {
        for (input, expected) in [
            ("HEAD", "HEAD"),
            ("main", "refs/heads/main"),
            ("refs/heads/main", "refs/heads/main"),
            ("refs/remotes/origin/main", "refs/remotes/origin/main"),
            ("refs/stash", "refs/stash"),
            ("topic/nested", "topic/nested"),
            ("révision", "refs/heads/révision"),
        ] {
            assert_eq!(parse_reflog_ref(input).unwrap().as_str(), expected);
        }
        for spec in ["", " main ", "refs/bad..name", "bad name", "a.lock", "@{"] {
            assert!(
                matches!(parse_reflog_ref(spec), Err(Error::ReflogUnknownRef { refname }) if refname == spec)
            );
        }
    }

    #[test]
    fn selectors_parse_references_and_newest_first_indices() {
        for (input, name, index) in [
            ("HEAD@{0}", "HEAD", 0),
            ("main@{12}", "refs/heads/main", 12),
            ("  main  @{002} ", "refs/heads/main", 2),
            ("refs/stash@{+1}", "refs/stash", 1),
            ("topic/nested@{3}", "topic/nested", 3),
        ] {
            let parsed: ReflogSelector = input.parse().unwrap();
            assert_eq!(parsed.ref_name.as_str(), name);
            assert_eq!(parsed.index, index);
        }
        let max: ReflogSelector = format!("HEAD@{{{}}}", usize::MAX).parse().unwrap();
        assert_eq!(max.index, usize::MAX);
    }

    #[test]
    fn selectors_return_distinct_typed_failures() {
        for input in ["", "HEAD", "HEAD@0", "HEAD@{0", "HEAD@{0}tail", " @{0} "] {
            assert!(
                matches!(input.parse::<ReflogSelector>(), Err(Error::ReflogInvalidSelector { spec }) if spec == input.trim())
            );
        }
        for input in [
            "HEAD@{}",
            "HEAD@{-1}",
            "HEAD@{1.0}",
            "HEAD@{ 1}",
            "HEAD@{0}}",
            "HEAD@{1}@{2}",
        ] {
            assert!(
                matches!(input.parse::<ReflogSelector>(), Err(Error::ReflogInvalidIndex { spec, .. }) if spec == input)
            );
        }
        let overflow = format!("HEAD@{{{}0}}", usize::MAX);
        let err = overflow.parse::<ReflogSelector>().unwrap_err();
        assert!(
            matches!(&err, Error::ReflogInvalidIndex { source, .. } if *source.kind() == std::num::IntErrorKind::PosOverflow)
        );
        assert!(std::error::Error::source(&err).is_some());
        assert!(
            matches!("bad name@{0}".parse::<ReflogSelector>(), Err(Error::ReflogUnknownRef { refname }) if refname == "bad name")
        );
        // Preserve index validation before checking an empty reference.
        assert!(matches!(
            "@{bad}".parse::<ReflogSelector>(),
            Err(Error::ReflogInvalidIndex { .. })
        ));
    }
}
