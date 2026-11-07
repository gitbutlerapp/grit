//! Additional `[includeIf]` and include-validation paths for config line coverage.

use crate::support;

use std::fs;
use std::path::PathBuf;

use grit_lib::config::environment::Environment;
use grit_lib::config::{ConfigScope, ConfigSet, IncludeContext};
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::{InitOptions, Repository};
use support::{grit_file_from_content, isolated_env};
use tempfile::tempdir;

fn ctx(git_dir: &std::path::Path, env: &Environment) -> IncludeContext {
    IncludeContext {
        git_dir: Some(git_dir.to_path_buf()),
        cwd: env.cwd.clone(),
        pwd: env.pwd.clone(),
        env: std::sync::Arc::new(env.clone()),
        ..Default::default()
    }
}

#[test]
fn includeif_onbranch_matches_current_branch() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("feature/x".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str("[includeIf \"onbranch:feature/x\"]\n\tpath = branch.conf\n");
    fs::write(git_dir.join("config"), &cfg).expect("write cfg");
    fs::write(git_dir.join("branch.conf"), "[hit]\n\tk = yes\n").expect("branch");

    let env = Environment::empty();
    let set = ConfigSet::load(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("hit.k").unwrap().as_deref(), Some("yes"));
}

#[test]
fn includeif_hasconfig_remote_url() {
    let dir = tempdir().expect("tempdir");
    Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str(
        "[remote \"origin\"]\n\turl = https://example.com/proj.git\n\
         [includeIf \"hasconfig:remote.*.url:https://example.com/*\"]\n\tpath = remote.conf\n",
    );
    fs::write(git_dir.join("config"), &cfg).expect("write cfg");
    fs::write(git_dir.join("remote.conf"), "[fromremote]\n\tk = 1\n").expect("remote inc");

    let env = Environment::empty();
    let set = ConfigSet::load(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("fromremote.k").unwrap().as_deref(), Some("1"));
}

#[test]
fn hasconfig_include_rejects_remote_url_in_included_file() {
    let dir = tempdir().expect("tempdir");
    let main = dir.path().join("main.conf");
    fs::write(
        &main,
        "[includeIf \"hasconfig:remote.*.url:https://x/*\"]\n\tpath = bad.conf\n",
    )
    .expect("main");
    fs::write(
        dir.path().join("bad.conf"),
        "[remote \"r\"]\n\turl = https://evil.example/x\n",
    )
    .expect("bad");

    let content = fs::read_to_string(&main).expect("read");
    let file = grit_file_from_content(&main, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    let err = set
        .merge_file_with_includes(&file, true, &ctx(dir.path(), &env))
        .expect_err("remote url in hasconfig include");
    assert!(matches!(
        err,
        Error::Config(ConfigError::RemoteUrlInHasconfigInclude)
    ));
}

const WORK_URL_PATTERN: &str = "https://github.com/work-org/**";
const PERSONAL_EMAIL: &str = "me@personal.example";
const WORK_EMAIL: &str = "me@work.example";

/// A global config that picks a work identity by remote URL, and an empty repository.
struct HasconfigFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    global: PathBuf,
    repo: PathBuf,
    git_dir: PathBuf,
}

