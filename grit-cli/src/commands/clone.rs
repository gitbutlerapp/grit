//! `grit clone` — copy a remote repository into a new directory.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use grit_lib::remote::clone::{
    cleanup_failed_clone, clone as clone_repo, clone_effective_single_branch, derive_clone_dir,
    dest_preexisted_empty, CloneCheckout, CloneError, CloneHead, CloneOptions,
    CloneOutcome as LibCloneOutcome, CloneTags,
};
use grit_lib::remote::partial_clone::ObjectFilterSpec;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, progress, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::summary::{
    join, plural, row_label, stdout_color, ObjectTypes, TransferSummary,
};
use crate::progress::{format_bytes, format_duration, paint, TerminalProgress, BOLD, DIM};
use grit_lib::progress::{NeverCancel, ProgressPhase, ProgressUnit};

/// Timing for one step of the clone.
#[derive(Serialize, JsonSchema)]
pub struct ClonePhase {
    /// `receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`.
    pub phase: String,
    /// Unit of `count`: `bytes`, `objects` or `files`.
    pub unit: String,
    /// Units processed in this step.
    pub count: u64,
    /// Wall-clock seconds spent in this step.
    pub seconds: f64,
}

/// Usage mistakes specific to `grit clone` (exit code 2).
#[derive(Debug)]
pub struct CloneUsageError;

impl std::fmt::Display for CloneUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid clone arguments")
    }
}

impl std::error::Error for CloneUsageError {}

/// JSON value for `--blobs`: the string `"none"` or a byte limit.
#[derive(Clone, Copy, Debug)]
pub enum CloneBlobsJson {
    None,
    Limit(u64),
}

