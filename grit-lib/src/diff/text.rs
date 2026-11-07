//! Unified diff text for two in-memory sides, configured by one [`UnifiedDiffOptions`].
//!
//! [`unified_diff`] renders the same bytes as `git diff --no-index` from the `---` line on, and
//! [`line_change_counts`] counts the `+` / `-` lines of that patch without rendering it.

use crate::quote_path::format_diff_path_with_prefix;
use crate::userdiff::FuncnameMatcher;

use super::core::{
    blank_line_ignorable, compacted_line_ops_for_options, count_line_changes_with_options,
    extract_function_context, imara_unified_hunk_slices, patience_compacted_line_ops,
    unified_body_from_ops, unified_hunk_body_for_diff_options,
};
use super::detail::LineStat;
use super::options::{DiffAlgorithm, DiffOptions};
use super::unified_emit::{LineSide, UnifiedEmitOptions};
use super::WhitespaceMode;

/// Marker line Git writes after a patch line whose source line has no trailing newline.
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// One side's `---` / `+++` header label in [`UnifiedDiffOptions`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLabel<'a> {
    /// `/dev/null`: this side does not exist (an added or deleted file).
    DevNull,
    /// `prefix` followed by `path` (for example `a/` and `src/lib.rs`).
    ///
    /// The whole label is C-quoted when the path needs it (see [`PathQuoting`]), and a tab is
    /// appended when the path contains a space, exactly as Git writes it.
    Path {
        /// Side prefix, usually `a/` or `b/`; empty for `--no-prefix` output.
        prefix: &'a str,
        /// Repository-relative or filesystem path of this side.
        path: &'a str,
    },
    /// Written as given, with no quoting.
    Verbatim(&'a str),
}

/// How [`DiffLabel::Path`] labels quote unusual path bytes (Git's `core.quotePath`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathQuoting {
    /// Quote control characters, `"`, `\` and every byte above `0x7f` (`core.quotePath=true`,
    /// Git's default).
    #[default]
    EscapeNonAscii,
    /// Quote control characters, `"` and `\`, but keep non-ASCII bytes as they are
    /// (`core.quotePath=false`).
    KeepNonAscii,
}

/// Whether patches carry Git's `\ No newline at end of file` marker lines.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoNewlineMarker {
    /// Write the marker after each patch line whose source line lacks a newline (Git's output).
    #[default]
    Emit,
    /// Leave the markers out, for display that does not need to round-trip through `git apply`.
    Omit,
}

/// Git's indent heuristic for placing a change whose boundaries are ambiguous.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndentHeuristic {
    /// Slide ambiguous change groups to the most readable position (Git's default).
    #[default]
    Enabled,
    /// Keep plain change compaction (`--no-indent-heuristic`).
    Disabled,
}

/// How far each hunk reaches beyond its changed lines.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HunkExtent {
    /// [`UnifiedDiffOptions::context_lines`] unchanged lines on each side (Git `-U`).
    #[default]
    Context,
    /// The whole enclosing function, found with the function-name rules (Git `-W`).
    Function,
}