impl HasconfigFixture {
    fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical tempdir");
        let home = root.join("home");
        fs::create_dir_all(&home).expect("home");
        let work = root.join("work.gitconfig");
        fs::write(&work, format!("[user]\n\temail = {WORK_EMAIL}\n")).expect("work");
        let global = root.join("gitconfig");
        fs::write(
            &global,
            format!(
                "[user]\n\tname = Me\n\temail = {PERSONAL_EMAIL}\n\
                 [includeIf \"hasconfig:remote.*.url:{WORK_URL_PATTERN}\"]\n\tpath = {}\n",
                work.display()
            ),
        )
        .expect("global");
        let repo = root.join("proj");
        Repository::init(
            &repo,
            &InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
        )
        .expect("init");
        let git_dir = repo.join(".git");
        Self {
            _dir: dir,
            root,
            home,
            global,
            repo,
            git_dir,
        }
    }

    fn append_local(&self, text: &str) {
        let path = self.git_dir.join("config");
        let mut cfg = fs::read_to_string(&path).expect("read local");
        cfg.push_str(text);
        fs::write(&path, cfg).expect("write local");
    }

    fn add_remote(&self, name: &str, url: &str) {
        self.append_local(&format!("[remote \"{name}\"]\n\turl = {url}\n"));
    }

    fn env(&self) -> Environment {
        let mut env = support::env_with_global(&self.home, &self.global);
        env.cwd = self.repo.clone();
        env.pwd = Some(self.repo.display().to_string());
        env
    }

    fn grit_load(&self, env: &Environment) -> grit_lib::error::Result<ConfigSet> {
        ConfigSet::load(
            env,
            Some(&self.git_dir),
            &support::default_load_opts(&self.git_dir, env),
        )
    }

    /// `git config --get <key>` in the repository: `Ok(value)` or `Err(stderr)` on a fatal error.
    fn git_get(&self, key: &str, extra_env: &[(&str, &str)]) -> Result<Option<String>, String> {
        let mut cmd = std::process::Command::new("git");
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_CONFIG",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            cmd.env_remove(var);
        }
        cmd.current_dir(&self.repo)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", &self.global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(extra_env.iter().copied())
            .args(["config", "--get", key]);
        let out = cmd.output().expect("spawn git config");
        match out.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim_end().to_owned(),
            )),
            Some(1) => Ok(None),
            _ => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        }
    }

    /// Assert grit and system git agree on `user.email` and that it equals `expected`.
    fn assert_email(&self, env: &Environment, git_env: &[(&str, &str)], expected: &str) {
        let git = self.git_get("user.email", git_env).expect("git config");
        assert_eq!(git.as_deref(), Some(expected), "system git user.email");
        let set = self.grit_load(env).expect("grit load");
        assert_eq!(
            set.get("user.email").unwrap().as_deref(),
            Some(expected),
            "grit user.email"
        );
    }
}

fn is_remote_url_in_hasconfig_error(err: &Error) -> bool {
    matches!(err, Error::Config(ConfigError::RemoteUrlInHasconfigInclude))
}

#[test]
fn hasconfig_in_global_sees_remote_defined_in_repo_config() {
    let fx = HasconfigFixture::new();
    fx.assert_email(&fx.env(), &[], PERSONAL_EMAIL);
    fx.add_remote("origin", "https://github.com/work-org/app.git");
    fx.assert_email(&fx.env(), &[], WORK_EMAIL);
}

#[test]
fn hasconfig_include_applies_in_place_so_later_local_email_wins() {
    let fx = HasconfigFixture::new();
    fx.add_remote("origin", "https://github.com/work-org/app.git");
    fx.append_local("[user]\n\temail = me@local.example\n");
    fx.assert_email(&fx.env(), &[], "me@local.example");
}

#[test]
fn hasconfig_skips_include_when_no_remote_url_matches() {
    let fx = HasconfigFixture::new();
    fx.add_remote("origin", "https://github.com/personal/app.git");
    fx.add_remote("fork", "https://gitlab.com/work-org/app.git");
    fx.assert_email(&fx.env(), &[], PERSONAL_EMAIL);
}

#[test]
fn hasconfig_matches_any_remote_url_value_not_only_the_last() {
    let fx = HasconfigFixture::new();
    fx.append_local(
        "[remote \"origin\"]\n\turl = https://github.com/work-org/app.git\n\
         \turl = https://mirror.example/app.git\n",
    );
    fx.add_remote("zzz", "https://other.example/x.git");
    fx.assert_email(&fx.env(), &[], WORK_EMAIL);
}

