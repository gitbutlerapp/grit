//! `grit show` — show information about a commit, tag, or branch.
//!
//! Resolves the given name (default `HEAD`) and prints: for a branch, a header
//! plus the commit it points at; for a lightweight tag, the same; for an
//! annotated tag, the tag's own metadata and message followed by the target
//! commit; and for any commit, its metadata, message, and the change it
//! introduced (delta-style patch by default, or `--stat` / other diff formats).
//!
//! Merge commits default to combined parent diffs (`--diff-merges=combined`, like
//! `git show --cc`). Use `--diff-merges=first-parent` for first-parent-only diffs.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::diff::DiffMerges;
use grit_lib::object_inspect::{
    format_tree_cat_file_p, tree_entry_cat_file_type, tree_entry_mode_cat_file_p, ResolveError,
};
use grit_lib::objects::{parse_tag, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{CommitDiffOptions, DiffOutcome, FileDiff, LineKind};
use crate::commits_json::CommitJsonLoader;
use crate::context;
use crate::diff_args::DiffFormatArgs;
use crate::markdown;
use crate::output::{outln, ChangeStatus, CommitJson, HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

/// Width budget for the `+`/`-` change bars in the diffstat.
const BAR_WIDTH: usize = 40;

/// Inputs for `grit show`.
pub struct ShowRunOptions {
    pub objects: Vec<String>,
    pub paths: Vec<String>,
    pub format: DiffFormatArgs,
    pub word_level_inline: bool,
    pub diff_merges: DiffMerges,
}

/// One resolved object in a multi-object show (commit metadata or `rev:path` contents).
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum ShowObjectResult {
    /// Commit, branch, tag, or annotated tag.
    Commit(ShowOutcome),
    /// Blob or tree at `revision:path`.
    Path(ShowPathOutcome),
}

/// `grit show REV:path` JSON shape.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShowPathOutcome {
    Blob {
        spec: String,
        path: String,
        oid: String,
        size: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        binary: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data_base64: Option<String>,
    },
    Tree {
        spec: String,
        path: String,
        oid: String,
        size: u64,
        entries: Vec<ShowTreeEntryJson>,
    },
}

#[derive(Serialize, JsonSchema)]
pub struct ShowTreeEntryJson {
    pub mode: String,
    #[serde(rename = "type")]
    pub object_type: String,
    pub oid: String,
    pub path: String,
}

/// Top-level show result: one object keeps the legacy JSON shape; several wrap in `objects`.
#[allow(clippy::large_enum_variant)]
pub enum ShowRunResult {
    Single(ShowObjectResult),
    Multiple(Vec<ShowObjectResult>),
}

impl Serialize for ShowRunResult {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ShowRunResult::Single(ShowObjectResult::Commit(o)) => o.serialize(serializer),
            ShowRunResult::Single(ShowObjectResult::Path(p)) => p.serialize(serializer),
            ShowRunResult::Multiple(objects) => {
                use serde::ser::SerializeStruct;
                let mut st = serializer.serialize_struct("ShowMultiple", 1)?;
                st.serialize_field("objects", objects)?;
                st.end()
            }
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct ShowMultipleJson {
    objects: Vec<ShowObjectResult>,
}

impl JsonSchema for ShowRunResult {
    fn schema_name() -> String {
        "ShowRunResult".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::{SchemaObject, SubschemaValidation};
        SchemaObject {
            subschemas: Some(Box::new(SubschemaValidation {
                one_of: Some(vec![
                    gen.subschema_for::<ShowOutcome>(),
                    gen.subschema_for::<ShowPathOutcome>(),
                    gen.subschema_for::<ShowMultipleJson>(),
                ]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

impl HumanRender for ShowRunResult {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowRunResult::Single(obj) => obj.render_human(w, ctx),
            ShowRunResult::Multiple(objects) => {
                for (i, obj) in objects.iter().enumerate() {
                    if i > 0 {
                        outln!(w);
                    }
                    obj.render_human(w, ctx)?;
                }
                Ok(())
            }
        }
    }
}

impl MarkdownRender for ShowRunResult {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowRunResult::Single(obj) => obj.render_markdown(w, ctx),
            ShowRunResult::Multiple(objects) => {
                for (i, obj) in objects.iter().enumerate() {
                    if i > 0 {
                        outln!(w);
                    }
                    obj.render_markdown(w, ctx)?;
                }
                Ok(())
            }
        }
    }
}

impl HumanRender for ShowObjectResult {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowObjectResult::Commit(o) => o.render_human(w, ctx),
            ShowObjectResult::Path(p) => p.render_human(w, ctx),
        }
    }
}

impl MarkdownRender for ShowObjectResult {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        match self {
            ShowObjectResult::Commit(o) => o.render_markdown(w, ctx),
            ShowObjectResult::Path(p) => p.render_markdown(w, ctx),
        }
    }
}

/// What `grit show` resolved (`--json` contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShowKind {
    Commit,
    Branch,
    Tag,
    AnnotatedTag,
}

/// Result of `grit show`.
#[derive(Serialize, JsonSchema)]
pub struct ShowOutcome {
    /// Non-fatal warnings emitted while resolving the object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub warnings: Vec<crate::diagnostics::WarningRecord>,
    pub kind: ShowKind,
    /// Branch or tag name, when shown via a ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    /// Annotated-tag metadata, when `kind == "annotated_tag"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagInfo>,
    pub commit: CommitJson,
    /// Per-file diff rows (same shape as `grit diff --json`).
    pub files: Vec<FileDiff>,
    /// Summary diffstat (always present for scripting convenience).
    pub stat: DiffStat,
    #[serde(skip)]
    #[schemars(skip)]
    diff: DiffOutcome,
}

#[derive(Serialize, JsonSchema)]
pub struct TagInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tagger: Option<Person>,
    pub message: String,
}

