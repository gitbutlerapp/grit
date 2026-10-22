//! Compare [`grit_lib::remote::push_plan::resolve_push`] + dry-run push with
//! `git push --porcelain --dry-run` on the same bare remote.

use grit_lib::config::environment::Environment;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::remote::push_plan::{resolve_push, PushPlanContext, PushRequest};
use grit_lib::remote::transfer::{push_local, PushOptions, PushRefSpec};
use grit_lib::repo::Repository;
use grit_test_support::{git, git_cmd};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

struct Fixture {
    _tmp: tempfile::TempDir,
    local: PathBuf,
    local_git: PathBuf,
    bare: PathBuf,
}

fn setup_feature_and_main() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let local = tmp.path().join("local");
    let bare = tmp.path().join("remote.git");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&bare).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "1\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-q", "-m", "c1"]);
    git(&local, &["branch", "feature"]);
    git(&local, &["checkout", "-q", "feature"]);
    std::fs::write(local.join("g"), "2\n").unwrap();
    git(&local, &["add", "g"]);
    git(&local, &["commit", "-q", "-m", "c2"]);
    git(&bare, &["init", "-q", "--bare", "-b", "main", "."]);
    git(&local, &["remote", "add", "origin", bare.to_str().unwrap()]);
    Fixture {
        _tmp: tmp,
        local_git: local.join(".git"),
        local,
        bare,
    }
}

fn open_repo(path: &Path) -> Repository {
    Repository::discover(Some(path), &RepositoryOptions::empty()).expect("open repo")
}

fn load_config(repo: &Repository) -> ConfigSet {
    ConfigSet::load(&Environment::empty(), Some(repo.git_dir()), true).expect("config")
}

fn git_porcelain_dry_run(cwd: &Path, args: &[&str]) -> Vec<PorcelainLine> {
    let mut cmd_args = vec!["push", "--porcelain", "--dry-run"];
    cmd_args.extend(args);
    let out = git_cmd(&cmd_args).in_dir(cwd).exec();
    assert!(out.ok(), "git push failed: {}", out.stderr);
    parse_porcelain(&out.stdout)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PorcelainLine {
    flag: char,
    spec: String,
    forced: bool,
}

fn parse_porcelain(stdout: &str) -> Vec<PorcelainLine> {
    let mut lines = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("To ") || line == "Done" {
            continue;
        }
        let Some((flag_str, rest)) = line.split_once('\t') else {
            continue;
        };
        let flag = flag_str.chars().next().unwrap_or(' ');
        let forced = flag == '+';
        let spec = rest.split('\t').next().unwrap_or(rest).to_owned();
        lines.push(PorcelainLine { flag, spec, forced });
    }
    lines
}

fn plan_specs(
    repo: &Repository,
    config: &ConfigSet,
    request: PushRequest,
    ctx: &PushPlanContext,
) -> Vec<PushRefSpec> {
    resolve_push(repo, config, &request, ctx)
        .expect("resolve_push")
        .specs
}

fn dry_run_push_local(local_git: &Path, bare: &Path, specs: &[PushRefSpec]) {
    let _ = push_local(
        local_git,
        bare,
        specs,
        &PushOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .expect("push_local dry-run");
}

#[test]
fn short_dst_matches_remote_tag() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "main"]);
    git(&fx.local, &["push", "-q", "origin", "main"]);
    git(&fx.bare, &["tag", "x"]);
    git(&fx.local, &["checkout", "-q", "main"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let mut ctx = PushPlanContext::default();
    ctx.remote_ref_names = list_remote_refs(&fx.bare);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["main:x".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &ctx,
    );
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].dst, "refs/tags/x");
    compare_with_git(&fx.local, &["origin", "main:x"], &specs, &fx.bare);
}

#[test]
fn matching_push_config_only_existing_remote_heads() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["push", "-q", "origin", "main"]);
    git(&fx.local, &["config", "remote.origin.push", ":"]);
    git(&fx.local, &["checkout", "-q", "main"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let mut ctx = PushPlanContext::default();
    ctx.remote_ref_names = list_remote_refs(&fx.bare);
    let specs = plan_specs(&repo, &config, PushRequest::default(), &ctx);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].dst, "refs/heads/main");
    assert!(specs[0].src_ref.as_deref() == Some("refs/heads/main"));
}

fn compare_with_git(local: &Path, git_args: &[&str], specs: &[PushRefSpec], bare: &Path) {
    let git_lines = git_porcelain_dry_run(local, git_args);
    dry_run_push_local(&local.join(".git"), bare, specs);
    assert_eq!(git_lines.len(), specs.len(), "ref count");
    for (g, s) in git_lines.iter().zip(specs.iter()) {
        let (gs, gd) = g.spec.split_once(':').expect("git spec");
        assert_eq!(gd, s.dst);
        // Git porcelain dry-run often uses a space flag even when the refspec had `+`.
        if g.forced {
            assert!(s.force, "git marked forced but spec did not");
        }
        let _ = gs;
    }
}