/// Everything that shapes a unified diff of two text sides.
///
/// [`Default`] matches `git diff --no-index` with no options, except that no `---` / `+++`
/// labels are set, so the output starts at the first `@@` hunk header. Set labels with
/// [`with_paths`](Self::with_paths) or [`with_labels`](Self::with_labels).
///
/// # Examples
///
/// ```
/// use grit_lib::diff::{unified_diff, DiffAlgorithm, UnifiedDiffOptions};
///
/// let options = UnifiedDiffOptions::new()
///     .with_paths("greeting.txt", "greeting.txt")
///     .with_algorithm(DiffAlgorithm::Histogram)
///     .with_context_lines(1);
/// let patch = unified_diff("hello\nworld\n", "hello\nthere\n", &options);
/// assert_eq!(
///     patch,
///     "--- a/greeting.txt\n+++ b/greeting.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+there\n"
/// );
/// ```
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct UnifiedDiffOptions<'a> {
    /// Label for the `---` line; `None` omits the line.
    pub old_label: Option<DiffLabel<'a>>,
    /// Label for the `+++` line; `None` omits the line.
    pub new_label: Option<DiffLabel<'a>>,
    /// Quoting for [`DiffLabel::Path`] labels.
    pub path_quoting: PathQuoting,
    /// Unchanged lines kept around each change (Git `-U`, default 3).
    pub context_lines: usize,
    /// Extra unchanged lines allowed between two changes before they split into separate hunks
    /// (Git `--inter-hunk-context`, default 0).
    pub inter_hunk_context: usize,
    /// Function-name rules for the text after each `@@ … @@` header and for
    /// [`HunkExtent::Function`]. `None` uses Git's default rule: the
    /// nearest earlier line that starts with a letter, `_` or `$`.
    pub funcname: Option<&'a FuncnameMatcher>,
    /// Line alignment algorithm. Ignored when [`anchors`](Self::anchors) is non-empty, because
    /// anchored diffs always use patience alignment, as in Git.
    pub algorithm: DiffAlgorithm,
    /// Placement of changes whose boundaries are ambiguous.
    pub indent_heuristic: IndentHeuristic,
    /// How far hunks reach: context lines, or the whole enclosing function (`-W`).
    pub extent: HunkExtent,
    /// `--anchored` prefixes: a line that starts with one of them and occurs exactly once on
    /// each side is kept as unchanged context instead of appearing as a move.
    pub anchors: Vec<String>,
    /// Whitespace rules for comparing lines (`-w`, `-b`, `--ignore-space-at-eol`,
    /// `--ignore-blank-lines`). Patch lines always show the original text.
    pub whitespace: WhitespaceMode,
    /// Whether `\ No newline at end of file` markers are written.
    pub no_newline_marker: NoNewlineMarker,
}

impl Default for UnifiedDiffOptions<'_> {
    fn default() -> Self {
        Self {
            old_label: None,
            new_label: None,
            path_quoting: PathQuoting::default(),
            context_lines: 3,
            inter_hunk_context: 0,
            funcname: None,
            algorithm: DiffAlgorithm::default(),
            indent_heuristic: IndentHeuristic::default(),
            extent: HunkExtent::default(),
            anchors: Vec::new(),
            whitespace: WhitespaceMode::default(),
            no_newline_marker: NoNewlineMarker::default(),
        }
    }
}

impl<'a> UnifiedDiffOptions<'a> {
    /// Same as [`Default::default`]: Git's defaults with no header labels.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Label the sides `a/<old_path>` and `b/<new_path>`, as `git diff` does.
    #[must_use]
    pub fn with_paths(self, old_path: &'a str, new_path: &'a str) -> Self {
        self.with_labels(
            DiffLabel::Path {
                prefix: "a/",
                path: old_path,
            },
            DiffLabel::Path {
                prefix: "b/",
                path: new_path,
            },
        )
    }

    /// Set both `---` / `+++` labels.
    #[must_use]
    pub fn with_labels(mut self, old: DiffLabel<'a>, new: DiffLabel<'a>) -> Self {
        self.old_label = Some(old);
        self.new_label = Some(new);
        self
    }

    /// Set how path labels quote unusual bytes.
    #[must_use]
    pub fn with_path_quoting(mut self, path_quoting: PathQuoting) -> Self {
        self.path_quoting = path_quoting;
        self
    }

    /// Set the context line count (`-U`).
    #[must_use]
    pub fn with_context_lines(mut self, context_lines: usize) -> Self {
        self.context_lines = context_lines;
        self
    }

    /// Set the inter-hunk context (`--inter-hunk-context`).
    #[must_use]
    pub fn with_inter_hunk_context(mut self, inter_hunk_context: usize) -> Self {
        self.inter_hunk_context = inter_hunk_context;
        self
    }

    /// Use `matcher` for hunk-header function names and function context.
    #[must_use]
    pub fn with_funcname(mut self, matcher: &'a FuncnameMatcher) -> Self {
        self.funcname = Some(matcher);
        self
    }