pub use crate::output::Person;

pub fn run(opts: ShowRunOptions) -> Result<ShowRunResult> {
    let mut specs = opts.objects.clone();
    if specs.is_empty() {
        specs.push("HEAD".to_owned());
    }
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        results.push(show_one(&spec, &opts)?);
    }
    Ok(match results.len() {
        0 => unreachable!("at least HEAD was requested"),
        1 => ShowRunResult::Single(
            results
                .into_iter()
                .next()
                .unwrap_or_else(|| unreachable!("single result")),
        ),
        _ => ShowRunResult::Multiple(results),
    })
}

fn show_one(spec: &str, opts: &ShowRunOptions) -> Result<ShowObjectResult> {
    if is_rev_path_spec(spec) {
        return Ok(ShowObjectResult::Path(show_rev_path(spec)?));
    }
    Ok(ShowObjectResult::Commit(show_commit(spec, opts)?))
}

fn show_commit(spec: &str, opts: &ShowRunOptions) -> Result<ShowOutcome> {
    let (repo, sink) = context::discover_with_warnings()?;
    let (kind, ref_name, tag) = classify(&repo, spec)?;

    let resolved = grit_lib::revwalk::rev_parse::resolve_revision_without_index_dwim(&repo, spec)
        .with_context(|| format!("could not resolve '{spec}'"))?;
    let (commit_oid, peeled_kind) = repo.odb().peel_tags(resolved)?;
    if peeled_kind != ObjectKind::Commit {
        bail!("'{}' does not resolve to a commit", commit_oid.to_hex());
    }
    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = CommitJsonLoader::new(&repo)?;
    let commit = loader.load_one(&repo, &commit_oid, now)?;

    let diff_opts = CommitDiffOptions {
        paths: opts.paths.clone(),
        format: opts.format.clone(),
        word_level_inline: opts.word_level_inline,
        diff_merges: opts.diff_merges,
    };
    let diff = crate::commands::diff::diff_for_commit(&repo, &commit_oid, diff_opts)?;
    let stat = diffstat(&diff);
    let files = diff.files.clone();

    Ok(ShowOutcome {
        warnings: sink.warnings(),
        kind,
        ref_name,
        tag,
        commit,
        files,
        stat,
        diff,
    })
}

