//! Pre-clap argv normalization for Git-style diff short options.

use grit_lib::diff::parse_diff_rename_threshold_percent;

/// Expand glued Git diff short options so clap sees separate tokens (`-M30` → `-M`, `30`).
///
/// Handles `-M`, `-M<n>`, `-M<n>%`, and `-U<n>` the way Git attaches optional values to short flags.
#[must_use]
pub fn expand_git_diff_short_options<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for arg in args {
        let s = arg.as_ref();
        if let Some(rest) = s.strip_prefix("-M") {
            if rest.is_empty() {
                out.push("-M".to_string());
            } else if parse_diff_rename_threshold_percent(rest).is_some() {
                out.push("-M".to_string());
                out.push(rest.to_string());
            } else {
                out.push(s.to_string());
            }
        } else if let Some(rest) = s.strip_prefix("-U") {
            if rest.is_empty() {
                out.push("-U".to_string());
            } else if rest.chars().all(|c| c.is_ascii_digit()) {
                out.push("-U".to_string());
                out.push(rest.to_string());
            } else {
                out.push(s.to_string());
            }
        } else {
            out.push(s.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_glued_rename_and_unified() {
        assert_eq!(
            expand_git_diff_short_options(["grit", "diff", "-M30", "HEAD"]),
            vec!["grit", "diff", "-M", "30", "HEAD"]
        );
        assert_eq!(
            expand_git_diff_short_options(["grit", "diff", "-M30%", "HEAD"]),
            vec!["grit", "diff", "-M", "30%", "HEAD"]
        );
        assert_eq!(
            expand_git_diff_short_options(["grit", "diff", "-M", "HEAD"]),
            vec!["grit", "diff", "-M", "HEAD"]
        );
        assert_eq!(
            expand_git_diff_short_options(["grit", "diff", "-U0", "-M50"]),
            vec!["grit", "diff", "-U", "0", "-M", "50"]
        );
    }

    #[test]
    fn expand_leaves_unrelated_flags() {
        assert_eq!(
            expand_git_diff_short_options(["grit", "diff", "-w", "--stat"]),
            vec!["grit", "diff", "-w", "--stat"]
        );
    }
}
