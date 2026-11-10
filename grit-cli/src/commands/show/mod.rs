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

use crate::revision_errors::map_resolve_error;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use grit_lib::diff::DiffMerges;
use grit_lib::objects::ObjectKind;
use grit_lib::revwalk::object_inspect::{
    format_tree_entries_cat_file_p, tree_entry_cat_file_type, tree_entry_mode_cat_file_p,
    RevisionLabel,
};
use grit_lib::revwalk::{IndexDwim, ResolveOptions};
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{
    resolve_commit_diff_pathspecs, CommitDiffOptions, DiffOutcome, DiffTotalsJson, FileDiff,
    ShowLogPatchTextParams,
};
use crate::commits_json::CommitJsonLoader;
use crate::context;
use crate::diff_format::DiffFormatArgs;
use crate::diff_render::diffstat_from_files;
pub use crate::diff_render::DiffStat;

/// Inputs for `grit show`.
pub struct ShowRunOptions {
    pub objects: Vec<String>,
    pub pathspecs: crate::pathspec_args::TrailingPathspecArgs,
    pub format: DiffFormatArgs,
    pub word_level_inline: bool,
    pub diff_merges: DiffMerges,
    /// Collect ref decorations for `commit.refs` (only JSON output shows them).
    pub include_refs: bool,
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
        /// Preformatted `git cat-file -p` lines for human stdout (built when the tree is parsed).
        #[serde(skip)]
        #[schemars(skip)]
        cat_file_p: Vec<u8>,
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
    pub(crate) diff: DiffOutcome,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) patch_text: Option<ShowLogPatchTextParams>,
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
    let (kind, ref_name, tag) = match repo.revision_label(spec)? {
        Some(RevisionLabel::Branch { name }) => (ShowKind::Branch, Some(name), None),
        Some(RevisionLabel::Tag {
            name,
            annotation: Some(tag),
        }) => {
            let info = TagInfo {
                name: name.clone(),
                tagger: tag.tagger.as_deref().map(Person::from_identity_line),
                message: tag.message.trim_end().to_owned(),
            };
            (ShowKind::AnnotatedTag, Some(name), Some(info))
        }
        Some(RevisionLabel::Tag {
            name,
            annotation: None,
        }) => (ShowKind::Tag, Some(name), None),
        // No label, or a label kind this CLI does not render.
        _ => (ShowKind::Commit, None, None),
    };

    let resolved = repo
        .revwalk()
        .resolve(
            spec,
            &ResolveOptions::default().with_index_dwim(IndexDwim::Off),
        )
        .with_context(|| format!("could not resolve '{spec}'"))?;
    let commit_oid = repo
        .odb()
        .peel_to_commit(resolved)?
        .with_context(|| format!("'{}' does not resolve to a commit", resolved.to_hex()))?;

    let now = context::wall_clock_now(repo.environment()).unix_timestamp();
    let loader = if opts.include_refs {
        CommitJsonLoader::for_listed_commits()
    } else {
        CommitJsonLoader::without_refs()
    };
    let commit_json = loader.load_one(&repo, &commit_oid, now)?;
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

    let pathspec =
        resolve_commit_diff_pathspecs(&repo, &opts.pathspecs.pathspec_source_tokens(&[])?)?;
    let path_tokens = pathspec.specs().to_vec();
    let patch_text = if opts.format.log_patch_text() {
        Some(ShowLogPatchTextParams {
            oid: commit_oid,
            format: opts.format,
            pathspec: pathspec.clone(),
            diff_merges: opts.diff_merges,
        })
    } else {
        None
    };
    let diff = if opts.format.log_patch_text() {
        crate::commands::diff::streamed_log_patch_text_outcome(opts.format)
    } else {
        let diff_opts = CommitDiffOptions {
            paths: path_tokens.clone(),
            format: opts.format,
            word_level_inline: opts.word_level_inline,
            diff_merges: opts.diff_merges,
        };
        crate::commands::diff::diff_for_commit(&repo, &commit_oid, diff_opts)?
    };
    let stat = match opts.format.detail() {
        grit_lib::diff::DiffDetail::NameOnly | grit_lib::diff::DiffDetail::NameStatus => {
            let stat_diff = crate::commands::diff::diff_for_commit(
                &repo,
                &commit_oid,
                CommitDiffOptions {
                    paths: path_tokens.clone(),
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
        patch_text,
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
            let cat_file_p = format_tree_entries_cat_file_p(&parsed);
            let entries = parsed
                .iter()
                .map(|e| ShowTreeEntryJson {
                    mode: tree_entry_mode_cat_file_p(e.mode),
                    object_type: tree_entry_cat_file_type(e.mode).to_owned(),
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
                cat_file_p,
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

/// Back-compat wrapper for stash and log.
pub(crate) fn diffstat(diff: &crate::commands::diff::DiffOutcome) -> DiffStat {
    diffstat_from_files(&diff.files)
}

pub(crate) fn message_body(full: &str, subject: &str) -> String {
    let mut lines = full.lines();
    let first = lines.next().unwrap_or("");
    if first == subject {
        lines.collect::<Vec<_>>().join("\n").trim().to_owned()
    } else {
        full.trim().to_owned()
    }
}

mod dispatch;
pub use dispatch::dispatch;
