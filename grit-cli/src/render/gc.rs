//! Render `grit gc` outcomes.

use grit_lib::progress::{CancelToken, NullProgress};

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use grit_lib::config::git_path::user_display_path;
use grit_lib::maintenance::{
    AutoGcReason, DropPromisedBlobsReport, DropPromisedBlobsSkip, GcPhaseReport, GcStats,
    OffloadPack, OffloadPackRole, OffloadRepo, OffloadReport,
};
use schemars::JsonSchema;
use serde::Serialize;
use time::OffsetDateTime;

use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::progress::format_bytes;

/// Result of `grit gc` and its task subcommands.
#[derive(Serialize, JsonSchema)]
pub struct GcOutcome {
    #[serde(skip)]
    pub quiet: bool,
    pub dry_run: bool,
    /// Decision of `grit gc --auto`; absent for a plain `grit gc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<GcAutoReport>,
    pub tasks: Vec<GcTaskReport>,
}

/// What `grit gc --auto` decided (JSON uses a `status` discriminator).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum GcAutoReport {
    /// Below `gc.auto` and `gc.autoPackLimit` (or `gc.auto=0`); nothing ran.
    NotNeeded,
    /// Another maintenance run holds `gc.pid`; nothing ran.
    AlreadyRunning {
        pid: Option<u64>,
        host: Option<String>,
    },
    /// The `pre-auto-gc` hook exited non-zero; nothing ran.
    DeclinedByHook { exit_code: i32 },
    /// Maintenance ran (or would run, with `--dry-run`).
    Ran {
        #[serde(flatten)]
        trigger: GcAutoTrigger,
    },
}

/// Threshold that triggered `grit gc --auto`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "trigger", rename_all = "kebab-case")]
pub enum GcAutoTrigger {
    /// Estimated loose objects above `gc.auto`.
    LooseObjects { estimate: usize, limit: usize },
    /// Packs without `.keep` above `gc.autoPackLimit`.
    Packs { count: usize, limit: usize },
}

impl From<AutoGcReason> for GcAutoTrigger {
    fn from(reason: AutoGcReason) -> Self {
        match reason {
            AutoGcReason::TooManyLoose { estimate, limit } => {
                Self::LooseObjects { estimate, limit }
            }
            AutoGcReason::TooManyPacks { count, limit } => Self::Packs { count, limit },
        }
    }
}

impl GcAutoReport {
    fn render_human_line(&self, w: &mut dyn Write, dry_run: bool) -> Result<()> {
        match self {
            Self::NotNeeded => outln!(
                w,
                "auto: nothing to do (below gc.auto and gc.autoPackLimit)."
            ),
            Self::AlreadyRunning { pid, host } => {
                let holder = match (pid, host) {
                    (Some(pid), Some(host)) => format!(" (process {pid} on {host})"),
                    (Some(pid), None) => format!(" (process {pid})"),
                    _ => String::new(),
                };
                outln!(w, "auto: skipped, maintenance already running{holder}.");
            }
            Self::DeclinedByHook { exit_code } => outln!(
                w,
                "auto: skipped, pre-auto-gc hook exited with status {exit_code}."
            ),
            Self::Ran { trigger } => {
                let verb = if dry_run { "would run" } else { "ran" };
                match trigger {
                    GcAutoTrigger::LooseObjects { estimate, limit } => outln!(
                        w,
                        "auto: {verb} for about {estimate} loose objects (gc.auto = {limit})."
                    ),
                    GcAutoTrigger::Packs { count, limit } => outln!(
                        w,
                        "auto: {verb} for {count} packs (gc.autoPackLimit = {limit})."
                    ),
                }
            }
        }
        Ok(())
    }

    fn render_markdown(&self, w: &mut dyn Write) -> Result<()> {
        let status = match self {
            Self::NotNeeded => "not-needed",
            Self::AlreadyRunning { .. } => "already-running",
            Self::DeclinedByHook { .. } => "declined-by-hook",
            Self::Ran { .. } => "ran",
        };
        outln!(w, "- **auto:** {status}");
        match self {
            Self::NotNeeded => {}
            Self::AlreadyRunning { pid, host } => {
                if let Some(pid) = pid {
                    outln!(w, "- **pid:** {pid}");
                }
                if let Some(host) = host {
                    outln!(w, "- **host:** {host}");
                }
            }
            Self::DeclinedByHook { exit_code } => outln!(w, "- **exit_code:** {exit_code}"),
            Self::Ran {
                trigger: GcAutoTrigger::LooseObjects { estimate, limit },
            } => {
                outln!(w, "- **trigger:** loose-objects");
                outln!(w, "- **estimate:** {estimate}");
                outln!(w, "- **limit:** {limit}");
            }
            Self::Ran {
                trigger: GcAutoTrigger::Packs { count, limit },
            } => {
                outln!(w, "- **trigger:** packs");
                outln!(w, "- **count:** {count}");
                outln!(w, "- **limit:** {limit}");
            }
        }
        Ok(())
    }
}

