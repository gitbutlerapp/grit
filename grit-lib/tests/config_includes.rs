//! Config includes and includeIf vs system `git config` (t1305, t1309, t1311).

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::{ConfigScope, ConfigSet, IncludeContext};
use grit_lib::environment::Environment;
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::init_repository;
use support::{
    git_file_get, git_file_get_includes, git_file_list, git_local_list_with_git_dir,
    grit_file_from_content, grit_list_lines, isolated_env, normalize_config_corpus, GitConfigLine,
};
use tempfile::tempdir;

fn include_ctx(git_dir: &Path, env: &Environment) -> IncludeContext {
    IncludeContext {
        git_dir: Some(git_dir.to_path_buf()),
        cwd: env.cwd.clone(),
        pwd: env.pwd.clone(),
        env: std::sync::Arc::new(env.clone()),
        ..Default::default()
    }
}

#[test]
fn t1305_include_and_includeif_match_git_show_origin() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    init_repository(root, false, "main", None, "files").expect("initialize repository");
    let gd = root.join(".git");
    let main = gd.join("config");
    fs::write(
        &main,
        "[include]\n\tpath = child.conf\n[includeIf \"onbranch:main\"]\n\tpath = branch.conf\n",
    )
    .expect("main");
    fs::write(gd.join("child.conf"), "[user]\n\temail = child@x\n").expect("child");
    fs::write(gd.join("branch.conf"), "[user]\n\tname = OnMain\n").expect("branch");

    let git_lines = git_local_list_with_git_dir(&gd, true).expect("git local config list");
    let content = fs::read_to_string(&main).expect("read");
    let file = grit_file_from_content(&main, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    let ctx = include_ctx(&gd, &env);
    set.merge_file_with_includes(&file, true, &ctx)
        .expect("merge includes");

    let git_corpus = normalize_config_corpus(&git_lines);
    let grit_corpus = normalize_config_corpus(&grit_list_lines(&set));
    assert_eq!(
        grit_corpus, git_corpus,
        "full ordered config corpus (scope, origin, key, value)"
    );
}

#[test]
fn include_relative_absolute_and_home() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");

    fs::write(repo.join(".git/config"), "[include]\n\tpath = rel.conf\n").expect("config");
    fs::write(repo.join(".git/rel.conf"), "[a]\n\tk = rel\n").expect("rel");

    let mut env = isolated_env(&home);
    env.cwd = repo.clone();
    let opts = support::default_load_opts(&repo.join(".git"), &env);
    let set = ConfigSet::load_with_options(&env, Some(&repo.join(".git")), &opts).expect("load");
    assert_eq!(set.get("a.k").as_deref(), Some("rel"));

    let abs_conf = dir.path().join("abs.conf");
    fs::write(
        repo.join(".git/config"),
        format!("[include]\n\tpath = {}\n", abs_conf.display()),
    )
    .expect("config");
    fs::write(&abs_conf, "[b]\n\tk = abs\n").expect("abs");
    let set2 = ConfigSet::load_with_options(&env, Some(&repo.join(".git")), &opts).expect("load2");
    assert_eq!(set2.get("b.k").as_deref(), Some("abs"));
    assert_eq!(
        git_file_get_includes(&repo.join(".git/config"), "b.k", Some(&repo.join(".git")))
            .as_deref(),
        Some("abs")
    );
}

#[test]
fn include_cycle_is_typed_error() {
    let dir = tempdir().expect("tempdir");
    let a = dir.path().join("a.conf");
    let b = dir.path().join("b.conf");
    fs::write(&a, "[include]\n\tpath = b.conf\n[a]\n\tx = 1\n").expect("a");
    fs::write(&b, "[include]\n\tpath = a.conf\n[b]\n\ty = 2\n").expect("b");

    let content = fs::read_to_string(&a).expect("read");
    let file = grit_file_from_content(&a, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    let ctx = include_ctx(dir.path(), &env);
    let err = set
        .merge_file_with_includes(&file, true, &ctx)
        .expect_err("cycle");
    match err {
        Error::Config(ConfigError::IncludeDepthExceeded { depth, limit }) => {
            assert!(depth > limit, "depth {depth} should exceed limit {limit}");
            assert_eq!(limit, 10);
        }
        other => panic!("expected IncludeDepthExceeded, got {other:?}"),
    }
}

#[test]
fn missing_include_is_ignored() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config");
    fs::write(&cfg, "[include]\n\tpath = missing.conf\n[ok]\n\ty = 1\n").expect("write");
    let content = fs::read_to_string(&cfg).expect("read");
    let file = grit_file_from_content(&cfg, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    set.merge_file_with_includes(&file, true, &include_ctx(dir.path(), &env))
        .expect("missing include ok");
    assert_eq!(set.get("ok.y").as_deref(), Some("1"));
    assert!(git_file_get(&cfg, "ok.y").as_deref() == Some("1"));
}

#[test]
fn read_early_config_matches_git_layers() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(home.join(".gitconfig"), "[early]\n\tk = global\n").expect("global");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    fs::write(repo.join(".git/config"), "[early]\n\tk = local\n").expect("local");
    fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");

    let mut env = isolated_env(&home);
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());

    let grit_vals =
        ConfigSet::read_early_config(&env, Some(&repo.join(".git")), "early.k").expect("early");
    assert!(
        grit_vals.contains(&"global".to_owned()) && grit_vals.contains(&"local".to_owned()),
        "early config order: {grit_vals:?}"
    );
}