impl JsonSchema for CloneBlobsJson {
    fn schema_name() -> String {
        "CloneBlobsJson".to_string()
    }

    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        use schemars::schema::{Metadata, SchemaObject, SubschemaValidation};
        SchemaObject {
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "Partial-clone blob filter: `\"none\"` or a byte limit.".to_owned(),
                ),
                ..Default::default()
            })),
            subschemas: Some(Box::new(SubschemaValidation {
                one_of: Some(vec![
                    SchemaObject {
                        instance_type: Some(schemars::schema::InstanceType::String.into()),
                        const_value: Some(serde_json::Value::String("none".to_owned())),
                        ..Default::default()
                    }
                    .into(),
                    SchemaObject {
                        instance_type: Some(schemars::schema::InstanceType::Integer.into()),
                        format: Some("uint64".to_owned()),
                        number: Some(Box::new(schemars::schema::NumberValidation {
                            minimum: Some(0.0),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }
                    .into(),
                ]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

impl CloneBlobsJson {
    fn from_spec(spec: &ObjectFilterSpec) -> Self {
        match spec {
            ObjectFilterSpec::BlobNone => Self::None,
            ObjectFilterSpec::BlobLimit(n) => Self::Limit(*n),
        }
    }
}

impl Serialize for CloneBlobsJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::None => serializer.serialize_str("none"),
            Self::Limit(n) => serializer.serialize_u64(*n),
        }
    }
}

/// Options for [`run`].
pub struct CloneRunOptions {
    pub url: String,
    pub dir: Option<String>,
    pub depth: Option<u32>,
    pub branch: Option<String>,
    pub shallow: bool,
    pub single_branch: Option<bool>,
    pub no_tags: bool,
    pub origin: Option<String>,
    pub bare: bool,
    pub no_checkout: bool,
    pub blobs: Option<String>,
    pub quiet: crate::quiet_args::QuietArgs,
    pub mode: OutputMode,
}

/// Result of `grit clone`.
#[derive(Serialize, JsonSchema)]
pub struct CloneOutcome {
    pub url: String,
    /// Destination directory.
    pub path: String,
    /// Remote name configured for the source (`origin` by default).
    pub remote: String,
    /// Whether only one branch was fetched (`git clone --single-branch` semantics).
    pub single_branch: bool,
    /// Tag fetch mode used for the initial fetch: `all`, `following`, or `none`.
    pub tag_fetch: String,
    /// Branch checked out, or `null` when detached at a tag.
    pub branch: Option<String>,
    /// True when `HEAD` is detached (tag checkout).
    pub detached: bool,
    /// Tag name when `-b` named a tag; omitted on branch checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Shallow depth when `--depth` or `--shallow` was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// Partial-clone blob filter when `--blobs` was used (`"none"` or a byte limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<CloneBlobsJson>,
    /// True when `--blobs` configured a promisor partial clone.
    #[serde(default, skip_serializing_if = "CloneOutcome::partial_not_set")]
    #[schemars(default)]
    pub partial: bool,
    /// Objects received during the fetch.
    pub objects: u64,
    /// Pack bytes received during the fetch.
    pub bytes: u64,
    /// Remote branches fetched.
    pub branches: u64,
    /// Remote tags fetched.
    pub tags: u64,
    /// Working-tree files checked out.
    pub files: u64,
    /// Objects received by type, or `null` when the pack was copied from a
    /// local repository without being re-indexed.
    pub object_types: Option<ObjectTypes>,
    /// Total size of the checked-out files, in bytes.
    pub worktree_bytes: u64,
    /// Wall-clock seconds for the whole clone.
    pub seconds: f64,
    /// Time spent in each step, in order.
    pub phases: Vec<ClonePhase>,
    /// Why no working-tree checkout happened, when `files` is zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_checkout_reason: Option<String>,
    /// Whether the clone destination is a bare repository.
    pub bare: bool,
    /// Remote branch names that have commits when `no_checkout_reason` is `unborn_default_branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches_with_commits: Option<Vec<String>>,
    /// Index paths that collided on a case-insensitive filesystem during checkout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_collisions: Vec<String>,
    /// Timing and transfer totals for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
}

impl CloneOutcome {
    fn partial_not_set(partial: &bool) -> bool {
        !*partial
    }

    fn partial_clone_summary(blobs: Option<CloneBlobsJson>) -> &'static str {
        match blobs {
            Some(CloneBlobsJson::None) => "blobless clone; blobs are fetched on demand",
            Some(CloneBlobsJson::Limit(_)) => "partial clone; large blobs are fetched on demand",
            None => "partial clone",
        }
    }
}

impl HumanRender for CloneOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let color = stdout_color();
        render_summary(self, w, color)
    }
}

/// The end-of-clone overview: where it went, how long each step took, and
/// what was transferred.
fn render_summary(o: &CloneOutcome, w: &mut dyn Write, color: bool) -> Result<()> {
    let label = |text: &str| row_label(color, text);
    let total = format_duration(o.transfer.elapsed);
    if color {
        // Separate the summary from the progress lines above it on a terminal.
        outln!(w);
    }
    outln!(
        w,
        "{} Cloned into {} on {} in {}",
        paint(color, "32", "✓"),
        paint(color, BOLD, &o.path),
        paint(color, BOLD, &checkout_label(o)),
        paint(color, BOLD, &total),
    );
    if o.partial {
        outln!(
            w,
            "  {}  {}",
            paint(color, DIM, "partial"),
            CloneOutcome::partial_clone_summary(o.blobs)
        );
    }
    outln!(w);

    o.transfer.render_rows(w, color)?;

    outln!(
        w,
        "  {}  {}",
        label("refs"),
        join(
            &[
                plural(o.branches, "branch", "branches"),
                plural(o.tags, "tag", "tags"),
            ],
            color
        )
    );

    if o.files > 0 {
        outln!(
            w,
            "  {}  {}",
            label("files"),
            join(
                &[
                    plural(o.files, "file", "files"),
                    format!("{} in the work tree", format_bytes(o.worktree_bytes)),
                ],
                color
            )
        );
    }
    Ok(())
}

fn checkout_label(o: &CloneOutcome) -> String {
    if o.detached {
        format!("detached at {}", o.tag.as_deref().unwrap_or("tag"))
    } else {
        o.branch.clone().unwrap_or_else(|| "HEAD".to_owned())
    }
}

fn phase_json_name(phase: ProgressPhase) -> &'static str {
    match phase {
        ProgressPhase::ReceivingObjects => "receiving",
        ProgressPhase::IndexingObjects => "indexing",
        ProgressPhase::ResolvingDeltas => "resolving_deltas",
        ProgressPhase::CopyingObjects => "copying",
        ProgressPhase::CheckingOutFiles => "checkout",
        _ => "other",
    }
}