/// One maintenance task report (JSON uses a `task` discriminator).
#[derive(Serialize, JsonSchema)]
#[serde(tag = "task", rename_all = "kebab-case")]
pub enum GcTaskReport {
    PackRefs {
        duration_ms: u64,
        refs_packed: usize,
        refs_pruned: usize,
        path: String,
    },
    ReflogExpire {
        duration_ms: u64,
        entries_pruned: usize,
    },
    Repack {
        duration_ms: u64,
        #[serde(flatten)]
        report: RepackSummary,
    },
    Prune {
        duration_ms: u64,
        pruned: usize,
        kept: usize,
    },
    CommitGraph {
        duration_ms: u64,
        commit_count: usize,
        path: String,
        removed_chain: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<String>,
    },
    Midx {
        duration_ms: u64,
        pack_count: usize,
        path: String,
        bitmap: String,
    },
    Offload {
        duration_ms: u64,
        #[serde(flatten)]
        report: OffloadSummary,
    },
    DropLargeBlobs {
        duration_ms: u64,
        #[serde(flatten)]
        report: DropLargeBlobsSummary,
    },
}

/// Full-gc library phase reports as CLI task DTOs.
pub(crate) fn task_reports_from_gc_stats(stats: GcStats) -> Vec<GcTaskReport> {
    stats.phase_reports().into_iter().map(Into::into).collect()
}

impl From<GcPhaseReport> for GcTaskReport {
    fn from(phase: GcPhaseReport) -> Self {
        match phase {
            GcPhaseReport::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path,
            } => Self::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path: user_display_path(&path),
            },
            GcPhaseReport::ReflogExpire {
                duration_ms,
                entries_pruned,
            } => Self::ReflogExpire {
                duration_ms,
                entries_pruned,
            },
            GcPhaseReport::Repack { duration_ms, stats } => Self::Repack {
                duration_ms,
                report: stats.into(),
            },
            GcPhaseReport::Prune {
                duration_ms,
                pruned,
                kept,
            } => Self::Prune {
                duration_ms,
                pruned,
                kept,
            },
            GcPhaseReport::DropLargeBlobs {
                duration_ms,
                report,
            } => Self::DropLargeBlobs {
                duration_ms,
                report: report.into(),
            },
            GcPhaseReport::CommitGraph {
                duration_ms,
                commit_count,
                path,
                removed_chain,
                skipped,
            } => Self::CommitGraph {
                duration_ms,
                commit_count,
                path: user_display_path(&path),
                removed_chain,
                skipped: skipped.map(|reason| reason.as_str().to_owned()),
            },
            #[allow(unreachable_patterns)]
            _ => unreachable!("full gc phase reports are exhaustive for the current library"),
        }
    }
}

/// `grit gc drop-large-blobs` results (see `content/docs/commands/gc.md`).
#[derive(Serialize, JsonSchema)]
pub struct DropLargeBlobsSummary {
    /// Blobs at least this many bytes were candidates.
    pub min_size: u64,
    /// Blobs needed at or after this time (Unix seconds) stayed; absent: no recency window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_newer_than: Option<i64>,
    /// Blobs stored in promisor packs at or above `min_size`.
    pub candidates: usize,
    /// Blobs dropped (on a dry run, that would be).
    pub dropped: usize,
    /// Bytes freed (on a dry run, an estimate).
    pub bytes_reclaimed: u64,
    pub packs_rewritten: usize,
    pub packs_removed: usize,
    /// Candidates kept, by reason.
    pub kept: DropLargeBlobsKeptJson,
    /// The dropped blobs, sorted by id.
    pub blobs: Vec<DroppedBlobJson>,
    /// Why `grit gc` skipped the phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<DropSkipReason>,
}

/// Why `grit gc` skipped the drop-large-blobs phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DropSkipReason {
    /// The task is not enabled, or `expire` is `never`.
    Disabled,
    /// The repository has no promisor remote.
    NoPromisorRemote,
    /// A merge, rebase, cherry-pick, revert, am or bisect is in progress.
    OperationInProgress,
    /// `extensions.preciousObjects` is set.
    PreciousObjects,
    /// gc was given no current time to measure recency against.
    NoCurrentTime,
    /// A reason this version of grit does not know.
    Other,
}