#[test]
fn feature_branch_shorthand() {
    let fx = setup_feature_and_main();
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["feature".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    compare_with_git(&fx.local, &["origin", "feature"], &specs, &fx.bare);
}

#[test]
fn head_colon_refs_heads_x() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "feature"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["HEAD:refs/heads/x".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    compare_with_git(
        &fx.local,
        &["origin", "HEAD:refs/heads/x"],
        &specs,
        &fx.bare,
    );
}

#[test]
fn tag_v1_0() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["tag", "v1.0"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["v1.0".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    compare_with_git(&fx.local, &["origin", "v1.0"], &specs, &fx.bare);
}

#[test]
fn force_main_non_fast_forward() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "main"]);
    git(&fx.local, &["push", "-q", "origin", "main"]);
    std::fs::write(fx.local.join("f"), "3\n").unwrap();
    git(&fx.local, &["commit", "-q", "-am", "ahead"]);
    // Diverge remote
    let old = git(&fx.local, &["rev-parse", "HEAD~1"]).trim().to_owned();
    git_cmd(&["update-ref", "refs/heads/main", &old])
        .in_dir(&fx.bare)
        .exec();
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["+main:main".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    assert!(specs[0].force);
    compare_with_git(&fx.local, &["origin", "+main:main"], &specs, &fx.bare);
}

#[test]
fn push_remote_precedence() {
    let fx = setup_feature_and_main();
    git(
        &fx.local,
        &["config", "branch.feature.pushRemote", "upstream"],
    );
    git(
        &fx.local,
        &["remote", "add", "upstream", fx.bare.to_str().unwrap()],
    );
    git(&fx.local, &["config", "remote.pushDefault", "origin"]);
    git(&fx.local, &["checkout", "-q", "feature"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let resolved = resolve_push(
        &repo,
        &config,
        &PushRequest::default(),
        &PushPlanContext::default(),
    )
    .expect("plan");
    assert_eq!(resolved.remote, "upstream");
}

#[test]
fn remote_push_config_applied_when_no_cli_refspec() {
    let fx = setup_feature_and_main();
    git(
        &fx.local,
        &["config", "remote.origin.push", "refs/heads/*:refs/heads/*"],
    );
    git(&fx.local, &["checkout", "-q", "feature"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest::default(),
        &PushPlanContext::default(),
    );
    assert!(!specs.is_empty());
    assert!(specs.iter().any(|s| s.dst == "refs/heads/feature"));
}

#[test]
fn detached_head_without_refspec_errors() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "--detach", "HEAD"]);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let err = resolve_push(
        &repo,
        &config,
        &PushRequest::default(),
        &PushPlanContext::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("detached"));
}

#[test]
fn real_push_matches_git_file_remote() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "feature"]);
    let expected = git(&fx.local, &["rev-parse", "feature"]).trim().to_owned();
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["feature".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    push_local(&fx.local_git, &fx.bare, &specs, &PushOptions::default()).expect("grit push");
    let grit_oid = git(&fx.bare, &["rev-parse", "refs/heads/feature"]);
    assert_eq!(expected, grit_oid.trim());
    let (ok, out) = git_try(&fx.bare, &["fsck", "--strict"]);
    assert!(ok, "fsck: {out}");
}

fn git_try(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_cmd(args).in_dir(dir).exec();
    let ok = out.ok();
    let mut combined = out.stdout;
    combined.push_str(&out.stderr);
    (ok, combined)
}

#[test]
fn dry_run_leaves_bare_refs_unchanged() {
    let fx = setup_feature_and_main();
    git(&fx.local, &["checkout", "-q", "feature"]);
    let before = list_remote_refs(&fx.bare);
    let repo = open_repo(&fx.local);
    let config = load_config(&repo);
    let specs = plan_specs(
        &repo,
        &config,
        PushRequest {
            refspecs: vec!["feature".into()],
            remote: Some("origin".into()),
            ..Default::default()
        },
        &PushPlanContext::default(),
    );
    push_local(
        &fx.local_git,
        &fx.bare,
        &specs,
        &PushOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .expect("dry-run");
    let after = list_remote_refs(&fx.bare);
    assert_eq!(before, after);
}

fn list_remote_refs(bare: &Path) -> HashSet<String> {
    let out = git(bare, &["for-each-ref", "--format=%(refname)"]);
    out.lines().map(str::to_owned).collect()
}
