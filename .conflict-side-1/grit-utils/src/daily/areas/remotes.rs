//! Remote headline and matrix scenarios (fetch, pull, push, clone, ls-remote).

use super::super::helpers::{headline_spec, matrix_spec, prepare_none, sh_devnull, sh_script};
use super::super::plan::{DailyHeadlineContext, DailyHeadlinePlan, DailyPrepare};
use super::super::remote_fixture::{
    clone_bench_script, clone_bench_script_grit, setup_git_git_file_remote,
    setup_git_git_grit_http_fetch_remote, setup_git_git_grit_http_push_remote,
    setup_many_refs_file_remote, setup_synth10k_file_remote, setup_synth10k_grit_http_remote,
    verify_clone_http, verify_clone_local, verify_fetch_incremental, verify_fetch_incremental_http,
    verify_fetch_noop, verify_ls_remote_all, verify_pull_fast_forward, verify_push_incremental,
    verify_push_incremental_http,
};
use super::super::runner::BenchArgv;

fn prepare_network_fetch_incr(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-fetch-incr")
}

fn prepare_network_fetch_noop(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-fetch-noop")
}

fn prepare_network_push(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-push")
}

fn prepare_network_clone(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-clone")
}

fn correctness_fetch_incr(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_incremental(ctx, &ctx.repo)
}

fn correctness_pull_ff(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_pull_fast_forward(ctx, &ctx.repo)
}

fn correctness_push_incr(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_push_incremental(ctx, &ctx.repo)
}

fn correctness_clone_local(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_local(ctx, &ctx.repo)
}

fn correctness_fetch_noop(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_noop(ctx, &ctx.repo)
}

fn correctness_ls_remote(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_ls_remote_all(ctx, &ctx.repo)
}

fn correctness_clone_http(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_http(ctx, &ctx.repo)
}

fn correctness_fetch_incr_http(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_incremental_http(ctx, &ctx.repo)
}

fn correctness_push_incr_http(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_push_incremental_http(ctx, &ctx.repo)
}

fn fetch_incr_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.git, &["fetch", "origin"])
}

fn fetch_incr_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["fetch", "origin"])
}

fn pull_ff_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = ctx
        .remote
        .as_ref()
        .map(|r| r.meta.default_branch.as_str())
        .unwrap_or("main");
    sh_devnull(&ctx.git, &["pull", "--ff-only", "-q", "origin", branch])
}

fn pull_ff_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["pull"])
}

fn push_incr_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = ctx
        .remote
        .as_ref()
        .map(|r| r.meta.default_branch.as_str())
        .unwrap_or("main");
    sh_devnull(&ctx.git, &["push", "-q", "origin", branch])
}

fn push_incr_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["push"])
}

fn clone_local_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bench_script(
        &ctx.git,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_local_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bench_script_grit(
        &ctx.grit,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn ls_remote_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(url) = ctx.remote.as_ref().map(|r| r.remote_url.as_str()) else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_devnull(&ctx.git, &["ls-remote", url])
}

fn ls_remote_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(url) = ctx.remote.as_ref().map(|r| r.remote_url.as_str()) else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_devnull(&ctx.grit, &["remote", "refs", url])
}

fn clone_http_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_local_git(ctx)
}

fn clone_http_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_local_grit(ctx)
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    let mut out = headline_plans();
    out.extend(matrix_plans());
    out
}

fn headline_plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("fetch/incremental@git-git"),
            pending: false,
            setup: setup_git_git_file_remote,
            prepare: prepare_network_fetch_incr,
            build_git: fetch_incr_git,
            build_grit: fetch_incr_grit,
            correctness: Some(correctness_fetch_incr),
        },
        DailyHeadlinePlan {
            spec: headline_spec("pull/fast-forward@git-git"),
            pending: false,
            setup: setup_git_git_file_remote,
            prepare: prepare_network_fetch_incr,
            build_git: pull_ff_git,
            build_grit: pull_ff_grit,
            correctness: Some(correctness_pull_ff),
        },
        DailyHeadlinePlan {
            spec: headline_spec("push/incremental@git-git"),
            pending: false,
            setup: setup_git_git_file_remote,
            prepare: prepare_network_push,
            build_git: push_incr_git,
            build_grit: push_incr_grit,
            correctness: Some(correctness_push_incr),
        },
        DailyHeadlinePlan {
            spec: headline_spec("clone/local@synth-10k"),
            pending: false,
            setup: setup_synth10k_file_remote,
            prepare: prepare_network_clone,
            build_git: clone_local_git,
            build_grit: clone_local_grit,
            correctness: Some(correctness_clone_local),
        },
    ]
}

fn matrix_plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: matrix_spec("fetch/noop@git-git"),
            pending: false,
            setup: setup_git_git_file_remote,
            prepare: prepare_network_fetch_noop,
            build_git: fetch_incr_git,
            build_grit: fetch_incr_grit,
            correctness: Some(correctness_fetch_noop),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("ls-remote/all@many-refs"),
            pending: false,
            setup: setup_many_refs_file_remote,
            prepare: prepare_none,
            build_git: ls_remote_git,
            build_grit: ls_remote_grit,
            correctness: Some(correctness_ls_remote),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/http@synth-10k"),
            pending: false,
            setup: setup_synth10k_grit_http_remote,
            prepare: prepare_network_clone,
            build_git: clone_http_git,
            build_grit: clone_http_grit,
            correctness: Some(correctness_clone_http),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("fetch/incremental-http@git-git"),
            pending: false,
            setup: setup_git_git_grit_http_fetch_remote,
            prepare: prepare_network_fetch_incr,
            build_git: fetch_incr_git,
            build_grit: fetch_incr_grit,
            correctness: Some(correctness_fetch_incr_http),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("push/incremental-http@git-git"),
            pending: false,
            setup: setup_git_git_grit_http_push_remote,
            prepare: prepare_network_push,
            build_git: push_incr_git,
            build_grit: push_incr_grit,
            correctness: Some(correctness_push_incr_http),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::super::super::registry::{HEADLINE_SCENARIOS, REMOTE_MATRIX_SCENARIOS};

    use crate::network_suite::network_http_root;

    #[test]
    fn grit_http_docroot_matches_network_prepare_mirror() {
        use std::path::PathBuf;

        let bare = PathBuf::from("/tmp/daily-scratch/remote.git");
        let legacy_daily = bare.parent().unwrap().join("http-root");
        let aligned = network_http_root(&bare);
        assert_eq!(
            aligned,
            PathBuf::from("/tmp/daily-scratch/remote.git-http-root")
        );
        assert_ne!(legacy_daily, aligned);
    }

    #[test]
    fn remote_headline_ids_registered() {
        for id in [
            "fetch/incremental@git-git",
            "pull/fast-forward@git-git",
            "push/incremental@git-git",
            "clone/local@synth-10k",
        ] {
            assert!(HEADLINE_SCENARIOS.iter().any(|s| s.id == id));
        }
    }

    #[test]
    fn remote_matrix_ids_distinct() {
        let mut seen = std::collections::HashSet::new();
        for spec in REMOTE_MATRIX_SCENARIOS {
            assert!(seen.insert(spec.id));
        }
    }
}