impl From<DropPromisedBlobsSkip> for DropSkipReason {
    fn from(skip: DropPromisedBlobsSkip) -> Self {
        match skip {
            DropPromisedBlobsSkip::Disabled => Self::Disabled,
            DropPromisedBlobsSkip::NoPromisorRemote => Self::NoPromisorRemote,
            DropPromisedBlobsSkip::OperationInProgress(_) => Self::OperationInProgress,
            DropPromisedBlobsSkip::PreciousObjects => Self::PreciousObjects,
            DropPromisedBlobsSkip::NoCurrentTime => Self::NoCurrentTime,
            _ => Self::Other,
        }
    }
}

impl DropSkipReason {
    /// The JSON value, for Markdown output.
    fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoPromisorRemote => "no-promisor-remote",
            Self::OperationInProgress => "operation-in-progress",
            Self::PreciousObjects => "precious-objects",
            Self::NoCurrentTime => "no-current-time",
            Self::Other => "other",
        }
    }

    /// Human wording.
    fn describe(self) -> &'static str {
        match self {
            Self::Disabled => "the task is not enabled",
            Self::NoPromisorRemote => "the repository has no promisor remote",
            Self::OperationInProgress => "an operation is in progress",
            Self::PreciousObjects => "extensions.preciousObjects is set",
            Self::NoCurrentTime => "no current time was given",
            Self::Other => "for a reason this grit does not know",
        }
    }
}

/// Why candidate blobs stayed (each counted once, under the first reason that applies).
#[derive(Serialize, JsonSchema)]
pub struct DropLargeBlobsKeptJson {
    pub kept_pack: usize,
    pub outside_promisor_packs: usize,
    /// No promisor stores it, and every promisor remote is on this machine.
    pub not_in_promisor: usize,
    /// No promisor on this machine stores it, and offload has run here, so the
    /// network promisors cannot be trusted to have it.
    pub unverified: usize,
    pub checked_out: usize,
    pub recently_checked_out: usize,
    pub recently_fetched: usize,
    pub not_promised_elsewhere: usize,
}

/// One dropped blob.
#[derive(Serialize, JsonSchema)]
pub struct DroppedBlobJson {
    pub oid: String,
    pub size: u64,
}

impl From<DropPromisedBlobsReport> for DropLargeBlobsSummary {
    fn from(r: DropPromisedBlobsReport) -> Self {
        Self {
            min_size: r.min_size,
            keep_newer_than: r.keep_newer_than.map(OffsetDateTime::unix_timestamp),
            candidates: r.candidates,
            dropped: r.dropped.len(),
            bytes_reclaimed: r.bytes_reclaimed,
            packs_rewritten: r.packs_rewritten,
            packs_removed: r.packs_removed,
            kept: DropLargeBlobsKeptJson {
                kept_pack: r.kept.kept_pack,
                outside_promisor_packs: r.kept.outside_promisor_packs,
                checked_out: r.kept.checked_out,
                recently_checked_out: r.kept.recently_checked_out,
                recently_fetched: r.kept.recently_fetched,
                not_promised_elsewhere: r.kept.not_promised_elsewhere,
                not_in_promisor: r.kept.not_in_promisor,
                unverified: r.kept.unverified,
            },
            blobs: r
                .dropped
                .iter()
                .map(|b| DroppedBlobJson {
                    oid: b.oid.to_hex(),
                    size: b.size,
                })
                .collect(),
            skipped: r.skipped.map(DropSkipReason::from),
        }
    }
}

impl DropLargeBlobsSummary {
    /// `1 checked out, 2 recently fetched`, listing only reasons that kept something.
    fn kept_reasons(&self) -> String {
        let k = &self.kept;
        [
            (k.checked_out, "checked out"),
            (k.recently_checked_out, "recently checked out"),
            (k.recently_fetched, "recently fetched"),
            (
                k.outside_promisor_packs,
                "also stored outside promisor packs",
            ),
            (k.not_promised_elsewhere, "not promised by a promisor pack"),
            (k.not_in_promisor, "not in the promisor"),
            (k.unverified, "not confirmed in the promisor"),
            (k.kept_pack, "in kept packs"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, why)| format!("{n} {why}"))
        .collect::<Vec<_>>()
        .join(", ")
    }

