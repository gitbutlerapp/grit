---
title: Embedding grit-lib
summary: Run multiple repositories in one process with explicit Environment, sinks, subprocess injection, and typed errors.
---

Grit is designed to be **linked into other Rust programs**, not only invoked as the `grit` CLI. The library avoids hidden process globals: discovery variables, config identity, subprocesses, warnings, and wall-clock references are supplied through explicit types at the boundary.

## Environment

[`Environment`](rustdoc:grit_lib::config::environment::Environment) holds Git discovery and configuration variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_CONFIG_*`, home paths, author/committer overrides, transport trace flags, and related fields). Construct it with:

- [`Environment::capture_process`](rustdoc:grit_lib::config::environment::Environment::capture_process) — an explicit snapshot of the calling process (`HOME`, `GIT_*`, Windows `ProgramFiles`, identity variables, …). This is what the `grit` CLI uses, and what a tool working on a user's repositories should use. Call it in your program, never from library code.
- [`Environment::from_vars`](rustdoc:grit_lib::config::environment::Environment::from_vars) — parse an iterator of `(key, value)` pairs plus an explicit working directory (no reads from `std::env` inside the library).
- [`Environment::empty`](rustdoc:grit_lib::config::environment::Environment::empty) — no variables at all (`cwd = "."`). There is no home directory, so the global `~/.gitconfig` is not read, and on Windows the Git for Windows system gitconfig cannot be found. On Unix `/etc/gitconfig` still loads unless you set `git_config_nosystem`.

Pass the same `Environment` into `Repository` (`discover` / `open`) and into `ConfigSet` loading.

### Matching `git` and `grit` on user repositories

System and global config decide much of what a repository operation does: `core.autocrlf` (Git for Windows sets it to `true` in its system config), `user.name` / `user.email`, `safe.directory`, `core.excludesFile`, credential helpers. A program that should give the same answers as `git` and `grit` on the user's machine opens repositories with [`RepositoryOptions::from_process`](rustdoc:grit_lib::config::environment::RepositoryOptions::from_process):

```rust
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
```

With [`RepositoryOptions::empty`](rustdoc:grit_lib::config::environment::RepositoryOptions::empty) none of that config applies. For example, on Windows `status` then reports CRLF files that `git status` calls clean as modified. Use `empty()` for hermetic work such as tests or a repository your program created itself. The examples on these pages that open your repository use `from_process()`; the ones that create a throwaway repository (`guide_embedding` below, and `guide_index`, `guide_objects` and `guide_replay_conflicts` when run without a path) stay on `empty()`, and `guide_embedding` sets its home directory and disables the system config explicitly.

Clones share the captured variable map, avoiding a copy of every inherited variable during
repository setup. Typed fields remain independent: changing or clearing an override on a
clone leaves the original snapshot unchanged. Untyped variables, including non-Unicode
values, remain available to subprocesses after the original snapshot is dropped.
Empty snapshots need no allocation for the captured map.

## RepositoryOptions

[`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) bundles everything needed to open a repository besides paths:

| Field | Role |
| --- | --- |
| `environment` | Discovery/config/identity snapshot |
| `command_runner` | Injectable subprocess execution (hooks, filters, signing helpers) |
| `diagnostics` | `DiagnosticSink` for warnings and trace events |
| `network_trace` | When true, emit network trace events via the diagnostics sink |
| `reference_unix_time` | Wall clock for rev-parse date selectors (`@{yesterday}`, etc.) |
| Test knobs | `test_assume_different_owner`, … |

Construct options with one of the associated constructors, then chain the builder methods:

| Constructor | Environment |
| --- | --- |
| [`RepositoryOptions::from_process()`](rustdoc:grit_lib::config::environment::RepositoryOptions::from_process) | Snapshot of the calling process, like `git` and `grit` |
| [`RepositoryOptions::with_environment(env)`](rustdoc:grit_lib::config::environment::RepositoryOptions::with_environment) | An [`Environment`](rustdoc:grit_lib::config::environment::Environment) you built |
| [`RepositoryOptions::empty()`](rustdoc:grit_lib::config::environment::RepositoryOptions::empty) | [`Environment::empty`](rustdoc:grit_lib::config::environment::Environment::empty) |

