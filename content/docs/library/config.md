---
title: Config
summary: Merged config reads and repository-local writes through Repository::config().
---

Configuration in grit-lib follows Git’s cascade (system, global, local, worktree, and environment overrides). Use [`Repository::config`](rustdoc:grit_lib::repo::Repository::config) for hot paths: it returns [`Config`](rustdoc:grit_lib::config::Config), which reads the merged snapshot via [`Config::snapshot`](rustdoc:grit_lib::config::Config::snapshot) and can update the repository-local file.

## Loading outside a handle

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet::load) parses the cascade from disk given an [`Environment`](rustdoc:grit_lib::config::environment::Environment), an optional git directory, and [`LoadConfigOptions`](rustdoc:grit_lib::config::LoadConfigOptions). Helpers such as [`LoadConfigOptions::standard`](rustdoc:grit_lib::config::LoadConfigOptions::standard), [`LoadConfigOptions::repo_local_only`](rustdoc:grit_lib::config::LoadConfigOptions::repo_local_only), and [`LoadConfigOptions::protected`](rustdoc:grit_lib::config::LoadConfigOptions::protected) select Git’s usual scopes. The library does not read `std::env` during load — embedders pass variables explicitly (see [Embedding](../embedding/) and [Repository](../repository/)).

## Typed values

On both [`Config`](rustdoc:grit_lib::config::Config) and [`ConfigSet`](rustdoc:grit_lib::config::ConfigSet), typed getters share one shape: `get`, `get_all`, `get_bool`, `get_i64`, `get_path`, `get_color`, and `entries_with_prefix` return `Result<Option<T>, ConfigError>` (or `Result<Vec<_>, ConfigError>` for `get_all`). An unset key is `Ok(None)`; a malformed key or value is `Err(ConfigError::…)`.

Most boolean settings fall back to Git's default when the key is unset or not a boolean. Domain accessors on `ConfigSet` (for example [`ignore_case`](rustdoc:grit_lib::config::ConfigSet::ignore_case) and [`file_mode`](rustdoc:grit_lib::config::ConfigSet::file_mode)) encode those defaults.

## Errors

Every config failure is a specific [`ConfigError`](rustdoc:grit_lib::error::ConfigError) variant with the offending key or value in its fields, so callers match on the variant instead of the message. For example, [`canonical_key`](rustdoc:grit_lib::config::canonical_key) returns `ConfigError::InvalidKey` with an [`InvalidKeyReason`](rustdoc:grit_lib::error::InvalidKeyReason) naming the malformed part. Bad `GIT_CONFIG_COUNT` or `GIT_CONFIG_PARAMETERS` input is `InvalidEnvironmentCount`, `MissingEnvironmentKey` or `MalformedConfigParameters`. A value outside a key's fixed word list (`protocol.version`, `gpg.format`) is `UnknownValue`, and a `--local` or `--worktree` location without a repository is `RepositoryRequired`. Submodule git-dir layout problems are not config errors. They are reported as `Error::Submodule` with a [`SubmoduleError`](rustdoc:grit_lib::error::SubmoduleError).

## Local writes

[`Config::set_local`](rustdoc:grit_lib::config::Config::set_local) writes one key to the repository-local config (or `commondir/config` for linked worktrees without a per-worktree config file), then reloads the in-memory snapshot on the open `Repository`.

Without an open `Repository`, call [`Repository::discover`](rustdoc:grit_lib::repo::Repository) (or [`Repository::open`](rustdoc:grit_lib::repo::Repository) when you already know the git directory), pass [`Repository::git_dir`](rustdoc:grit_lib::repo::Repository) to [`set_config_value`](rustdoc:grit_lib::config::set_config_value) with `ConfigLocation::Local`, then reload config on a handle if you need the merged cascade. The `grit config` command discovers a repository the same way before editing local config.

Writes go through `config.lock` and an atomic rename and keep the file's permission bits. Like Git, they do not fsync the file, so a crash may lose the update, but readers never see a partial file.

## Example

<!-- include: grit-examples/src/bin/guide_config.rs -->

```bash
cargo run --bin guide_config /path/to/repo
git -C /path/to/repo config user.name
```

The integration test checks the printed `user.name` against `git config user.name`.