    fn render_human(&self, w: &mut dyn Write, duration_ms: u64, dry_run: bool) -> Result<()> {
        if let Some(reason) = self.skipped {
            let why = reason.describe();
            outln!(w, "drop-large-blobs: skipped, {why} ({duration_ms} ms).");
            return Ok(());
        }
        let kept = self.kept_reasons();
        let kept = if kept.is_empty() {
            String::new()
        } else {
            format!(" (kept: {kept})")
        };
        let threshold = format_bytes(self.min_size);
        if self.dropped == 0 {
            outln!(
                w,
                "drop-large-blobs: nothing to drop, {} {} >= {threshold}{kept} ({duration_ms} ms).",
                self.candidates,
                if self.candidates == 1 {
                    "candidate"
                } else {
                    "candidates"
                }
            );
            return Ok(());
        }
        let noun = if self.dropped == 1 { "blob" } else { "blobs" };
        if dry_run {
            outln!(
                w,
                "drop-large-blobs: would drop {} promised {noun} >= {threshold}, about {}{kept} ({duration_ms} ms).",
                self.dropped,
                format_bytes(self.bytes_reclaimed)
            );
            for blob in &self.blobs {
                outln!(w, "  {} {}", blob.oid, blob.size);
            }
        } else {
            outln!(
                w,
                "drop-large-blobs: dropped {} promised {noun} >= {threshold}, reclaimed {}{kept} ({duration_ms} ms).",
                self.dropped,
                format_bytes(self.bytes_reclaimed)
            );
        }
        Ok(())
    }

    fn render_markdown_fields(&self, w: &mut dyn Write) -> Result<()> {
        if let Some(reason) = self.skipped {
            outln!(w, "- **skipped:** {}", reason.label());
        }
        outln!(w, "- **min_size:** {}", self.min_size);
        if let Some(cutoff) = self.keep_newer_than {
            outln!(w, "- **keep_newer_than:** {cutoff}");
        }
        outln!(w, "- **candidates:** {}", self.candidates);
        outln!(w, "- **dropped:** {}", self.dropped);
        outln!(w, "- **bytes_reclaimed:** {}", self.bytes_reclaimed);
        outln!(w, "- **packs_rewritten:** {}", self.packs_rewritten);
        outln!(w, "- **packs_removed:** {}", self.packs_removed);
        let k = &self.kept;
        outln!(w, "- **kept_checked_out:** {}", k.checked_out);
        outln!(
            w,
            "- **kept_recently_checked_out:** {}",
            k.recently_checked_out
        );
        outln!(w, "- **kept_recently_fetched:** {}", k.recently_fetched);
        outln!(
            w,
            "- **kept_outside_promisor_packs:** {}",
            k.outside_promisor_packs
        );
        outln!(
            w,
            "- **kept_not_promised_elsewhere:** {}",
            k.not_promised_elsewhere
        );
        outln!(w, "- **kept_not_in_promisor:** {}", k.not_in_promisor);
        outln!(w, "- **kept_unverified:** {}", k.unverified);
        outln!(w, "- **kept_pack:** {}", k.kept_pack);
        if !self.blobs.is_empty() {
            outln!(w, "\n| oid | size |\n| --- | ---: |");
            for blob in &self.blobs {
                outln!(w, "| `{}` | {} |", blob.oid, blob.size);
            }
        }
        Ok(())
    }
}

/// `grit gc offload` results (see `content/docs/commands/gc.md`).
#[derive(Serialize, JsonSchema)]
pub struct OffloadSummary {
    /// Promisor remote the blobs moved to.
    pub remote: String,
    /// The promisor's git directory.
    pub lop_git_dir: String,
    /// Filter that chose the blobs (`blob:limit=<bytes>` or `blob:none`).
    pub filter: String,
    pub candidates: usize,
    pub candidate_bytes: u64,
    pub copied_to_lop: usize,
    pub already_in_lop: usize,
    pub dropped_from_main: usize,
    pub skipped_checked_out: usize,
    pub skipped_kept_pack: usize,
    pub lop_pack: Option<String>,
    pub main_pack: Option<String>,
    /// Every pack the run wrote, in order.
    pub packs: Vec<OffloadPackJson>,
    pub main_bytes_before: u64,
    pub main_bytes_after: u64,
    pub config_changes: Vec<OffloadConfigChangeJson>,
    pub lop_ready: LopReadyJson,
    /// `promisor.advertise` is on, so clients are told about the promisor.
    pub advertised: bool,
}

/// One pack `grit gc offload` wrote.
#[derive(Serialize, JsonSchema)]
pub struct OffloadPackJson {
    /// Which pack this is.
    pub role: OffloadPackRoleJson,
    /// The `.pack` file.
    pub path: String,
    /// Objects in the pack.
    pub objects: usize,
    /// Size of the `.pack` file in bytes.
    pub bytes: u64,
}