#[test]
fn hasconfig_sees_remote_url_from_command_line_parameters() {
    let fx = HasconfigFixture::new();
    let mut env = fx.env();
    env.git_config_parameters =
        Some("'remote.cli.url'='https://github.com/work-org/cli.git'".into());
    fx.assert_email(
        &env,
        &[(
            "GIT_CONFIG_PARAMETERS",
            "'remote.cli.url'='https://github.com/work-org/cli.git'",
        )],
        WORK_EMAIL,
    );
}

#[test]
fn hasconfig_sees_remote_url_from_git_config_count() {
    let fx = HasconfigFixture::new();
    let mut env = fx.env();
    env.git_config_count = Some("1".into());
    env.git_config_pairs = vec![(
        "remote.cli.url".into(),
        "https://github.com/work-org/cli.git".into(),
    )];
    fx.assert_email(
        &env,
        &[
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "remote.cli.url"),
            ("GIT_CONFIG_VALUE_0", "https://github.com/work-org/cli.git"),
        ],
        WORK_EMAIL,
    );
}

#[test]
fn hasconfig_sees_remote_url_from_plain_include_in_later_file() {
    let fx = HasconfigFixture::new();
    let remotes = fx.root.join("remotes.gitconfig");
    fs::write(
        &remotes,
        "[remote \"origin\"]\n\turl = https://github.com/work-org/app.git\n",
    )
    .expect("remotes");
    fx.append_local(&format!("[include]\n\tpath = {}\n", remotes.display()));
    fx.assert_email(&fx.env(), &[], WORK_EMAIL);
}

#[test]
fn hasconfig_rejects_remote_url_in_any_hasconfig_include_even_without_match() {
    let fx = HasconfigFixture::new();
    let bad = fx.root.join("bad.gitconfig");
    fs::write(&bad, "[remote \"evil\"]\n\turl = https://evil.example/x\n").expect("bad");
    fx.append_local(&format!(
        "[includeIf \"hasconfig:remote.*.url:https://never.example/**\"]\n\tpath = {}\n",
        bad.display()
    ));

    let git = fx.git_get("user.email", &[]).expect_err("system git dies");
    assert!(git.contains("remote URLs cannot be configured"), "{git}");
    let err = fx.grit_load(&fx.env()).expect_err("grit rejects");
    assert!(is_remote_url_in_hasconfig_error(&err), "{err:?}");
}

#[test]
fn hasconfig_rejects_remote_url_in_conditional_include_once_evaluated() {
    let fx = HasconfigFixture::new();
    let by_dir = fx.root.join("bydir.gitconfig");
    fs::write(
        &by_dir,
        "[remote \"origin\"]\n\turl = https://github.com/work-org/app.git\n",
    )
    .expect("bydir");
    fx.append_local(&format!(
        "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
        fx.repo.display(),
        by_dir.display()
    ));

    let git = fx.git_get("user.email", &[]).expect_err("system git dies");
    assert!(git.contains("remote URLs cannot be configured"), "{git}");
    let err = fx.grit_load(&fx.env()).expect_err("grit rejects");
    assert!(is_remote_url_in_hasconfig_error(&err), "{err:?}");
}

#[test]
fn hasconfig_single_file_merge_scans_only_that_file() {
    let fx = HasconfigFixture::new();
    fx.add_remote("origin", "https://github.com/work-org/app.git");
    let global_arg = fx.global.display().to_string();
    let git = support::git_file_get_includes_with_home(
        &fx.global,
        "user.email",
        Some(&fx.git_dir),
        Some(&fx.home),
    );
    assert_eq!(
        git.as_deref(),
        Some(PERSONAL_EMAIL),
        "git config -f {global_arg}"
    );

    let content = fs::read_to_string(&fx.global).expect("read global");
    let file = grit_file_from_content(&fx.global, &content, ConfigScope::Global);
    let mut set = ConfigSet::new();
    let env = fx.env();
    set.merge_file_with_includes(&file, true, &ctx(&fx.git_dir, &env))
        .expect("merge");
    assert_eq!(
        set.get("user.email").unwrap().as_deref(),
        Some(PERSONAL_EMAIL)
    );
}

