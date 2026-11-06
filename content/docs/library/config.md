---
title: Config
summary: Merged config reads and scoped writes through Repository::config and ConfigEditor.
---

Configuration in grit-lib follows Git’s cascade (system, global, local, worktree, and environment overrides). Use [`Repository::config`](rustdoc:grit_lib::repo::Repository::config) for reads and for writes that target a scope in an open repository. For global, system, or `-f` file edits outside a repository, use [`ConfigEditor`](rustdoc:grit_lib::config::ConfigEditor).

## Loading outside a handle

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) parses the cascade from disk given an [`Environment`](rustdoc:grit_lib::config::environment::Environment) and optional git directory. The library does not read `std::env` during load — embedders pass variables explicitly (see [Embedding](../embedding/) and [Repository](../repository/)).

## Typed values

[`ConfigSet::get_bool`](rustdoc:grit_lib::config::ConfigSet::get_bool) and [`ConfigSet::get_i64`](rustdoc:grit_lib::config::ConfigSet::get_i64) return `None` for an unset key. A value of the wrong type is a [`ConfigError::InvalidValue`](rustdoc:grit_lib::error::ConfigError) carrying the key and a [`ConfigValueError`](rustdoc:grit_lib::error::ConfigValueError) that says what was wrong: not a boolean, an empty or malformed integer, an integer that overflows `i64` once its `k`/`m`/`g` suffix is applied, or a bad color. `get_bool(key).transpose()?` reads an optional boolean and propagates a bad value as `grit_lib::error::Error::Config`. The key-less parsers [`parse_bool`](rustdoc:grit_lib::config::parse_bool), [`parse_i64`](rustdoc:grit_lib::config::parse_i64) and [`parse_color`](rustdoc:grit_lib::config::parse_color) return the bare `ConfigValueError`; call `for_key` on it to attach the key.

Most boolean settings fall back to Git's default when the key is unset or not a boolean. [`ConfigSet::bool_or`](rustdoc:grit_lib::config::ConfigSet::bool_or) reads them in one call: `config.bool_or("commit.autoStage", true)`. The `core` booleans that many operations consult have accessors carrying Git's default: [`ignore_case`](rustdoc:grit_lib::config::ConfigSet::ignore_case) and [`precompose_unicode`](rustdoc:grit_lib::config::ConfigSet::precompose_unicode) (both `false`), and [`preload_index`](rustdoc:grit_lib::config::ConfigSet::preload_index) and [`file_mode`](rustdoc:grit_lib::config::ConfigSet::file_mode) (both `true`).

## Errors

Every config failure is a specific [`ConfigError`](rustdoc:grit_lib::error::ConfigError) variant with the offending key or value in its fields, so callers match on the variant instead of the message. For example, [`canonical_key`](rustdoc:grit_lib::config::canonical_key) returns `ConfigError::InvalidKey` with an [`InvalidKeyReason`](rustdoc:grit_lib::error::InvalidKeyReason) naming the malformed part. Bad `GIT_CONFIG_COUNT` or `GIT_CONFIG_PARAMETERS` input is `InvalidEnvironmentCount`, `MissingEnvironmentKey` or `MalformedConfigParameters`. A value outside a key's fixed word list (`protocol.version`, `gpg.format`) is `UnknownValue`, and a `--local` or `--worktree` location without a repository is `RepositoryRequired`. Submodule git-dir layout problems are not config errors. They are reported as `Error::Submodule` with a [`SubmoduleError`](rustdoc:grit_lib::error::SubmoduleError).

## Writes

Pass a [`ConfigScope`](rustdoc:grit_lib::config::ConfigScope) (`Local`, `Worktree`, `Global`, or `System`) and [`ConfigWriteOptions`](rustdoc:grit_lib::config::ConfigWriteOptions) to [`Config::set`](rustdoc:grit_lib::config::Config::set), [`Config::add_value`](rustdoc:grit_lib::config::Config::add_value), [`Config::unset`](rustdoc:grit_lib::config::Config::unset), [`Config::unset_all`](rustdoc:grit_lib::config::Config::unset_all), [`Config::rename_section`](rustdoc:grit_lib::config::Config::rename_section), or [`Config::remove_section`](rustdoc:grit_lib::config::Config::remove_section). Successful writes reload the in-memory snapshot on the open `Repository`. `ConfigWriteOptions` carries dry-run mode, optional `--type` canonicalization, Git-style `replace-all` with an optional value pattern, and unset value patterns instead of bare boolean flags.

---
title: Config
summary: Merged config reads and scoped writes through Repository::config and ConfigEditor.
---

Configuration in grit-lib follows Git’s cascade (system, global, local, worktree, and environment overrides). Use [`Repository::config`](rustdoc:grit_lib::repo::Repository::config) for reads and for writes that target a scope in an open repository. For global, system, or `-f` file edits outside a repository, use [`ConfigEditor`](rustdoc:grit_lib::config::ConfigEditor).

## Loading outside a handle

[`ConfigSet::load`](rustdoc:grit_lib::config::ConfigSet) parses the cascade from disk given an [`Environment`](rustdoc:grit_lib::config::environment::Environment) and optional git directory. The library does not read `std::env` during load — embedders pass variables explicitly (see [Embedding](../embedding/) and [Repository](../repository/)).

## Typed values