fn show_rev_path(spec: &str) -> Result<ShowPathOutcome> {
    let (repo, _sink) = context::discover_with_warnings()?;
    let path = rev_path_suffix(spec);
    let resolved = repo.resolve(spec).map_err(|e| map_resolve_error(spec, e))?;
    match resolved.kind {
        ObjectKind::Blob => {
            let obj = repo.read_object_raw_for_display(&resolved.oid)?;
            let (text, data_base64, binary) = blob_json_fields(&obj.data);
            Ok(ShowPathOutcome::Blob {
                spec: spec.to_owned(),
                path,
                oid: resolved.oid.to_hex(),
                size: resolved.size,
                binary,
                text,
                data_base64,
            })
        }
        ObjectKind::Tree => {
            let obj = repo.read_object_raw_for_display(&resolved.oid)?;
            let parsed = grit_lib::objects::parse_tree(&obj.data)?;
            let entries = parsed
                .iter()
                .map(|e| ShowTreeEntryJson {
                    mode: tree_entry_mode_cat_file_p(e.mode),
                    object_type: tree_entry_cat_file_type(&repo, e.mode, &e.oid).to_owned(),
                    oid: e.oid.to_hex(),
                    path: String::from_utf8_lossy(&e.name).into_owned(),
                })
                .collect();
            Ok(ShowPathOutcome::Tree {
                spec: spec.to_owned(),
                path,
                oid: resolved.oid.to_hex(),
                size: resolved.size,
                entries,
            })
        }
        other => bail!("'{spec}' resolved to {other:?}, expected a blob or tree"),
    }
}

fn rev_path_suffix(spec: &str) -> String {
    grit_lib::revwalk::rev_parse::split_treeish_colon(spec)
        .map(|(_, path)| path.to_owned())
        .unwrap_or_default()
}

fn is_rev_path_spec(spec: &str) -> bool {
    let Some((rev, _path)) = grit_lib::revwalk::rev_parse::split_treeish_colon(spec) else {
        return false;
    };
    !rev.is_empty() && !rev.contains('\\')
}

fn blob_json_fields(data: &[u8]) -> (Option<String>, Option<String>, bool) {
    if data.is_empty() || std::str::from_utf8(data).is_ok() {
        (
            Some(String::from_utf8_lossy(data).into_owned()),
            None,
            false,
        )
    } else {
        (None, Some(STANDARD.encode(data)), true)
    }
}

fn map_resolve_error(spec: &str, err: ResolveError) -> anyhow::Error {
    match err {
        ResolveError::AmbiguousShortObject { candidates, .. } => {
            let mut msg = format!("ambiguous short object id '{spec}'");
            for hint in candidates {
                let kind = hint.kind.unwrap_or("object");
                msg.push_str(&format!("\n  {} {}", kind, hint.oid_hex));
            }
            anyhow::anyhow!(msg)
        }
        ResolveError::PathNotInTree { path, revision } => {
            anyhow::anyhow!("path '{path}' does not exist in '{revision}'")
        }
        ResolveError::UnknownRevision { .. } => anyhow::anyhow!("unknown revision '{spec}'"),
        _ => anyhow::anyhow!("could not resolve '{spec}'"),
    }
}

impl HumanRender for ShowPathOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ShowPathOutcome::Blob {
                binary, text, spec, ..
            } => {
                if *binary {
                    outln!(w, "Binary file");
                    return Ok(());
                }
                if let Some(t) = text {
                    w.write_all(t.as_bytes())
                        .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                    return Ok(());
                }
                let (repo, _) = context::discover_with_warnings()?;
                let resolved = repo.resolve(spec)?;
                let obj = repo.read_object_raw_for_display(&resolved.oid)?;
                w.write_all(&obj.data)
                    .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                Ok(())
            }
            ShowPathOutcome::Tree { spec, .. } => {
                let (repo, _) = context::discover_with_warnings()?;
                let resolved = repo.resolve(spec)?;
                let obj = repo.read_object_raw_for_display(&resolved.oid)?;
                let human = format_tree_cat_file_p(&repo, &obj.data)?;
                w.write_all(&human)
                    .map_err(|e| anyhow::anyhow!("writing tree: {e}"))?;
                Ok(())
            }
        }
    }
}