`with_environment` takes the environment and returns new options, so write `RepositoryOptions::with_environment(env).with_command_runner(runner)`; `RepositoryOptions::empty().with_environment(env)` does not compile. Assign `diagnostics` (or call `with_diagnostics`) when you need a custom sink.

## DiagnosticSink and ProgressSink

Non-fatal conditions are `Warning` values delivered to a `DiagnosticSink`. The default is `NullDiagnostics`. Tests and embedders often use `CollectingDiagnostics` to assert warnings without parsing stderr.

Long-running work (status, staging and commit, stash, merge, rebase, checkout and switch, fetch, pack indexing) takes a [`ProgressSink`](rustdoc:grit_lib::progress::ProgressSink) and a [`Cancel`](rustdoc:grit_lib::progress::Cancel). Pass [`NullProgress`](rustdoc:grit_lib::progress::NullProgress) and [`NeverCancel`](rustdoc:grit_lib::progress::NeverCancel) when you don't want progress updates or interruption; see [Progress and cancellation](parallelism.md#progress-and-cancellation) for where a cancel lands. Progress is separate from diagnostics: progress reports throughput; diagnostics report Git-style warnings.

## CommandRunner

`CommandRunner` is the only production path that spawns OS processes from the library (`SystemCommandRunner`). Hooks, smudge/clean filters, credential helpers, and signing helpers build a `CommandSpec` and call `spawn`.

Shell commands use a `CommandSpec` whose program is `sh`. `SystemCommandRunner` runs that the way Git does: `/bin/sh` on Unix. On Windows it uses the `sh` of the Git for Windows install that owns the `git` on the child's `PATH` (`usr\bin`, then `bin`), then any `sh` on that `PATH`. Filters and helpers therefore work when only Git's `cmd` directory is on `PATH`.

For tests, [`RecordingRunner`](rustdoc:grit_lib::command_runner::RecordingRunner) records specs and returns scripted exit codes. Install it on [`RepositoryOptions`](rustdoc:grit_lib::config::environment::RepositoryOptions) before [`Repository::open`](rustdoc:grit_lib::repo::Repository) or [`Repository::discover`](rustdoc:grit_lib::repo::Repository).

## Typed errors

Library APIs return `grit_lib::error::Result`. Match on `Error` variants (`RevParse`, `RevList`, `HookError`, `FilterError`, …) instead of parsing `"fatal:"` strings. The CLI maps variants to exit codes and human messages in its own words.

Common conditions use dedicated variants rather than `Error::Message`: bare or missing work tree → [`Error::RequiresWorkTree`](rustdoc:grit_lib::error::Error::RequiresWorkTree); empty commit body → [`Error::EmptyCommitMessage`](rustdoc:grit_lib::error::Error::EmptyCommitMessage); path missing from a revision tree → [`RevParseError::PathNotInRevision`](rustdoc:grit_lib::revwalk::rev_parse_error::RevParseError::PathNotInRevision); parent navigation past root → [`RevParseError::RevisionOutOfRange`](rustdoc:grit_lib::revwalk::rev_parse_error::RevParseError::RevisionOutOfRange); pack deltas whose bases are in neither the pack nor the object database (a thin pack received without its bases) → [`Error::UnresolvedDeltas`](rustdoc:grit_lib::error::Error::UnresolvedDeltas); a `.idx` whose trailing checksum is wrong, on reads that verify it → [`Error::PackIndexChecksumMismatch`](rustdoc:grit_lib::error::Error::PackIndexChecksumMismatch).

## Code layout

Enable the `hot-text-layout` feature to have grit-lib's hot code path (the library, runtime and dependency functions its common operations run) placed contiguously in your binary:

```toml
[dependencies]
grit-lib = { version = "0.5", features = ["hot-text-layout"] }
```

