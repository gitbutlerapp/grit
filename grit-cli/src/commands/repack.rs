//! `grit repack` — pack loose objects and remove redundant packs.


use anyhow::Result;
use grit_lib::maintenance::{RepackOptions, RepackStats};

use crate::context;
use crate::ui::filesystem_path_for_user;

pub use crate::render::repack::RepackOutcome;

impl From<(RepackStats, bool, bool)> for RepackOutcome {
    fn from((stats, dry_run, quiet): (RepackStats, bool, bool)) -> Self {
        Self {
            quiet,
            dry_run,
            loose_before: stats.loose_before,
            packs_before: stats.packs_before,
            objects_packed: stats.objects_packed,
            pack_bytes: stats.pack_bytes,
            pack_path: stats.pack_path.as_deref().map(filesystem_path_for_user),
            removed_pack_files: stats.removed_pack_files,
            pruned_loose: stats.pruned_loose,
        }
    }
}

pub fn run(force: bool, dry_run: bool, quiet: bool) -> Result<RepackOutcome> {
    let repo = context::discover()?;
    let mut opts = RepackOptions::with_dry_run(dry_run);
    opts.force_recompress = force;
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let stats = repo
        .maintenance()
        .repack_all(
            &opts,
            &mut grit_lib::progress::NullProgress,
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(crate::json_error::map_lib_error)?;
    Ok((stats, dry_run, quiet).into())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::output::{HumanRender, RenderContext};

    fn sample_ctx() -> RenderContext {
        crate::render::RenderContext::human_for_tests()
    }

    #[test]
    fn repack_outcome_strips_verbatim_pack_path() {
        let mut stats = RepackStats::default();
        stats.pack_path = Some(PathBuf::from(
            r"\\?\C:\repo\.git\objects\pack\pack-abc.pack",
        ));
        stats.objects_packed = Some(96);
        let outcome: RepackOutcome = (stats, false, false).into();
        let path = outcome.pack_path.as_ref().expect("pack_path");
        assert!(!path.contains(r"\\?\"));
        assert_eq!(path, "C:/repo/.git/objects/pack/pack-abc.pack");

        let mut buf = Vec::new();
        outcome
            .render_human(&mut buf, &sample_ctx())
            .expect("render human");
        let line = String::from_utf8(buf).expect("utf8");
        assert!(!line.contains(r"\\?\"));
        assert!(line.contains("C:/repo/.git/objects/pack/pack-abc.pack"));
    }
}

/// Dispatch parsed CLI args for `Repack`.
pub fn dispatch(
    args: crate::args::RepackArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let RepackArgs {
        force,
        dry_run,
        quiet,
    } = args;
    emit_success(commands::repack::run(force, dry_run, quiet)?, opts, ctx)
}