#[test]
fn hasconfig_early_and_protected_config_scan_their_own_layers() {
    let fx = HasconfigFixture::new();
    fx.add_remote("origin", "https://github.com/work-org/app.git");
    let env = fx.env();

    let early = ConfigSet::read_early_config(&env, Some(&fx.git_dir), "user.email").expect("early");
    assert_eq!(early.last().map(String::as_str), Some(WORK_EMAIL));

    let protected = ConfigSet::load(
        &env,
        None,
        &crate::config::LoadConfigOptions::protected(&env, false),
    )
    .expect("protected");
    assert_eq!(
        protected.get("user.email").unwrap().as_deref(),
        Some(PERSONAL_EMAIL)
    );

    let mut env = env;
    env.git_config_parameters =
        Some("'remote.cli.url'='https://github.com/work-org/cli.git'".into());
    let protected = ConfigSet::load(
        &env,
        None,
        &crate::config::LoadConfigOptions::protected(&env, false),
    )
    .expect("protected with -c");
    assert_eq!(
        protected.get("user.email").unwrap().as_deref(),
        Some(WORK_EMAIL)
    );
}

#[test]
fn commit_identity_follows_hasconfig_selected_email() {
    use grit_lib::config::environment::RepositoryOptions;
    use grit_lib::config::ident_resolve::IdentRole;

    let fx = HasconfigFixture::new();
    fx.add_remote("origin", "https://github.com/work-org/app.git");
    let repo = Repository::open(&fx.git_dir, &RepositoryOptions::with_environment(fx.env()))
        .expect("open");
    let author = repo
        .identity(IdentRole::Author, repo.identity_now())
        .expect("author");
    assert_eq!(author.email, WORK_EMAIL);
    assert_eq!(author.name, "Me");
}

#[test]
fn parse_rejects_bad_lines_and_negotiation_algorithm() {
    use grit_lib::config::ConfigFile;

    let err = ConfigFile::parse(
        std::path::Path::new("c"),
        "[s]\n\tfetch.negotiationalgorithm\n",
        ConfigScope::Local,
    )
    .expect_err("missing value");
    assert!(matches!(err, Error::Verbatim(_) | Error::Config(_)));

    let err = ConfigFile::parse(
        std::path::Path::new("c"),
        "[s]\n\tkey = \"unclosed\n",
        ConfigScope::Local,
    )
    .expect_err("unclosed quote");
    assert!(matches!(
        err,
        Error::Config(ConfigError::BadConfigLineInFile { .. })
    ));

    let file = ConfigFile::parse(
        std::path::Path::new("c"),
        "[inline \"sub\"]\n\tinside = 1\n",
        ConfigScope::Local,
    )
    .expect("inline section key");
    assert_eq!(file.get("inline.sub.inside").unwrap().as_deref(), Some("1"));
}

#[test]
fn includeif_gitdir_icase_matches() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let repo = home.join("Repo");
    Repository::init(
        &repo,
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )
    .expect("init");
    let git_dir = repo.join(".git");
    let pattern = format!("gitdir/i:{}/", repo.display());
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str(&format!("[includeIf \"{pattern}\"]\n\tpath = icase.conf\n"));
    fs::write(git_dir.join("config"), &cfg).expect("cfg");
    fs::write(git_dir.join("icase.conf"), "[icase]\n\tk = 1\n").expect("inc");

    let mut env = isolated_env(&home);
    env.cwd = repo.clone();
    env.pwd = Some(repo.display().to_string());
    let set = ConfigSet::load(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("icase.k").unwrap().as_deref(), Some("1"));
}
