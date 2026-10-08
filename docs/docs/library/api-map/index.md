# grit-lib API map

> Generated index of public grit-lib modules and key types with docs.rs links.

This page is regenerated from the local `grit-lib` rustdoc build (`cargo doc -p grit-lib --no-deps`) whenever you run `make docs`. Each row links to [docs.rs](https://docs.rs/grit-lib/latest/grit_lib/) for the matching item.



| Item | Kind | Summary | docs.rs |
| --- | --- | --- | --- |
| `grit_lib::apply` | module | Unified/git-diff patch parsing for grit apply. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/index.html) |
| `grit_lib::apply::Hunk` | struct | A single hunk in a unified diff. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/struct.Hunk.html) |
| `grit_lib::attributes` | module | Gitattributes parsing and pattern matching for check-attr and validation. | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/index.html) |
| `grit_lib::blame` | module | Blame line-mapping algorithm. | [API](https://docs.rs/grit-lib/latest/grit_lib/blame/index.html) |
| `grit_lib::bloom` | module | Changed-path Bloom filters for commit-graph files (Git bloom.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/bloom/index.html) |
| `grit_lib::branch_tracking` | module | Branch vs remote-tracking comparison for status, checkout, and commit (matches git/remote.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/branch_tracking/index.html) |
| `grit_lib::check_ref_format` | module | Ref-name validation — git check-ref-format rules. | [API](https://docs.rs/grit-lib/latest/grit_lib/check_ref_format/index.html) |
| `grit_lib::combined_diff_patch` | module | Git-style combined merge diff hunks (diff --cc / diff --combined). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_diff_patch/index.html) |
| `grit_lib::combined_tree_diff` | module | Multi-parent combined tree diff (Git diff_tree_paths / find_paths_multitree). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/index.html) |
| `grit_lib::commit` | module | Commit-metadata helpers shared by the porcelain commands. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit/index.html) |
| `grit_lib::commit_encoding` | module | Git commit encoding labels (encoding header, i18n.commitEncoding) mapped to codecs. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_encoding/index.html) |
| `grit_lib::commit_graph_file` | module | Parsing Git commit-graph files and Bloom filter lookup (commit-graph.c / bloom.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/index.html) |
| `grit_lib::commit_graph_write` | module | Serialize Git commit-graph v1 files with GDA2 + optional Bloom chunks (commit-graph.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_write/index.html) |
| `grit_lib::commit_pretty` | module | Human-oriented commit one-line formats shared by porcelain commands. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_pretty/index.html) |
| `grit_lib::commit_trailers` | module | Cherry-pick / sign-off trailer handling compatible with Git’s sequencer.c and trailer.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_trailers/index.html) |
| `grit_lib::config` | module | Git-compatible configuration file parser and accessor. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/index.html) |
| `grit_lib::configuration` | module | Configuration and identity: config cascade, .gitmodules, author/committer idents. | [API](https://docs.rs/grit-lib/latest/grit_lib/configuration/index.html) |
| `grit_lib::connectivity` | module | Reachability checks for push / receive-pack connectivity verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/connectivity/index.html) |
| `grit_lib::credentials` | module | Credential layer — Git-compatible credential filling/approval/rejection for library embedders. | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/index.html) |
| `grit_lib::credentials::Credential` | struct | A structured Git credential. | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.Credential.html) |
| `grit_lib::crlf` | module | CRLF / EOL conversion and clean/smudge filter support. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/index.html) |
| `grit_lib::crlf::CoreEol` | enum | What core.eol is set to. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.CoreEol.html) |
| `grit_lib::crlf::EolAttr` | enum | Per-file eol attribute from .gitattributes. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.EolAttr.html) |
| `grit_lib::delta_encode` | module | Encode Git pack binary deltas (format decoded by crate::unpack_objects::apply_delta). | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_encode/index.html) |
| `grit_lib::delta_islands` | module | Delta islands — restrict cross-island deltas in pack-objects (--delta-islands). | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_islands/index.html) |
| `grit_lib::diff` | module | Diff machinery — compare trees, index entries, and working tree files. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/index.html) |
| `grit_lib::diff_indent_heuristic` | module | Git-compatible diff hunk sliding (xdl_change_compact) including the indent heuristic. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_indent_heuristic/index.html) |
| `grit_lib::diff_moved` | module | Move detection for git diff --color-moved, implementing the same behavior as git’s add_lines_to_move_detection + mark_color_as_moved + dim_moved_lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_moved/index.html) |
| `grit_lib::diffing` | module | Diffing: tree/content diff, rename detection, diffstat, line-log, pickaxe bloom. | [API](https://docs.rs/grit-lib/latest/grit_lib/diffing/index.html) |
| `grit_lib::diffstat` | module | Git-compatible --stat / diffstat layout (width, name truncation, bar scaling). | [API](https://docs.rs/grit-lib/latest/grit_lib/diffstat/index.html) |
| `grit_lib::dotfile` | module | Git-compatible .git* / NTFS / HFS path checks (path.c, utf8.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/dotfile/index.html) |
| `grit_lib::error` | module | Shared error types for grit-lib. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/index.html) |
| `grit_lib::error::Error` | enum | The top-level error type for all grit-lib operations. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) |
| `grit_lib::fetch` | module | Wire-protocol fetch orchestration over a crate::transport::Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/index.html) |
| `grit_lib::fetch::NoProgress` | struct | A Progress that discards everything. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/struct.NoProgress.html) |
| `grit_lib::fetch::Progress` | trait | Sink for the remote’s human-readable progress (side-band channel 2). | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) |
| `grit_lib::fetch_head` | module | Parsing FETCH_HEAD lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_head/index.html) |
| `grit_lib::fetch_negotiator` | module | Skipping fetch negotiator — implements Git’s “skipping” negotiation strategy. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_negotiator/index.html) |
| `grit_lib::fetch_submodules` | module | Logic for git fetch --recurse-submodules (changed-submodule detection and config). | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_submodules/index.html) |
| `grit_lib::filter_process` | module | Long-running Git filter protocol (filter.<name>.process), matching git-filter v2. | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/index.html) |
| `grit_lib::fmt_merge_msg` | module | Merge commit message formatter — git fmt-merge-msg logic. | [API](https://docs.rs/grit-lib/latest/grit_lib/fmt_merge_msg/index.html) |
| `grit_lib::fsck_standalone` | module | Standalone object fsck for hash-object and similar entry points. | [API](https://docs.rs/grit-lib/latest/grit_lib/fsck_standalone/index.html) |
| `grit_lib::gc` | module | In-process maintenance primitives that embedders such as jj use in place of shelling out to git gc / git remote show / gix::refs::transaction. | [API](https://docs.rs/grit-lib/latest/grit_lib/gc/index.html) |
| `grit_lib::git_binary_base85` | module | Base-85 codec for GIT binary patch sections (matches git/base85.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_binary_base85/index.html) |
| `grit_lib::git_date::approx` | module | Git-compatible approxidate parsing. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/approx/index.html) |
| `grit_lib::git_date` | module | Git-compatible date parsing and display. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/index.html) |
| `grit_lib::git_date::parse` | module | Git-compatible date parsing (parse_date_basic, parse_date). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/parse/index.html) |
| `grit_lib::git_date::show` | module | Git-compatible date display (show_date, show_date_relative, strftime handling). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/show/index.html) |
| `grit_lib::git_date::tm` | module | Time conversion helpers that produce Git-compatible timestamps. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/tm/index.html) |
| `grit_lib::git_path` | module | Git-compatible path normalization and helpers for test-tool path-utils. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_path/index.html) |
| `grit_lib::gitmodules` | module | .gitmodules validation (Git fsck / submodule-config parity). | [API](https://docs.rs/grit-lib/latest/grit_lib/gitmodules/index.html) |
| `grit_lib::hash` | module | Incremental hashing for Git object ids and file trailers. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/index.html) |
| `grit_lib::hash::Backend` | enum | Which implementation the sha1 / sha2 dependency selects for algo on this CPU. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.Backend.html) |
| `grit_lib::hash::Parallelism` | struct | Resolved worker thread count for parallel hash helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/struct.Parallelism.html) |
| `grit_lib::hide_refs` | module | transfer.hideRefs / receive.hideRefs / uploadpack.hideRefs matching (Git ref_is_hidden). | [API](https://docs.rs/grit-lib/latest/grit_lib/hide_refs/index.html) |
| `grit_lib::hooks` | module | Hook execution utilities. | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/index.html) |
| `grit_lib::ident` | module | Git author/committer identity lines (ident in Git’s fsck.c / commit.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident/index.html) |
| `grit_lib::ident_config` | module | Default identity values from config and the system (Git ident.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_config/index.html) |
| `grit_lib::ident_resolve` | module | Git-compatible author/committer identity resolution (see upstream ident.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/index.html) |
| `grit_lib::ignore` | module | Ignore and exclude matching for check-ignore. | [API](https://docs.rs/grit-lib/latest/grit_lib/ignore/index.html) |
| `grit_lib::index` | module | Git index (staging area) reading and writing. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/index.html) |
| `grit_lib::index::Index` | struct | The in-memory representation of the Git index file. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html) |
| `grit_lib::index_name_hash_lazy` | module | Lazy, optionally multi-threaded index name/dir hash initialization compatible with Git’s name-hash.c (used by test-tool lazy-init-name-hash and regression test t3008). | [API](https://docs.rs/grit-lib/latest/grit_lib/index_name_hash_lazy/index.html) |
| `grit_lib::index_pack` | module | Install a received packfile into the object store (index-pack path). | [API](https://docs.rs/grit-lib/latest/grit_lib/index_pack/index.html) |
| `grit_lib::init_filesystem` | module | Filesystem capability probes during repository initialization. | [API](https://docs.rs/grit-lib/latest/grit_lib/init_filesystem/index.html) |
| `grit_lib::interpret_trailers` | module | Commit message trailer parsing and rewriting (Git-compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/interpret_trailers/index.html) |
| `grit_lib::line_log` | module | Line-level history (git log -L) — range tracking across diffs. | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/index.html) |
| `grit_lib::line_log::Range` | struct | Half-open line range using 0-based indices (Git internal). | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.Range.html) |
| `grit_lib::ls_remote` | module | ls-remote — enumerate references from a local repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/ls_remote/index.html) |
| `grit_lib::ls_remote::Options` | struct | Options controlling which references ls_remote returns. | [API](https://docs.rs/grit-lib/latest/grit_lib/ls_remote/struct.Options.html) |
| `grit_lib::ls_remote::RefEntry` | struct | A single reference entry produced by ls_remote. | [API](https://docs.rs/grit-lib/latest/grit_lib/ls_remote/struct.RefEntry.html) |
| `grit_lib::mailmap` | module | Parse .mailmap and resolve author/committer identities (Git-compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/mailmap/index.html) |
| `grit_lib::merge_base` | module | Merge-base and reachability primitives. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_base/index.html) |
| `grit_lib::merge_diff` | module | Merge commit and combined (--cc / -c) diff helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_diff/index.html) |
| `grit_lib::merge_file` | module | Three-way file merge — the engine behind grit merge-file. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/index.html) |
| `grit_lib::merge_trees` | module | Rename-aware three-way tree merge for cherry-pick / revert style merges. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_trees/index.html) |
| `grit_lib::merging` | module | Merging: merge-base, tree/file merges, rerere, merge-message formatting. | [API](https://docs.rs/grit-lib/latest/grit_lib/merging/index.html) |
| `grit_lib::midx` | module | Multi-pack-index (MIDX) file writing and minimal reading. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/index.html) |
| `grit_lib::name_rev` | module | Name-rev: name commits relative to refs. | [API](https://docs.rs/grit-lib/latest/grit_lib/name_rev/index.html) |
| `grit_lib::net_trace` | module | Lightweight, env-gated tracing for the networking paths (transport connect, fetch/push negotiation, pack transfer). | [API](https://docs.rs/grit-lib/latest/grit_lib/net_trace/index.html) |
| `grit_lib::notes` | module | git notes tree manipulation — the fanout tree mapping object -> note blob. | [API](https://docs.rs/grit-lib/latest/grit_lib/notes/index.html) |
| `grit_lib::object_store` | module | Object storage: ids/kinds, the object database, packs, multi-pack index, deltas. | [API](https://docs.rs/grit-lib/latest/grit_lib/object_store/index.html) |
| `grit_lib::objects` | module | Git object model: object IDs, kinds, and in-memory representations. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/index.html) |
| `grit_lib::objects::Object` | struct | A decompressed, header-stripped Git object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.Object.html) |
| `grit_lib::objects::TagData` | struct | Parsed representation of an annotated tag object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.TagData.html) |
| `grit_lib::odb` | module | Loose object database: reading and writing zlib-compressed Git objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/index.html) |
| `grit_lib::odb::Odb` | struct | A loose-object database rooted at a given objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) |
| `grit_lib::pack` | module | Pack and pack-index helpers for object counting and verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/index.html) |
| `grit_lib::pack_geometry` | module | Pack geometry for git repack --geometric (factor-based progression). | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_geometry/index.html) |
| `grit_lib::pack_name_hash` | module | Git pack bitmap name-hash functions (pack_name_hash / pack_name_hash_v2). | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_name_hash/index.html) |
| `grit_lib::pack_rev` | module | On-disk pack reverse index (.rev) — RIDX format matching Git’s pack-write.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_rev/index.html) |
| `grit_lib::patch_ids` | module | Patch-ID computation for commit equivalence detection. | [API](https://docs.rs/grit-lib/latest/grit_lib/patch_ids/index.html) |
| `grit_lib::path` | module | Git-compatible verification of tree/index path components. | [API](https://docs.rs/grit-lib/latest/grit_lib/path/index.html) |
| `grit_lib::path_icase` | module | Case-insensitive path identity when core.ignorecase is enabled. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_icase/index.html) |
| `grit_lib::path_walk` | module | Path-batched object graph walk matching Git’s walk_objects_by_path / test-tool path-walk. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_walk/index.html) |
| `grit_lib::pathspec` | module | Git-compatible pathspec matching (magic tokens and global flags). | [API](https://docs.rs/grit-lib/latest/grit_lib/pathspec/index.html) |
| `grit_lib::pkt_line` | module | Git pkt-line format helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/pkt_line/index.html) |
| `grit_lib::pkt_line::Packet` | enum | A single packet read from the wire. | [API](https://docs.rs/grit-lib/latest/grit_lib/pkt_line/enum.Packet.html) |
| `grit_lib::plumbing` | module | Plumbing operations — low-level, machine-stable building blocks that map ~1:1 to a Git plumbing command (e.g. | [API](https://docs.rs/grit-lib/latest/grit_lib/plumbing/index.html) |
| `grit_lib::porcelain::add` | module | Stage working-tree changes into the index (git add). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/index.html) |
| `grit_lib::porcelain::checkout` | module | git checkout worktree-apply primitives. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/checkout/index.html) |
| `grit_lib::porcelain::cherry_pick` | module | git cherry-pick / git revert pick-engine core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/cherry_pick/index.html) |
| `grit_lib::porcelain::commit` | module | Create a commit from the current index and move the checked-out branch. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/commit/index.html) |
| `grit_lib::porcelain` | module | Porcelain operations — user-facing engines that assemble a structured result model from plumbing pieces (e.g. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/index.html) |
| `grit_lib::porcelain::log` | module | git log ref decoration as structured data. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/log/index.html) |
| `grit_lib::porcelain::merge` | module | git merge index/head algorithm core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/merge/index.html) |
| `grit_lib::porcelain::rebase` | module | git rebase todo-list model and squash/fixup message assembly. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/rebase/index.html) |
| `grit_lib::porcelain::revert` | module | git revert pick-engine core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/revert/index.html) |
| `grit_lib::porcelain::stage_tracked` | module | Stage modifications and deletions of already-tracked paths (git commit -a / -a staging). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stage_tracked/index.html) |
| `grit_lib::porcelain::staging` | module | Stage worktree changes into the index (grit add / commit -a engine). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/staging/index.html) |
| `grit_lib::porcelain::stash` | module | git stash apply core, plus the tree-flattening and worktree-mutation primitives the stash engine is built on. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stash/index.html) |
| `grit_lib::porcelain::status` | module | git status as a structured operation. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/index.html) |
| `grit_lib::porcelain::tag` | module | git tag listing/filtering core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/tag/index.html) |
| `grit_lib::porcelain::worktree_guard` | module | Lightweight worktree cleanliness checks without a full status pass. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/worktree_guard/index.html) |
| `grit_lib::precompose_config` | module | Read core.precomposeunicode without opening a full Repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/precompose_config/index.html) |
| `grit_lib::prelude` | module | The types most callers need, re-exported for use grit_lib::prelude::*;. | [API](https://docs.rs/grit-lib/latest/grit_lib/prelude/index.html) |
| `grit_lib::progress` | module | Progress reporting and cancellation across the library/CLI boundary. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/index.html) |
| `grit_lib::progress::Cancel` | trait | A cancellation signal checked by long-running library operations at loop boundaries. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.Cancel.html) |
| `grit_lib::promisor` | module | Partial-clone promisor bookkeeping used by Grit. | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor/index.html) |
| `grit_lib::promisor_remote` | module | The “promisor-remote” protocol v2 capability (see gitprotocol-v2(5) and Documentation/config/promisor.adoc). | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor_remote/index.html) |
| `grit_lib::protocol` | module | Protocol allow/deny policy and client wire-protocol version selection. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol/index.html) |
| `grit_lib::protocol_v2` | module | Pure protocol-v2 capability parsing and request-fragment building. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol_v2/index.html) |
| `grit_lib::prune_packed` | module | Library implementation of prune-packed. | [API](https://docs.rs/grit-lib/latest/grit_lib/prune_packed/index.html) |
| `grit_lib::push` | module | Wire-protocol push orchestration over a crate::transport::Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/push/index.html) |
| `grit_lib::push_cert` | module | Signed-push certificate generation and verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_cert/index.html) |
| `grit_lib::push_report` | module | Push status reporting that matches Git’s output. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_report/index.html) |
| `grit_lib::push_submodules` | module | Submodule recursion for git push (--recurse-submodules). | [API](https://docs.rs/grit-lib/latest/grit_lib/push_submodules/index.html) |
| `grit_lib::quote_path` | module | C-style path quoting compatible with Git’s quote.c / core.quotepath. | [API](https://docs.rs/grit-lib/latest/grit_lib/quote_path/index.html) |
| `grit_lib::receive_pack` | module | Receive-pack configuration and pack-header helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/receive_pack/index.html) |
| `grit_lib::ref_exclusions` | module | Reference exclusion rules for rev-list / rev-parse (--exclude, --exclude-hidden). | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_exclusions/index.html) |
| `grit_lib::ref_exclusions::RefExclusions` | struct | Patterns that exclude refs from --all / glob expansion, including hidden-ref config. | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_exclusions/struct.RefExclusions.html) |
| `grit_lib::ref_namespace` | module | Git GIT_NAMESPACE handling: map logical ref names to storage under refs/namespaces/.../. | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_namespace/index.html) |
| `grit_lib::references` | module | References: the refs backends, reflog, refspecs, name validation, namespaces. | [API](https://docs.rs/grit-lib/latest/grit_lib/references/index.html) |
| `grit_lib::reflog` | module | Reflog reading and management. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/index.html) |
| `grit_lib::refs` | module | Reference storage — files backend + reftable backend. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/index.html) |
| `grit_lib::refs::Ref` | enum | A symbolic or direct reference. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.Ref.html) |
| `grit_lib::refs_fsck` | module | Reference database consistency checks for git refs verify and git fsck --references. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs_fsck/index.html) |
| `grit_lib::refspec` | module | Refspec parsing and validation — a port of git/refspec.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/refspec/index.html) |
| `grit_lib::reftable` | module | Reftable format — binary reference storage. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/index.html) |
| `grit_lib::reftable::RefValue` | enum | A single reference record as stored in a reftable. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/enum.RefValue.html) |
| `grit_lib::reftable::LogRecord` | struct | A decoded log record. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.LogRecord.html) |
| `grit_lib::reftable::RefRecord` | struct | A decoded ref record. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.RefRecord.html) |
| `grit_lib::repo` | module | Repository discovery and the primary Repository handle. | [API](https://docs.rs/grit-lib/latest/grit_lib/repo/index.html) |
| `grit_lib::repo::Repository` | struct | A handle to an open Git repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) |
| `grit_lib::rerere` | module | Git-compatible rerere (MERGE_RR, rr-cache/, conflict ID hashing). | [API](https://docs.rs/grit-lib/latest/grit_lib/rerere/index.html) |
| `grit_lib::resolve_undo` | module | Git index REUC (resolve-undo) extension — records unmerged stages when a conflict is resolved. | [API](https://docs.rs/grit-lib/latest/grit_lib/resolve_undo/index.html) |
| `grit_lib::rev_list` | module | Commit traversal and output planning for rev-list. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/index.html) |
| `grit_lib::rev_list_error` | module | Typed failures from crate::rev_list. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list_error/index.html) |
| `grit_lib::rev_parse` | module | Revision parsing and repository discovery helpers for rev-parse. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/index.html) |
| `grit_lib::rev_parse_error` | module | Typed failures from revision parsing (crate::rev_parse). | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse_error/index.html) |
| `grit_lib::revision` | module | Revision machinery: rev-parse, rev-list, name-rev, commit-graph. | [API](https://docs.rs/grit-lib/latest/grit_lib/revision/index.html) |
| `grit_lib::serve` | module | Server side of the Git wire protocol: answering fetches and accepting pushes. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/index.html) |
| `grit_lib::shallow` | module | Shallow repository metadata (.git/shallow). | [API](https://docs.rs/grit-lib/latest/grit_lib/shallow/index.html) |
| `grit_lib::shared_repo` | module | Shared-repository permission helpers (core.sharedRepository, --shared). | [API](https://docs.rs/grit-lib/latest/grit_lib/shared_repo/index.html) |
| `grit_lib::signing` | module | Commit/tag GPG (and gpgsm/ssh) signing and signature verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/index.html) |
| `grit_lib::signing::GpgFormat` | enum | The signature format selected via gpg.format. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/enum.GpgFormat.html) |
| `grit_lib::signing::GpgConfig` | struct | Resolved signing/verification configuration. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/struct.GpgConfig.html) |
| `grit_lib::sparse_checkout` | module | Sparse-checkout pattern parsing and path membership (cone and non-cone). | [API](https://docs.rs/grit-lib/latest/grit_lib/sparse_checkout/index.html) |
| `grit_lib::split_index` | module | Split index: link extension and sharedindex.<sha1> (Git split-index.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/split_index/index.html) |
| `grit_lib::state` | module | Repository state machine — HEAD resolution, branch status, and in-progress operation detection. | [API](https://docs.rs/grit-lib/latest/grit_lib/state/index.html) |
| `grit_lib::stripspace` | module | Core logic for git stripspace. | [API](https://docs.rs/grit-lib/latest/grit_lib/stripspace/index.html) |
| `grit_lib::stripspace::Mode` | enum | Processing mode for process. | [API](https://docs.rs/grit-lib/latest/grit_lib/stripspace/enum.Mode.html) |
| `grit_lib::submodule_active` | module | Submodule “active” state (submodule.c is_submodule_active parity). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_active/index.html) |
| `grit_lib::submodule_config` | module | Submodule registration and activation (Git submodule.c parity for tooling). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config/index.html) |
| `grit_lib::submodule_config_cache` | module | Submodule configuration cache (Git submodule-config.c subset for test-tool). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config_cache/index.html) |
| `grit_lib::submodule_gitdir` | module | Submodule gitdir paths when extensions.submodulePathConfig is enabled. | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_gitdir/index.html) |
| `grit_lib::terminal` | module | Cross-platform terminal capability detection for ANSI color output. | [API](https://docs.rs/grit-lib/latest/grit_lib/terminal/index.html) |
| `grit_lib::textconv_cache` | module | Git-compatible diff.<driver>.cachetextconv storage under refs/notes/textconv/<driver>. | [API](https://docs.rs/grit-lib/latest/grit_lib/textconv_cache/index.html) |
| `grit_lib::transfer` | module | Embedder-facing transfer (fetch / push) result & option types, plus the negotiation-driven pack builder. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/index.html) |
| `grit_lib::transfer::TagMode` | enum | Which tags to fetch alongside the requested refs. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/enum.TagMode.html) |
| `grit_lib::transfer::RefUpdate` | struct | The resolved outcome of one reference during a fetch. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.RefUpdate.html) |
| `grit_lib::transport::http` | module | Smart-HTTP Git transport over a pluggable HTTP client. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/http/index.html) |
| `grit_lib::transport` | module | Embedder-facing transport abstraction for the Git wire protocols. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/index.html) |
| `grit_lib::transport::Service` | enum | The Git service a Connection speaks. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/enum.Service.html) |
| `grit_lib::transport::SshCommand` | enum | How the SshTransport invokes ssh. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/enum.SshCommand.html) |
| `grit_lib::transport::Advertisement` | struct | The captured ref/capability advertisement for a v0/v1 connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.Advertisement.html) |
| `grit_lib::transport::SshConnection` | struct | A live connection to a remote Git service over an ssh subprocess. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshConnection.html) |
| `grit_lib::transport::SshTransport` | struct | The ssh transport: spawn ssh [opts] <host> git-upload-pack '<path>' and expose the child’s stdio as a Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshTransport.html) |
| `grit_lib::transport::SshUrl` | struct | A parsed SSH remote (scp-style host:path, ssh://, or git+ssh://). | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshUrl.html) |
| `grit_lib::transport::Connection` | trait | A live, bidirectional pkt-line connection to a Git service, with the ref/capability advertisement captured during the handshake. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/trait.Connection.html) |
| `grit_lib::transport::Transport` | trait | A factory that connects to a remote and performs the protocol handshake. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/trait.Transport.html) |
| `grit_lib::transport_path` | module | Safety checks and path resolution for local transport URLs (matches Git connect.c / path.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/transport_path/index.html) |
| `grit_lib::tree_path_follow` | module | Resolve tree paths with symlink following (get_tree_entry_follow_symlinks). | [API](https://docs.rs/grit-lib/latest/grit_lib/tree_path_follow/index.html) |
| `grit_lib::unicode_normalization` | module | UTF-8 NFC path normalization for macOS-style filesystems (core.precomposeUnicode). | [API](https://docs.rs/grit-lib/latest/grit_lib/unicode_normalization/index.html) |
| `grit_lib::unpack_objects` | module | unpack-objects: unpack a pack stream into loose objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/index.html) |
| `grit_lib::untracked_cache` | module | Git index UNTR (untracked cache) — git/dir.c / read-cache.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/untracked_cache/index.html) |
| `grit_lib::upload_filter` | module | Upload-pack object filter policy. | [API](https://docs.rs/grit-lib/latest/grit_lib/upload_filter/index.html) |
| `grit_lib::url_rewrite` | module | URL rewrite helpers for url.*.insteadOf / url.*.pushInsteadOf. | [API](https://docs.rs/grit-lib/latest/grit_lib/url_rewrite/index.html) |
| `grit_lib::userdiff` | module | User-defined and built-in diff function-name matching. | [API](https://docs.rs/grit-lib/latest/grit_lib/userdiff/index.html) |
| `grit_lib::whitespace_rule` | module | Git-compatible core.whitespace rules and ws_fix_copy (git/ws.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/whitespace_rule/index.html) |
| `grit_lib::wildmatch` | module | Git-compatible wildmatch pattern matching. | [API](https://docs.rs/grit-lib/latest/grit_lib/wildmatch/index.html) |
| `grit_lib::worktree` | module | Linked worktree registry: discovery, listing, and admin-dir layout. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree/index.html) |
| `grit_lib::worktree_cwd` | module | Process current working directory relative to a Git work tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_cwd/index.html) |
| `grit_lib::worktree_index` | module | Index and working tree: the index, sparse checkout, attributes, ignore, CRLF. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_index/index.html) |
| `grit_lib::worktree_ref` | module | Per-worktree ref name parsing and storage location (Git parse_worktree_ref / files_ref_path). | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_ref/index.html) |
| `grit_lib::worktree_rules::file_load_counters` | module | Counts disk reads of attribute/ignore pattern files (tests only). | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_rules/file_load_counters/index.html) |
| `grit_lib::worktree_rules` | module | Per-operation worktree attribute and ignore context. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_rules/index.html) |
| `grit_lib::write_tree` | module | Build tree objects from index entries (git write-tree core logic). | [API](https://docs.rs/grit-lib/latest/grit_lib/write_tree/index.html) |
| `grit_lib::ws` | module | Git-compatible whitespace rules (core.whitespace, whitespace attribute). | [API](https://docs.rs/grit-lib/latest/grit_lib/ws/index.html) |