/// Which pack `grit gc offload` wrote.
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OffloadPackRoleJson {
    /// The promisor's new pack of the blobs that moved.
    Lop,
    /// The main repository's `.promisor` pack of every object it keeps.
    MainPromisor,
    /// A pack this version of grit does not know.
    Other,
}

impl From<OffloadPackRole> for OffloadPackRoleJson {
    fn from(role: OffloadPackRole) -> Self {
        match role {
            OffloadPackRole::Lop => Self::Lop,
            OffloadPackRole::MainPromisor => Self::MainPromisor,
            _ => Self::Other,
        }
    }
}

impl OffloadPackRoleJson {
    /// The JSON spelling, for Markdown output.
    const fn slug(self) -> &'static str {
        match self {
            Self::Lop => "lop",
            Self::MainPromisor => "main-promisor",
            Self::Other => "other",
        }
    }
}

impl From<&OffloadPack> for OffloadPackJson {
    fn from(p: &OffloadPack) -> Self {
        Self {
            role: p.role.into(),
            path: user_display_path(&p.path),
            objects: p.objects,
            bytes: p.bytes,
        }
    }
}

/// One config value `grit gc offload` set.
#[derive(Serialize, JsonSchema)]
pub struct OffloadConfigChangeJson {
    /// `main` or `lop`.
    pub repo: String,
    pub key: String,
    pub value: String,
}

/// Whether the promisor can answer clients' lazy fetches.
#[derive(Serialize, JsonSchema)]
pub struct LopReadyJson {
    pub allow_filter: bool,
    pub allow_any_sha1_in_want: bool,
}

impl From<OffloadReport> for OffloadSummary {
    fn from(r: OffloadReport) -> Self {
        let packs = r.packs().map(OffloadPackJson::from).collect();
        Self {
            remote: r.remote,
            lop_git_dir: user_display_path(&r.lop_git_dir),
            filter: r.filter.to_wire_spec(),
            candidates: r.candidates,
            candidate_bytes: r.candidate_bytes,
            copied_to_lop: r.copied_to_lop,
            already_in_lop: r.already_in_lop,
            dropped_from_main: r.dropped_from_main,
            skipped_checked_out: r.skipped_checked_out,
            skipped_kept_pack: r.skipped_kept_pack,
            lop_pack: r.lop_pack.as_ref().map(|p| user_display_path(&p.path)),
            main_pack: r.main_pack.as_ref().map(|p| user_display_path(&p.path)),
            packs,
            main_bytes_before: r.main_bytes_before,
            main_bytes_after: r.main_bytes_after,
            config_changes: r
                .config_changes
                .into_iter()
                .map(|c| OffloadConfigChangeJson {
                    repo: match c.repo {
                        OffloadRepo::Lop => "lop",
                        _ => "main",
                    }
                    .to_owned(),
                    key: c.key,
                    value: c.value,
                })
                .collect(),
            lop_ready: LopReadyJson {
                allow_filter: r.lop_ready.allow_filter,
                allow_any_sha1_in_want: r.lop_ready.allow_any_sha1_in_want,
            },
            advertised: r.advertised,
        }
    }
}

/// `1 blob`, `2 blobs`.
fn blobs(n: usize) -> String {
    if n == 1 {
        "1 blob".to_owned()
    } else {
        format!("{n} blobs")
    }
}