fn unit_json_name(unit: ProgressUnit) -> &'static str {
    match unit {
        ProgressUnit::Bytes => "bytes",
        ProgressUnit::Objects => "objects",
        ProgressUnit::Files => "files",
    }
}

impl MarkdownRender for CloneOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let size = format_bytes(self.bytes);
        let secs = self.transfer.elapsed.as_secs_f64();
        outln!(w, "## Clone complete");
        outln!(w);
        outln!(w, "- **path**: `{}`", self.path);
        if self.detached {
            outln!(
                w,
                "- **checkout**: detached at `{}`",
                self.tag.as_deref().unwrap_or("tag")
            );
        } else {
            outln!(
                w,
                "- **branch**: `{}`",
                self.branch.as_deref().unwrap_or("HEAD")
            );
        }
        if let Some(depth) = self.depth {
            outln!(w, "- **depth**: {depth}");
        }
        if self.partial {
            outln!(
                w,
                "- **partial**: {}",
                CloneOutcome::partial_clone_summary(self.blobs)
            );
            if let Some(blobs) = self.blobs {
                match blobs {
                    CloneBlobsJson::None => outln!(w, "- **blobs**: none"),
                    CloneBlobsJson::Limit(n) => outln!(w, "- **blobs**: {n} bytes"),
                }
            }
        }
        outln!(w, "- **url**: `{}`", self.url);
        outln!(w, "- **objects**: {}", self.objects);
        if let Some(t) = self.object_types {
            outln!(
                w,
                "- **object types**: {} commits, {} trees, {} blobs, {} tags",
                t.commits,
                t.trees,
                t.blobs,
                t.tags
            );
        }
        outln!(w, "- **bytes**: {} ({size})", self.bytes);
        outln!(w, "- **branches**: {}", self.branches);
        outln!(w, "- **tags**: {}", self.tags);
        outln!(w, "- **files**: {}", self.files);
        if !self.path_collisions.is_empty() {
            outln!(w, "- **path collisions**:");
            for path in &self.path_collisions {
                outln!(w, "  - `{path}`");
            }
        }
        outln!(
            w,
            "- **worktree bytes**: {} ({})",
            self.worktree_bytes,
            format_bytes(self.worktree_bytes)
        );
        outln!(w, "- **elapsed**: {secs:.2}s");
        if !self.phases.is_empty() {
            outln!(w);
            outln!(w, "| Phase | Count | Unit | Seconds |");
            outln!(w, "| --- | ---: | --- | ---: |");
            for p in &self.phases {
                outln!(
                    w,
                    "| {} | {} | {} | {:.2} |",
                    p.phase,
                    p.count,
                    p.unit,
                    p.seconds
                );
            }
        }
        Ok(())
    }
}

fn emit_path_collision_warning(mode: OutputMode, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    match mode {
        OutputMode::Human => {
            eprintln!(
                "warning: these tracked paths collide on this filesystem (case-insensitive paths share one file); only one path from each group exists on disk:"
            );
            for path in paths {
                eprintln!("  '{path}'");
            }
        }
        OutputMode::Json | OutputMode::Markdown => {}
    }
}