impl MarkdownRender for ShowPathOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ShowPathOutcome::Blob {
                spec,
                oid,
                size,
                binary,
                text,
                ..
            } => {
                outln!(w, "## `{}`\n", spec);
                outln!(w, "**Oid:** `{oid}`");
                outln!(w, "**Size:** {size} bytes\n");
                if *binary {
                    outln!(w, "_binary blob (see `data_base64` in JSON)_");
                } else if let Some(t) = text {
                    outln!(w, "```text");
                    outln!(w, "{t}");
                    outln!(w, "```");
                }
            }
            ShowPathOutcome::Tree { spec, entries, .. } => {
                outln!(w, "## `{}`\n", spec);
                outln!(w, "| Mode | Type | OID | Path |");
                outln!(w, "| --- | --- | --- | --- |");
                for e in entries {
                    outln!(
                        w,
                        "| {} | {} | `{}` | `{}` |",
                        e.mode,
                        e.object_type,
                        e.oid,
                        e.path
                    );
                }
            }
        }
        Ok(())
    }
}

/// A diffstat: per-file insertion/deletion counts plus totals.
#[derive(Serialize, JsonSchema)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct FileStat {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
}

/// Reduce a full diff to a per-file insertion/deletion summary.
pub(crate) fn diffstat(diff: &DiffOutcome) -> DiffStat {
    let mut files = Vec::new();
    let mut insertions = 0;
    let mut deletions = 0;
    for file in &diff.files {
        let mut ins = 0;
        let mut del = 0;
        for hunk in &file.hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Add => ins += 1,
                    LineKind::Del => del += 1,
                    LineKind::Context => {}
                }
            }
        }
        insertions += ins;
        deletions += del;
        files.push(FileStat {
            path: file.path.clone(),
            old_path: file.old_path.clone(),
            status: file.status,
            insertions: ins,
            deletions: del,
            binary: file.binary,
        });
    }
    DiffStat {
        files_changed: files.len(),
        files,
        insertions,
        deletions,
    }
}

/// Determine what `target` names: a branch, a tag (light or annotated), or a
/// bare commit. Returns `(kind, ref_name, annotated_tag_info)`.
fn classify(
    repo: &Repository,
    target: &str,
) -> Result<(ShowKind, Option<String>, Option<TagInfo>)> {
    let git_dir = repo.git_dir();

    if target == "HEAD" {
        if let Ok(Some(sym)) = refs::read_symbolic_ref(git_dir, "HEAD") {
            if let Some(branch) = sym.strip_prefix("refs/heads/") {
                return Ok((ShowKind::Branch, Some(branch.to_owned()), None));
            }
        }
        return Ok((ShowKind::Commit, None, None));
    }

    if refs::resolve_ref(git_dir, &format!("refs/heads/{target}")).is_ok() {
        return Ok((ShowKind::Branch, Some(target.to_owned()), None));
    }

    if let Ok(tag_oid) = refs::resolve_ref(git_dir, &format!("refs/tags/{target}")) {
        let object = repo.odb().read(&tag_oid)?;
        if object.kind == ObjectKind::Tag {
            let td = parse_tag(&object.data)?;
            let info = TagInfo {
                name: target.to_owned(),
                tagger: td.tagger.as_deref().map(Person::from_identity_line),
                message: td.message.trim_end().to_owned(),
            };
            return Ok((ShowKind::AnnotatedTag, Some(target.to_owned()), Some(info)));
        }
        return Ok((ShowKind::Tag, Some(target.to_owned()), None));
    }

    Ok((ShowKind::Commit, None, None))
}

