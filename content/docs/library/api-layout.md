# grit-lib API layout

Grit’s stable surface is the **canonical domain tree** (≤25 root modules plus [`prelude`](rustdoc:grit_lib::prelude)). Flat implementation modules such as `odb` or `fetch` at the crate root are **private**; use `grit_lib::objects::odb` and `grit_lib::remote::fetch` instead. The map in [`grit-lib/canonical-modules.toml`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/canonical-modules.toml) lists every previously flat path and its canonical replacement.

## Domain roots

| Domain | Role |
| --- | --- |
| [`objects`](rustdoc:grit_lib::objects) | Object ids/kinds, ODB, packs, MIDX, hashing, deltas, bitmaps |
| [`refs`](rustdoc:grit_lib::refs) | Ref backends, reflog, refspecs, reftable, name validation |
| [`index`](rustdoc:grit_lib::index) | Index file, sparse checkout, split index, untracked cache |
| [`revwalk`](rustdoc:grit_lib::revwalk) | `rev-parse`, `rev-list`, merge-base, commit-graph |
| [`diff`](rustdoc:grit_lib::diff) | Tree/content diff, merge diffs, rerere, patch-ids helpers |
| [`config`](rustdoc:grit_lib::config) | Config cascade, identity, init/bootstrap |
| [`remote`](rustdoc:grit_lib::remote) | Remotes, fetch/push, transport, credentials |
| [`worktree`](rustdoc:grit_lib::worktree) | Working tree, ignore, attributes, pathspec |
| [`stash`](rustdoc:grit_lib::stash) | Stash stack (porcelain types) |
| [`notes`](rustdoc:grit_lib::notes) | Git notes |
| [`bundle`](rustdoc:grit_lib::bundle) | Bundle files and bundle-over-remote |
| [`parallel`](rustdoc:grit_lib::parallel) | Thread policy and parallel helpers (root) |

Cross-cutting entry points (`repo`, `error`, `porcelain`, `progress`, …) remain at the crate root.

## Flat → canonical map

The authoritative map lives in [`grit-lib/canonical-modules.toml`](https://github.com/gitbutlerapp/grit/blob/main/grit-lib/canonical-modules.toml). CI checks that every `grit_lib::…` import in `grit-cli`, `grit-protocol`, `grit-examples`, `grit-utils`, `grit-test-support`, and `grit-lib/tests` appears in that file (`scripts/check_canonical_layout.py`, wired from `canonical_paths` integration test). Imports must use the **canonical domain path** (for example `grit_lib::objects::odb`, not the private flat `odb` module); `scripts/check_noncanonical_grit_lib_imports.py` enforces that (`no_noncanonical_grit_lib_imports` in the same integration test, and `scripts/tests/test_canonical_layout.py` in the docs CI job).

Examples:

| Private flat module | Canonical path |
| --- | --- |
| `odb` | `grit_lib::objects::odb` |
| `pack_bitmap` | `grit_lib::objects::bitmap` |
| `bitmap_walk` | `grit_lib::objects::bitmap` |
| `rev_list` | `grit_lib::revwalk::rev_list` |
| `fetch` | `grit_lib::remote::fetch` |
| `ignore` | `grit_lib::worktree::ignore` |
| `sparse_checkout` | `grit_lib::index::sparse_checkout` |
| `reflog` | `grit_lib::refs::reflog` |
| `serve` | `grit_lib::remote::serve` |
| `transfer` | `grit_lib::remote::transfer` |

Removed **view modules** (use domains above instead): `object_store`, `references`, `worktree_index`, `revision`, `diffing`, `merging`, `configuration`, and the empty `plumbing` scaffold.

Removed **test-only functions** that reproduced Git `test-tool` dumps or printed Git's CLI warnings, with no caller outside `grit-lib`'s own tests:

| Removed | Use instead |
| --- | --- |
| `objects::midx::format_midx_dump`, `format_midx_dump_layer`, `format_midx_show_objects`, `format_midx_show_objects_layer`, `format_midx_bitmapped_packs` | [`read_midx_objects`](rustdoc:grit_lib::objects::midx::read_midx_objects), [`read_midx_btmp_ranges`](rustdoc:grit_lib::objects::midx::read_midx_btmp_ranges) |
| `index::index_name_hash_lazy::dump_lazy_init_name_hash`, `test_lazy_init_name_hash`, `memihash_cont` | [`memihash`](rustdoc:grit_lib::index::index_name_hash_lazy::memihash) |
| `config::gitmodules::write_gitmodules_cli_option_warnings` | [`looks_like_command_line_option`](rustdoc:grit_lib::config::gitmodules::looks_like_command_line_option) |
| `config::gitmodules::gitdir::die_path_inside_submodule_when_disabled` | [`path_inside_registered_submodule`](rustdoc:grit_lib::config::gitmodules::gitdir::path_inside_registered_submodule), [`path_inside_indexed_submodule`](rustdoc:grit_lib::config::gitmodules::gitdir::path_inside_indexed_submodule) |

## Nested implementation modules

Some canonical paths re-export **submodules** of the flat implementation (for example `grit_lib::objects::odb::store` is the public path for the object-store backend). Prefer the canonical prefix in all new code.
