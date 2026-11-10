//! Outcome types and renderers for `grit clone`.

use std::io::Write;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{outln, HumanRender, MarkdownRender, OutputMode, RenderContext};
use crate::progress::summary::{join, plural, row_label, ObjectTypes, TransferSummary};
use crate::progress::{format_bytes, format_duration, paint, BOLD, DIM};
use crate::render::promisor::{
    human_lines, markdown_line, markdown_warning_lines, PromisorRemoteJson, PromisorWarningJson,
    StoredFieldJson,
};

/// Why the library's bundle seed was not used, shown as a warning in human output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneBundleFailure {
    /// The bundle URI the server offered, when it named one (credentials removed).
    pub uri: Option<String>,
    /// What went wrong.
    pub reason: String,
}

/// Why a clone left the work tree empty although files were wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneNoCheckout {
    /// The remote has no refs at all.
    EmptyRemote,
    /// The remote's default branch has no commits yet.
    UnbornDefaultBranch,
}

/// Timing for one step of the clone.
#[derive(Serialize, JsonSchema)]
pub struct ClonePhase {
    /// `bundle`, `receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`.
    pub phase: String,
    /// Unit of `count`: `bytes`, `objects` or `files`.
    pub unit: String,
    /// Units processed in this step.
    pub count: u64,
    /// Wall-clock seconds spent in this step.
    pub seconds: f64,
}

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

/// The server's prebuilt bundle a clone started from (`bundle-uri`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CloneBundleJson {
    /// Where the bundle was downloaded from.
    pub uri: String,
    /// Size of the bundle file in bytes.
    pub bytes: u64,
    /// Bytes an earlier, interrupted clone had already downloaded.
    pub resumed_from: u64,
}

/// Result of `grit clone`.
#[derive(Serialize, JsonSchema)]
pub struct CloneOutcome {
    /// Source URL or path, with any credentials removed.
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
    /// True when the server applied the `--blobs` filter on the initial fetch.
    #[serde(default, skip_serializing_if = "CloneOutcome::partial_not_set")]
    #[schemars(default)]
    pub partial: bool,
    /// True with `--blobs auto`: the filter comes from the promisor remotes the server
    /// advertised and the clone accepted (`blobs` is the filter it resolved to).
    #[serde(default, skip_serializing_if = "CloneOutcome::partial_not_set")]
    #[schemars(default)]
    pub filter_auto: bool,
    /// True when the server does not support filters: the `blobs` filter was not sent and
    /// every blob was downloaded (the clone is still configured as a partial clone).
    #[serde(default, skip_serializing_if = "CloneOutcome::partial_not_set")]
    #[schemars(default)]
    pub filter_ignored: bool,
    /// Promisor remotes the server advertised, and whether the clone accepted each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_remotes: Vec<PromisorRemoteJson>,
    /// Remote fields stored from the advertisement (`promisor.storeFields`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub stored_fields: Vec<StoredFieldJson>,
    /// Promisor config or advertised values that were ignored or could not be used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(default)]
    pub promisor_warnings: Vec<PromisorWarningJson>,
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
    /// Gitlink paths listed in `.gitmodules` (submodules grit does not fetch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub submodule_paths: Vec<String>,
    /// Gitlink paths with no `.gitmodules` entry (empty after checkout).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bare_gitlink_paths: Vec<String>,
    /// The server's prebuilt bundle the clone started from, when it offered
    /// one. `objects` and `bytes` include what it brought.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<CloneBundleJson>,
    /// Your space's project, `<you>/<project>`, when this is a clone of an
    /// organization's project on grithub (the default grit host): pushes from
    /// the clone go there (`remote.<remote>.pushurl`), and the `space` remote
    /// fetches from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Timing and transfer totals for the human summary.
    #[serde(skip)]
    #[schemars(skip)]
    pub transfer: TransferSummary,
    /// Set when the server's bundle could not be used and the clone fetched everything.
    #[serde(skip)]
    #[schemars(skip)]
    pub bundle_failure: Option<CloneBundleFailure>,
    /// Set when the clone ended without a checkout because the remote has no commits to show.
    #[serde(skip)]
    #[schemars(skip)]
    pub no_checkout_note: Option<CloneNoCheckout>,
}

impl CloneOutcome {
    fn partial_not_set(partial: &bool) -> bool {
        !*partial
    }

    /// Summary line shown instead of the partial-clone line when the server ignored
    /// the filter.
    const FILTER_IGNORED_SUMMARY: &'static str =
        "the server does not support filters; every blob was downloaded";

    fn partial_clone_summary(blobs: Option<CloneBlobsJson>) -> &'static str {
        match blobs {
            Some(CloneBlobsJson::None) => "blobless clone; blobs are fetched on demand",
            Some(CloneBlobsJson::Limit(_)) => "partial clone; large blobs are fetched on demand",
            None => "partial clone",
        }
    }
}