// --- Human rendering --------------------------------------------------------

impl MarkdownRender for ShowOutcome {
    fn render_markdown(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        if let Some(tag) = &self.tag {
            outln!(w, "# Tag `{}`", tag.name);
            if let Some(tagger) = &tag.tagger {
                outln!(w);
                outln!(w, "**Tagger:** {} <{}>", tagger.name, tagger.email);
                outln!(w, "**Date:** {}", tagger.date);
            }
            if !tag.message.trim().is_empty() {
                outln!(w);
                print_markdown_message(w, &tag.message)?;
            }
            outln!(w);
        } else if self.kind == ShowKind::Branch {
            if let Some(name) = &self.ref_name {
                outln!(w, "# Branch `{name}`");
                outln!(w);
            }
        } else if self.kind == ShowKind::Tag {
            if let Some(name) = &self.ref_name {
                outln!(w, "# Tag `{name}`");
                outln!(w);
            }
        }

        let short = self.commit.oid.get(..7).unwrap_or(self.commit.oid.as_str());
        outln!(w, "# Commit `{short}`");
        outln!(w);
        outln!(w, "**Oid:** `{}`", self.commit.oid);
        if self.commit.parents.len() > 1 {
            let parents: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| format!("`{}`", p.get(..7).unwrap_or(p.as_str())))
                .collect();
            outln!(w, "**Merge parents:** {}", parents.join(", "));
        }
        outln!(
            w,
            "**Author:** {} <{}>",
            self.commit.author.name,
            self.commit.author.email
        );
        outln!(w, "**Date:** {}", self.commit.author.date);
        outln!(w);
        if !self.commit.subject.is_empty() {
            outln!(w, "## {}", self.commit.subject);
            outln!(w);
        }
        let body = message_body(&self.commit.message, &self.commit.subject);
        if !body.is_empty() {
            print_markdown_message(w, &body)?;
            outln!(w);
        }
        self.diff.render_markdown(w, ctx)?;
        Ok(())
    }
}

impl HumanRender for ShowOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        let color = use_color();

        if let Some(tag) = &self.tag {
            outln!(w, "{}", paint(color, "33;1", &format!("tag {}", tag.name)));
            if let Some(tagger) = &tag.tagger {
                outln!(w, "Tagger: {} <{}>", tagger.name, tagger.email);
                outln!(w, "Date:   {}", tagger.date);
            }
            outln!(w);
            print_message(w, &tag.message)?;
            outln!(w);
        } else if self.kind == ShowKind::Branch {
            if let Some(name) = &self.ref_name {
                outln!(w, "{}", paint(color, "36", &format!("branch {name}")));
            }
        } else if self.kind == ShowKind::Tag {
            if let Some(name) = &self.ref_name {
                outln!(w, "{}", paint(color, "36", &format!("tag {name}")));
            }
        }

        outln!(
            w,
            "{}",
            paint(color, "33", &format!("commit {}", self.commit.oid))
        );
        if self.commit.parents.len() > 1 {
            let abbrev: Vec<String> = self
                .commit
                .parents
                .iter()
                .map(|p| p.get(..7).unwrap_or(p).to_owned())
                .collect();
            outln!(w, "Merge:  {}", abbrev.join(" "));
        }
        outln!(
            w,
            "Author: {} <{}>",
            self.commit.author.name,
            self.commit.author.email
        );
        outln!(w, "Date:   {}", self.commit.author.date);
        outln!(w);
        print_message(w, &self.commit.message)?;

        if !self.diff.files.is_empty() {
            outln!(w);
            self.diff.render_human(w, ctx)?;
        }
        Ok(())
    }
}

/// Print a message indented four spaces, like `git show`.
fn print_message(w: &mut dyn Write, message: &str) -> Result<()> {
    for line in message.lines() {
        if line.is_empty() {
            outln!(w);
        } else {
            outln!(w, "    {line}");
        }
    }
    Ok(())
}