impl OffloadSummary {
    fn render_human(&self, w: &mut dyn Write, duration_ms: u64, dry_run: bool) -> Result<()> {
        let size = format_bytes(self.candidate_bytes);
        if dry_run {
            outln!(
                w,
                "offload: would move {} ({size}) to {}; {} already there ({duration_ms} ms).",
                blobs(self.candidates),
                self.remote,
                self.already_in_lop
            );
        } else if self.candidates == 0 {
            outln!(
                w,
                "offload: nothing to move to {} ({}, {duration_ms} ms).",
                self.remote,
                self.filter
            );
        } else {
            outln!(
                w,
                "offload: moved {} ({size}) to {}; main packs {} -> {} ({duration_ms} ms).",
                blobs(self.dropped_from_main),
                self.remote,
                format_bytes(self.main_bytes_before),
                format_bytes(self.main_bytes_after)
            );
        }
        for pack in &self.packs {
            let name = Path::new(&pack.path)
                .file_name()
                .map_or_else(|| pack.path.clone(), |n| n.to_string_lossy().into_owned());
            let label = match pack.role {
                OffloadPackRoleJson::Lop => format!("{} pack", self.remote),
                OffloadPackRoleJson::MainPromisor => "promisor pack".to_owned(),
                OffloadPackRoleJson::Other => "pack".to_owned(),
            };
            outln!(
                w,
                "  wrote {label} {name} ({} {}, {})",
                pack.objects,
                if pack.objects == 1 {
                    "object"
                } else {
                    "objects"
                },
                format_bytes(pack.bytes)
            );
        }
        let skipped = self.skipped_checked_out + self.skipped_kept_pack;
        if skipped > 0 {
            outln!(
                w,
                "  kept {} here ({} checked out, {} in kept packs).",
                blobs(skipped),
                self.skipped_checked_out,
                self.skipped_kept_pack
            );
        }
        for change in &self.config_changes {
            outln!(
                w,
                "  set {} = {} ({})",
                change.key,
                change.value,
                change.repo
            );
        }
        if !self.lop_ready.allow_filter || !self.lop_ready.allow_any_sha1_in_want {
            outln!(
                w,
                "  hint: {} cannot serve clients' lazy fetches yet; rerun with --configure-lop or set uploadpack.allowFilter and uploadpack.allowAnySHA1InWant there.",
                self.remote
            );
        }
        if !self.advertised {
            outln!(
                w,
                "  hint: clients are not told about {}; set promisor.advertise = true (and promisor.sendFields = partialCloneFilter).",
                self.remote
            );
        }
        Ok(())
    }

    fn render_markdown_fields(&self, w: &mut dyn Write) -> Result<()> {
        outln!(w, "- **remote:** {}", self.remote);
        outln!(w, "- **lop_git_dir:** `{}`", self.lop_git_dir);
        outln!(w, "- **filter:** `{}`", self.filter);
        outln!(w, "- **candidates:** {}", self.candidates);
        outln!(w, "- **candidate_bytes:** {}", self.candidate_bytes);
        outln!(w, "- **copied_to_lop:** {}", self.copied_to_lop);
        outln!(w, "- **already_in_lop:** {}", self.already_in_lop);
        outln!(w, "- **dropped_from_main:** {}", self.dropped_from_main);
        outln!(w, "- **skipped_checked_out:** {}", self.skipped_checked_out);
        outln!(w, "- **skipped_kept_pack:** {}", self.skipped_kept_pack);
        if let Some(path) = &self.lop_pack {
            outln!(w, "- **lop_pack:** `{path}`");
        }
        if let Some(path) = &self.main_pack {
            outln!(w, "- **main_pack:** `{path}`");
        }
        for pack in &self.packs {
            outln!(
                w,
                "- **pack:** `{}` ({}, {} objects, {} bytes)",
                pack.path,
                pack.role.slug(),
                pack.objects,
                pack.bytes
            );
        }
        outln!(w, "- **main_bytes_before:** {}", self.main_bytes_before);
        outln!(w, "- **main_bytes_after:** {}", self.main_bytes_after);
        for change in &self.config_changes {
            outln!(
                w,
                "- **config_change:** `{} = {}` ({})",
                change.key,
                change.value,
                change.repo
            );
        }
        outln!(w, "- **lop_allow_filter:** {}", self.lop_ready.allow_filter);
        outln!(
            w,
            "- **lop_allow_any_sha1_in_want:** {}",
            self.lop_ready.allow_any_sha1_in_want
        );
        outln!(w, "- **advertised:** {}", self.advertised);
        Ok(())
    }
}

#[derive(Serialize, JsonSchema)]
pub struct RepackSummary {
    pub loose_before: usize,
    pub packs_before: usize,
    pub objects_packed: Option<usize>,
    pub pack_bytes: Option<u64>,
    pub pack_path: Option<String>,
    pub removed_pack_files: usize,
    pub pruned_loose: usize,
    pub unreachable_loose_pruned: usize,
}

impl From<grit_lib::maintenance::RepackStats> for RepackSummary {
    fn from(stats: grit_lib::maintenance::RepackStats) -> Self {
        Self {
            loose_before: stats.loose_before,
            packs_before: stats.packs_before,
            objects_packed: stats.objects_packed,
            pack_bytes: stats.pack_bytes,
            pack_path: stats.pack_path.as_deref().map(user_display_path),
            removed_pack_files: stats.removed_pack_files,
            pruned_loose: stats.pruned_loose,
            unreachable_loose_pruned: stats.unreachable_loose_pruned,
        }
    }
}