    /// Set the line alignment algorithm.
    #[must_use]
    pub fn with_algorithm(mut self, algorithm: DiffAlgorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set the indent heuristic.
    #[must_use]
    pub fn with_indent_heuristic(mut self, indent_heuristic: IndentHeuristic) -> Self {
        self.indent_heuristic = indent_heuristic;
        self
    }

    /// Set how far hunks reach (`HunkExtent::Function` is Git's `-W`).
    #[must_use]
    pub fn with_extent(mut self, extent: HunkExtent) -> Self {
        self.extent = extent;
        self
    }

    /// Replace the `--anchored` prefixes.
    #[must_use]
    pub fn with_anchors<I, S>(mut self, anchors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.anchors = anchors.into_iter().map(Into::into).collect();
        self
    }

    /// Set the whitespace comparison rules.
    #[must_use]
    pub fn with_whitespace(mut self, whitespace: WhitespaceMode) -> Self {
        self.whitespace = whitespace;
        self
    }

    /// Choose whether `\ No newline at end of file` markers are written.
    #[must_use]
    pub fn with_no_newline_marker(mut self, marker: NoNewlineMarker) -> Self {
        self.no_newline_marker = marker;
        self
    }

    /// The line-alignment subset, in the form the tree and structured diff engines take.
    fn line_options(&self) -> DiffOptions {
        DiffOptions {
            context_lines: self.context_lines,
            inter_hunk_context: self.inter_hunk_context,
            algorithm: self.algorithm,
            indent_heuristic: self.indent_heuristic == IndentHeuristic::Enabled,
            whitespace: self.whitespace,
            function_context: self.extent == HunkExtent::Function,
            ..DiffOptions::default()
        }
    }

    fn emit_options(&self) -> UnifiedEmitOptions {
        UnifiedEmitOptions {
            context_lines: self.context_lines,
            inter_hunk_context: self.inter_hunk_context,
            function_context: self.extent == HunkExtent::Function,
        }
    }

    /// Whether counts must come from rendered hunks: anchors change the alignment, and
    /// `--ignore-blank-lines` drops changes only when hunks are emitted.
    fn needs_rendered_counts(&self) -> bool {
        !self.anchors.is_empty() || self.whitespace.ignore_blank_lines
    }
}

/// Carries the line settings of a repository diff (context, inter-hunk context, algorithm,
/// indent heuristic, whitespace rules and `-W`) over to a text diff. Labels, function-name
/// rules and anchors stay at their defaults.
impl From<&DiffOptions> for UnifiedDiffOptions<'_> {
    fn from(options: &DiffOptions) -> Self {
        Self {
            context_lines: options.context_lines,
            inter_hunk_context: options.inter_hunk_context,
            algorithm: options.algorithm,
            indent_heuristic: if options.indent_heuristic {
                IndentHeuristic::Enabled
            } else {
                IndentHeuristic::Disabled
            },
            extent: if options.function_context {
                HunkExtent::Function
            } else {
                HunkExtent::Context
            },
            whitespace: options.whitespace,
            ..Self::default()
        }
    }
}

/// Unified diff of two text sides.
///
/// Writes the `---` / `+++` lines for the labels that are set, then one `@@` hunk per group of
/// changes, with function names after the hunk headers. The bytes match `git diff --no-index`
/// with the same options from the `---` line on.
///
/// Returns an empty string when no hunk survives: the sides are equal, or every difference is
/// ignored by [`UnifiedDiffOptions::whitespace`].
///
/// # Examples
///
/// ```
/// use grit_lib::diff::{unified_diff, UnifiedDiffOptions, WhitespaceMode};
///
/// let options = UnifiedDiffOptions::new().with_whitespace(WhitespaceMode::ignore_space_change());
/// assert!(unified_diff("a  b\n", "a b\n", &options).is_empty());
/// assert_eq!(
///     unified_diff("one\n", "one\ntwo", &UnifiedDiffOptions::new()),
///     "@@ -1 +1,2 @@\n one\n+two\n\\ No newline at end of file\n"
/// );
/// ```
#[must_use]
pub fn unified_diff(old: &str, new: &str, options: &UnifiedDiffOptions<'_>) -> String {
    let body = hunk_body(old, new, options);
    if body.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(body.len() + 128);
    if let Some(label) = &options.old_label {
        push_label_line(&mut out, "--- ", label, options.path_quoting);
    }
    if let Some(label) = &options.new_label {
        push_label_line(&mut out, "+++ ", label, options.path_quoting);
    }
    push_hunks_with_funcnames(&mut out, &body, old, options.funcname);
    if options.no_newline_marker == NoNewlineMarker::Omit {
        // Patch lines always carry a ` `, `+` or `-` prefix, so only a whole line equal to the
        // marker is an annotation; source text that reads like the marker must survive.
        out = out
            .split_inclusive('\n')
            .filter(|line| *line != NO_NEWLINE_MARKER)
            .collect();
    }
    out
}

