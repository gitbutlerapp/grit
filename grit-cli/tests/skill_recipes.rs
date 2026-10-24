//! Run `grit skill` recipe blocks from `skill.md` against fixtures (CI gate).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use grit_test_support::{git, git_fsck};
use serde_json::Value;

const SKILL: &str = include_str!("../src/commands/skill.md");
const GS: &str = env!("CARGO_BIN_EXE_grit");

#[derive(Debug, Clone)]
struct RecipeStep {
    exit_code: i32,
    fields: Vec<String>,
    command: String,
}

#[derive(Debug)]
struct Recipe {
    id: String,
    steps: Vec<RecipeStep>,
}

#[test]
fn skill_recipes_run() {
    let recipes = parse_recipes(SKILL);
    assert_eq!(
        recipes.len(),
        5,
        "expected five recipes, got: {:?}",
        recipes.iter().map(|r| &r.id).collect::<Vec<_>>()
    );
    for recipe in recipes {
        run_recipe(&recipe);
    }
}

fn parse_recipes(skill: &str) -> Vec<Recipe> {
    let mut recipes = Vec::new();
    let mut search = skill;
    while let Some(start) = search.find("```console recipe:") {
        let after = &search[start + "```console recipe:".len()..];
        let id_end = after.find('\n').unwrap_or(after.len());
        let id = after[..id_end].trim().to_owned();
        let body_start = id_end + 1;
        let body_end = after[body_start..]
            .find("```")
            .expect("unclosed recipe fence");
        let body = &after[body_start..body_start + body_end];
        recipes.push(parse_recipe_body(id, body));
        search = &after[body_start + body_end + 3..];
    }
    recipes
}

fn parse_recipe_body(id: String, body: &str) -> Recipe {
    let mut steps = Vec::new();
    let mut pending_exit = 0_i32;
    let mut pending_fields = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(meta) = trimmed.strip_prefix("<!-- step exit=") {
            let rest = meta.strip_suffix("-->").unwrap_or(meta).trim();
            let (code, fields) = rest.split_once(" fields=").unwrap_or((rest, ""));
            pending_exit = code.trim().parse().expect("exit code");
            pending_fields = if fields.is_empty() {
                Vec::new()
            } else {
                fields
                    .split(',')
                    .map(str::trim)
                    .map(str::to_owned)
                    .collect()
            };
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let cmd = trimmed
            .strip_prefix("$ ")
            .or_else(|| trimmed.strip_prefix("$"))
            .unwrap_or(trimmed)
            .trim()
            .to_owned();
        if cmd.starts_with("grit ") {
            steps.push(RecipeStep {
                exit_code: pending_exit,
                fields: pending_fields.clone(),
                command: cmd,
            });
            pending_fields.clear();
        }
    }
    Recipe { id, steps }
}

fn run_recipe(recipe: &Recipe) {
    match recipe.id.as_str() {
        "precise-commit" => run_precise_commit(recipe),
        "merge-conflict" => run_merge_conflict(recipe),
        "rebase-push-lease" => run_rebase_push_lease(recipe),
        "fetch-pr" => run_fetch_pr(recipe),
        "shallow-clone" => run_shallow_clone(recipe),
        other => panic!("unknown recipe id {other}"),
    }
}

struct RecipeCtx {
    cwd: PathBuf,
    vars: Vec<(String, String)>,
    repos_to_fsck: Vec<PathBuf>,
}

impl RecipeCtx {
    fn grit(&self, command: &str) -> Output {
        let args = expand_command(command, &self.vars);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        Command::new(GS)
            .args(&argv[1..])
            .current_dir(&self.cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("grit")
    }

    fn run_step(&mut self, step: &RecipeStep) {
        let out = self.grit(&step.command);
        assert_eq!(
            out.status.code(),
            Some(step.exit_code),
            "recipe step failed: {}\ncwd={}\nstdout={}\nstderr={}",
            step.command,
            self.cwd.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if step.command.contains("--json") {
            let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
                panic!(
                    "json parse {}: {e}\nstdout={}",
                    step.command,
                    String::from_utf8_lossy(&out.stdout)
                )
            });
            for field in &step.fields {
                assert_json_field(&v, field, step.exit_code, &step.command);
            }
            if step.fields.iter().any(|f| f.contains("hunks.0.id")) {
                let id = v
                    .pointer("/files/0/hunks/0/id")
                    .and_then(Value::as_str)
                    .expect("hunk id");
                self.vars.push(("HUNK0".into(), id.to_owned()));
            }
        }
    }
}

