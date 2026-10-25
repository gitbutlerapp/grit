//! Remote headline and matrix scenarios (fetch, pull, push, clone, ls-remote).

use super::super::helpers::{headline_spec, matrix_spec, prepare_none, sh_devnull, sh_script};
use super::super::matrix_helpers::{prepare_remote_rename_reset, remote_rename_bench};
use super::super::plan::{DailyHeadlineContext, DailyHeadlinePlan, DailyPrepare};
use super::super::remote_fixture::{
    clone_bare_bench_script, clone_bare_bench_script_grit, clone_bench_script,
    clone_bench_script_grit, clone_bench_script_grit_shallow, clone_bench_script_shallow,
    clone_blobless_bench_script_git, clone_blobless_bench_script_grit,
    clone_shallow_bench_script_git, clone_shallow_bench_script_grit, setup_git_git_clone_remote,
    setup_git_git_grit_http_fetch_remote, setup_git_git_grit_http_push_remote,
    setup_git_git_remote, setup_git_git_remote_blobless, setup_many_refs_remote,
    setup_many_refs_remote_show, setup_remote_remove_many_refs, setup_remote_rename_many_refs,
    setup_synth100k_path_remote, setup_synth10k_grit_http_remote,
    setup_synth10k_pull_rebase_remote, setup_synth10k_remote, verify_clone_bare,
    verify_clone_blobless, verify_clone_http, verify_clone_local, verify_clone_shallow,
    verify_clone_shallow_http, verify_fetch_deepen, verify_fetch_dry_run, verify_fetch_incremental,
    verify_fetch_incremental_http, verify_fetch_noop, verify_fetch_one_branch,
    verify_ls_remote_all, verify_pull_check, verify_pull_fast_forward, verify_pull_rebase,
    verify_push_force_with_lease, verify_push_incremental, verify_push_incremental_http,
    verify_push_refspec,
};
use super::super::runner::BenchArgv;
use crate::shell::shell_quote;

fn prepare_network_fetch_incr(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-fetch-incr")
}

fn prepare_network_fetch_noop(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-fetch-noop")
}

fn prepare_network_push(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-push")
}

fn prepare_network_force_with_lease(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-force-with-lease")
}

fn prepare_network_clone(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-clone")
}

fn prepare_network_pull_rebase(_: &DailyHeadlineContext) -> DailyPrepare {
    DailyPrepare::Network("prepare-network-pull-rebase")
}

fn correctness_fetch_incr(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_incremental(ctx, &ctx.repo)
}

fn correctness_pull_ff(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_pull_fast_forward(ctx, &ctx.repo)
}

fn correctness_pull_rebase(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_pull_rebase(ctx, &ctx.repo)
}

fn correctness_push_incr(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_push_incremental(ctx, &ctx.repo)
}

fn correctness_push_refspec(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_push_refspec(ctx, &ctx.repo)
}

fn correctness_push_fwl(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_push_force_with_lease(ctx, &ctx.repo)
}

fn correctness_clone_local(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_local(ctx, &ctx.repo)
}

fn correctness_fetch_noop(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_noop(ctx, &ctx.repo)
}

fn correctness_fetch_one_branch(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_one_branch(ctx, &ctx.repo)
}

fn correctness_fetch_dry_run(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_dry_run(ctx, &ctx.repo)
}

fn correctness_fetch_deepen(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_fetch_deepen(ctx, &ctx.repo)
}

fn prepare_fetch_deepen_client(ctx: &DailyHeadlineContext) -> DailyPrepare {
    let branch = default_remote_branch(ctx);
    DailyPrepare::Shell(sh_script(format!(
        "set -e\ncd {}\ngit fetch --depth 1 -q origin {}\n{} fetch --depth 1 origin {}\n",
        ctx.repo.display(),
        branch,
        ctx.grit.display(),
        branch,
    )))
}

fn fetch_deepen_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.git, &["fetch", "--deepen", "50", "-q", "origin"])
}

fn fetch_deepen_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["fetch", "--deepen", "50", "origin"])
}

fn correctness_pull_check(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_pull_check(ctx, &ctx.repo)
}

fn correctness_ls_remote(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_ls_remote_all(ctx, &ctx.repo)
}

fn correctness_clone_http(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_http(ctx, &ctx.repo)
}

fn correctness_clone_bare(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_bare(ctx, &ctx.repo)
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

fn default_remote_branch(ctx: &DailyHeadlineContext) -> &str {
    ctx.remote
        .as_ref()
        .map(|r| r.meta.default_branch.as_str())
        .unwrap_or("master")
}

fn fetch_one_branch_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    sh_devnull(&ctx.git, &["fetch", "-q", "origin", branch])
}