A short operation then faults in a few contiguous code pages instead of windows scattered over the whole binary. `grit status` on 10,000 files drops from 14.2 to 11.5 MB of peak RSS with it, and nothing changes at run time. The feature acts only on `x86_64-unknown-linux-gnu`, where rustc links with LLD; elsewhere it does nothing. Set `GRIT_HOT_TEXT_LAYOUT=0` at build time if you link with another linker. See [Hot text layout](../benchmarking.md#hot-text-layout).

## Serializable results

Enable the `serde-models` feature to get `serde::Serialize` on the results a front end renders, so you don't write your own JSON types for them:

```toml
[dependencies]
grit-lib = { version = "0.5", features = ["serde-models"] }
```

The feature covers [`StatusModel`](rustdoc:grit_lib::porcelain::status::StatusModel) (including its paused [`Operation`](rustdoc:grit_lib::operation::Operation), state flags and conflict kinds; the loaded index is not serialized), [`DiffEntry`](rustdoc:grit_lib::diff::DiffEntry), [`LoadedCommit`](rustdoc:grit_lib::porcelain::log::LoadedCommit), and the outcomes of commit, switch, reset, branch listing, stash, fetch, push, history rewrites and rebase ([`CommitOutcome`](rustdoc:grit_lib::porcelain::commit::CommitOutcome), [`SwitchOutcome`](rustdoc:grit_lib::porcelain::switch::SwitchOutcome), [`ResetOutcome`](rustdoc:grit_lib::porcelain::reset::ResetOutcome), [`BranchList`](rustdoc:grit_lib::porcelain::branch::BranchList), [`StashEntry`](rustdoc:grit_lib::porcelain::stash::StashEntry), [`StashApplyOutcome`](rustdoc:grit_lib::porcelain::stash::StashApplyOutcome), [`FetchOutcome`](rustdoc:grit_lib::remote::transfer::FetchOutcome), [`PushOutcome`](rustdoc:grit_lib::remote::transfer::PushOutcome), [`RewritePlan`](rustdoc:grit_lib::revwalk::history::RewritePlan), [`RebaseOutcome`](rustdoc:grit_lib::revwalk::history::RebaseOutcome)). The shapes:

| Type | JSON |
| --- | --- |
| [`ObjectId`](rustdoc:grit_lib::objects::ObjectId), [`ChangeId`](rustdoc:grit_lib::change_id::ChangeId) | hex string |
| [`FileMode`](rustdoc:grit_lib::objects::FileMode) | octal string such as `"100644"`; `"000000"` for the missing side of an add or delete |
| [`Signature`](rustdoc:grit_lib::ident::Signature) | `{"name", "email", "date"}`, with `date` in RFC 3339 keeping the recorded offset |
| [`Error`](rustdoc:grit_lib::error::Error) | Always `kind` and `message` ([`Error::kind_name`](rustdoc:grit_lib::error::Error::kind_name)). Selected variants also expose structured fields: `LockHeld` → `path`, optional `holder` `{pid, running}`; `WouldOverwrite` → `reason`, `paths`; `StashCouldNotRestoreUntracked` → `path`; `DirtyWorktree` → `operation`, `paths`; `UnmergedConflictMarkers` → `paths` |
| enums | snake_case variant names (`"type_changed"`, `"merge"`); hook names use Git's spelling (`"pre-commit"`) |

`LoadedCommit` holds typed author and committer signatures. A commit whose identity line has a malformed date still loads, with the Unix epoch as its date, the way `git log` shows it ([`Signature::from_header_lossy`](rustdoc:grit_lib::ident::Signature::from_header_lossy)).

## Concurrency

Two or more `Repository` handles may be used from different threads in one process when each handle is opened with its **own** `Environment`, `RepositoryOptions`, diagnostics sink, and command runner. Repository-scoped caches (config, attributes, pack read caches) do not leak between handles.

Object writes are safe to race. Every loose object is staged in its own uniquely named `objects/xx/tmp_obj_*` file and renamed into place, so handles, threads or processes writing the same object at once all get its id back (whichever rename lands first publishes it, the same as Git). Pack installs are safe to race too, so parallel fetches into one repository all succeed: [`install_pack_path`](rustdoc:grit_lib::objects::index_pack::install_pack_path) stages each install in its own directory, and fetches from a local remote hard-link or copy its packs through a private temp file. Both publish without replacing a `.pack` or `.idx` that is already there under the same content-addressed name (the `.idx` always lands after its `.pack`), and a failed or cancelled install removes only its private staging files, never a published `.pack` or `.idx` another writer may already rely on. Ref updates such as adding a note are compare-and-swap: when two writers race, one wins and the other gets a typed ref-store error instead of losing the first update.

Integration coverage: `grit-lib/tests/concurrent_repos.rs` runs parallel commit loops and a fetch/merge/notes scenario with isolated config and sinks; `grit-lib/tests/loose_write_concurrent_issue_1875.rs` races identical small and large blob writes from separate handles and checks the result with `git fsck --strict`; `grit-lib/tests/index_pack_parallel.rs` installs one pack from several threads into one repository and checks it the same way.

## Example

<!-- include: grit-examples/src/bin/guide_embedding.rs -->

Build and run:

```bash
cargo run -p grit-examples --bin guide_embedding
```

## Blame and repository-relative paths

[`blame_file`](rustdoc:grit_lib::blame::blame_file),
[`compute_blame`](rustdoc:grit_lib::blame::compute_blame), and
[`resolve_blame_line_range`](rustdoc:grit_lib::blame::resolve_blame_line_range)
take **repository-relative** paths: the same string names the same file no matter
where the host process is running. Pass status paths unchanged. For a raw byte
path, build the input with `repo_relpath::storage_string`. Non-UTF-8 paths and
names beginning with the reserved `grit+pathhex:` prefix are escaped. Absolute
paths under the work tree and `./` / `../` segments normalize against the
work-tree root. To turn command-line operands into that form (cwd-relative, like
`git blame`), resolve each path with
[`Repository::resolve_worktree_pathspec`](rustdoc:grit_lib::repo::Repository::resolve_worktree_pathspec)
using [`Repository::pathspec_argv_prefix`](rustdoc:grit_lib::repo::Repository::pathspec_argv_prefix)
before calling blame APIs.

Every blame `original_path`, including hunks and rename/copy origins, uses that
same storage representation. Recover its exact bytes with
`repo_relpath::storage_bytes`. Decode once:
a literal filename that itself resembles an encoded path is a separate file.
Line ranges and Git-written changed-path Bloom filters use the decoded path too.

### Blaming uncommitted content

By default `blame_file` blames the file as committed at
[`BlameOptions::start`](rustdoc:grit_lib::blame::BlameOptions::start) (or
`HEAD`). Set
[`BlameOptions::final_source`](rustdoc:grit_lib::blame::BlameOptions::final_source)
to [`BlameFinalSource::Worktree`](rustdoc:grit_lib::blame::BlameFinalSource::Worktree)
to blame the working-tree file, or to
[`BlameFinalSource::Contents`](rustdoc:grit_lib::blame::BlameFinalSource::Contents)
with an editor buffer's bytes. The content is blamed as a commit on top of the
start revision (plus any `MERGE_HEAD` commits), so returned line numbers match
the buffer. Lines that exist only in that content have
[`BlameAttributedLine::not_committed`](rustdoc:grit_lib::blame::BlameAttributedLine::not_committed)
set and the null object id as `commit`; their
[`BlameCommitInfo`](rustdoc:grit_lib::blame::BlameCommitInfo) carries
[`BlameOptions::now`](rustdoc:grit_lib::blame::BlameOptions::now) as the author
time and the placeholder identity Git reports (`Not Committed Yet`, or
`External file (--contents)` for a buffer; see
[`BlameFinalSource::uncommitted_author_name`](rustdoc:grit_lib::blame::BlameFinalSource::uncommitted_author_name)).
[`BlameResult::hunks`](rustdoc:grit_lib::blame::BlameResult::hunks) groups the
lines like `git blame --porcelain`; each
[`BlameHunk`](rustdoc:grit_lib::blame::BlameHunk) flags `not_committed` and
hands back its own lines through
[`BlameResult::hunk_lines`](rustdoc:grit_lib::blame::BlameResult::hunk_lines). Resolve `-L` specs against the same
text with
[`resolve_blame_line_range_with_options`](rustdoc:grit_lib::blame::resolve_blame_line_range_with_options).
Nothing is written to the object database.

```rust
use grit_lib::blame::{blame_file, BlameFinalSource, BlameOptions};

# fn editor(repo: &grit_lib::repo::Repository, buffer: Vec<u8>) -> grit_lib::error::Result<()> {
let result = blame_file(
    repo,
    "src/main.rs",
    &BlameOptions {
        final_source: BlameFinalSource::Contents(buffer),
        ..BlameOptions::default()
    },
)?;
for line in result.lines.iter().filter(|line| line.not_committed) {
    println!("unsaved or uncommitted: {}", line.content);
}
# Ok(())
# }
```