#[test]
fn load_repo_local_only_ignores_global() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(
        home.join(".gitconfig"),
        "[remote]\n\tpushDefault = global\n",
    )
    .expect("global");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    fs::write(
        repo.join(".git/config"),
        "[remote]\n\tpushDefault = local\n",
    )
    .expect("local");

    let set = ConfigSet::load_repo_local_only(&repo.join(".git")).expect("local only");
    assert_eq!(set.get("remote.pushdefault").as_deref(), Some("local"));
    assert!(set.get("remote.pushdefault").as_deref() != Some("global"));
}

#[test]
fn load_protected_skips_repo_config() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(
        home.join(".gitconfig"),
        "[uploadpack]\n\tpackObjectsHook = global-hook\n",
    )
    .expect("global");
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).expect("git");
    fs::write(
        repo.join(".git/config"),
        "[uploadpack]\n\tpackObjectsHook = local-hook\n",
    )
    .expect("local");

    let mut env = isolated_env(&home);
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());
    let set = ConfigSet::load_protected(&env, false).expect("protected");
    assert_eq!(
        set.get("uploadpack.packobjectshook").as_deref(),
        Some("global-hook")
    );
    assert_ne!(
        set.get("uploadpack.packobjectshook").as_deref(),
        Some("local-hook")
    );
}

#[test]
fn includeif_gitdir_trailing_slash_and_wildcard() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let repo = home.join("nested").join("repo");
    init_repository(&repo, false, "main", None, "files").expect("init");
    let git_dir = repo.join(".git");
    let gitdir_pattern = format!("gitdir:{}/", repo.display());
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read config");
    cfg.push_str(&format!(
        "[includeIf \"{gitdir_pattern}\"]\n\tpath = extra.conf\n"
    ));
    fs::write(git_dir.join("config"), &cfg).expect("config");
    fs::write(git_dir.join("extra.conf"), "[mark]\n\tk = hit\n").expect("extra");

    let mut env = isolated_env(&home);
    env.home = Some(home.as_os_str().to_os_string());
    env.cwd = repo.clone();
    env.pwd = Some(repo.display().to_string());

    let opts = support::default_load_opts(&git_dir, &env);
    let set = ConfigSet::load_with_options(&env, Some(&git_dir), &opts).expect("load");
    assert_eq!(set.get("mark.k").as_deref(), Some("hit"));
    assert_eq!(
        support::git_file_get_includes_with_home(
            &git_dir.join("config"),
            "mark.k",
            Some(&git_dir),
            Some(&home),
        )
        .as_deref(),
        Some("hit")
    );
}

#[test]
fn extensions_worktree_config_layering() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    if !cfg.contains("worktreeConfig") {
        cfg.push_str("\n[extensions]\n\tworktreeConfig = true\n");
        fs::write(git_dir.join("config"), &cfg).expect("write");
    }
    fs::write(git_dir.join("config.worktree"), "[wt]\n\tkey = worktree\n").expect("wt config");

    let env = Environment::empty();
    let opts = support::default_load_opts(&git_dir, &env);
    let set = ConfigSet::load_with_options(&env, Some(&git_dir), &opts).expect("load");
    assert_eq!(set.get("wt.key").as_deref(), Some("worktree"));
}

#[test]
fn optional_include_git_parity() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config");
    fs::write(
        &cfg,
        "[includeIf \"gitdir:/nonexistent/path/\"]\n\tpath = opt.conf\n[set]\n\tx = 1\n",
    )
    .expect("write");
    let git_val = git_file_get(&cfg, "set.x");
    let content = fs::read_to_string(&cfg).expect("read");
    let file = grit_file_from_content(&cfg, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    set.merge_file_with_includes(&file, true, &include_ctx(dir.path(), &env))
        .expect("optional include");
    assert_eq!(set.get("set.x").as_deref(), git_val.as_deref());
}

fn normalize_git_lines(lines: &[GitConfigLine]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter(|l| !l.key.starts_with("include"))
        .map(|l| (l.key.clone(), l.value.clone()))
        .collect()
}

#[test]
fn show_origin_order_matches_git_for_simple_include() {
    let dir = tempdir().expect("tempdir");
    let main = dir.path().join("main.conf");
    fs::write(&main, "[include]\n\tpath = inc.conf\n").expect("main");
    fs::write(dir.path().join("inc.conf"), "[z]\n\ta = 1\n").expect("inc");

    let git_lines = normalize_git_lines(&git_file_list(&main, true).expect("git"));
    let file = grit_file_from_content(
        &main,
        &fs::read_to_string(&main).unwrap(),
        ConfigScope::Local,
    );
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    set.merge_file_with_includes(&file, true, &include_ctx(dir.path(), &env))
        .expect("merge");
    let grit_lines: Vec<_> = grit_list_lines(&set)
        .into_iter()
        .filter(|l| !l.key.starts_with("include"))
        .map(|l| (l.key, l.value))
        .collect();
    assert_eq!(grit_lines, git_lines);
}