fn expand_command(command: &str, vars: &[(String, String)]) -> Vec<String> {
    let mut line = command.to_owned();
    for (name, value) in vars {
        line = line.replace(&format!("${name}"), value);
    }
    shell_words::split(&line).expect("tokenize")
}

fn assert_json_field(v: &Value, field: &str, exit_code: i32, command: &str) {
    let pointer = field.split('.').fold(String::new(), |mut p, seg| {
        p.push('/');
        p.push_str(seg);
        p
    });
    let at = v
        .pointer(&pointer)
        .unwrap_or_else(|| panic!("missing json field `{field}` in {command}: {v}"));
    if field == "rejected" {
        let expect = exit_code != 0;
        assert_eq!(
            at.as_bool(),
            Some(expect),
            "{command}: rejected expected {expect}"
        );
        return;
    }
    assert!(
        !at.is_null(),
        "json field `{field}` is null in {command}: {v}"
    );
}

fn fsck_all(paths: &[PathBuf]) {
    for path in paths {
        let outcome = git_fsck(path, true);
        assert!(
            outcome.ok,
            "git fsck --strict {}: {:?}",
            path.display(),
            outcome.msg_ids
        );
    }
}

fn run_precise_commit(recipe: &Recipe) {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    git(
        repo.parent().unwrap(),
        &["init", "-q", "-b", "main", repo.to_str().unwrap()],
    );
    fs::write(repo.join("partial.txt"), "one\ntwo\nthree\n").unwrap();
    fs::write(repo.join("partial-other.txt"), "alpha\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);

    fs::write(repo.join("partial.txt"), "one\nTWO\nTHREE\n").unwrap();
    fs::write(repo.join("partial-other.txt"), "beta\n").unwrap();

    let mut ctx = RecipeCtx {
        cwd: repo.clone(),
        vars: Vec::new(),
        repos_to_fsck: vec![repo.clone()],
    };

    for (i, step) in recipe.steps.iter().enumerate() {
        if i == 2 {
            fs::write(ctx.cwd.join("staged-only.txt"), "staged\n").unwrap();
            grit_ok(&ctx.cwd, &["add", "staged-only.txt"]);
        }
        if i == 3 {
            fs::write(ctx.cwd.join("partial-other.txt"), "gamma\n").unwrap();
            grit_ok(&ctx.cwd, &["add", "partial-other.txt"]);
        }
        ctx.run_step(step);
    }
    fsck_all(&ctx.repos_to_fsck);
}

fn run_merge_conflict(recipe: &Recipe) {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    git(
        base.path(),
        &["init", "-q", "-b", "main", repo.to_str().unwrap()],
    );
    fs::write(repo.join("conflict.txt"), "base\n").unwrap();
    grit_ok(&repo, &["add", "conflict.txt"]);
    grit_ok(&repo, &["commit", "-m", "base"]);
    grit_ok(&repo, &["switch", "--create", "topic"]);
    fs::write(repo.join("conflict.txt"), "topic\n").unwrap();
    grit_ok(&repo, &["commit", "-am", "topic change"]);
    grit_ok(&repo, &["switch", "main"]);
    fs::write(repo.join("conflict.txt"), "main\n").unwrap();
    grit_ok(&repo, &["commit", "-am", "main change"]);

    let mut ctx = RecipeCtx {
        cwd: repo.clone(),
        vars: Vec::new(),
        repos_to_fsck: vec![repo.clone()],
    };
    for step in &recipe.steps {
        ctx.run_step(step);
    }
    fsck_all(&ctx.repos_to_fsck);
}

fn run_rebase_push_lease(recipe: &Recipe) {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("origin.git");
    git(
        base.path(),
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let bare_url = format!("file://{}", bare.canonicalize().unwrap().display());

    let seed = base.path().join("seed");
    git(
        base.path(),
        &["clone", "-q", bare_url.as_str(), seed.to_str().unwrap()],
    );
    fs::write(seed.join("README"), "seed\n").unwrap();
    git(&seed, &["add", "README"]);
    git(&seed, &["commit", "-m", "initial"]);
    git(&seed, &["push", "-u", "origin", "main"]);

    fs::write(seed.join("feature.txt"), "work\n").unwrap();
    git(&seed, &["add", "feature.txt"]);
    git(&seed, &["commit", "-m", "feature"]);
    git(&seed, &["push", "origin", "main"]);

    git(&seed, &["commit", "--amend", "-m", "feature rewritten"]);

    let side = base.path().join("side");
    git(
        base.path(),
        &["clone", "-q", bare_url.as_str(), side.to_str().unwrap()],
    );
    fs::write(side.join("other.txt"), "other\n").unwrap();
    git(&side, &["add", "other.txt"]);
    git(&side, &["commit", "-m", "remote advance"]);
    git(&side, &["push", "origin", "main"]);

    let mut ctx = RecipeCtx {
        cwd: seed.clone(),
        vars: vec![("REMOTE_URL".into(), bare_url.clone())],
        repos_to_fsck: vec![seed.clone(), bare.clone(), side.clone()],
    };

    for (i, step) in recipe.steps.iter().enumerate() {
        if i == 3 {
            // Remote moved again after rebase; first --force-with-lease should reject.
            fs::write(side.join("race.txt"), "race\n").unwrap();
            git(&side, &["add", "race.txt"]);
            git(&side, &["commit", "-m", "race"]);
            git(&side, &["push", "origin", "main"]);
        }
        ctx.run_step(step);
    }
    fsck_all(&ctx.repos_to_fsck);
}

fn run_fetch_pr(recipe: &Recipe) {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("origin.git");
    git(
        base.path(),
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let seed = base.path().join("seed");
    git(
        base.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    fs::write(seed.join("pr.txt"), "pr\n").unwrap();
    git(&seed, &["add", "pr.txt"]);
    git(&seed, &["commit", "-m", "pr commit"]);
    git(&seed, &["push", "origin", "main"]);
    let head = git_out(&bare, &["rev-parse", "refs/heads/main"]);
    git(&bare, &["update-ref", "refs/pull/1/head", head.as_str()]);

    let client = base.path().join("client");
    git(
        base.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            client.to_str().unwrap(),
        ],
    );

    let mut ctx = RecipeCtx {
        cwd: client.clone(),
        vars: Vec::new(),
        repos_to_fsck: vec![client.clone(), bare.clone()],
    };
    for step in &recipe.steps {
        ctx.run_step(step);
    }
    fsck_all(&ctx.repos_to_fsck);
}

fn run_shallow_clone(recipe: &Recipe) {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("origin.git");
    git(
        base.path(),
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let seed = base.path().join("seed");
    git(
        base.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    for n in 0..3 {
        fs::write(seed.join(format!("file{n}.txt")), format!("v{n}\n")).unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", &format!("commit {n}")]);
    }
    git(&seed, &["push", "origin", "main"]);

    let bare_url = format!("file://{}", bare.canonicalize().unwrap().display());
    let clone_dir = base.path().join("shallow-work");

    let mut ctx = RecipeCtx {
        cwd: base.path().to_path_buf(),
        vars: vec![
            ("REMOTE_URL".into(), bare_url),
            ("CLONE_DIR".into(), clone_dir.to_str().unwrap().into()),
        ],
        repos_to_fsck: vec![bare.clone()],
    };

    for (i, step) in recipe.steps.iter().enumerate() {
        if i >= 1 {
            ctx.cwd = clone_dir.clone();
            ctx.repos_to_fsck.push(clone_dir.clone());
        }
        ctx.run_step(step);
    }
    fsck_all(&ctx.repos_to_fsck);
}

fn grit_ok(cwd: &Path, args: &[&str]) {
    let out = Command::new(GS)
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
