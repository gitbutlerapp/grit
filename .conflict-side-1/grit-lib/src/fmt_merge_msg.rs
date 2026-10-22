//! Merge commit message formatter — `git fmt-merge-msg` logic.
//!
//! Reads FETCH_HEAD-style lines and produces a human-readable merge commit
//! message of the form:
//!
//! ```text
//! Merge branch 'foo' of https://example.com/repo
//! ```
//!
//! or for multiple branches:
//!
//! ```text
//! Merge branches 'a', 'b' and 'c'
//! ```
//!
//! # Input format
//!
//! Each line is one of:
//!
//! ```text
//! <sha1>TAB<desc>TABbranch '<name>' of <url>
//! <sha1>TABnot-for-mergeTabranch '<name>' of <url>
//! ```
//!
//! Lines with `not-for-merge` are skipped.

/// Options for [`fmt_merge_msg`].
#[derive(Debug, Clone, Default)]
pub struct FmtMergeMsgOptions {
    /// Override the first line of the message with this text.
    /// When set, the branch-name title is suppressed.
    pub message: Option<String>,
    /// Override the target branch name shown in `into <branch>`.
    pub into_name: Option<String>,
}

/// Format a merge commit message from FETCH_HEAD-style input.
///
/// `input` should contain lines in FETCH_HEAD format.  Returns the formatted
/// message including a trailing newline, or an empty string when there is
/// nothing to merge.
pub fn fmt_merge_msg(input: &str, opts: &FmtMergeMsgOptions) -> String {
    let entries = parse_fetch_head(input);

    let mut output = String::new();

    if let Some(ref msg) = opts.message {
        output.push_str(msg);
    } else if !entries.is_empty() {
        let title = build_title(&entries, opts.into_name.as_deref());
        output.push_str(&title);
    }

    // Ensure trailing newline.
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }

    output
}

/// A parsed FETCH_HEAD entry that is for-merge.
#[derive(Debug, Clone)]
struct FetchEntry {
    /// The description field (everything after the first two TABs, or after
    /// the first TAB for simplified formats).
    description: String,
}

/// Parse FETCH_HEAD lines and return only for-merge entries.
fn parse_fetch_head(input: &str) -> Vec<FetchEntry> {
    let mut entries = Vec::new();

    for line in input.lines() {
        // Find the first TAB.
        let first_tab = match line.find('\t') {
            Some(p) => p,
            None => continue,
        };

        let rest = &line[first_tab + 1..];

        // Skip not-for-merge lines.
        if rest.starts_with("not-for-merge") {
            continue;
        }

        // For for-merge lines the real FETCH_HEAD format is:
        //   <sha1>\t\t<description>
        // The second field is empty (an empty flag field), so we have two
        // consecutive TABs.  Skip the second TAB if present.
        let desc = rest.strip_prefix('\t').unwrap_or(rest);

        if desc.is_empty() {
            continue;
        }

        entries.push(FetchEntry {
            description: desc.to_owned(),
        });
    }

    entries
}

/// Categorize an entry description.
#[derive(Debug, Clone)]
enum MergeKind {
    Branch { name: String, url: Option<String> },
    Tag { name: String, url: Option<String> },
    RemoteTracking { name: String, url: Option<String> },
    Generic(String),
}

impl MergeKind {
    fn from_description(desc: &str) -> Self {
        if let Some(rest) = desc.strip_prefix("branch '") {
            parse_quoted_name_and_url(rest, KindTag::Branch)
        } else if let Some(rest) = desc.strip_prefix("tag '") {
            parse_quoted_name_and_url(rest, KindTag::Tag)
        } else if let Some(rest) = desc.strip_prefix("remote-tracking branch '") {
            parse_quoted_name_and_url(rest, KindTag::RemoteTracking)
        } else {
            MergeKind::Generic(desc.to_owned())
        }
    }
}

enum KindTag {
    Branch,
    Tag,
    RemoteTracking,
}

fn parse_quoted_name_and_url(rest: &str, tag: KindTag) -> MergeKind {
    let close = match rest.find('\'') {
        Some(p) => p,
        None => return MergeKind::Generic(rest.to_owned()),
    };
    let name = rest[..close].to_owned();
    let after = &rest[close + 1..];
    let url = after
        .strip_prefix(" of ")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());

    match tag {
        KindTag::Branch => MergeKind::Branch { name, url },
        KindTag::Tag => MergeKind::Tag { name, url },
        KindTag::RemoteTracking => MergeKind::RemoteTracking { name, url },
    }
}

/// Per-source-repository merge data.
#[derive(Debug, Default)]
struct SrcData {
    branches: Vec<String>,
    tags: Vec<String>,
    remote_branches: Vec<String>,
    generics: Vec<String>,
}