[`ConfigSet::get_bool`](rustdoc:grit_lib::config::ConfigSet::get_bool) and [`ConfigSet::get_i64`](rustdoc:grit_lib::config::ConfigSet::get_i64) return `None` for an unset key. A value of the wrong type is a [`ConfigError::InvalidValue`](rustdoc:grit_lib::error::ConfigError) carrying the key and a [`ConfigValueError`](rustdoc:grit_lib::error::ConfigValueError) that says what was wrong: not a boolean, an empty or malformed integer, an integer that overflows `i64` once its `k`/`m`/`g` suffix is applied, or a bad color. `get_bool(key).transpose()?` reads an optional boolean and propagates a bad value as `grit_lib::error::Error::Config`. The key-less parsers [`parse_bool`](rustdoc:grit_lib::config::parse_bool), [`parse_i64`](rustdoc:grit_lib::config::parse_i64) and [`parse_color`](rustdoc:grit_lib::config::parse_color) return the bare `ConfigValueError`; call `for_key` on it to attach the key.

Most boolean settings fall back to Git's default when the key is unset or not a boolean. [`ConfigSet::bool_or`](rustdoc:grit_lib::config::ConfigSet::bool_or) reads them in one call: `config.bool_or("commit.autoStage", true)`. The `core` booleans that many operations consult have accessors carrying Git's default: [`ignore_case`](rustdoc:grit_lib::config::ConfigSet::ignore_case) and [`precompose_unicode`](rustdoc:grit_lib::config::ConfigSet::precompose_unicode) (both `false`), and [`preload_index`](rustdoc:grit_lib::config::ConfigSet::preload_index) and [`file_mode`](rustdoc:grit_lib::config::ConfigSet::file_mode) (both `true`).

## Errors

Every config failure is a specific [`ConfigError`](rustdoc:grit_lib::error::ConfigError) variant with the offending key or value in its fields, so callers match on the variant instead of the message. For example, [`canonical_key`](rustdoc:grit_lib::config::canonical_key) returns `ConfigError::InvalidKey` with an [`InvalidKeyReason`](rustdoc:grit_lib::error::InvalidKeyReason) naming the malformed part. Bad `GIT_CONFIG_COUNT` or `GIT_CONFIG_PARAMETERS` input is `InvalidEnvironmentCount`, `MissingEnvironmentKey` or `MalformedConfigParameters`. A value outside a key's fixed word list (`protocol.version`, `gpg.format`) is `UnknownValue`, and a `--local` or `--worktree` location without a repository is `RepositoryRequired`. Submodule git-dir layout problems are not config errors. They are reported as `Error::Submodule` with a [`SubmoduleError`](rustdoc:grit_lib::error::SubmoduleError).

## Writes

Pass a [`ConfigScope`](rustdoc:grit_lib::config::ConfigScope) (`Local`, `Worktree`, `Global`, or `System`) and [`ConfigWriteOptions`](rustdoc:grit_lib::config::ConfigWriteOptions) to [`Config::set`](rustdoc:grit_lib::config::Config::set), [`Config::add_value`](rustdoc:grit_lib::config::Config::add_value), [`Config::unset`](rustdoc:grit_lib::config::Config::unset), [`Config::unset_all`](rustdoc:grit_lib::config::Config::unset_all), [`Config::rename_section`](rustdoc:grit_lib::config::Config::rename_section), or [`Config::remove_section`](rustdoc:grit_lib::config::Config::remove_section). Successful writes reload the in-memory snapshot on the open `Repository`. `ConfigWriteOptions` carries dry-run mode, optional `--type` canonicalization, Git-style `replace-all` with an optional value pattern, and unset value patterns instead of bare boolean flags.

Without an open `Repository`, [`ConfigEditor::global`](rustdoc:grit_lib::config::ConfigEditor::global), [`ConfigEditor::system`](rustdoc:grit_lib::config::ConfigEditor::system), or [`ConfigEditor::open`](rustdoc:grit_lib::config::ConfigEditor::open) edit one file. For local or worktree config tied to a path you discover yourself, call [`Repository::discover`](rustdoc:grit_lib::repo::Repository) (or [`Repository::open`](rustdoc:grit_lib::repo::Repository) when you already know the git directory) and use [`Repository::config`](rustdoc:grit_lib::repo::Repository::config) with the right [`ConfigScope`](rustdoc:grit_lib::config::ConfigScope) (`Local` or `Worktree`). The `grit config` command follows the same split: it edits with `ConfigEditor` when there is no repository, and `Repository::config` when one is open.

Writes go through `config.lock` and an atomic rename and keep the file's permission bits. Like Git, they do not fsync the file, so a crash may lose the update, but readers never see a partial file.

## Example

<!-- include: grit-examples/src/bin/guide_config.rs -->

```bash
cargo run --bin guide_config /path/to/repo
git -C /path/to/repo config user.name
```

The integration test checks the printed `user.name` against `git config user.name`.

Writes go through `config.lock` and an atomic rename and keep the file's permission bits. Like Git, they do not fsync the file, so a crash may lose the update, but readers never see a partial file.

## Example

<!-- include: grit-examples/src/bin/guide_config.rs -->

```bash
cargo run --bin guide_config /path/to/repo
git -C /path/to/repo config user.name
```

The integration test checks the printed `user.name` against `git config user.name`.