impl RepackSummary {
    fn render_markdown_fields(&self, w: &mut dyn Write) -> Result<()> {
        outln!(w, "- **loose_before:** {}", self.loose_before);
        outln!(w, "- **packs_before:** {}", self.packs_before);
        if let Some(n) = self.objects_packed {
            outln!(w, "- **objects_packed:** {n}");
        }
        if let Some(bytes) = self.pack_bytes {
            outln!(w, "- **pack_bytes:** {bytes}");
        }
        if let Some(path) = &self.pack_path {
            outln!(w, "- **pack_path:** `{path}`");
        }
        outln!(w, "- **removed_pack_files:** {}", self.removed_pack_files);
        outln!(w, "- **pruned_loose:** {}", self.pruned_loose);
        outln!(
            w,
            "- **unreachable_loose_pruned:** {}",
            self.unreachable_loose_pruned
        );
        Ok(())
    }
}

impl HumanRender for GcOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.quiet {
            return Ok(());
        }
        if self.dry_run {
            outln!(w, "Dry run: no changes written.");
        }
        if let Some(auto) = &self.auto {
            auto.render_human_line(w, self.dry_run)?;
        }
        for task in &self.tasks {
            task.render_human_line(w, self.dry_run)?;
        }
        Ok(())
    }
}

impl MarkdownRender for GcOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Garbage collection\n");
        outln!(w, "- **dry_run:** {}", self.dry_run);
        if let Some(auto) = &self.auto {
            auto.render_markdown(w)?;
        }
        outln!(w, "\n### Tasks\n");
        for task in &self.tasks {
            task.render_markdown(w)?;
        }
        Ok(())
    }
}