fn fetch_one_branch_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    sh_devnull(&ctx.grit, &["fetch", "-q", "origin", branch])
}

fn fetch_dry_run_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    sh_devnull(&ctx.git, &["fetch", "--dry-run", "-q", "origin", branch])
}

fn fetch_dry_run_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    sh_devnull(&ctx.grit, &["fetch", "--dry-run", "origin", branch])
}

fn pull_check_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    let git_q = shell_quote(&ctx.git.to_string_lossy());
    sh_script(format!(
        "{git_q} fetch -q origin {branch} && {git_q} rev-list --left-right --count HEAD...origin/{branch} >/dev/null"
    ))
}

fn pull_check_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = default_remote_branch(ctx);
    sh_devnull(&ctx.grit, &["pull", "--check", "origin", branch])
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

fn pull_rebase_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = ctx
        .remote
        .as_ref()
        .map(|r| r.meta.default_branch.as_str())
        .unwrap_or("main");
    sh_devnull(&ctx.git, &["pull", "--rebase", "-q", "origin", branch])
}

fn pull_rebase_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["pull", "--rebase"])
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

fn push_refspec_git(_ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(
        &_ctx.git,
        &["push", "-q", "origin", "HEAD:refs/heads/bench"],
    )
}

fn push_refspec_grit(_ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&_ctx.grit, &["push", "origin", "HEAD:refs/heads/bench"])
}

fn push_fwl_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let branch = ctx
        .remote
        .as_ref()
        .map(|r| r.meta.default_branch.as_str())
        .unwrap_or("main");
    sh_devnull(
        &ctx.git,
        &["push", "--force-with-lease", "-q", "origin", branch],
    )
}

fn push_fwl_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["push", "--force-with-lease"])
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

fn remote_show_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.git, &["remote", "show", "origin"])
}

fn remote_show_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    sh_devnull(&ctx.grit, &["remote", "show", "origin"])
}

fn clone_http_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_local_git(ctx)
}

fn clone_http_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_local_grit(ctx)
}

