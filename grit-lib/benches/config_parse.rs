//! Config parsing and per-call config validation on an open repository.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::config::{canonical_key, ConfigFile, ConfigScope};
use grit_lib::repo::{InitOptions, Repository};

/// A user-level config of the size people carry around: identity, aliases, URL rewrites,
/// filters, and a few quoted and commented values.
const GLOBAL_CONFIG: &str = r#"[user]
	name = A U Thor
	email = author@example.com
	signingKey = ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPqiJsBjAsv4KymedFcUR891X1lgC90DW8yMtjcHJ
[alias]
	co = checkout
	st = status
	lg = "log --graph --pretty=format:'%h %s' --abbrev-commit" ; pretty log
	unstage = reset HEAD --
[core]
	editor = vim
	autocrlf = input
	excludesFile = ~/.gitignore_global # personal ignores
[push]
	autoSetupRemote = true
	default = simple
[pull]
	rebase = true
[fetch]
	prune = true
[init]
	defaultBranch = main
[gpg]
	format = ssh
[commit]
	gpgsign = true
[url "https://github.com/"]
	insteadOf = git@github.com:
	insteadOf = ssh://git@github.com/
	insteadOf = git+ssh://git@github.com/
[filter "lfs"]
	required = true
	clean = git-lfs clean -- %f
	smudge = git-lfs smudge -- %f
	process = git-lfs filter-process
[color]
	ui = auto
[diff]
	algorithm = histogram
	colorMoved = default
[merge]
	conflictStyle = zdiff3
[rerere]
	enabled = true
"#;

fn config_parse(c: &mut Criterion) {
    let path = Path::new("/home/user/.gitconfig");
    c.bench_function("config_parse/global_40_entries", |b| {
        b.iter(|| {
            ConfigFile::parse(path, black_box(GLOBAL_CONFIG), ConfigScope::Global).expect("parse")
        });
    });
    c.bench_function("config_parse/canonical_key", |b| {
        b.iter(|| {
            canonical_key(black_box("Branch.Feature-X.Remote")).expect("key");
            canonical_key(black_box("core.bare")).expect("key")
        });
    });
}

fn repository_config_access(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let config_path = repo.git_dir().join("config");
    let mut local = std::fs::read_to_string(&config_path).expect("read config");
    local.push_str(GLOBAL_CONFIG);
    std::fs::write(&config_path, local).expect("write config");
    repo.config().expect("first load");
    c.bench_function("repository_config/revalidate", |b| {
        b.iter(|| black_box(repo.config().expect("config")));
    });
}

criterion_group!(benches, config_parse, repository_config_access);
criterion_main!(benches);