/// Lines added and removed by the patch [`unified_diff`] renders for the same options.
///
/// Labels, function-name rules and markers do not change the counts. For the common case (no
/// anchors and no `--ignore-blank-lines`) no patch text is built.
///
/// # Examples
///
/// ```
/// use grit_lib::diff::{line_change_counts, UnifiedDiffOptions};
///
/// let stat = line_change_counts("a\nb\nc\n", "a\nB\nc\nd\n", &UnifiedDiffOptions::new());
/// assert_eq!((stat.insertions, stat.deletions), (2, 1));
/// ```
#[must_use]
pub fn line_change_counts(old: &str, new: &str, options: &UnifiedDiffOptions<'_>) -> LineStat {
    if !options.needs_rendered_counts() {
        let (insertions, deletions) =
            count_line_changes_with_options(old, new, &options.line_options());
        return LineStat {
            insertions,
            deletions,
        };
    }
    hunk_body(old, new, options)
        .lines()
        .fold(LineStat::default(), |mut stat, line| {
            match line.as_bytes().first() {
                Some(b'+') => stat.insertions += 1,
                Some(b'-') => stat.deletions += 1,
                _ => {}
            }
            stat
        })
}

/// Hunks only (`@@` headers without function names), with no-newline markers.
fn hunk_body(old: &str, new: &str, options: &UnifiedDiffOptions<'_>) -> String {
    if options.anchors.is_empty() && options.extent == HunkExtent::Context {
        return unified_hunk_body_for_diff_options(old, new, &options.line_options());
    }
    let before = LineSide::from_text(old);
    let after = LineSide::from_text(new);
    let ops = if options.anchors.is_empty() {
        compacted_line_ops_for_options(old, new, &options.line_options())
    } else {
        let anchored: Vec<bool> = after
            .lines()
            .map(|line| {
                options
                    .anchors
                    .iter()
                    .any(|anchor| line.body.starts_with(anchor.as_str()))
            })
            .collect();
        patience_compacted_line_ops(
            &before,
            &after,
            options.indent_heuristic == IndentHeuristic::Enabled,
            options.whitespace,
            &anchored,
        )
    };
    let ws = options.whitespace;
    unified_body_from_ops(
        &before,
        &after,
        &ops,
        ws,
        options.emit_options(),
        options.funcname,
        ws.ignore_blank_lines.then(|| blank_line_ignorable(ws)),
    )
}

fn push_label_line(out: &mut String, lead: &str, label: &DiffLabel<'_>, quoting: PathQuoting) {
    out.push_str(lead);
    match *label {
        DiffLabel::DevNull => out.push_str("/dev/null"),
        DiffLabel::Verbatim(text) => out.push_str(text),
        DiffLabel::Path { prefix, path } => {
            let quote_non_ascii = quoting == PathQuoting::EscapeNonAscii;
            out.push_str(&format_diff_path_with_prefix(prefix, path, quote_non_ascii));
            if path.contains(' ') {
                out.push('\t');
            }
        }
    }
    out.push('\n');
}

