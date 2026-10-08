//! User-facing messages for ref short-name validation errors.

use grit_lib::check_ref_format::RefNameError;

/// Format a branch creation failure for stdout/stderr.
#[must_use]
pub fn branch_short_name_error_message(name: &str, err: &RefNameError) -> String {
    if matches!(
        err,
        RefNameError::ReservedBranchName | RefNameError::BranchStartsWithDash
    ) {
        return format!("'{name}' is not a valid branch name");
    }
    format!("'{name}' is not a valid branch name ({err})")
}

/// Format a tag creation failure for stdout/stderr.
#[must_use]
pub fn tag_short_name_error_message(name: &str, err: &RefNameError) -> String {
    format!("'{name}' is not a valid tag name ({err})")
}
