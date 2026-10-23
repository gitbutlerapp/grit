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

use std::io::Write;

use crate::revision_errors::map_resolve_error;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::diff::DiffMerges;
use grit_lib::object_inspect::{
    format_tree_cat_file_p, tree_entry_cat_file_type, tree_entry_mode_cat_file_p,
};
use grit_lib::objects::{parse_tag, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{CommitDiffOptions, DiffOutcome, DiffTotalsJson, FileDiff};
use crate::commits_json::CommitJsonLoader;
use crate::context;
use crate::diff_format::DiffFormatArgs;
pub use crate::diff_render::DiffStat;
use crate::diff_render::{self, diffstat_from_files};
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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
        /// Raw blob bytes for human stdout (same as `git show rev:path`).
        #[serde(skip)]
        #[schemars(skip)]
        raw: Vec<u8>,
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
    pub commit: CommitInfo,
    /// Per-file diff rows (same shape as `grit diff --json`).
    pub files: Vec<FileDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub totals: Option<DiffTotalsJson>,
    /// Summary diffstat (always present for scripting convenience).
    pub stat: DiffStat,
    #[serde(skip)]
    #[schemars(skip)]
    diff: DiffOutcome,
}

#[derive(Serialize, JsonSchema)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: Person,
    pub committer: Person,
    pub subject: String,
    pub message: String,
    pub refs: Vec<crate::output::CommitRefJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub relative_date: Option<String>,
    /// Stable change id (reverse-hex; stored header or derived from commit id).
    pub change_id: String,
    /// `true` when `change_id` came from a `change-id` commit header.
    pub change_id_stored: bool,
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
    let commit_oid = repo
        .odb()
        .peel_to_commit(resolved)?
        .with_context(|| format!("'{}' does not resolve to a commit", resolved.to_hex()))?;

    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let commit_json = CommitJsonLoader::for_listed_commits().load_one(&repo, &commit_oid, now)?;
    let commit = CommitInfo {
        oid: commit_json.oid,
        parents: commit_json.parents,
        author: commit_json.author,
        committer: commit_json.committer,
        subject: commit_json.subject,
        message: commit_json.message,
        refs: commit_json.refs,
        relative_date: commit_json.relative_date,
        change_id: commit_json.change_id,
        change_id_stored: commit_json.change_id_stored,
    };

    let diff_opts = CommitDiffOptions {
        paths: opts.paths.clone(),
        format: opts.format,
        word_level_inline: opts.word_level_inline,
        diff_merges: opts.diff_merges,
    };
    let diff = crate::commands::diff::diff_for_commit(&repo, &commit_oid, diff_opts)?;
    let stat = match opts.format.detail() {
        grit_lib::diff::DiffDetail::NameOnly | grit_lib::diff::DiffDetail::NameStatus => {
            let stat_diff = crate::commands::diff::diff_for_commit(
                &repo,
                &commit_oid,
                CommitDiffOptions {
                    paths: opts.paths.clone(),
                    format: {
                        let mut f = opts.format;
                        f.stat = true;
                        f.name_only = false;
                        f.name_status = false;
                        f
                    },
                    word_level_inline: false,
                    diff_merges: opts.diff_merges,
                },
            )?;
            diffstat_from_files(&stat_diff.files)
        }
        _ => diffstat_from_files(&diff.files),
    };

    Ok(ShowOutcome {
        warnings: sink.warnings(),
        kind,
        ref_name,
        tag,
        commit,
        files: diff.files.clone(),
        totals: diff.totals,
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
                raw: obj.data,
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

impl HumanRender for ShowPathOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            ShowPathOutcome::Blob {
                binary,
                text,
                data_base64,
                spec,
                ..
            } => {
                if let Some(t) = text {
                    w.write_all(t.as_bytes())
                        .map_err(|e| anyhow::anyhow!("writing blob: {e}"))?;
                    return Ok(());
                }
                if *binary {
                    let bytes = if let Some(b64) = data_base64 {
                        STANDARD
                            .decode(b64)
                            .map_err(|e| anyhow::anyhow!("decoding blob base64: {e}"))?
                    } else {
                        let (repo, _) = context::discover_with_warnings()?;
                        let resolved = repo.resolve(spec)?;
                        repo.read_object_raw_for_display(&resolved.oid)?.data
                    };
                    w.write_all(&bytes)
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

/// Back-compat wrapper for stash and log.
pub(crate) fn diffstat(diff: &crate::commands::diff::DiffOutcome) -> DiffStat {
    diffstat_from_files(&diff.files)
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
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
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
        crate::diff_render::render_markdown_stat(w, &self.stat, None)?;
        self.diff.render_markdown(w, _ctx)?;
        Ok(())
    }
}

impl HumanRender for ShowOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = diff_render::use_color();

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

        let fmt = self.diff.format_flags();
        if fmt.human_stat_block() {
            diff_render::render_stat_with_leading_blank(
                w,
                &self.stat,
                color,
                self.diff.path_display_opts(),
            )?;
        } else {
            if !self.diff.files.is_empty() && fmt.detail() == grit_lib::diff::DiffDetail::Patch {
                outln!(w);
            }
            self.diff.render_human_diff(w, None)?;
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
