# grit-lib public API

This document describes how the **supported** surface of `grit-lib` is shaped and how it may change on the path to **0.6.0** and afterward. It complements the [API layout guide](https://grit-scm.com/docs/library/api-layout) (canonical module paths) and the [rustdoc](https://docs.rs/grit-lib) generated from source.

## Conventions

These rules apply to every public item in `grit-lib`. They are the bar for reaching **0.6.0** with an API embedders can rely on.

### 1. One entry point

[`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) is the primary handle for an on-disk or injected repository. Long-lived subsystems are exposed as **focused handles** obtained from it (for example objects, references, the index, configuration, revision walking, and diffs), not as free functions that take `&Repository` or a bare git-dir path.

Pure helpers (parsing, formatting, hashing) may remain free functions when they do not need repository state.

### 2. Options structs, not variants

Optional behavior belongs in a single `#[non_exhaustive]` options struct with `Default` and builder-style setters on that struct. Avoid:

- `_with_x_and_y` function families
- More than one bare `bool` parameter on a public function (prefer an enum or the options struct)
- `#[allow(clippy::too_many_arguments)]` on public functions

### 3. One type per concept

Use one cancellation type, one progress sink, [`RefName`](https://docs.rs/grit-lib/latest/grit_lib/refs/check_ref_format/struct.RefName.html) (or equivalent) for ref names, a dedicated pathspec type instead of `&[String]`, and configuration through `repo.config()` rather than ad hoc config reads deep in call stacks.

### 4. Typed errors per area

Each functional area exposes its own `#[non_exhaustive]` `thiserror` enum with structured fields. The public API must not use a catch-all `Message(String)` variant or `Result<_, String>` as the primary error shape.

### 5. Typed outcomes

Operations return structured results (counts, classifications, names, conflict state) so callers and the CLI do not re-derive the same facts from logs or side effects.

### 6. Public means supported

- Default to `pub(crate)`; lift to `pub` only for the supported API.
- One public path per concept (see [API layout](https://grit-scm.com/docs/library/api-layout)).
- Nothing named for tests, probes, fixtures, or benchmarks is public; helpers for those live behind the **`test-support`** cargo feature.
- No `libc` types and no `pub unsafe fn` in the public API.

### 7. Documented

Every public item has rustdoc. Entry points include examples. Crate-level documentation states the current release line (major.minor) and links here for stability policy.

## Stability policy

**Before 0.6.0:** breaking changes are acceptable when they **consolidate** the API (fewer, clearer ways to do each thing, following the conventions above). Do not break the public API for unrelated refactors.

**From 0.6.0 through 0.6.x:** the public API only **grows**. No removals, renames, or signature changes without keeping the old form: add the replacement, mark the old entry `#[deprecated(since = "0.6.x", note = "use …")]`, and remove it in **0.7**.

**Enforcement:** CI runs a semver check against the recorded public API snapshot; the exact command and workflow step are documented under [Gates](#gates) once the ratchet lands in the repository.

## How to add a public item

Use this checklist before merging a new or widened public API:

1. **Placement** — Does the operation belong on the right handle (`Repository` or a focused borrow) rather than as a crate-root free function?
2. **Options** — If behavior is configurable, add or extend a `#[non_exhaustive]` options struct with `Default` and setters.
3. **Errors** — Extend the area’s `thiserror` enum (or add a new one); no stringly errors.
4. **Documentation** — Rustdoc on the item, with an example for entry-level APIs.
5. **Tests** — Unit tests in `grit-lib` and, for public surface, an integration or coverage test that exercises the API path.
6. **Snapshot** — Run `make public-api` when the intentional public surface changes and commit the updated `grit-lib/public-api.txt`.
7. **Ratchet** — When the API count ratchet is enabled, update its baseline if your change is approved (see [Gates](#gates)).
8. **Docs site** — For user-visible or guide-worthy APIs, add or update a page under `content/docs/library/` and ensure `cd site && npm run check` passes.

## Audit

A table of every public module and handle, with merge/rename notes, will be added in a follow-up step of the API 0.6 roadmap.

## Gates

Documentation for `make semver-check`, the public API ratchet, and related CI gates will be filled in when those targets land in the repository.