/// Build a `Merge …` title line from for-merge entries.
fn build_title(entries: &[FetchEntry], into_name: Option<&str>) -> String {
    // Preserve insertion order for sources while still allowing fast lookup.
    let mut src_order: Vec<String> = Vec::new();
    let mut src_map: std::collections::HashMap<String, SrcData> = std::collections::HashMap::new();

    for entry in entries {
        let kind = MergeKind::from_description(&entry.description);

        let (src, cat, name): (String, &str, String) = match kind {
            MergeKind::Branch { name, url } => {
                (url.unwrap_or_else(|| ".".to_owned()), "branch", name)
            }
            MergeKind::Tag { name, url } => (url.unwrap_or_else(|| ".".to_owned()), "tag", name),
            MergeKind::RemoteTracking { name, url } => {
                (url.unwrap_or_else(|| ".".to_owned()), "remote", name)
            }
            MergeKind::Generic(desc) => (".".to_owned(), "generic", desc),
        };

        if !src_map.contains_key(&src) {
            src_order.push(src.clone());
            src_map.insert(src.clone(), SrcData::default());
        }
        // Safety: we just inserted `src` if it was missing, so get_mut always succeeds.
        let Some(data) = src_map.get_mut(&src) else {
            continue;
        };
        match cat {
            "branch" => data.branches.push(name),
            "tag" => data.tags.push(name),
            "remote" => data.remote_branches.push(name),
            _ => data.generics.push(name),
        }
    }

    if src_order.is_empty() {
        return String::new();
    }

    let mut out = String::from("Merge ");
    let mut first_src = true;

    for src in &src_order {
        let data = &src_map[src];

        if !first_src {
            out.push_str("; ");
        }
        first_src = false;

        let mut subsep = "";

        if !data.branches.is_empty() {
            out.push_str(subsep);
            subsep = ", ";
            append_joined("branch ", "branches ", &data.branches, &mut out);
        }
        if !data.remote_branches.is_empty() {
            out.push_str(subsep);
            subsep = ", ";
            append_joined(
                "remote-tracking branch ",
                "remote-tracking branches ",
                &data.remote_branches,
                &mut out,
            );
        }
        if !data.tags.is_empty() {
            out.push_str(subsep);
            subsep = ", ";
            append_joined("tag ", "tags ", &data.tags, &mut out);
        }
        if !data.generics.is_empty() {
            out.push_str(subsep);
            append_joined("commit ", "commits ", &data.generics, &mut out);
        }

        if src != "." {
            out.push_str(" of ");
            out.push_str(src);
        }
    }

    // Append "into <branch>" unless the destination is suppressed.
    if let Some(name) = into_name {
        if !is_suppressed_dest(name) {
            out.push_str(" into ");
            out.push_str(name);
        }
    }

    out
}

/// Git's default suppress-dest patterns (`main`, `master`).
fn is_suppressed_dest(dest: &str) -> bool {
    dest == "main" || dest == "master"
}

/// Append `singular'<n>'` or `plural'<a>', '<b>' and '<c>'` to `out`.
fn append_joined(singular: &str, plural: &str, names: &[String], out: &mut String) {
    match names.len() {
        0 => {}
        1 => {
            out.push_str(singular);
            out.push('\'');
            out.push_str(&names[0]);
            out.push('\'');
        }
        n => {
            out.push_str(plural);
            for (i, name) in names[..(n - 1)].iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('\'');
                out.push_str(name);
                out.push('\'');
            }
            out.push_str(" and '");
            out.push_str(&names[n - 1]);
            out.push('\'');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let out = fmt_merge_msg("", &FmtMergeMsgOptions::default());
        assert!(out.is_empty());
    }

    #[test]
    fn not_for_merge_skipped() {
        let input = "abc123\tnot-for-merge\tbranch 'old' of https://x.com\n";
        let out = fmt_merge_msg(input, &FmtMergeMsgOptions::default());
        assert!(out.is_empty(), "got: {out:?}");
    }

    #[test]
    fn single_branch_local() {
        // Local merge (no URL) — two TABs.
        let input = "abc123\t\tbranch 'feature'\n";
        let out = fmt_merge_msg(input, &FmtMergeMsgOptions::default());
        assert_eq!(out.trim_end(), "Merge branch 'feature'");
    }

    #[test]
    fn single_branch_remote() {
        let input = "abc123\t\tbranch 'main' of https://example.com/repo\n";
        let out = fmt_merge_msg(input, &FmtMergeMsgOptions::default());
        assert!(out.contains("branch 'main'"), "got: {out:?}");
        assert!(out.contains("of https://example.com/repo"), "got: {out:?}");
    }

    #[test]
    fn multiple_branches() {
        let input = "a1\t\tbranch 'foo'\nb2\t\tbranch 'bar'\n";
        let out = fmt_merge_msg(input, &FmtMergeMsgOptions::default());
        assert!(out.contains("branches"), "got: {out:?}");
        assert!(out.contains("'foo'"), "got: {out:?}");
        assert!(out.contains("'bar'"), "got: {out:?}");
    }

    #[test]
    fn custom_message() {
        let input = "abc123\t\tbranch 'foo'\n";
        let opts = FmtMergeMsgOptions {
            message: Some("Custom".to_owned()),
            into_name: None,
        };
        let out = fmt_merge_msg(input, &opts);
        assert!(out.starts_with("Custom"), "got: {out:?}");
    }

    #[test]
    fn into_name_suppressed_for_main() {
        let input = "abc123\t\tbranch 'feature'\n";
        let opts = FmtMergeMsgOptions {
            message: None,
            into_name: Some("main".to_owned()),
        };
        let out = fmt_merge_msg(input, &opts);
        assert!(!out.contains("into main"), "got: {out:?}");
    }

    #[test]
    fn into_name_shown_for_other() {
        let input = "abc123\t\tbranch 'feature'\n";
        let opts = FmtMergeMsgOptions {
            message: None,
            into_name: Some("develop".to_owned()),
        };
        let out = fmt_merge_msg(input, &opts);
        assert!(out.contains("into develop"), "got: {out:?}");
    }

    #[test]
    fn append_joined_two() {
        let mut s = String::new();
        append_joined(
            "branch ",
            "branches ",
            &["foo".to_owned(), "bar".to_owned()],
            &mut s,
        );
        assert_eq!(s, "branches 'foo' and 'bar'");
    }

    #[test]
    fn append_joined_three() {
        let mut s = String::new();
        append_joined(
            "branch ",
            "branches ",
            &["a".to_owned(), "b".to_owned(), "c".to_owned()],
            &mut s,
        );
        assert_eq!(s, "branches 'a', 'b' and 'c'");
    }
}