fn print_markdown_message(w: &mut dyn Write, message: &str) -> Result<()> {
    for line in message.lines() {
        outln!(w, "{line}");
    }
    Ok(())
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn message_body(full: &str, subject: &str) -> String {
    let mut lines = full.lines();
    let first = lines.next().unwrap_or("");
    if first == subject {
        lines.collect::<Vec<_>>().join("\n").trim().to_owned()
    } else {
        full.trim().to_owned()
    }
}

pub(crate) fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

pub(crate) fn render_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    outln!(w);

    let name_width = stat
        .files
        .iter()
        .map(|f| stat_path(f, path_opts).chars().count())
        .max()
        .unwrap_or(0)
        .min(60);
    let max_changes = stat
        .files
        .iter()
        .map(|f| f.insertions + f.deletions)
        .max()
        .unwrap_or(0);

    for file in &stat.files {
        let path = stat_path(file, path_opts);
        if file.binary {
            outln!(w, " {path:<name_width$} | Bin");
            continue;
        }
        let total = file.insertions + file.deletions;
        let bar = change_bar(file.insertions, file.deletions, max_changes, color);
        outln!(w, " {path:<name_width$} | {total:>3} {bar}");
    }

    outln!(w, " {}", summary_line(stat));
    Ok(())
}

fn stat_path(file: &FileStat, path_opts: Option<&PathDisplayOptions>) -> String {
    match &file.old_path {
        Some(old) => {
            let old_shown = path_opts
                .map(|o| o.format_path_for_human(old))
                .unwrap_or_else(|| old.clone());
            let new_shown = path_opts
                .map(|o| o.format_path_for_human(&file.path))
                .unwrap_or_else(|| file.path.clone());
            format!("{old_shown} => {new_shown}")
        }
        None => path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone()),
    }
}

fn change_bar(insertions: usize, deletions: usize, max_changes: usize, color: bool) -> String {
    let total = insertions + deletions;
    if total == 0 {
        return String::new();
    }
    let scaled = if max_changes > BAR_WIDTH {
        ((total * BAR_WIDTH).div_ceil(max_changes)).max(1)
    } else {
        total
    };
    let mut plus = ((insertions * scaled) as f64 / total as f64).round() as usize;
    let mut minus = scaled.saturating_sub(plus);
    if insertions > 0 && plus == 0 {
        plus = 1;
        minus = minus.saturating_sub(1);
    }
    if deletions > 0 && minus == 0 {
        minus = 1;
        plus = plus.saturating_sub(1);
    }
    let plus_bar = "+".repeat(plus);
    let minus_bar = "-".repeat(minus);
    if color {
        format!(
            "{}{}",
            paint(true, "32", &plus_bar),
            paint(true, "31", &minus_bar)
        )
    } else {
        format!("{plus_bar}{minus_bar}")
    }
}

fn summary_line(stat: &DiffStat) -> String {
    let mut parts = vec![format!(
        "{} file{} changed",
        stat.files_changed,
        plural(stat.files_changed)
    )];
    if stat.insertions > 0 {
        parts.push(format!(
            "{} insertion{}(+)",
            stat.insertions,
            plural(stat.insertions)
        ));
    }
    if stat.deletions > 0 {
        parts.push(format!(
            "{} deletion{}(-)",
            stat.deletions,
            plural(stat.deletions)
        ));
    }
    parts.join(", ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub(crate) fn render_markdown_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    outln!(w, "## Changes");
    outln!(w);
    outln!(w, "| File | + | − |");
    outln!(w, "| --- | ---: | ---: |");
    for file in &stat.files {
        let path = markdown::escape_table_cell(&stat_path(file, path_opts));
        let path_code = markdown::format_inline_code(&path);
        if file.binary {
            outln!(w, "| {path_code} | binary | |");
        } else {
            outln!(
                w,
                "| {path_code} | {} | {} |",
                file.insertions,
                file.deletions
            );
        }
    }
    outln!(w);
    outln!(w, "{}", summary_line(stat));
    Ok(())
}