/// Append `body`'s hunks, adding the enclosing function name after each `@@ … @@` header.
fn push_hunks_with_funcnames(
    out: &mut String,
    body: &str,
    old: &str,
    funcname: Option<&FuncnameMatcher>,
) {
    let old_lines: Vec<&str> = old.lines().collect();
    for hunk in imara_unified_hunk_slices(body) {
        let Some((header, rest)) = hunk.split_once('\n') else {
            out.push_str(hunk);
            continue;
        };
        out.push_str(header);
        if let Some(name) = extract_function_context(header, &old_lines, funcname, None) {
            out.push(' ');
            out.push_str(&name);
        }
        out.push('\n');
        out.push_str(rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_git_defaults_and_no_labels() {
        let options = UnifiedDiffOptions::default();
        assert_eq!(options.context_lines, 3);
        assert_eq!(options.inter_hunk_context, 0);
        assert_eq!(options.algorithm, DiffAlgorithm::Myers);
        assert_eq!(options.indent_heuristic, IndentHeuristic::Enabled);
        assert_eq!(options.extent, HunkExtent::Context);
        assert!(options.old_label.is_none() && options.new_label.is_none());
        assert_eq!(options.no_newline_marker, NoNewlineMarker::Emit);
        assert_eq!(options.path_quoting, PathQuoting::EscapeNonAscii);
    }

    #[test]
    fn labels_quote_and_tab_like_git() {
        let mut out = String::new();
        let label = DiffLabel::Path {
            prefix: "a/",
            path: "my file",
        };
        push_label_line(&mut out, "--- ", &label, PathQuoting::EscapeNonAscii);
        push_label_line(
            &mut out,
            "+++ ",
            &DiffLabel::DevNull,
            PathQuoting::default(),
        );
        push_label_line(
            &mut out,
            "+++ ",
            &DiffLabel::Verbatim("x y"),
            PathQuoting::default(),
        );
        assert_eq!(out, "--- a/my file\t\n+++ /dev/null\n+++ x y\n");

        let mut quoted = String::new();
        let label = DiffLabel::Path {
            prefix: "b/",
            path: "caf\u{e9}",
        };
        push_label_line(&mut quoted, "+++ ", &label, PathQuoting::EscapeNonAscii);
        assert_eq!(quoted, "+++ \"b/caf\\303\\251\"\n");
        let mut kept = String::new();
        push_label_line(&mut kept, "+++ ", &label, PathQuoting::KeepNonAscii);
        assert_eq!(kept, "+++ b/caf\u{e9}\n");
    }

    #[test]
    fn omit_marker_drops_no_newline_lines() {
        let options = UnifiedDiffOptions::new().with_no_newline_marker(NoNewlineMarker::Omit);
        assert_eq!(unified_diff("a", "b", &options), "@@ -1 +1 @@\n-a\n+b\n");
    }

    #[test]
    fn omit_marker_keeps_source_lines_that_read_like_the_marker() {
        let omit = UnifiedDiffOptions::new().with_no_newline_marker(NoNewlineMarker::Omit);
        let marker = "\\ No newline at end of file";
        let cases = [
            (
                format!("{marker}\nold\n"),
                format!("{marker}\nnew\n"),
                format!("@@ -1,2 +1,2 @@\n {marker}\n-old\n+new\n"),
            ),
            (
                "a\n".to_owned(),
                format!("a\n{marker}\n"),
                format!("@@ -1 +1,2 @@\n a\n+{marker}\n"),
            ),
            (
                format!("a\n{marker}\n"),
                "a\n".to_owned(),
                format!("@@ -1,2 +1 @@\n a\n-{marker}\n"),
            ),
            (
                format!("{marker}\nx"),
                format!("{marker}\ny"),
                format!("@@ -1,2 +1,2 @@\n {marker}\n-x\n+y\n"),
            ),
        ];
        for (old, new, expected) in cases {
            assert_eq!(
                unified_diff(&old, &new, &omit),
                expected,
                "{old:?} -> {new:?}"
            );
        }
    }

    #[test]
    fn equal_sides_render_nothing_even_with_labels() {
        let options = UnifiedDiffOptions::new().with_paths("f", "f");
        assert_eq!(unified_diff("same\n", "same\n", &options), "");
    }

    #[test]
    fn from_diff_options_carries_line_settings() {
        let diff = DiffOptions::default()
            .with_context_lines(1)
            .with_inter_hunk_context(4)
            .with_algorithm(DiffAlgorithm::Patience)
            .with_indent_heuristic(false)
            .with_whitespace(WhitespaceMode::ignore_all_space());
        let options = UnifiedDiffOptions::from(&diff);
        assert_eq!(options.context_lines, 1);
        assert_eq!(options.inter_hunk_context, 4);
        assert_eq!(options.algorithm, DiffAlgorithm::Patience);
        assert_eq!(options.indent_heuristic, IndentHeuristic::Disabled);
        assert_eq!(options.whitespace, WhitespaceMode::ignore_all_space());
        assert!(options.old_label.is_none());
    }

    #[test]
    fn anchored_counts_follow_rendered_patch() {
        let old = "a\nb\nc\n";
        let new = "c\na\nb\n";
        let anchored = UnifiedDiffOptions::new().with_anchors(["c"]);
        let stat = line_change_counts(old, new, &anchored);
        assert_eq!((stat.insertions, stat.deletions), (2, 2));
        assert_eq!(
            unified_diff(old, new, &anchored),
            "@@ -1,3 +1,3 @@\n-a\n-b\n c\n+a\n+b\n"
        );
        let plain = line_change_counts(old, new, &UnifiedDiffOptions::new());
        assert_eq!((plain.insertions, plain.deletions), (1, 1));
    }
}