fn emit_no_checkout_note(mode: OutputMode, outcome: &LibCloneOutcome) {
    match outcome.checkout {
        CloneCheckout::CheckedOut(_) => {}
        CloneCheckout::EmptyRemote => {
            if mode == OutputMode::Human {
                eprintln!("note: the remote repository is empty.");
            } else if mode == OutputMode::Markdown {
                eprintln!("> The remote repository is empty.");
            }
        }
        CloneCheckout::Skipped => {}
        CloneCheckout::UnbornDefaultBranch => {
            let branch = outcome.head.branch_name().unwrap_or("HEAD");
            if mode == OutputMode::Human {
                eprintln!(
                    "warning: the remote default branch '{branch}' has no commits yet; no files were checked out."
                );
                if !outcome.branches_with_commits.is_empty() {
                    let names = outcome.branches_with_commits.join(", ");
                    eprintln!("note: remote branches with commits: {names}");
                    if let Some(first) = outcome.branches_with_commits.first() {
                        eprintln!("hint: run `grit switch {first}` to check one out");
                    }
                }
            } else if mode == OutputMode::Markdown {
                eprintln!(
                    "> **Warning:** the remote default branch `{branch}` has no commits yet; no files were checked out."
                );
                if !outcome.branches_with_commits.is_empty() {
                    let names = outcome
                        .branches_with_commits
                        .iter()
                        .map(|b| format!("`{b}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    eprintln!("> Remote branches with commits: {names}.");
                    if let Some(first) = outcome.branches_with_commits.first() {
                        eprintln!("> Try `grit switch {first}`.");
                    }
                }
            }
        }
    }
}

fn tag_fetch_mode_label(mode: CloneTags) -> &'static str {
    match mode {
        CloneTags::All => "all",
        CloneTags::Following => "following",
        CloneTags::None => "none",
    }
}

fn resolved_clone_tags(opts: &CloneRunOptions) -> Option<CloneTags> {
    if opts.no_tags {
        Some(CloneTags::None)
    } else {
        None
    }
}

fn effective_tag_fetch_mode(opts: &CloneRunOptions) -> CloneTags {
    resolved_clone_tags(opts).unwrap_or(if opts.shallow || opts.depth.is_some() {
        CloneTags::Following
    } else {
        CloneTags::All
    })
}

fn parse_blobs_filter(raw: Option<&str>) -> Result<Option<ObjectFilterSpec>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    ObjectFilterSpec::from_blobs_cli(raw)
        .map(Some)
        .map_err(|e| {
            anyhow::Error::from(CloneUsageError).context(format!("invalid --blobs value: {e}"))
        })
}