impl HumanRender for CloneOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> Result<()> {
        render_summary(self, w, ctx.color)
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
    } else if o.filter_ignored {
        outln!(
            w,
            "  {}  {}",
            paint(color, DIM, "filter"),
            CloneOutcome::FILTER_IGNORED_SUMMARY
        );
    }
    for line in human_lines(&o.promisor_remotes, &o.stored_fields, &o.promisor_warnings) {
        outln!(w, "  {}  {line}", paint(color, DIM, "promisor"));
    }
    outln!(w);

    o.transfer.render_rows(w, color)?;

    if let Some(bundle) = &o.bundle {
        let mut parts = vec![format!("{} prebuilt", format_bytes(bundle.bytes))];
        if bundle.resumed_from > 0 {
            parts.push(format!("resumed at {}", format_bytes(bundle.resumed_from)));
        }
        outln!(w, "  {}  {}", label("bundle"), join(&parts, color));
    }

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
    if let Some(space) = &o.space {
        outln!(
            w,
            "  {}  pushes go to your space, {space}; `grit sync` keeps the two in step",
            label("space")
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
        } else if self.filter_ignored {
            outln!(w, "- **filter**: {}", CloneOutcome::FILTER_IGNORED_SUMMARY);
        }
        if let Some(line) = markdown_line(&self.promisor_remotes) {
            outln!(w, "{line}");
        }
        for s in &self.stored_fields {
            outln!(w, "- **stored**: `{}` for `{}`", s.field, s.remote);
        }
        for line in markdown_warning_lines(&self.promisor_warnings) {
            outln!(w, "{line}");
        }
        outln!(w, "- **url**: `{}`", self.url);
        if let Some(bundle) = &self.bundle {
            outln!(
                w,
                "- **bundle**: `{}` ({} bytes, resumed at {})",
                bundle.uri,
                bundle.bytes,
                bundle.resumed_from
            );
        }
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
        if !self.submodule_paths.is_empty() {
            outln!(w, "- **submodule paths** (not fetched):");
            for path in &self.submodule_paths {
                outln!(w, "  - `{path}`");
            }
        }
        if !self.bare_gitlink_paths.is_empty() {
            outln!(
                w,
                "- **bare gitlink paths** (empty checkout, not in `.gitmodules`):"
            );
            for path in &self.bare_gitlink_paths {
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

impl CloneOutcome {
    /// Write the notes and warnings gathered while cloning to `w` (stderr in the CLI).
    ///
    /// Human and Markdown modes each get their own wording; JSON mode stays quiet except
    /// for the warning that the server ignored `--blobs`, which is shown in every mode.
    ///
    /// # Errors
    ///
    /// Returns an error when writing to `w` fails.
    pub fn render_notes(&self, w: &mut dyn Write, mode: OutputMode) -> Result<()> {
        if let Some(failure) = &self.bundle_failure {
            render_bundle_failure(w, mode, failure)?;
        }
        render_path_collision_warning(w, mode, &self.path_collisions)?;
        render_submodule_note(w, mode, &self.submodule_paths)?;
        render_bare_gitlink_note(w, mode, &self.bare_gitlink_paths)?;
        if let Some(note) = self.no_checkout_note {
            self.render_no_checkout_note(w, mode, note)?;
        }
        if self.filter_ignored {
            outln!(
                w,
                "warning: the server did not advertise filter support; fetched all objects without applying --blobs"
            );
        }
        Ok(())
    }

    fn render_no_checkout_note(
        &self,
        w: &mut dyn Write,
        mode: OutputMode,
        note: CloneNoCheckout,
    ) -> Result<()> {
        let tips = self.branches_with_commits.as_deref().unwrap_or(&[]);
        match (note, mode) {
            (_, OutputMode::Json) => {}
            (CloneNoCheckout::EmptyRemote, OutputMode::Human) => {
                outln!(w, "note: the remote repository is empty.");
            }
            (CloneNoCheckout::EmptyRemote, OutputMode::Markdown) => {
                outln!(w, "> The remote repository is empty.");
            }
            (CloneNoCheckout::UnbornDefaultBranch, OutputMode::Human) => {
                let branch = self.branch.as_deref().unwrap_or("HEAD");
                outln!(
                    w,
                    "warning: the remote default branch '{branch}' has no commits yet; no files were checked out."
                );
                if let Some(first) = tips.first() {
                    outln!(w, "note: remote branches with commits: {}", tips.join(", "));
                    outln!(w, "hint: run `grit switch {first}` to check one out");
                }
            }
            (CloneNoCheckout::UnbornDefaultBranch, OutputMode::Markdown) => {
                let branch = self.branch.as_deref().unwrap_or("HEAD");
                outln!(
                    w,
                    "> **Warning:** the remote default branch `{branch}` has no commits yet; no files were checked out."
                );
                if let Some(first) = tips.first() {
                    let names = tips
                        .iter()
                        .map(|b| format!("`{b}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    outln!(w, "> Remote branches with commits: {names}.");
                    outln!(w, "> Try `grit switch {first}`.");
                }
            }
        }
        Ok(())
    }
}

fn render_bundle_failure(
    w: &mut dyn Write,
    mode: OutputMode,
    failure: &CloneBundleFailure,
) -> Result<()> {
    if mode != OutputMode::Human {
        return Ok(());
    }
    let from = failure
        .uri
        .as_deref()
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    outln!(
        w,
        "warning: could not use the server's bundle{from} ({}); fetched everything instead",
        failure.reason
    );
    Ok(())
}

/// Warn that tracked paths collide on this filesystem (human mode only).
///
/// # Errors
///
/// Returns an error when writing to `w` fails.
pub fn render_path_collision_warning(
    w: &mut dyn Write,
    mode: OutputMode,
    paths: &[String],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    match mode {
        OutputMode::Human => {
            outln!(
                w,
                "warning: these tracked paths collide on this filesystem (case or Unicode normalization); only one path from each group exists on disk:"
            );
            for path in paths {
                outln!(w, "  '{path}'");
            }
        }
        OutputMode::Markdown => {
            outln!(
                w,
                "> **Warning:** these tracked paths collide on this filesystem (case or Unicode normalization); only one path from each group exists on disk:"
            );
            for path in paths {
                outln!(w, "> - `{path}`");
            }
        }
        OutputMode::Json => {}
    }
    Ok(())
}

/// Path-collision warning plus a way out when staging or commit is blocked (status, add, commit).
///
/// # Errors
///
/// Returns an error when writing to `w` fails.
pub fn render_worktree_case_collision_guidance(
    w: &mut dyn Write,
    mode: OutputMode,
    paths: &[String],
) -> Result<()> {
    render_path_collision_warning(w, mode, paths)?;
    if paths.is_empty() {
        return Ok(());
    }
    match mode {
        OutputMode::Human => outln!(
            w,
            "hint: use a case-sensitive volume, or run `grit reset --hard` to match the index to HEAD"
        ),
        OutputMode::Markdown => outln!(
            w,
            "> **Hint:** use a case-sensitive volume, or run `grit reset --hard` to match the index to HEAD."
        ),
        OutputMode::Json => {}
    }
    Ok(())
}

fn render_submodule_note(w: &mut dyn Write, mode: OutputMode, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let count = paths.len();
    let list = paths.join(", ");
    let noun = if count == 1 {
        "submodule"
    } else {
        "submodules"
    };
    match mode {
        OutputMode::Human => outln!(
            w,
            "note: this repository has {count} {noun} ({list}); grit does not fetch submodules"
        ),
        OutputMode::Markdown => outln!(
            w,
            "> **Note:** this repository has {count} {noun} (`{list}`); grit does not fetch submodules."
        ),
        OutputMode::Json => {}
    }
    Ok(())
}

fn render_bare_gitlink_note(w: &mut dyn Write, mode: OutputMode, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let count = paths.len();
    let list = paths.join(", ");
    let noun = if count == 1 { "gitlink" } else { "gitlinks" };
    match mode {
        OutputMode::Human => outln!(
            w,
            "note: checkout left {count} empty {noun} ({list}) with no `.gitmodules` entry; nested repository files were not copied"
        ),
        OutputMode::Markdown => outln!(
            w,
            "> **Note:** checkout left {count} empty {noun} (`{list}`) with no `.gitmodules` entry; nested repository files were not copied."
        ),
        OutputMode::Json => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::PhaseRecord;
    use grit_lib::progress::{ProgressPhase, ProgressUnit};
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
            space: None,
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
            filter_auto: false,
            filter_ignored: false,
            promisor_remotes: Vec::new(),
            stored_fields: Vec::new(),
            promisor_warnings: Vec::new(),
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
                milestones: Vec::new(),
                elapsed: Duration::from_secs(2),
                records,
                objects: 1_234,
                bytes: 2 * 1024 * 1024,
                object_types,
                transfer_sent: false,
            },
            no_checkout_reason: None,
            branches_with_commits: None,
            path_collisions: Vec::new(),
            submodule_paths: Vec::new(),
            bare_gitlink_paths: Vec::new(),
            bundle: None,
            bare: false,
            bundle_failure: None,
            no_checkout_note: None,
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
            milestones: Vec::new(),
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