impl GcTaskReport {
    fn render_human_line(&self, w: &mut dyn Write, dry_run: bool) -> Result<()> {
        match self {
            Self::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "pack-refs: would pack {refs_packed} refs ({refs_pruned} loose removed) → {path} ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "pack-refs: packed {refs_packed} refs ({refs_pruned} loose removed) → {path} ({duration_ms} ms)."
                    );
                }
            }
            Self::ReflogExpire {
                duration_ms,
                entries_pruned,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "reflog-expire: would remove {entries_pruned} entries ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "reflog-expire: removed {entries_pruned} entries ({duration_ms} ms)."
                    );
                }
            }
            Self::Repack {
                duration_ms,
                report,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "repack: would pack {} objects ({} loose, {} pack files now) ({duration_ms} ms).",
                        report.objects_packed.unwrap_or(0),
                        report.loose_before,
                        report.packs_before
                    );
                } else if let Some(path) = &report.pack_path {
                    outln!(
                        w,
                        "repack: packed {} objects into {path} ({} ms).",
                        report.objects_packed.unwrap_or(0),
                        duration_ms
                    );
                } else {
                    outln!(w, "repack: nothing to pack ({duration_ms} ms).");
                }
            }
            Self::Prune {
                duration_ms,
                pruned,
                kept,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "prune: would remove {pruned} unreachable loose objects ({kept} kept, {duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "prune: removed {pruned} unreachable loose objects ({kept} kept, {duration_ms} ms)."
                    );
                }
            }
            Self::CommitGraph {
                duration_ms,
                skipped: Some(reason),
                ..
            } => {
                if dry_run {
                    outln!(
                        w,
                        "commit-graph: would skip, parents are rewritten ({reason}); any existing graph would be removed ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "commit-graph: skipped, parents are rewritten ({reason}); any existing graph removed ({duration_ms} ms)."
                    );
                }
            }
            Self::CommitGraph {
                duration_ms,
                commit_count: 0,
                skipped: None,
                ..
            } => {
                if dry_run {
                    outln!(
                        w,
                        "commit-graph: would skip; no reachable commits ({duration_ms} ms)."
                    );
                } else {
                    outln!(w, "commit-graph: no reachable commits ({duration_ms} ms).");
                }
            }
            Self::CommitGraph {
                duration_ms,
                commit_count,
                path,
                ..
            } => {
                if dry_run {
                    outln!(
                        w,
                        "commit-graph: would write {commit_count} commits to {path} ({duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "commit-graph: wrote {commit_count} commits to {path} ({duration_ms} ms)."
                    );
                }
            }
            Self::Midx {
                duration_ms,
                pack_count,
                path,
                bitmap,
            } => {
                if dry_run {
                    outln!(
                        w,
                        "midx: would index {pack_count} packs at {path} (bitmap: {bitmap}, {duration_ms} ms)."
                    );
                } else {
                    outln!(
                        w,
                        "midx: indexed {pack_count} packs at {path} (bitmap: {bitmap}, {duration_ms} ms)."
                    );
                }
            }
            Self::Offload {
                duration_ms,
                report,
            } => report.render_human(w, *duration_ms, dry_run)?,
            Self::DropLargeBlobs {
                duration_ms,
                report,
            } => report.render_human(w, *duration_ms, dry_run)?,
        }
        Ok(())
    }

    fn render_markdown(&self, w: &mut dyn Write) -> Result<()> {
        match self {
            Self::PackRefs {
                duration_ms,
                refs_packed,
                refs_pruned,
                path,
            } => {
                outln!(w, "#### pack-refs\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **refs_packed:** {refs_packed}");
                outln!(w, "- **refs_pruned:** {refs_pruned}");
                outln!(w, "- **path:** `{path}`");
                outln!(w);
            }
            Self::ReflogExpire {
                duration_ms,
                entries_pruned,
            } => {
                outln!(w, "#### reflog-expire\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **entries_pruned:** {entries_pruned}");
                outln!(w);
            }
            Self::Repack {
                duration_ms,
                report,
            } => {
                outln!(w, "#### repack\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                report.render_markdown_fields(w)?;
                outln!(w);
            }
            Self::Prune {
                duration_ms,
                pruned,
                kept,
            } => {
                outln!(w, "#### prune\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **pruned:** {pruned}");
                outln!(w, "- **kept:** {kept}");
                outln!(w);
            }
            Self::CommitGraph {
                duration_ms,
                commit_count,
                path,
                removed_chain,
                skipped,
            } => {
                outln!(w, "#### commit-graph\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **commit_count:** {commit_count}");
                outln!(w, "- **path:** `{path}`");
                outln!(w, "- **removed_chain:** {removed_chain}");
                if let Some(reason) = skipped {
                    outln!(w, "- **skipped:** {reason}");
                }
                outln!(w);
            }
            Self::Midx {
                duration_ms,
                pack_count,
                path,
                bitmap,
            } => {
                outln!(w, "#### midx\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                outln!(w, "- **pack_count:** {pack_count}");
                outln!(w, "- **path:** `{path}`");
                outln!(w, "- **bitmap:** {bitmap}");
                outln!(w);
            }
            Self::Offload {
                duration_ms,
                report,
            } => {
                outln!(w, "#### offload\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                report.render_markdown_fields(w)?;
                outln!(w);
            }
            Self::DropLargeBlobs {
                duration_ms,
                report,
            } => {
                outln!(w, "#### drop-large-blobs\n");
                outln!(w, "- **duration_ms:** {duration_ms}");
                report.render_markdown_fields(w)?;
                outln!(w);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use grit_lib::config::git_path::user_display_path;

    use super::*;

    #[test]
    fn gc_paths_strip_verbatim_prefix_for_user_output() {
        let mut stats = grit_lib::maintenance::RepackStats::default();
        stats.pack_path = Some(PathBuf::from(
            r"\\?\C:\repo\.git\objects\pack\pack-abc.pack",
        ));
        let summary = RepackSummary::from(stats);
        let pack_path = summary.pack_path.expect("pack_path");
        assert!(!pack_path.contains(r"\\?\"));
        assert_eq!(pack_path, "C:/repo/.git/objects/pack/pack-abc.pack");

        let packed_refs = user_display_path(std::path::Path::new(r"\\?\C:\repo\.git\packed-refs"));
        assert_eq!(packed_refs, "C:/repo/.git/packed-refs");

        let task = GcTaskReport::PackRefs {
            duration_ms: 4,
            refs_packed: 2,
            refs_pruned: 2,
            path: packed_refs,
        };
        let mut buf = Vec::new();
        task.render_human_line(&mut buf, false).expect("render");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(!line.contains(r"\\?\"));
        assert!(line.contains("C:/repo/.git/packed-refs"));
    }

    #[test]
    fn gc_repack_dry_run_human_line_matches_repack_command() {
        let report = RepackSummary {
            loose_before: 307,
            packs_before: 0,
            objects_packed: Some(307),
            pack_bytes: None,
            pack_path: None,
            removed_pack_files: 0,
            pruned_loose: 0,
            unreachable_loose_pruned: 0,
        };
        let task = GcTaskReport::Repack {
            duration_ms: 268,
            report,
        };
        let mut buf = Vec::new();
        task.render_human_line(&mut buf, true).expect("render");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(line.contains("repack: would pack 307 objects (307 loose, 0 pack files now)"));
        assert!(!line.contains("nothing to pack"));
    }
}