pub fn run(mut opts: CloneRunOptions) -> Result<CloneOutcome> {
    let url = &opts.url;
    let mode = opts.mode;
    let filter = parse_blobs_filter(opts.blobs.as_deref())?;
    opts.depth = opts.depth.or_else(|| opts.shallow.then_some(1));
    if matches!(opts.depth, Some(0)) {
        return Err(anyhow::anyhow!("depth 0 is not a positive number"));
    }
    let dir = match &opts.dir {
        Some(d) => d.clone(),
        None => derive_clone_dir(url, opts.bare).map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let path = PathBuf::from(&dir);
    let dest_preexisted_empty = dest_preexisted_empty(&path);
    let started = Instant::now();

    // A clone always ends with a checkout, so the overall bar can size it.
    let mut term_progress = opts.quiet.progress(mode).with_overall(true);
    if term_progress.is_drawing() {
        let color = term_progress.uses_color();
        eprintln!(
            "{} {} {} {}",
            paint(color, BOLD, "Cloning"),
            paint(color, DIM, url),
            paint(color, DIM, "→"),
            paint(color, BOLD, &dir)
        );
    } else if !opts.quiet.quiet {
        progress(mode, &format!("Cloning into '{dir}' ..."));
    }
    match clone_into(
        &opts,
        filter.as_ref(),
        &path,
        &dir,
        started,
        &mut term_progress,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let skip_cleanup = matches!(
                err.downcast_ref::<CloneError>(),
                Some(e) if !e.should_cleanup_failed_clone()
            );
            if !skip_cleanup {
                cleanup_failed_clone(&path, dest_preexisted_empty);
            }
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn clone_into(
    run: &CloneRunOptions,
    filter: Option<&ObjectFilterSpec>,
    path: &Path,
    dir: &str,
    started: Instant,
    term_progress: &mut TerminalProgress,
) -> Result<CloneOutcome> {
    let factory = crate::context::http_client_factory();
    let remote_name = run
        .origin
        .clone()
        .unwrap_or_else(|| grit_lib::remote::DEFAULT_REMOTE.to_owned());
    let tag_mode = effective_tag_fetch_mode(run);
    let clone_opts = CloneOptions {
        url: run.url.clone(),
        dest: path.to_path_buf(),
        environment: crate::context::environment(),
        remote_name: remote_name.clone(),
        initial_branch: "main".to_owned(),
        thread_policy: crate::context::thread_policy_for_repo(),
        depth: run.depth,
        branch: run.branch.clone(),
        filter: filter.copied(),
        single_branch: run.single_branch,
        tags: resolved_clone_tags(run),
        promisor_fetcher: Some(std::sync::Arc::new(
            grit_lib::objects::promisor::DefaultPromisorFetcher::new(std::sync::Arc::new(
                factory.clone(),
            )),
        )),
        bare: run.bare,
        checkout: !run.no_checkout,
    };
    let single_branch = clone_effective_single_branch(&clone_opts);
    let outcome = clone_repo(
        clone_opts,
        &mut *term_progress,
        &NeverCancel,
        Some(&factory),
    )?;

    let files = outcome.files;
    let worktree_bytes = outcome.worktree_bytes;
    let path_collisions = outcome.path_collisions.clone();
    emit_path_collision_warning(run.mode, &path_collisions);
    let (no_checkout_reason, branches_with_commits) = match outcome.checkout {
        CloneCheckout::CheckedOut(_) => (None, None),
        CloneCheckout::Skipped => {
            let reason = if run.bare {
                Some("bare".to_owned())
            } else {
                Some("no_checkout".to_owned())
            };
            (reason, None)
        }
        CloneCheckout::EmptyRemote | CloneCheckout::UnbornDefaultBranch => {
            emit_no_checkout_note(run.mode, &outcome);
            let reason = match outcome.checkout {
                CloneCheckout::EmptyRemote => Some("empty_remote".to_owned()),
                CloneCheckout::UnbornDefaultBranch => Some("unborn_default_branch".to_owned()),
                CloneCheckout::CheckedOut(_) | CloneCheckout::Skipped => None,
            };
            let tips = (!outcome.branches_with_commits.is_empty())
                .then(|| outcome.branches_with_commits.clone());
            (reason, tips)
        }
    };
    let elapsed = started.elapsed();
    let transfer = TransferSummary::new(outcome.transfer, term_progress.phases().to_vec(), elapsed);

    let (branch, detached, tag) = match &outcome.head {
        CloneHead::Branch(name) => (Some(name.clone()), false, None),
        CloneHead::Detached { tag, .. } => (None, true, Some(tag.clone())),
    };

    Ok(CloneOutcome {
        url: run.url.clone(),
        path: dir.to_owned(),
        remote: remote_name,
        single_branch,
        tag_fetch: tag_fetch_mode_label(tag_mode).to_owned(),
        branch,
        detached,
        tag,
        depth: run.depth,
        blobs: filter.map(CloneBlobsJson::from_spec),
        partial: filter.is_some(),
        objects: outcome.transfer.objects,
        bytes: outcome.transfer.bytes,
        branches: outcome.branches,
        tags: outcome.tags,
        files,
        object_types: transfer.object_types,
        worktree_bytes,
        seconds: elapsed.as_secs_f64(),
        phases: transfer
            .records
            .iter()
            .map(|r| ClonePhase {
                phase: phase_json_name(r.phase).to_owned(),
                unit: unit_json_name(r.unit).to_owned(),
                count: r.count,
                seconds: r.elapsed.as_secs_f64(),
            })
            .collect(),
        transfer,
        no_checkout_reason,
        branches_with_commits,
        path_collisions,
        bare: outcome.bare,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::PhaseRecord;
    use std::time::Duration;

    fn rec(
        phase: ProgressPhase,
        unit: ProgressUnit,
        start_ms: u64,
        ms: u64,
        count: u64,
    ) -> PhaseRecord {
        PhaseRecord {
            phase,
            unit,
            count,
            started_at: Duration::from_millis(start_ms),
            elapsed: Duration::from_millis(ms),
            samples: vec![1.0, 2.0],
            peak_rate: 4.0 * 1024.0 * 1024.0,
        }
    }

    fn outcome(records: Vec<PhaseRecord>) -> CloneOutcome {
        let object_types = Some(ObjectTypes {
            commits: 100,
            trees: 400,
            blobs: 734,
            tags: 0,
        });
        CloneOutcome {
            url: "https://example.com/r.git".to_owned(),
            path: "r".to_owned(),
            remote: "origin".to_owned(),
            single_branch: false,
            tag_fetch: "all".to_owned(),
            branch: Some("main".to_owned()),
            detached: false,
            tag: None,
            depth: None,
            blobs: None,
            partial: false,
            objects: 1_234,
            bytes: 2 * 1024 * 1024,
            branches: 1,
            tags: 3,
            files: 42,
            object_types,
            worktree_bytes: 512 * 1024,
            seconds: 2.0,
            phases: Vec::new(),
            transfer: TransferSummary {
                elapsed: Duration::from_secs(2),
                records,
                objects: 1_234,
                bytes: 2 * 1024 * 1024,
                object_types,
            },
            no_checkout_reason: None,
            branches_with_commits: None,
            path_collisions: Vec::new(),
            bare: false,
        }
    }

    #[test]
    fn plain_summary_reports_time_size_objects_refs_and_files() {
        let o = outcome(vec![rec(
            ProgressPhase::ReceivingObjects,
            ProgressUnit::Bytes,
            0,
            1000,
            2 * 1024 * 1024,
        )]);
        let mut out = Vec::new();
        render_summary(&o, &mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.starts_with("✓ Cloned into r on main in 2.0s\n"),
            "{text}"
        );
        assert!(
            text.contains("2.00 MiB received · 2.00 MiB/s avg · 4.00 MiB/s peak"),
            "{text}"
        );
        assert!(
            text.contains("1,234 · 100 commits · 400 trees · 734 blobs"),
            "{text}"
        );
        assert!(
            !text.contains("0 tags ·"),
            "zero type counts are omitted: {text}"
        );
        assert!(text.contains("1 branch · 3 tags"), "{text}");
        assert!(
            text.contains("42 files · 512 KiB in the work tree"),
            "{text}"
        );
        assert!(!text.contains('\x1b'), "no escapes without color: {text}");
    }

    #[test]
    fn summary_skips_empty_sections_for_empty_remote() {
        let mut o = outcome(Vec::new());
        o.objects = 0;
        o.bytes = 0;
        o.files = 0;
        o.object_types = None;
        o.transfer = TransferSummary {
            elapsed: Duration::from_secs(2),
            ..TransferSummary::default()
        };
        let mut out = Vec::new();
        render_summary(&o, &mut out, false).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("data"), "{text}");
        assert!(!text.contains("objects"), "{text}");
        assert!(!text.contains("files"), "{text}");
        assert!(text.contains("refs"), "{text}");
    }
}