fn clone_bare_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bare_bench_script(
        &ctx.git,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_bare_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bare_bench_script_grit(
        &ctx.grit,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_shallow_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bench_script_shallow(
        &ctx.git,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_shallow_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_bench_script_grit_shallow(
        &ctx.grit,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_blobless_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_blobless_bench_script_git(
        &ctx.git,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_blobless_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_blobless_bench_script_grit(
        &ctx.grit,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_shallow_local_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_shallow_bench_script_git(
        &ctx.git,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_shallow_local_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    let Some(remote) = ctx.remote.as_ref() else {
        return BenchArgv::new("false", Vec::<&str>::new());
    };
    sh_script(clone_shallow_bench_script_grit(
        &ctx.grit,
        &remote.remote_url,
        &remote.meta.clone_dest,
    ))
}

fn clone_shallow_http_git(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_shallow_local_git(ctx)
}

fn clone_shallow_http_grit(ctx: &DailyHeadlineContext) -> BenchArgv {
    clone_shallow_local_grit(ctx)
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
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_incr,
            build_git: fetch_incr_git,
            build_grit: fetch_incr_grit,
            correctness: Some(correctness_fetch_incr),
        },
        DailyHeadlinePlan {
            spec: headline_spec("pull/fast-forward@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_incr,
            build_git: pull_ff_git,
            build_grit: pull_ff_grit,
            correctness: Some(correctness_pull_ff),
        },
        DailyHeadlinePlan {
            spec: headline_spec("pull/rebase@synth-10k"),
            pending: false,
            setup: setup_synth10k_pull_rebase_remote,
            prepare: prepare_network_pull_rebase,
            build_git: pull_rebase_git,
            build_grit: pull_rebase_grit,
            correctness: Some(correctness_pull_rebase),
        },
        DailyHeadlinePlan {
            spec: headline_spec("push/incremental@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_push,
            build_git: push_incr_git,
            build_grit: push_incr_grit,
            correctness: Some(correctness_push_incr),
        },
        DailyHeadlinePlan {
            spec: headline_spec("push/force-with-lease@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_force_with_lease,
            build_git: push_fwl_git,
            build_grit: push_fwl_grit,
            correctness: Some(correctness_push_fwl),
        },
        DailyHeadlinePlan {
            spec: headline_spec("clone/local@synth-10k"),
            pending: false,
            setup: setup_synth10k_remote,
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
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_noop,
            build_git: fetch_incr_git,
            build_grit: fetch_incr_grit,
            correctness: Some(correctness_fetch_noop),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("ls-remote/all@many-refs"),
            pending: false,
            setup: setup_many_refs_remote,
            prepare: prepare_none,
            build_git: ls_remote_git,
            build_grit: ls_remote_grit,
            correctness: Some(correctness_ls_remote),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("remote/show@many-refs"),
            pending: false,
            setup: setup_many_refs_remote_show,
            prepare: prepare_none,
            build_git: remote_show_git,
            build_grit: remote_show_grit,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: matrix_spec("remote/rename@many-refs"),
            pending: false,
            setup: setup_remote_rename_many_refs,
            prepare: prepare_remote_rename_reset,
            build_git: |c| remote_rename_bench(c).1,
            build_grit: |c| remote_rename_bench(c).0,
            correctness: None,
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
            spec: matrix_spec("clone/full@git-git"),
            pending: false,
            setup: setup_git_git_clone_remote,
            prepare: prepare_network_clone,
            build_git: clone_http_git,
            build_grit: clone_http_grit,
            correctness: Some(correctness_clone_http),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/bare@synth-10k"),
            pending: false,
            setup: setup_synth10k_remote,
            prepare: prepare_network_clone,
            build_git: clone_bare_git,
            build_grit: clone_bare_grit,
            correctness: Some(correctness_clone_bare),
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
        DailyHeadlinePlan {
            spec: matrix_spec("push/refspec@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_push,
            build_git: push_refspec_git,
            build_grit: push_refspec_grit,
            correctness: Some(correctness_push_refspec),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("fetch/one-branch@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_incr,
            build_git: fetch_one_branch_git,
            build_grit: fetch_one_branch_grit,
            correctness: Some(correctness_fetch_one_branch),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("fetch/dry-run@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_incr,
            build_git: fetch_dry_run_git,
            build_grit: fetch_dry_run_grit,
            correctness: Some(correctness_fetch_dry_run),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("fetch/deepen@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_fetch_deepen_client,
            build_git: fetch_deepen_git,
            build_grit: fetch_deepen_grit,
            correctness: Some(correctness_fetch_deepen),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("pull/check@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_fetch_incr,
            build_git: pull_check_git,
            build_grit: pull_check_grit,
            correctness: Some(correctness_pull_check),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/shallow@git-git"),
            pending: false,
            setup: setup_git_git_remote,
            prepare: prepare_network_clone,
            build_git: clone_shallow_git,
            build_grit: clone_shallow_grit,
            correctness: Some(correctness_clone_shallow),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/blobless@git-git"),
            pending: false,
            setup: setup_git_git_remote_blobless,
            prepare: prepare_network_clone,
            build_git: clone_blobless_git,
            build_grit: clone_blobless_grit,
            correctness: Some(correctness_clone_blobless),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/checkout@synth-100k"),
            pending: false,
            setup: setup_synth100k_path_remote,
            prepare: prepare_network_clone,
            build_git: clone_local_git,
            build_grit: clone_local_grit,
            correctness: Some(correctness_clone_local),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/shallow-depth1@synth-10k"),
            pending: false,
            setup: setup_synth10k_remote,
            prepare: prepare_network_clone,
            build_git: clone_shallow_local_git,
            build_grit: clone_shallow_local_grit,
            correctness: Some(correctness_clone_shallow),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("clone/shallow-depth1-http@synth-10k"),
            pending: false,
            setup: setup_synth10k_grit_http_remote,
            prepare: prepare_network_clone,
            build_git: clone_shallow_http_git,
            build_grit: clone_shallow_http_grit,
            correctness: Some(correctness_clone_shallow_http),
        },
        DailyHeadlinePlan {
            spec: matrix_spec("remote/remove@many-refs"),
            pending: false,
            setup: setup_remote_remove_many_refs,
            prepare: super::super::matrix_helpers::prepare_remote_remove,
            build_git: |c| super::super::matrix_helpers::remote_remove_bench(c).1,
            build_grit: |c| super::super::matrix_helpers::remote_remove_bench(c).0,
            correctness: None,
        },
    ]
}

fn correctness_clone_shallow(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_shallow(ctx, &ctx.repo)
}

fn correctness_clone_blobless(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_blobless(ctx, &ctx.repo)
}

fn correctness_clone_shallow_http(ctx: &DailyHeadlineContext) -> anyhow::Result<()> {
    verify_clone_shallow_http(ctx, &ctx.repo)
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
            "pull/rebase@synth-10k",
            "push/incremental@git-git",
            "push/force-with-lease@git-git",
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
