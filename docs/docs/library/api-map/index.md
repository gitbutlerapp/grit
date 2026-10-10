# grit-lib API map

> Generated index of public grit-lib modules and key types with docs.rs links.

This page is regenerated from the local `grit-lib` rustdoc build (`cargo doc -p grit-lib --no-deps`) whenever you run `make docs`. Each row links to [docs.rs](https://docs.rs/grit-lib/latest/grit_lib/) for the matching item.



| Item | Kind | Summary | docs.rs |
| --- | --- | --- | --- |
| `grit_lib::apply` | module | Unified/git-diff patch parsing for grit apply. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/index.html) |
| `grit_lib::apply::BinaryPatchPayload` | struct | Binary patch payload as compressed base85 chunks for forward/reverse apply. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/struct.BinaryPatchPayload.html) |
| `grit_lib::apply::FilePatch` | struct | Represents one file in a unified diff. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/struct.FilePatch.html) |
| `grit_lib::apply::Hunk` | struct | A single hunk in a unified diff. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/struct.Hunk.html) |
| `grit_lib::apply::HunkLine` | enum | Inflate zlib-compressed binary payload. | [API](https://docs.rs/grit-lib/latest/grit_lib/apply/enum.HunkLine.html) |
| `grit_lib::attributes` | module | Gitattributes parsing and pattern matching for check-attr and validation. | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/index.html) |
| `grit_lib::attributes::AttrRule` | struct | One line in a gitattributes file. | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/struct.AttrRule.html) |
| `grit_lib::attributes::AttrValue` | enum | Parsed attribute value for display (check-attr output). | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/enum.AttrValue.html) |
| `grit_lib::attributes::MacroTable` | struct | Macro definitions from [attr]name ... | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/struct.MacroTable.html) |
| `grit_lib::attributes::ParsedGitAttributes` | struct | Result of parsing a gitattributes file. | [API](https://docs.rs/grit-lib/latest/grit_lib/attributes/struct.ParsedGitAttributes.html) |
| `grit_lib::blame` | module | Blame line-mapping algorithm. | [API](https://docs.rs/grit-lib/latest/grit_lib/blame/index.html) |
| `grit_lib::blame::BlameLine` | struct | A single line attribution. | [API](https://docs.rs/grit-lib/latest/grit_lib/blame/struct.BlameLine.html) |
| `grit_lib::blame::BlameTextconvContext` | struct | annotate-tests.sh “blame huge graft”: octopus graft with 29 parents on commit 00 and a two-line 0/0 file. | [API](https://docs.rs/grit-lib/latest/grit_lib/blame/struct.BlameTextconvContext.html) |
| `grit_lib::bloom` | module | Changed-path Bloom filters for commit-graph files (Git bloom.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/bloom/index.html) |
| `grit_lib::bloom::BloomBuildOutcome` | enum | Result of building a Bloom filter payload for one commit. | [API](https://docs.rs/grit-lib/latest/grit_lib/bloom/enum.BloomBuildOutcome.html) |
| `grit_lib::bloom::BloomFilterInvalid` | struct | Invalid or missing Bloom filter data (zero-length filter, etc.). | [API](https://docs.rs/grit-lib/latest/grit_lib/bloom/struct.BloomFilterInvalid.html) |
| `grit_lib::bloom::BloomFilterSettings` | struct | Settings stored in the BDAT chunk header and used for hashing. | [API](https://docs.rs/grit-lib/latest/grit_lib/bloom/struct.BloomFilterSettings.html) |
| `grit_lib::branch_tracking` | module | Branch vs remote-tracking comparison for status, checkout, and commit (matches git/remote.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/branch_tracking/index.html) |
| `grit_lib::branch_tracking::AheadBehindMode` | enum | How to compare local HEAD to a remote-tracking ref (AHEAD_BEHIND_FULL vs QUICK). | [API](https://docs.rs/grit-lib/latest/grit_lib/branch_tracking/enum.AheadBehindMode.html) |
| `grit_lib::branch_tracking::TrackingStat` | enum | Outcome of comparing refs/heads/<branch> to a tracking ref. | [API](https://docs.rs/grit-lib/latest/grit_lib/branch_tracking/enum.TrackingStat.html) |
| `grit_lib::check_ref_format` | module | Ref-name validation — git check-ref-format rules. | [API](https://docs.rs/grit-lib/latest/grit_lib/check_ref_format/index.html) |
| `grit_lib::check_ref_format::RefNameError` | enum | Errors returned by check_refname_format. | [API](https://docs.rs/grit-lib/latest/grit_lib/check_ref_format/enum.RefNameError.html) |
| `grit_lib::check_ref_format::RefNameOptions` | struct | Options controlling validation. | [API](https://docs.rs/grit-lib/latest/grit_lib/check_ref_format/struct.RefNameOptions.html) |
| `grit_lib::clone` | module | Clone a remote repository: initialize, configure origin, fetch, and set up the default branch. | [API](https://docs.rs/grit-lib/latest/grit_lib/clone/index.html) |
| `grit_lib::clone::CloneError` | enum | Errors during clone. | [API](https://docs.rs/grit-lib/latest/grit_lib/clone/enum.CloneError.html) |
| `grit_lib::clone::CloneOptions` | struct | Options for clone. | [API](https://docs.rs/grit-lib/latest/grit_lib/clone/struct.CloneOptions.html) |
| `grit_lib::clone::CloneOutcome` | struct | Result of a successful clone before working-tree checkout. | [API](https://docs.rs/grit-lib/latest/grit_lib/clone/struct.CloneOutcome.html) |
| `grit_lib::combined_diff_patch` | module | Git-style combined merge diff hunks (diff --cc / diff --combined). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_diff_patch/index.html) |
| `grit_lib::combined_diff_patch::CombinedDiffWsOptions` | struct | Whitespace handling for combined diffs (Git xdl_opts subset). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_diff_patch/struct.CombinedDiffWsOptions.html) |
| `grit_lib::combined_tree_diff` | module | Multi-parent combined tree diff (Git diff_tree_paths / find_paths_multitree). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/index.html) |
| `grit_lib::combined_tree_diff::CombinedDiffPath` | struct | One path in a combined diff: merge result tree vs each parent tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/struct.CombinedDiffPath.html) |
| `grit_lib::combined_tree_diff::CombinedParentSide` | struct | One parent’s contribution at a combined-diff path. | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/struct.CombinedParentSide.html) |
| `grit_lib::combined_tree_diff::CombinedParentStatus` | enum | Per-parent coarse status in a combined diff (A / M / D). | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/enum.CombinedParentStatus.html) |
| `grit_lib::combined_tree_diff::CombinedTreeDiffOptions` | struct | Options for the multitree walk. | [API](https://docs.rs/grit-lib/latest/grit_lib/combined_tree_diff/struct.CombinedTreeDiffOptions.html) |
| `grit_lib::command_runner` | module | Injectable subprocess execution for hooks, filters, credentials, and helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/index.html) |
| `grit_lib::command_runner::CommandEnvironment` | struct | How a child process should inherit or receive environment variables. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandEnvironment.html) |
| `grit_lib::command_runner::CommandExit` | struct | Exit status of a completed child. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandExit.html) |
| `grit_lib::command_runner::CommandHandleError` | struct | Returned when an operation requires a live OS child but the handle is synthetic. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandHandleError.html) |
| `grit_lib::command_runner::CommandOutput` | struct | Output from RunningCommand::wait_with_output. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandOutput.html) |
| `grit_lib::command_runner::CommandRunner` | trait | Spawns subprocesses; the only production implementation uses std::process::Command. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/trait.CommandRunner.html) |
| `grit_lib::command_runner::CommandSpec` | struct | Typed description of a subprocess to spawn. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.CommandSpec.html) |
| `grit_lib::command_runner::CommandStdin` | enum | Stdin source for the child. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/enum.CommandStdin.html) |
| `grit_lib::command_runner::CommandStdio` | enum | Stdio disposition for a child stream. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/enum.CommandStdio.html) |
| `grit_lib::command_runner::RecordedResponse` | enum | Scripted responses for RecordingRunner. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/enum.RecordedResponse.html) |
| `grit_lib::command_runner::RecordingRunner` | struct | Records CommandSpec invocations and returns configurable exit statuses. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.RecordingRunner.html) |
| `grit_lib::command_runner::RunningCommand` | struct | Live child process with optional piped stdio. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.RunningCommand.html) |
| `grit_lib::command_runner::ShellInvocation` | enum | Shell invocation mode (Git sh -c hooks and filters). | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/enum.ShellInvocation.html) |
| `grit_lib::command_runner::SystemCommandRunner` | struct | Runs commands with the real OS process API. | [API](https://docs.rs/grit-lib/latest/grit_lib/command_runner/struct.SystemCommandRunner.html) |
| `grit_lib::commit` | module | Commit-metadata helpers shared by the porcelain commands. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit/index.html) |
| `grit_lib::commit_encoding` | module | Git commit encoding labels (encoding header, i18n.commitEncoding) mapped to codecs. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_encoding/index.html) |
| `grit_lib::commit_graph_file` | module | Parsing Git commit-graph files and Bloom filter lookup (commit-graph.c / bloom.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/index.html) |
| `grit_lib::commit_graph_file::BloomWalkStats` | struct | Counters for GIT_TRACE2_PERF Bloom statistics (revision.c trace2_bloom_filter_statistics_atexit). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/struct.BloomWalkStats.html) |
| `grit_lib::commit_graph_file::CommitGraphChain` | struct | Loaded commit-graph chain (newest layer first, matching commit-graph-chain file order). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/struct.CommitGraphChain.html) |
| `grit_lib::commit_graph_file::CommitGraphLayer` | struct | One layer from .git/objects/info/commit-graph or commit-graphs/<hash>.graph. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/struct.CommitGraphLayer.html) |
| `grit_lib::commit_graph_file::ParsedGraphDump` | struct | Result of consulting Bloom filters before running a tree diff (matches revision.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_file/struct.ParsedGraphDump.html) |
| `grit_lib::commit_graph_write` | module | Serialize Git commit-graph v1 files with GDA2 + optional Bloom chunks (commit-graph.c compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_write/index.html) |
| `grit_lib::commit_graph_write::BloomWriteStats` | struct | Counters emitted as GIT_TRACE2_EVENT for Bloom generation (commit-graph.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_write/struct.BloomWriteStats.html) |
| `grit_lib::commit_graph_write::CommitGraphCommitInfo` | struct | Per-commit data needed to write CDAT / Bloom. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_graph_write/struct.CommitGraphCommitInfo.html) |
| `grit_lib::commit_pretty` | module | Human-oriented commit one-line formats shared by porcelain commands. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_pretty/index.html) |
| `grit_lib::commit_trailers` | module | Cherry-pick / sign-off trailer handling compatible with Git’s sequencer.c and trailer.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_trailers/index.html) |
| `grit_lib::commit_trailers::TrailerOpts` | struct | Options parsed from a %(trailers:...) pretty placeholder, mirroring Git’s process_trailer_options in pretty.c / trailer.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/commit_trailers/struct.TrailerOpts.html) |
| `grit_lib::config` | module | Git-compatible configuration file parser and accessor. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/index.html) |
| `grit_lib::config::ConfigEntry` | struct | A single configuration entry with its origin metadata. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigEntry.html) |
| `grit_lib::config::ConfigFile` | struct | A parsed configuration file that preserves the raw text for round-trip editing (set/unset/rename-section/remove-section). | [API](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigFile.html) |
| `grit_lib::config::ConfigIncludeOrigin` | enum | Where a ConfigFile was loaded from for Git include semantics. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/enum.ConfigIncludeOrigin.html) |
| `grit_lib::config::ConfigScope` | enum | The scope (origin) of a configuration value. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/enum.ConfigScope.html) |
| `grit_lib::config::ConfigSet` | struct | A merged view across all configuration scopes. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) |
| `grit_lib::config::GitConfigIntStrictError` | enum | Why parse_git_config_int_strict failed (mirrors Git errno after git_parse_signed). | [API](https://docs.rs/grit-lib/latest/grit_lib/config/enum.GitConfigIntStrictError.html) |
| `grit_lib::config::IncludeContext` | struct | Context for evaluating [includeIf] conditions (gitdir:, onbranch:, hasconfig:). | [API](https://docs.rs/grit-lib/latest/grit_lib/config/struct.IncludeContext.html) |
| `grit_lib::config::LoadConfigOptions` | struct | Options controlling how ConfigSet::load_with_options merges files and includes. | [API](https://docs.rs/grit-lib/latest/grit_lib/config/struct.LoadConfigOptions.html) |
| `grit_lib::configuration` | module | Configuration and identity: config cascade, .gitmodules, author/committer idents. | [API](https://docs.rs/grit-lib/latest/grit_lib/configuration/index.html) |
| `grit_lib::connectivity` | module | Reachability checks for push / receive-pack connectivity verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/connectivity/index.html) |
| `grit_lib::credentials` | module | Credential layer — Git-compatible credential filling/approval/rejection for library embedders. | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/index.html) |
| `grit_lib::credentials::Credential` | struct | A structured Git credential. | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.Credential.html) |
| `grit_lib::credentials::CredentialProvider` | trait | The pluggable credential seam an embedder implements (or wraps). | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/trait.CredentialProvider.html) |
| `grit_lib::credentials::HelperCredentialProvider` | struct | Git-compatible CredentialProvider that runs the configured credential.helper programs. | [API](https://docs.rs/grit-lib/latest/grit_lib/credentials/struct.HelperCredentialProvider.html) |
| `grit_lib::crlf` | module | CRLF / EOL conversion and clean/smudge filter support. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/index.html) |
| `grit_lib::crlf::AttrRule` | struct | A parsed .gitattributes rule. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/struct.AttrRule.html) |
| `grit_lib::crlf::AutoCrlf` | enum | What core.autocrlf is set to. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.AutoCrlf.html) |
| `grit_lib::crlf::ConversionConfig` | struct | Global conversion settings derived from config. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/struct.ConversionConfig.html) |
| `grit_lib::crlf::ConversionError` | enum | Error from internal encoding helpers (not public convert_to_git surface). | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.ConversionError.html) |
| `grit_lib::crlf::ConvertToGitOpts` | struct | Optional inputs for convert_to_git_with_opts (Git CONV_EOL_RENORMALIZE / index blob). | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/struct.ConvertToGitOpts.html) |
| `grit_lib::crlf::CoreEol` | enum | What core.eol is set to. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.CoreEol.html) |
| `grit_lib::crlf::CrlfLegacyAttr` | enum | Legacy crlf gitattribute (deprecated in Git; still honored for EOL conversion). | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.CrlfLegacyAttr.html) |
| `grit_lib::crlf::DiffAttr` | enum | How the diff gitattribute affects diff output. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.DiffAttr.html) |
| `grit_lib::crlf::EolAttr` | enum | Per-file eol attribute from .gitattributes. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.EolAttr.html) |
| `grit_lib::crlf::FileAttrs` | struct | Per-file attributes relevant to conversion. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/struct.FileAttrs.html) |
| `grit_lib::crlf::MergeAttr` | enum | Per-file merge attribute from .gitattributes. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.MergeAttr.html) |
| `grit_lib::crlf::SafeCrlf` | enum | What core.safecrlf is set to. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.SafeCrlf.html) |
| `grit_lib::crlf::TextAttr` | enum | Per-file text attribute from .gitattributes. | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.TextAttr.html) |
| `grit_lib::crlf::WorkTreeEncodingError` | enum | Working-tree encoding conversion failure (Git reencode_string_len returning NULL). | [API](https://docs.rs/grit-lib/latest/grit_lib/crlf/enum.WorkTreeEncodingError.html) |
| `grit_lib::delta_encode` | module | Encode Git pack binary deltas (format decoded by crate::unpack_objects::apply_delta). | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_encode/index.html) |
| `grit_lib::delta_islands` | module | Delta islands — restrict cross-island deltas in pack-objects (--delta-islands). | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_islands/index.html) |
| `grit_lib::delta_islands::DeltaIslands` | struct | Computed island marks for a pack-objects run. | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_islands/struct.DeltaIslands.html) |
| `grit_lib::delta_islands::IslandBitmap` | struct | One island membership bitmap (one bit per deduplicated island). | [API](https://docs.rs/grit-lib/latest/grit_lib/delta_islands/struct.IslandBitmap.html) |
| `grit_lib::diagnostics` | module | Git-style diagnostic prefixes and typed warning sinks for embedders and the CLI. | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/index.html) |
| `grit_lib::diagnostics::CollectingDiagnostics` | struct | Collects warnings (and traces) for tests and the CLI. | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/struct.CollectingDiagnostics.html) |
| `grit_lib::diagnostics::DiagnosticSink` | trait | Receives warnings and optional trace lines from grit-lib. | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/trait.DiagnosticSink.html) |
| `grit_lib::diagnostics::NullDiagnostics` | struct | Discards all diagnostics (default for crate::repo::Repository). | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/struct.NullDiagnostics.html) |
| `grit_lib::diagnostics::Trace` | enum | Optional trace events (network debugging, etc.), separate from Warning. | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/enum.Trace.html) |
| `grit_lib::diagnostics::Warning` | enum | A non-fatal condition worth surfacing to the user or an embedder. | [API](https://docs.rs/grit-lib/latest/grit_lib/diagnostics/enum.Warning.html) |
| `grit_lib::diff` | module | Diff machinery — compare trees, index entries, and working tree files. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/index.html) |
| `grit_lib::diff::DiffEntry` | struct | A single diff entry representing one changed path. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/struct.DiffEntry.html) |
| `grit_lib::diff::DiffIndexToWorktreeOptions` | struct | Additional inputs for diff_index_to_worktree_with_options. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/struct.DiffIndexToWorktreeOptions.html) |
| `grit_lib::diff::DiffStatus` | enum | The kind of change between two sides of a diff. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/enum.DiffStatus.html) |
| `grit_lib::diff::SubmodulePorcelainFlags` | struct | Submodule dirty bits aligned with Git’s DIRTY_SUBMODULE_* / porcelain v2 S??? | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/struct.SubmodulePorcelainFlags.html) |
| `grit_lib::diff::WorktreeAddRefresh` | enum | Outcome of comparing a tracked index entry to its worktree path for staging refresh. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/enum.WorktreeAddRefresh.html) |
| `grit_lib::diff::WorktreeAddRefreshParams` | struct | Inputs for classify_worktree_entry_for_add. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff/struct.WorktreeAddRefreshParams.html) |
| `grit_lib::diff_indent_heuristic` | module | Git-compatible diff hunk sliding (xdl_change_compact) including the indent heuristic. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_indent_heuristic/index.html) |
| `grit_lib::diff_moved` | module | Move detection for git diff --color-moved, implementing the same behavior as git’s add_lines_to_move_detection + mark_color_as_moved + dim_moved_lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_moved/index.html) |
| `grit_lib::diff_moved::ColorMovedMode` | enum | --color-moved mode. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_moved/enum.ColorMovedMode.html) |
| `grit_lib::diff_moved::MovedClass` | enum | The color class assigned to one emitted line after move detection. | [API](https://docs.rs/grit-lib/latest/grit_lib/diff_moved/enum.MovedClass.html) |
| `grit_lib::diffing` | module | Diffing: tree/content diff, rename detection, diffstat, line-log, pickaxe bloom. | [API](https://docs.rs/grit-lib/latest/grit_lib/diffing/index.html) |
| `grit_lib::diffstat` | module | Git-compatible --stat / diffstat layout (width, name truncation, bar scaling). | [API](https://docs.rs/grit-lib/latest/grit_lib/diffstat/index.html) |
| `grit_lib::diffstat::DiffstatOptions` | struct | Options for laying out diffstat lines (Git diff_options stat fields). | [API](https://docs.rs/grit-lib/latest/grit_lib/diffstat/struct.DiffstatOptions.html) |
| `grit_lib::diffstat::FileStatInput` | struct | Default total width for format-patch diffstat (MAIL_DEFAULT_WRAP in Git). | [API](https://docs.rs/grit-lib/latest/grit_lib/diffstat/struct.FileStatInput.html) |
| `grit_lib::dotfile` | module | Git-compatible .git* / NTFS / HFS path checks (path.c, utf8.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/dotfile/index.html) |
| `grit_lib::environment` | module | Explicit discovery and configuration environment for embedding callers. | [API](https://docs.rs/grit-lib/latest/grit_lib/environment/index.html) |
| `grit_lib::environment::Environment` | struct | Discovery and config variables that affect repository open/discover and ConfigSet loading. | [API](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.Environment.html) |
| `grit_lib::environment::RepositoryOptions` | struct | Options passed to repository open/discover with an explicit Environment. | [API](https://docs.rs/grit-lib/latest/grit_lib/environment/struct.RepositoryOptions.html) |
| `grit_lib::error` | module | Shared error types for grit-lib. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/index.html) |
| `grit_lib::error::BadNumericSource` | enum | Why a Git config numeric value was rejected (git config int strict parsing). | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.BadNumericSource.html) |
| `grit_lib::error::ConfigError` | enum | Configuration parse and validation failures. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.ConfigError.html) |
| `grit_lib::error::DiffPathError` | enum | Path lookup failures while formatting a diff. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.DiffPathError.html) |
| `grit_lib::error::Error` | enum | The top-level error type for all grit-lib operations. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.Error.html) |
| `grit_lib::error::FilterError` | enum | Clean/smudge filter and EOL conversion failures. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.FilterError.html) |
| `grit_lib::error::FilterPhase` | enum | Which filter hook failed. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.FilterPhase.html) |
| `grit_lib::error::PathOutsideRepoError` | enum | Path outside the repository during pathspec or transport resolution. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.PathOutsideRepoError.html) |
| `grit_lib::error::RefLockError` | enum | Reference lock failures while creating or updating a ref. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.RefLockError.html) |
| `grit_lib::error::SparseCheckoutWarning` | enum | Sparse-checkout cone pattern parse warnings (non-fatal). | [API](https://docs.rs/grit-lib/latest/grit_lib/error/enum.SparseCheckoutWarning.html) |
| `grit_lib::fetch` | module | Wire-protocol fetch orchestration over a crate::transport::Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/index.html) |
| `grit_lib::fetch::NoProgress` | struct | A Progress that discards everything. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/struct.NoProgress.html) |
| `grit_lib::fetch::Progress` | trait | Sink for the remote’s human-readable progress (side-band channel 2). | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch/trait.Progress.html) |
| `grit_lib::fetch_head` | module | Parsing FETCH_HEAD lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_head/index.html) |
| `grit_lib::fetch_negotiator` | module | Skipping fetch negotiator — implements Git’s “skipping” negotiation strategy. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_negotiator/index.html) |
| `grit_lib::fetch_negotiator::SkippingNegotiator` | struct | Skipping algorithm negotiator for fetch have lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_negotiator/struct.SkippingNegotiator.html) |
| `grit_lib::fetch_submodules` | module | Logic for git fetch --recurse-submodules (changed-submodule detection and config). | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_submodules/index.html) |
| `grit_lib::fetch_submodules::ChangedSubmoduleFetch` | struct | One submodule that gained new gitlink targets in rev-list <tips> --not <neg>. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_submodules/struct.ChangedSubmoduleFetch.html) |
| `grit_lib::fetch_submodules::FetchRecurseSubmodules` | enum | fetch.recurseSubmodules / --recurse-submodules modes for fetch. | [API](https://docs.rs/grit-lib/latest/grit_lib/fetch_submodules/enum.FetchRecurseSubmodules.html) |
| `grit_lib::filter_process` | module | Long-running Git filter protocol (filter.<name>.process), matching git-filter v2. | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/index.html) |
| `grit_lib::filter_process::DelayedCheckoutError` | enum | Failure from DelayedProcessCheckout::finish. | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/enum.DelayedCheckoutError.html) |
| `grit_lib::filter_process::DelayedCheckoutProblem` | enum | One delayed-checkout outcome that would have been printed as Git’s error: ... | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/enum.DelayedCheckoutProblem.html) |
| `grit_lib::filter_process::DelayedProcessCheckout` | struct | Paths waiting for list_available_blobs / retry smudge (Git finish_delayed_checkout). | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/struct.DelayedProcessCheckout.html) |
| `grit_lib::filter_process::DelayedProcessCheckoutEntry` | struct | One path deferred by a process filter that returned status=delayed (Git delayed_checkout). | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/struct.DelayedProcessCheckoutEntry.html) |
| `grit_lib::filter_process::FilterProcessState` | struct | Per-repository filter-process registry (long-running filter.*.process drivers). | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/struct.FilterProcessState.html) |
| `grit_lib::filter_process::FilterSmudgeMeta` | struct | Optional metadata sent with smudge (ref, treeish, blob hex). | [API](https://docs.rs/grit-lib/latest/grit_lib/filter_process/struct.FilterSmudgeMeta.html) |
| `grit_lib::fmt_merge_msg` | module | Merge commit message formatter — git fmt-merge-msg logic. | [API](https://docs.rs/grit-lib/latest/grit_lib/fmt_merge_msg/index.html) |
| `grit_lib::fmt_merge_msg::FmtMergeMsgOptions` | struct | Options for fmt_merge_msg. | [API](https://docs.rs/grit-lib/latest/grit_lib/fmt_merge_msg/struct.FmtMergeMsgOptions.html) |
| `grit_lib::fsck_standalone` | module | Standalone object fsck for loose-object validation before hashing. | [API](https://docs.rs/grit-lib/latest/grit_lib/fsck_standalone/index.html) |
| `grit_lib::fsck_standalone::FsckError` | struct | Git-compatible fsck failure for loose object validation. | [API](https://docs.rs/grit-lib/latest/grit_lib/fsck_standalone/struct.FsckError.html) |
| `grit_lib::fsck_standalone::FsckObjectOptions` | struct | Repository hash width for object-id fields in commit, tag, and tree headers. | [API](https://docs.rs/grit-lib/latest/grit_lib/fsck_standalone/struct.FsckObjectOptions.html) |
| `grit_lib::gc` | module | In-process maintenance primitives that embedders such as jj use in place of shelling out to git gc / git remote show / gix::refs::transaction. | [API](https://docs.rs/grit-lib/latest/grit_lib/gc/index.html) |
| `grit_lib::gc::PruneStats` | struct | Result of a loose-object prune. | [API](https://docs.rs/grit-lib/latest/grit_lib/gc/struct.PruneStats.html) |
| `grit_lib::gc::RefTransactionItem` | struct | A single ref change in an update_refs batch transaction. | [API](https://docs.rs/grit-lib/latest/grit_lib/gc/struct.RefTransactionItem.html) |
| `grit_lib::git_binary_base85` | module | Base-85 codec for GIT binary patch sections (matches git/base85.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_binary_base85/index.html) |
| `grit_lib::git_binary_base85::DecodeError` | enum | Errors returned while decoding a Git binary-patch base85 line. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_binary_base85/enum.DecodeError.html) |
| `grit_lib::git_date` | module | Git-compatible date parsing and display. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/index.html) |
| `grit_lib::git_date::TestToolDateResult` | enum | Result of test-tool date — either lines for stdout or a process exit code (no output). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/enum.TestToolDateResult.html) |
| `grit_lib::git_date::approx` | module | Git-compatible approxidate parsing. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/approx/index.html) |
| `grit_lib::git_date::parse` | module | Git-compatible date parsing (parse_date_basic, parse_date). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/parse/index.html) |
| `grit_lib::git_date::parse::DateParseError` | struct | Date string could not be parsed. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/parse/struct.DateParseError.html) |
| `grit_lib::git_date::show` | module | Git-compatible date display (show_date, show_date_relative, strftime handling). | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/show/index.html) |
| `grit_lib::git_date::tm` | module | Time conversion helpers that produce Git-compatible timestamps. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_date/tm/index.html) |
| `grit_lib::git_path` | module | Git-compatible path normalization and helpers for test-tool path-utils. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_path/index.html) |
| `grit_lib::git_path::GitPathError` | enum | Errors returned by Git-compatible path helper routines. | [API](https://docs.rs/grit-lib/latest/grit_lib/git_path/enum.GitPathError.html) |
| `grit_lib::gitmodules` | module | .gitmodules validation (Git fsck / submodule-config parity). | [API](https://docs.rs/grit-lib/latest/grit_lib/gitmodules/index.html) |
| `grit_lib::gitmodules::DotFsckIssue` | enum | Problems reported while walking trees / blobs for .gitmodules / .gitattributes fsck. | [API](https://docs.rs/grit-lib/latest/grit_lib/gitmodules/enum.DotFsckIssue.html) |
| `grit_lib::gitmodules::DotFsckTracker` | struct | Tracks .gitmodules / .gitattributes blob OIDs discovered in trees (Git fsck_options oidsets). | [API](https://docs.rs/grit-lib/latest/grit_lib/gitmodules/struct.DotFsckTracker.html) |
| `grit_lib::hash` | module | Incremental hashing for Git object ids and file trailers. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/index.html) |
| `grit_lib::hash::Backend` | enum | Which implementation the sha1 / sha2 dependency selects for algo on this CPU. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.Backend.html) |
| `grit_lib::hash::HashingWriter` | struct | A writer that forwards bytes to an inner sink while accumulating a checksum. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/struct.HashingWriter.html) |
| `grit_lib::hash::ObjectHasher` | enum | Incremental hasher for Git’s supported object-id algorithms. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.ObjectHasher.html) |
| `grit_lib::hash::ParallelHashError` | enum | Failure from try_par_hash_with when a closure returns an error. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/enum.ParallelHashError.html) |
| `grit_lib::hash::Parallelism` | struct | Resolved worker thread count for parallel hash helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/struct.Parallelism.html) |
| `grit_lib::hash::TrailerMismatch` | struct | Checksum at the end of bytes did not match a digest of the leading body. | [API](https://docs.rs/grit-lib/latest/grit_lib/hash/struct.TrailerMismatch.html) |
| `grit_lib::hide_refs` | module | transfer.hideRefs / receive.hideRefs / uploadpack.hideRefs matching (Git ref_is_hidden). | [API](https://docs.rs/grit-lib/latest/grit_lib/hide_refs/index.html) |
| `grit_lib::hooks` | module | Hook execution utilities. | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/index.html) |
| `grit_lib::hooks::CommitHookEnv` | struct | Environment for commit-style hooks (GIT_INDEX_FILE, GIT_EDITOR, GIT_PREFIX, and extra pairs). | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/struct.CommitHookEnv.html) |
| `grit_lib::hooks::HookError` | enum | Hook subprocess failure (non-zero exit or spawn error). | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/enum.HookError.html) |
| `grit_lib::hooks::HookResult` | enum | Result of running a hook. | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/enum.HookResult.html) |
| `grit_lib::hooks::RunHookOptions` | struct | Options for run_hook_opts. | [API](https://docs.rs/grit-lib/latest/grit_lib/hooks/struct.RunHookOptions.html) |
| `grit_lib::ident` | module | Git author/committer identity lines (ident in Git’s fsck.c / commit.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident/index.html) |
| `grit_lib::ident::ParsedSignatureTimes` | struct | Successful parse of the trailing <unix> <+HHMM> portion of a signature. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident/struct.ParsedSignatureTimes.html) |
| `grit_lib::ident::SignatureTail` | enum | Distinguishes a non-numeric date field from a numeric field that fails Git’s overflow rules. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident/enum.SignatureTail.html) |
| `grit_lib::ident::SignatureTimestamp` | enum | Parsed timestamp from a signature line for display and filtering. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident/enum.SignatureTimestamp.html) |
| `grit_lib::ident_config` | module | Default identity values from config and the system (Git ident.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_config/index.html) |
| `grit_lib::ident_resolve` | module | Git-compatible author/committer identity resolution (see upstream ident.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/index.html) |
| `grit_lib::ident_resolve::GitIdentityNameEnv` | enum | Whether GIT_AUTHOR_NAME / GIT_COMMITTER_NAME is unset vs set (possibly empty). | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/enum.GitIdentityNameEnv.html) |
| `grit_lib::ident_resolve::IdentRole` | enum | Author vs committer for GIT_* / author.* / committer.* lookup. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/enum.IdentRole.html) |
| `grit_lib::ident_resolve::IdentityEnv` | trait | Environment access used for identity resolution. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/trait.IdentityEnv.html) |
| `grit_lib::ident_resolve::IdentityError` | enum | Errors returned by strict identity resolution. | [API](https://docs.rs/grit-lib/latest/grit_lib/ident_resolve/enum.IdentityError.html) |
| `grit_lib::ignore` | module | Ignore and exclude matching for check-ignore. | [API](https://docs.rs/grit-lib/latest/grit_lib/ignore/index.html) |
| `grit_lib::ignore::IgnoreMatch` | struct | Metadata for a matching rule. | [API](https://docs.rs/grit-lib/latest/grit_lib/ignore/struct.IgnoreMatch.html) |
| `grit_lib::ignore::IgnoreMatcher` | struct | Engine used to evaluate ignore patterns against repository-relative paths. | [API](https://docs.rs/grit-lib/latest/grit_lib/ignore/struct.IgnoreMatcher.html) |
| `grit_lib::index` | module | Git index (staging area) reading and writing. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/index.html) |
| `grit_lib::index::CacheTreeNode` | struct | One node from Git’s TREE index extension. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.CacheTreeNode.html) |
| `grit_lib::index::Index` | struct | The in-memory representation of the Git index file. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html) |
| `grit_lib::index::IndexEntry` | struct | A single entry in the Git index. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.IndexEntry.html) |
| `grit_lib::index::IndexLoadOptions` | struct | Options for loading an index from disk. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.IndexLoadOptions.html) |
| `grit_lib::index::IndexStatFields` | struct | Stat fields copied from a worktree Metadata probe during index staging. | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.IndexStatFields.html) |
| `grit_lib::index::StatMatchPolicy` | struct | Git match_stat_data policy (core.trustctime, core.checkstat, core.usenanosec). | [API](https://docs.rs/grit-lib/latest/grit_lib/index/struct.StatMatchPolicy.html) |
| `grit_lib::index_name_hash_lazy` | module | Lazy, optionally multi-threaded index name/dir hash initialization compatible with Git’s name-hash.c (used by test-tool lazy-init-name-hash and regression test t3008). | [API](https://docs.rs/grit-lib/latest/grit_lib/index_name_hash_lazy/index.html) |
| `grit_lib::index_pack` | module | Install a received packfile into the object store (index-pack path). | [API](https://docs.rs/grit-lib/latest/grit_lib/index_pack/index.html) |
| `grit_lib::index_pack::IngestPackOptions` | struct | Options controlling how a received pack is ingested. | [API](https://docs.rs/grit-lib/latest/grit_lib/index_pack/struct.IngestPackOptions.html) |
| `grit_lib::init_filesystem` | module | Filesystem capability probes during repository initialization. | [API](https://docs.rs/grit-lib/latest/grit_lib/init_filesystem/index.html) |
| `grit_lib::init_filesystem::InitFilesystemConfigOptions` | struct | Controls when init-time filesystem probes may write local config. | [API](https://docs.rs/grit-lib/latest/grit_lib/init_filesystem/struct.InitFilesystemConfigOptions.html) |
| `grit_lib::interpret_trailers` | module | Commit message trailer parsing and rewriting (Git-compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/interpret_trailers/index.html) |
| `grit_lib::interpret_trailers::NewTrailerArg` | struct | Placement of a new trailer relative to an anchor trailer. | [API](https://docs.rs/grit-lib/latest/grit_lib/interpret_trailers/struct.NewTrailerArg.html) |
| `grit_lib::line_log` | module | Line-level history (git log -L) — range tracking across diffs. | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/index.html) |
| `grit_lib::line_log::DiffHunk` | struct | One aligned parent/target hunk from a line diff (half-open line indices). | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.DiffHunk.html) |
| `grit_lib::line_log::DiffRanges` | struct | Sequence of hunks from collect_diff_ranges (parent[i] aligns with target[i]). | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.DiffRanges.html) |
| `grit_lib::line_log::LineLogDisplay` | struct | Captured diff input for format_line_log_diff (one file per hunk group). | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.LineLogDisplay.html) |
| `grit_lib::line_log::LineLogFile` | struct | One tracked file with 0-based half-open line ranges. | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.LineLogFile.html) |
| `grit_lib::line_log::Range` | struct | Half-open line range using 0-based indices (Git internal). | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.Range.html) |
| `grit_lib::line_log::RangeSet` | struct | Sorted, disjoint, non-empty ranges. | [API](https://docs.rs/grit-lib/latest/grit_lib/line_log/struct.RangeSet.html) |
| `grit_lib::mailmap` | module | Parse .mailmap and resolve author/committer identities (Git-compatible). | [API](https://docs.rs/grit-lib/latest/grit_lib/mailmap/index.html) |
| `grit_lib::mailmap::MailmapEntry` | struct | Legacy line-shaped entry kept for API compatibility; prefer MailmapTable. | [API](https://docs.rs/grit-lib/latest/grit_lib/mailmap/struct.MailmapEntry.html) |
| `grit_lib::mailmap::MailmapTable` | struct | Parsed mailmap as a lookup table (Git string_list + nested namemap). | [API](https://docs.rs/grit-lib/latest/grit_lib/mailmap/struct.MailmapTable.html) |
| `grit_lib::merge_base` | module | Merge-base and reachability primitives. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_base/index.html) |
| `grit_lib::merge_base::MergeBaseForDiffError` | enum | Failure modes for merge_base_for_diff_index and merge_base_for_diff_two_commits. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_base/enum.MergeBaseForDiffError.html) |
| `grit_lib::merge_base::ReachableWalkLimit` | enum | Commits reachable from tips that are not ancestors of any hide tip, in committer-date order (newest first). | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_base/enum.ReachableWalkLimit.html) |
| `grit_lib::merge_diff` | module | Merge commit and combined (--cc / -c) diff helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_diff/index.html) |
| `grit_lib::merge_file` | module | Three-way file merge — the engine behind grit merge-file. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/index.html) |
| `grit_lib::merge_file::ConflictStyle` | enum | Conflict-marker output style. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/enum.ConflictStyle.html) |
| `grit_lib::merge_file::MergeFavor` | enum | How conflict regions should be resolved. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/enum.MergeFavor.html) |
| `grit_lib::merge_file::MergeInput` | struct | Input and options for a three-way merge. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/struct.MergeInput.html) |
| `grit_lib::merge_file::MergeOutput` | struct | Result of a three-way merge. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_file/struct.MergeOutput.html) |
| `grit_lib::merge_trees` | module | Rename-aware three-way tree merge for cherry-pick / revert style merges. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_trees/index.html) |
| `grit_lib::merge_trees::TreeMergeConflictPresentation` | struct | Labels and marker style for conflict output during tree merges. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_trees/struct.TreeMergeConflictPresentation.html) |
| `grit_lib::merge_trees::TreeMergeOutput` | struct | Result of merging three trees with optional conflict-marker blobs for checkout. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_trees/struct.TreeMergeOutput.html) |
| `grit_lib::merge_trees::WhitespaceMergeOptions` | struct | How to label the “theirs” side in textual conflict markers. | [API](https://docs.rs/grit-lib/latest/grit_lib/merge_trees/struct.WhitespaceMergeOptions.html) |
| `grit_lib::merging` | module | Merging: merge-base, tree/file merges, rerere, merge-message formatting. | [API](https://docs.rs/grit-lib/latest/grit_lib/merging/index.html) |
| `grit_lib::midx` | module | Multi-pack-index (MIDX) file writing and minimal reading. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/index.html) |
| `grit_lib::midx::CompactError` | enum | Failure modes of compact_multi_pack_index, each mapping to one of git’s user-facing diagnostics in cmd_multi_pack_index_compact. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/enum.CompactError.html) |
| `grit_lib::midx::MidxBtmpPackRange` | struct | One pack’s slice of the MIDX pseudo-bitmap namespace (BTMP chunk). | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/struct.MidxBtmpPackRange.html) |
| `grit_lib::midx::MidxObjectRef` | struct | A single MIDX-referenced object together with the pack it is attributed to. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/struct.MidxObjectRef.html) |
| `grit_lib::midx::MidxReuseTables` | struct | OID rows from the active multi-pack-index, plus reverse-index order for pack-reuse bitmap bits. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/struct.MidxReuseTables.html) |
| `grit_lib::midx::WriteMultiPackIndexOptions` | struct | Options for writing a multi-pack index (extension of the simple writer). | [API](https://docs.rs/grit-lib/latest/grit_lib/midx/struct.WriteMultiPackIndexOptions.html) |
| `grit_lib::midx_error` | module | Typed errors for multi-pack-index load and verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/midx_error/index.html) |
| `grit_lib::midx_error::MidxError` | enum | Unified diff / patch application errors. | [API](https://docs.rs/grit-lib/latest/grit_lib/error/../midx_error/enum.MidxError.html) |
| `grit_lib::midx_error::MidxError` | enum | A fatal multi-pack-index condition (Git die() after error: lines). | [API](https://docs.rs/grit-lib/latest/grit_lib/midx_error/enum.MidxError.html) |
| `grit_lib::name_rev` | module | Name-rev: name commits relative to refs. | [API](https://docs.rs/grit-lib/latest/grit_lib/name_rev/index.html) |
| `grit_lib::name_rev::NameRevOptions` | struct | Options that control which refs participate in naming. | [API](https://docs.rs/grit-lib/latest/grit_lib/name_rev/struct.NameRevOptions.html) |
| `grit_lib::net_trace` | module | Network operation tracing through crate::diagnostics::Trace::Network. | [API](https://docs.rs/grit-lib/latest/grit_lib/net_trace/index.html) |
| `grit_lib::notes` | module | git notes tree manipulation — the fanout tree mapping object -> note blob. | [API](https://docs.rs/grit-lib/latest/grit_lib/notes/index.html) |
| `grit_lib::notes::NotesTreeEntry` | struct | Per-worktree subdirectory holding the conflicted note blobs during a notes merge. | [API](https://docs.rs/grit-lib/latest/grit_lib/notes/struct.NotesTreeEntry.html) |
| `grit_lib::object_store` | module | Object storage: ids/kinds, the object database, packs, multi-pack index, deltas. | [API](https://docs.rs/grit-lib/latest/grit_lib/object_store/index.html) |
| `grit_lib::objects` | module | Git object model: object IDs, kinds, and in-memory representations. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/index.html) |
| `grit_lib::objects::CommitData` | struct | Parsed representation of a commit object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.CommitData.html) |
| `grit_lib::objects::HashAlgo` | enum | A Git hash algorithm. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/enum.HashAlgo.html) |
| `grit_lib::objects::HashVersionError` | struct | Error when converting an on-disk hash-version byte to HashAlgo. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.HashVersionError.html) |
| `grit_lib::objects::Object` | struct | A decompressed, header-stripped Git object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.Object.html) |
| `grit_lib::objects::ObjectId` | struct | A Git object identifier: a SHA-1 (20-byte) or SHA-256 (32-byte) digest. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectId.html) |
| `grit_lib::objects::ObjectInfo` | struct | Kind and uncompressed size of a Git object without loading its payload. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.ObjectInfo.html) |
| `grit_lib::objects::ObjectKind` | enum | The four Git object types. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/enum.ObjectKind.html) |
| `grit_lib::objects::TagData` | struct | Parsed representation of an annotated tag object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.TagData.html) |
| `grit_lib::objects::TreeEntry` | struct | A single entry in a Git tree object. | [API](https://docs.rs/grit-lib/latest/grit_lib/objects/struct.TreeEntry.html) |
| `grit_lib::odb` | module | Loose object database: reading and writing zlib-compressed Git objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/index.html) |
| `grit_lib::odb::LooseStore` | struct | On-disk loose object store for one objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.LooseStore.html) |
| `grit_lib::odb::LooseStore` | struct | Loose-object storage under objects/xx/<suffix> (zlib-compressed Git objects). | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/../struct.LooseStore.html) |
| `grit_lib::odb::MemoryStore` | struct | In-memory object map keyed by ObjectId. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.MemoryStore.html) |
| `grit_lib::odb::ObjectStore` | trait | Read-only object storage: lookup, metadata, streaming, and enumeration. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/trait.ObjectStore.html) |
| `grit_lib::odb::Odb` | struct | A loose-object database rooted at a given objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.Odb.html) |
| `grit_lib::odb::OdbBuilder` | struct | Configure and construct an Odb with a pluggable primary backend. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.OdbBuilder.html) |
| `grit_lib::odb::WritableObjectStore` | trait | Object storage that supports inserting new objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/trait.WritableObjectStore.html) |
| `grit_lib::odb::WriteOptions` | struct | Options for Odb::write_with_options. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/struct.WriteOptions.html) |
| `grit_lib::odb::builder` | module | Build an Odb with a custom primary store and optional read overlays. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/builder/index.html) |
| `grit_lib::odb::builder::OdbBuilder` | struct | Configure and construct an Odb with a pluggable primary backend. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/builder/struct.OdbBuilder.html) |
| `grit_lib::odb::store` | module | Pluggable object storage backends and the ObjectStore trait surface. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/index.html) |
| `grit_lib::odb::store::CompositeStore` | struct | Build an Odb with a custom primary store and optional read overlays. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.CompositeStore.html) |
| `grit_lib::odb::store::CompositeStore` | struct | Read-only store that consults an ordered list of backends. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.CompositeStore.html) |
| `grit_lib::odb::store::FilesSource` | struct | Read/write object storage backed by one objects/ directory tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.FilesSource.html) |
| `grit_lib::odb::store::MemoryStore` | struct | In-memory object map keyed by ObjectId. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.MemoryStore.html) |
| `grit_lib::odb::store::MidxObjects` | struct | MIDX-backed read-only ObjectStore for one objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.MidxObjects.html) |
| `grit_lib::odb::store::MidxObjectsStatus` | enum | Whether MidxObjects can serve reads for this repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/enum.MidxObjectsStatus.html) |
| `grit_lib::odb::store::ObjectStore` | trait | Read-only object storage: lookup, metadata, streaming, and enumeration. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.ObjectStore.html) |
| `grit_lib::odb::store::ObjectStream` | struct | Streaming handle for a single object’s uncompressed payload. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.ObjectStream.html) |
| `grit_lib::odb::store::PackFilter` | struct | Which local pack indexes participate in filtered membership checks. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.PackFilter.html) |
| `grit_lib::odb::store::PackedObjects` | struct | Pack-index-backed read-only ObjectStore for one objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/struct.PackedObjects.html) |
| `grit_lib::odb::store::WritableObjectStore` | trait | Object storage that supports inserting new objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/trait.WritableObjectStore.html) |
| `grit_lib::odb::store::loose` | module | Loose-object storage under objects/xx/<suffix> (zlib-compressed Git objects). | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/loose/index.html) |
| `grit_lib::odb::store::loose::LooseStore` | struct | On-disk loose object store for one objects/ directory. | [API](https://docs.rs/grit-lib/latest/grit_lib/odb/store/loose/struct.LooseStore.html) |
| `grit_lib::pack` | module | Pack and pack-index helpers for object counting and verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/index.html) |
| `grit_lib::pack::LocalPackInfo` | struct | Basic information about local packs. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.LocalPackInfo.html) |
| `grit_lib::pack::PackData` | struct | Immutable bytes of a .pack or MIDX file. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.PackData.html) |
| `grit_lib::pack::PackIndex` | struct | Parsed pack index with fanout-accelerated OID lookup and in-place table reads. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.PackIndex.html) |
| `grit_lib::pack::PackIndexEntry` | struct | Owned pack index row (collect from PackIndex::iter when needed). | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.PackIndexEntry.html) |
| `grit_lib::pack::PackIndexEntryRef` | struct | Borrowed view of one row in a pack index. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.PackIndexEntryRef.html) |
| `grit_lib::pack::PackLookupOptions` | struct | Options controlling which local packs participate in a lookup pass. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.PackLookupOptions.html) |
| `grit_lib::pack::PackedDeltaDependency` | enum | Dependency of a packed delta object at object_offset within pack_bytes. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/enum.PackedDeltaDependency.html) |
| `grit_lib::pack::PackedType` | enum | A pack object type as encoded in the packed stream header. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/enum.PackedType.html) |
| `grit_lib::pack::VerifyObjectRecord` | struct | A decoded object header record used by verify-pack. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack/struct.VerifyObjectRecord.html) |
| `grit_lib::pack_name_hash` | module | Git pack bitmap name-hash functions (pack_name_hash / pack_name_hash_v2). | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_name_hash/index.html) |
| `grit_lib::pack_receive` | module | Stream side-band pack data from fetch/clone into memory or a pack temp file. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_receive/index.html) |
| `grit_lib::pack_receive::PackReceiveTarget` | enum | Where demuxed pack bytes are stored. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_receive/enum.PackReceiveTarget.html) |
| `grit_lib::pack_receive::TempPackReceive` | struct | A temp pack file under objects/pack/ for streaming clone/fetch receive. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_receive/struct.TempPackReceive.html) |
| `grit_lib::pack_rev` | module | On-disk pack reverse index (.rev) — RIDX format matching Git’s pack-write.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_rev/index.html) |
| `grit_lib::pack_store` | module | Repository-scoped pack and MIDX read caches. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_store/index.html) |
| `grit_lib::pack_store::PackStore` | struct | Per-objects/ cache of pack indexes, bytes, MIDX views, and delta-base LRU state. | [API](https://docs.rs/grit-lib/latest/grit_lib/pack_store/struct.PackStore.html) |
| `grit_lib::patch_ids` | module | Patch-ID computation for commit equivalence detection. | [API](https://docs.rs/grit-lib/latest/grit_lib/patch_ids/index.html) |
| `grit_lib::patch_ids::PatchIdMode` | enum | How to compute a patch-ID from unified diff text. | [API](https://docs.rs/grit-lib/latest/grit_lib/patch_ids/enum.PatchIdMode.html) |
| `grit_lib::path` | module | Git-compatible verification of tree/index path components. | [API](https://docs.rs/grit-lib/latest/grit_lib/path/index.html) |
| `grit_lib::path::PathProtection` | struct | Path protection settings from core.protectHFS / core.protectNTFS. | [API](https://docs.rs/grit-lib/latest/grit_lib/path/struct.PathProtection.html) |
| `grit_lib::path_icase` | module | Case-insensitive path identity when core.ignorecase is enabled. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_icase/index.html) |
| `grit_lib::path_icase::Stage0IcasePathMap` | struct | Map from case-folded path hash buckets to index spellings (built once per staging batch). | [API](https://docs.rs/grit-lib/latest/grit_lib/path_icase/struct.Stage0IcasePathMap.html) |
| `grit_lib::path_icase::Stage0TrackedPaths` | struct | Stage-0 tracked paths for O(1) average lookups during untracked scans. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_icase/struct.Stage0TrackedPaths.html) |
| `grit_lib::path_walk` | module | Path-batched object graph walk matching Git’s walk_objects_by_path / test-tool path-walk. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_walk/index.html) |
| `grit_lib::path_walk::PathWalkCounts` | struct | One line of test-tool path-walk output (excluding trailing summary lines). | [API](https://docs.rs/grit-lib/latest/grit_lib/path_walk/struct.PathWalkCounts.html) |
| `grit_lib::path_walk::PathWalkOptions` | struct | Options for walk_objects_by_path, aligned with Git’s struct path_walk_info. | [API](https://docs.rs/grit-lib/latest/grit_lib/path_walk/struct.PathWalkOptions.html) |
| `grit_lib::pathspec` | module | Git-compatible pathspec matching (magic tokens and global flags). | [API](https://docs.rs/grit-lib/latest/grit_lib/pathspec/index.html) |
| `grit_lib::pathspec::PathOutsideRepository` | struct | Resolved path lies outside the repository work tree (Git prefix_path_gently failure). | [API](https://docs.rs/grit-lib/latest/grit_lib/pathspec/struct.PathOutsideRepository.html) |
| `grit_lib::pathspec::PathspecMatchContext` | struct | Optional path metadata for literal pathspecs with a trailing / (tree-walk / diff-tree). | [API](https://docs.rs/grit-lib/latest/grit_lib/pathspec/struct.PathspecMatchContext.html) |
| `grit_lib::pkt_line` | module | Git pkt-line format helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/pkt_line/index.html) |
| `grit_lib::pkt_line::Packet` | enum | A single packet read from the wire. | [API](https://docs.rs/grit-lib/latest/grit_lib/pkt_line/enum.Packet.html) |
| `grit_lib::plumbing` | module | Plumbing operations — low-level, machine-stable building blocks that map ~1:1 to a Git plumbing command (e.g. | [API](https://docs.rs/grit-lib/latest/grit_lib/plumbing/index.html) |
| `grit_lib::porcelain` | module | Porcelain operations — user-facing engines that assemble a structured result model from plumbing pieces (e.g. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/index.html) |
| `grit_lib::porcelain::add` | module | Stage working-tree changes into the index (git add). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/index.html) |
| `grit_lib::porcelain::add::StageMode` | enum | What paths stage should update (git add vs git add -u). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/enum.StageMode.html) |
| `grit_lib::porcelain::add::StageOptions` | struct | Inputs for stage. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/struct.StageOptions.html) |
| `grit_lib::porcelain::add::StageOutcome` | struct | Counts returned by stage. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/add/struct.StageOutcome.html) |
| `grit_lib::porcelain::checkout` | module | git checkout worktree-apply primitives. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/checkout/index.html) |
| `grit_lib::porcelain::cherry_pick` | module | git cherry-pick / git revert pick-engine core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/cherry_pick/index.html) |
| `grit_lib::porcelain::cherry_pick::WhitespaceStrategyOptions` | struct | Whitespace-handling flags parsed from -X<option> merge-strategy options. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/cherry_pick/struct.WhitespaceStrategyOptions.html) |
| `grit_lib::porcelain::commit` | module | Create a commit from the current index and move the checked-out branch. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/commit/index.html) |
| `grit_lib::porcelain::commit::CommitOutcome` | struct | Result of create_commit. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/commit/struct.CommitOutcome.html) |
| `grit_lib::porcelain::commit::CommitRequest` | struct | Inputs for create_commit. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/commit/struct.CommitRequest.html) |
| `grit_lib::porcelain::log` | module | git log ref decoration as structured data. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/log/index.html) |
| `grit_lib::porcelain::log::DecorationFilter` | struct | The --decorate-refs / --decorate-refs-exclude / log.excludeDecoration ref filter. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/log/struct.DecorationFilter.html) |
| `grit_lib::porcelain::log::DecorationItem` | struct | One ref (or synthetic label) attached to a commit for --decorate / %d. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/log/struct.DecorationItem.html) |
| `grit_lib::porcelain::log::DecorationKind` | enum | Decoration category for git log --decorate colouring (color.decorate.*). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/log/enum.DecorationKind.html) |
| `grit_lib::porcelain::merge` | module | git merge index/head algorithm core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/merge/index.html) |
| `grit_lib::porcelain::rebase` | module | git rebase todo-list model and squash/fixup message assembly. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/rebase/index.html) |
| `grit_lib::porcelain::rebase::FixupMessageMode` | enum | Whether a fixup -C/fixup -c step uses the replaced commit message verbatim or opens an editor to amend it. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/rebase/enum.FixupMessageMode.html) |
| `grit_lib::porcelain::rebase::RebaseTodoCmd` | enum | A linear interactive-rebase todo command keyword. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/rebase/enum.RebaseTodoCmd.html) |
| `grit_lib::porcelain::revert` | module | git revert pick-engine core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/revert/index.html) |
| `grit_lib::porcelain::stage_tracked` | module | Stage modifications and deletions of already-tracked paths (git commit -a / -a staging). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stage_tracked/index.html) |
| `grit_lib::porcelain::stage_tracked::StageTrackedSummary` | struct | Summary of paths updated while staging tracked modifications/deletions. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stage_tracked/struct.StageTrackedSummary.html) |
| `grit_lib::porcelain::staging` | module | Stage worktree changes into the index (grit add / commit -a engine). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/staging/index.html) |
| `grit_lib::porcelain::stash` | module | git stash apply core, plus the tree-flattening and worktree-mutation primitives the stash engine is built on. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stash/index.html) |
| `grit_lib::porcelain::stash::FlatTreeEntry` | struct | A single blob entry from a recursively flattened tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/stash/struct.FlatTreeEntry.html) |
| `grit_lib::porcelain::status` | module | git status as a structured operation. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/index.html) |
| `grit_lib::porcelain::status::IgnoredMode` | enum | How ignored files are reported (git status --ignored[=<mode>]). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/enum.IgnoredMode.html) |
| `grit_lib::porcelain::status::RenameDetection` | struct | Rename/copy detection settings for the status diffs (status.renames / --find-renames, status.renameLimit, copy detection). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/struct.RenameDetection.html) |
| `grit_lib::porcelain::status::StatusModel` | struct | The computed result of git status: everything the renderers need, with no presentation applied. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/struct.StatusModel.html) |
| `grit_lib::porcelain::status::StatusOptions` | struct | Inputs that drive what status computes. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/struct.StatusOptions.html) |
| `grit_lib::porcelain::status::UntrackedMode` | enum | How untracked files are reported (git status --untracked-files=<mode>). | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/status/enum.UntrackedMode.html) |
| `grit_lib::porcelain::tag` | module | git tag listing/filtering core. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/tag/index.html) |
| `grit_lib::porcelain::worktree_guard` | module | Lightweight worktree cleanliness checks without a full status pass. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/worktree_guard/index.html) |
| `grit_lib::porcelain::worktree_guard::TreeSwitchPlan` | struct | Inputs for a branch switch or fast-forward checkout after cleanliness checks. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/worktree_guard/struct.TreeSwitchPlan.html) |
| `grit_lib::porcelain::worktree_guard::WorktreeSnapshot` | struct | Loaded index and HEAD tree for worktree mutation guards. | [API](https://docs.rs/grit-lib/latest/grit_lib/porcelain/worktree_guard/struct.WorktreeSnapshot.html) |
| `grit_lib::precompose_config` | module | Read core.precomposeunicode without opening a full Repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/precompose_config/index.html) |
| `grit_lib::prelude` | module | The types most callers need, re-exported for use grit_lib::prelude::*;. | [API](https://docs.rs/grit-lib/latest/grit_lib/prelude/index.html) |
| `grit_lib::progress` | module | Progress reporting and cancellation across the library/CLI boundary. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/index.html) |
| `grit_lib::progress::Cancel` | trait | A cancellation signal checked by long-running library operations at loop boundaries. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.Cancel.html) |
| `grit_lib::progress::NeverCancel` | struct | A Cancel that never signals cancellation. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/struct.NeverCancel.html) |
| `grit_lib::progress::NullProgress` | struct | A ProgressSink that discards every update. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/struct.NullProgress.html) |
| `grit_lib::progress::ProgressSink` | trait | A sink for progress updates emitted by a library operation. | [API](https://docs.rs/grit-lib/latest/grit_lib/progress/trait.ProgressSink.html) |
| `grit_lib::promisor` | module | Partial-clone promisor bookkeeping used by Grit. | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor/index.html) |
| `grit_lib::promisor_remote` | module | The “promisor-remote” protocol v2 capability (see gitprotocol-v2(5) and Documentation/config/promisor.adoc). | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor_remote/index.html) |
| `grit_lib::promisor_remote::PromisorInfo` | struct | One advertised or configured promisor remote. | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor_remote/struct.PromisorInfo.html) |
| `grit_lib::promisor_remote::PromisorReply` | struct | The client’s reply to the server’s promisor-remote advertisement. | [API](https://docs.rs/grit-lib/latest/grit_lib/promisor_remote/struct.PromisorReply.html) |
| `grit_lib::protocol` | module | Protocol allow/deny policy and client wire-protocol version selection. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol/index.html) |
| `grit_lib::protocol::ClientProtocolVersionInputs` | struct | Explicit inputs for selecting the client-side Git wire protocol version. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol/struct.ClientProtocolVersionInputs.html) |
| `grit_lib::protocol::ProtocolError` | enum | Errors returned when a transport protocol is not allowed. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol/enum.ProtocolError.html) |
| `grit_lib::protocol::ProtocolPolicyInputs` | struct | Explicit inputs for protocol allow/deny evaluation. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol/struct.ProtocolPolicyInputs.html) |
| `grit_lib::protocol_v2` | module | Pure protocol-v2 capability parsing and request-fragment building. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol_v2/index.html) |
| `grit_lib::protocol_v2::FetchResponseSection` | enum | A section header in a protocol-v2 command=fetch response. | [API](https://docs.rs/grit-lib/latest/grit_lib/protocol_v2/enum.FetchResponseSection.html) |
| `grit_lib::prune_packed` | module | Library implementation of prune-packed. | [API](https://docs.rs/grit-lib/latest/grit_lib/prune_packed/index.html) |
| `grit_lib::prune_packed::PrunePackedOptions` | struct | Options controlling the behaviour of prune_packed_objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/prune_packed/struct.PrunePackedOptions.html) |
| `grit_lib::push` | module | Wire-protocol push orchestration over a crate::transport::Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/push/index.html) |
| `grit_lib::push_cert` | module | Signed-push certificate generation and verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_cert/index.html) |
| `grit_lib::push_cert::CertRefUpdate` | struct | A single ref update line in a push certificate. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_cert/struct.CertRefUpdate.html) |
| `grit_lib::push_cert::PushCertEnv` | struct | The hook-visible certificate environment, mirroring receive-pack’s GIT_PUSH_CERT* variables. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_cert/struct.PushCertEnv.html) |
| `grit_lib::push_report` | module | Push status reporting that matches Git’s output. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_report/index.html) |
| `grit_lib::push_report::PushRefResult` | struct | One reference’s resolved push result, ready for display. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_report/struct.PushRefResult.html) |
| `grit_lib::push_report::PushRefStatus` | enum | The resolved outcome of a single reference update during a push. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_report/enum.PushRefStatus.html) |
| `grit_lib::push_report::PushStatusOutput` | struct | Output produced by format_push_status. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_report/struct.PushStatusOutput.html) |
| `grit_lib::push_submodules` | module | Submodule recursion for git push (--recurse-submodules). | [API](https://docs.rs/grit-lib/latest/grit_lib/push_submodules/index.html) |
| `grit_lib::push_submodules::PushRecurseSubmodules` | enum | How git push should recurse into submodules. | [API](https://docs.rs/grit-lib/latest/grit_lib/push_submodules/enum.PushRecurseSubmodules.html) |
| `grit_lib::quote_path` | module | C-style path quoting compatible with Git’s quote.c / core.quotepath. | [API](https://docs.rs/grit-lib/latest/grit_lib/quote_path/index.html) |
| `grit_lib::receive_pack` | module | Receive-pack configuration and pack-header helpers. | [API](https://docs.rs/grit-lib/latest/grit_lib/receive_pack/index.html) |
| `grit_lib::ref_exclusions` | module | Reference exclusion rules for rev-list / rev-parse (--exclude, --exclude-hidden). | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_exclusions/index.html) |
| `grit_lib::ref_exclusions::RefExclusions` | struct | Patterns that exclude refs from --all / glob expansion, including hidden-ref config. | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_exclusions/struct.RefExclusions.html) |
| `grit_lib::ref_namespace` | module | Git GIT_NAMESPACE handling: map logical ref names to storage under refs/namespaces/.../. | [API](https://docs.rs/grit-lib/latest/grit_lib/ref_namespace/index.html) |
| `grit_lib::references` | module | References: the refs backends, reflog, refspecs, name validation, namespaces. | [API](https://docs.rs/grit-lib/latest/grit_lib/references/index.html) |
| `grit_lib::reflog` | module | Reflog reading and management. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/index.html) |
| `grit_lib::reflog::GcReflogExpireConfig` | struct | Per-ref gc.<pattern>.reflogExpire* rules plus global gc.reflogExpire / gc.reflogExpireUnreachable. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.GcReflogExpireConfig.html) |
| `grit_lib::reflog::GcReflogPattern` | struct | Per-ref gc.<pattern>.reflogExpire* rule from config. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.GcReflogPattern.html) |
| `grit_lib::reflog::ReflogEntry` | struct | A single reflog entry. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.ReflogEntry.html) |
| `grit_lib::reflog::ReflogExpireAction` | struct | Per-entry report from expire_reflog_git. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.ReflogExpireAction.html) |
| `grit_lib::reflog::ReflogExpireActionKind` | enum | What happened to one reflog entry during expire_reflog_git. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/enum.ReflogExpireActionKind.html) |
| `grit_lib::reflog::ReflogExpireParams` | struct | Options for expire_reflog_git. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.ReflogExpireParams.html) |
| `grit_lib::reflog::ReflogExpireResult` | struct | Result of expire_reflog_git: prune count plus a per-entry action list. | [API](https://docs.rs/grit-lib/latest/grit_lib/reflog/struct.ReflogExpireResult.html) |
| `grit_lib::refs` | module | Reference storage — files backend + reftable backend. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/index.html) |
| `grit_lib::refs::BranchCommitRefUpdate` | struct | Move a checked-out branch to new_oid and append matching branch and HEAD reflogs. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/struct.BranchCommitRefUpdate.html) |
| `grit_lib::refs::LogRefsConfig` | enum | Core logAllRefUpdates modes (after config lookup), matching Git’s log_refs_config. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.LogRefsConfig.html) |
| `grit_lib::refs::PackedRefs` | struct | A one-time, in-memory snapshot of a ref store’s packed-refs file. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/struct.PackedRefs.html) |
| `grit_lib::refs::RawRefLookup` | enum | Outcome of a single storage-level ref lookup (Git refs_read_raw_ref style). | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.RawRefLookup.html) |
| `grit_lib::refs::Ref` | enum | A symbolic or direct reference. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.Ref.html) |
| `grit_lib::refs::RefBatchItem` | struct | One ref create/update/delete in a batch transaction (see crate::gc::update_refs). | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/struct.RefBatchItem.html) |
| `grit_lib::refs::RefnameUnavailable` | enum | Why a reference name cannot be created (Git refs_verify_refname_available style). | [API](https://docs.rs/grit-lib/latest/grit_lib/refs/enum.RefnameUnavailable.html) |
| `grit_lib::refs_fsck` | module | Reference database consistency checks for git refs verify and git fsck --references. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs_fsck/index.html) |
| `grit_lib::refs_fsck::RefsFsckIssue` | struct | One diagnostic (use format_refs_fsck_line for Git-compatible output). | [API](https://docs.rs/grit-lib/latest/grit_lib/refs_fsck/struct.RefsFsckIssue.html) |
| `grit_lib::refs_fsck::RefsFsckSeverity` | enum | Severity of a refs-fsck diagnostic. | [API](https://docs.rs/grit-lib/latest/grit_lib/refs_fsck/enum.RefsFsckSeverity.html) |
| `grit_lib::refspec` | module | Refspec parsing and validation — a port of git/refspec.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/refspec/index.html) |
| `grit_lib::refspec::RefspecError` | enum | Error returned when a refspec cannot be parsed. | [API](https://docs.rs/grit-lib/latest/grit_lib/refspec/enum.RefspecError.html) |
| `grit_lib::refspec::RefspecItem` | struct | A parsed refspec item. | [API](https://docs.rs/grit-lib/latest/grit_lib/refspec/struct.RefspecItem.html) |
| `grit_lib::reftable` | module | Reftable format — binary reference storage. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/index.html) |
| `grit_lib::reftable::LogRecord` | struct | A decoded log record. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.LogRecord.html) |
| `grit_lib::reftable::RefRecord` | struct | A decoded ref record. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.RefRecord.html) |
| `grit_lib::reftable::RefValue` | enum | A single reference record as stored in a reftable. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/enum.RefValue.html) |
| `grit_lib::reftable::ReftableReader` | struct | Reads a single reftable file from a byte buffer. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.ReftableReader.html) |
| `grit_lib::reftable::ReftableStack` | struct | Manages the $GIT_DIR/reftable/ directory and tables.list stack. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.ReftableStack.html) |
| `grit_lib::reftable::ReftableTransactionUpdate` | struct | A ref update that should be written to a reftable transaction. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.ReftableTransactionUpdate.html) |
| `grit_lib::reftable::ReftableWriter` | struct | Writes a single reftable file. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.ReftableWriter.html) |
| `grit_lib::reftable::WriteOptions` | struct | Write options for reftable creation. | [API](https://docs.rs/grit-lib/latest/grit_lib/reftable/struct.WriteOptions.html) |
| `grit_lib::remote` | module | Unified remote URL typing and transport dispatch for fetch, push, and ref listing. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/index.html) |
| `grit_lib::remote::HttpClientFactory` | trait | Builds HTTP clients for smart-HTTP remotes. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/trait.HttpClientFactory.html) |
| `grit_lib::remote::ListRefsOptions` | struct | Options controlling Remote::list_refs output, aligned with git ls-remote. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.ListRefsOptions.html) |
| `grit_lib::remote::Remote` | struct | A resolved remote: name (if from config), URL(s), and fetch refspecs. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.Remote.html) |
| `grit_lib::remote::RemoteError` | enum | Errors specific to remote URL resolution and dispatch. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/enum.RemoteError.html) |
| `grit_lib::remote::RemoteRef` | struct | A single reference returned by Remote::list_refs. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/struct.RemoteRef.html) |
| `grit_lib::remote::RemoteUrl` | enum | A parsed remote URL by transport kind. | [API](https://docs.rs/grit-lib/latest/grit_lib/remote/enum.RemoteUrl.html) |
| `grit_lib::repo` | module | Repository discovery and the primary Repository handle. | [API](https://docs.rs/grit-lib/latest/grit_lib/repo/index.html) |
| `grit_lib::repo::Repository` | struct | A handle to an open Git repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) |
| `grit_lib::repo_caches` | module | Repository-scoped caches shared by an open Repository. | [API](https://docs.rs/grit-lib/latest/grit_lib/repo_caches/index.html) |
| `grit_lib::repo_caches::RepoCaches` | struct | Per-repository cache arena (held behind Arc on Repository). | [API](https://docs.rs/grit-lib/latest/grit_lib/repo_caches/struct.RepoCaches.html) |
| `grit_lib::rerere` | module | Git-compatible rerere (MERGE_RR, rr-cache/, conflict ID hashing). | [API](https://docs.rs/grit-lib/latest/grit_lib/rerere/index.html) |
| `grit_lib::rerere::RerereAutoupdate` | enum | Outcome of rerere handling one path (replaces stderr status lines). | [API](https://docs.rs/grit-lib/latest/grit_lib/rerere/enum.RerereAutoupdate.html) |
| `grit_lib::rerere::RerereEvent` | struct | One rerere status event for a path. | [API](https://docs.rs/grit-lib/latest/grit_lib/rerere/struct.RerereEvent.html) |
| `grit_lib::resolve_undo` | module | Git index REUC (resolve-undo) extension — records unmerged stages when a conflict is resolved. | [API](https://docs.rs/grit-lib/latest/grit_lib/resolve_undo/index.html) |
| `grit_lib::resolve_undo::ResolveUndoRecord` | struct | Per-path undo data: up to three conflict stages (index 0 = stage 1). | [API](https://docs.rs/grit-lib/latest/grit_lib/resolve_undo/struct.ResolveUndoRecord.html) |
| `grit_lib::rev_list` | module | Commit traversal and output planning for rev-list. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/index.html) |
| `grit_lib::rev_list::BisectEntry` | struct | Per-commit bisection score for rev-list --bisect* output. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.BisectEntry.html) |
| `grit_lib::rev_list::BisectSelection` | struct | Bisection analysis result for a selected revision set. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.BisectSelection.html) |
| `grit_lib::rev_list::FilterObjectKind` | enum | Kind selector for object:type=<kind> filters. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.FilterObjectKind.html) |
| `grit_lib::rev_list::MissingAction` | enum | Behavior when reachable objects are missing. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.MissingAction.html) |
| `grit_lib::rev_list::ObjectFilter` | enum | Object filter specification for --filter=. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.ObjectFilter.html) |
| `grit_lib::rev_list::ObjectWalkRoot` | struct | Non-commit root from revision arguments (tag, rev:path, raw tree/blob OID), for object walks. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.ObjectWalkRoot.html) |
| `grit_lib::rev_list::OrderingMode` | enum | Ordering mode for commit output. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.OrderingMode.html) |
| `grit_lib::rev_list::OutputMode` | enum | User-facing output mode for rev-list. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.OutputMode.html) |
| `grit_lib::rev_list::RevListOptions` | struct | Parsed and normalized options for rev-list traversal. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListOptions.html) |
| `grit_lib::rev_list::RevListResult` | struct | Final commit selection result. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListResult.html) |
| `grit_lib::rev_list_error` | module | Typed failures from crate::rev_list. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list_error/index.html) |
| `grit_lib::rev_list_error::RevListError` | enum | Failure modes when walking revisions or parsing rev-list options. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_list_error/enum.RevListError.html) |
| `grit_lib::rev_parse` | module | Revision parsing and repository discovery helpers for rev-parse. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/index.html) |
| `grit_lib::rev_parse::IndexColonSpec` | struct | Parsed :path / :N:path index revision syntax (leading colon, not :/search). | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/struct.IndexColonSpec.html) |
| `grit_lib::rev_parse::IndexPathEntry` | struct | One index entry resolved from a :path / :N:path revision string. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/struct.IndexPathEntry.html) |
| `grit_lib::rev_parse::TreeishBlobAtPath` | struct | Resolved blob (non-tree) at treeish:path for diff plumbing. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse/struct.TreeishBlobAtPath.html) |
| `grit_lib::rev_parse_error` | module | Typed failures from revision parsing (crate::rev_parse). | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse_error/index.html) |
| `grit_lib::rev_parse_error::AmbiguousObjectHint` | struct | One candidate line when a short object id is ambiguous. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse_error/struct.AmbiguousObjectHint.html) |
| `grit_lib::rev_parse_error::RevParseError` | enum | Failure modes when resolving revision specs, upstream/push refs, paths, and reflogs. | [API](https://docs.rs/grit-lib/latest/grit_lib/rev_parse_error/enum.RevParseError.html) |
| `grit_lib::revision` | module | Revision machinery: rev-parse, rev-list, name-rev, commit-graph. | [API](https://docs.rs/grit-lib/latest/grit_lib/revision/index.html) |
| `grit_lib::serve` | module | Server side of the Git wire protocol: answering fetches and accepting pushes. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/index.html) |
| `grit_lib::serve::ProtocolVersion` | enum | Wire protocol version requested by the client. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/enum.ProtocolVersion.html) |
| `grit_lib::serve::ReceiveOutcome` | struct | What a push session did. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/struct.ReceiveOutcome.html) |
| `grit_lib::serve::ReceivePolicy` | struct | Which ref updates the server refuses. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/struct.ReceivePolicy.html) |
| `grit_lib::serve::RefUpdateResult` | struct | The result of one requested ref update. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/struct.RefUpdateResult.html) |
| `grit_lib::serve::ServeError` | enum | Errors produced while serving a fetch or a push. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/enum.ServeError.html) |
| `grit_lib::serve::ServeOptions` | struct | How a serving session is framed on the wire. | [API](https://docs.rs/grit-lib/latest/grit_lib/serve/struct.ServeOptions.html) |
| `grit_lib::shallow` | module | Shallow repository metadata (.git/shallow). | [API](https://docs.rs/grit-lib/latest/grit_lib/shallow/index.html) |
| `grit_lib::shared_repo` | module | Shared-repository permission helpers (core.sharedRepository, --shared). | [API](https://docs.rs/grit-lib/latest/grit_lib/shared_repo/index.html) |
| `grit_lib::signing` | module | Commit/tag GPG (and gpgsm/ssh) signing and signature verification. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/index.html) |
| `grit_lib::signing::GpgConfig` | struct | Resolved signing/verification configuration. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/struct.GpgConfig.html) |
| `grit_lib::signing::GpgFormat` | enum | The signature format selected via gpg.format. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/enum.GpgFormat.html) |
| `grit_lib::signing::SignatureCheck` | struct | A parsed signature check result. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/struct.SignatureCheck.html) |
| `grit_lib::signing::TrustLevel` | enum | Signature trust level, as reported for a verified signature. | [API](https://docs.rs/grit-lib/latest/grit_lib/signing/enum.TrustLevel.html) |
| `grit_lib::sparse_checkout` | module | Sparse-checkout pattern parsing and path membership (cone and non-cone). | [API](https://docs.rs/grit-lib/latest/grit_lib/sparse_checkout/index.html) |
| `grit_lib::sparse_checkout::ConePatterns` | struct | Cone-mode sparse state: keys use a leading / (Git’s internal form). | [API](https://docs.rs/grit-lib/latest/grit_lib/sparse_checkout/struct.ConePatterns.html) |
| `grit_lib::sparse_checkout::ConeWorkspace` | struct | Mutable cone sparse state (Git pattern_list hashmaps) for building sparse-checkout files. | [API](https://docs.rs/grit-lib/latest/grit_lib/sparse_checkout/struct.ConeWorkspace.html) |
| `grit_lib::sparse_checkout::NonConePatterns` | struct | Parsed non-cone sparse-checkout patterns in file order (last match wins). | [API](https://docs.rs/grit-lib/latest/grit_lib/sparse_checkout/struct.NonConePatterns.html) |
| `grit_lib::split_index` | module | Split index: link extension and sharedindex.<sha1> (Git split-index.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/split_index/index.html) |
| `grit_lib::split_index::WriteSplitIndexRequest` | struct | Request from update-index for the next index write. | [API](https://docs.rs/grit-lib/latest/grit_lib/split_index/struct.WriteSplitIndexRequest.html) |
| `grit_lib::state` | module | Repository state machine — HEAD resolution, branch status, and in-progress operation detection. | [API](https://docs.rs/grit-lib/latest/grit_lib/state/index.html) |
| `grit_lib::state::HeadState` | enum | The current state of HEAD. | [API](https://docs.rs/grit-lib/latest/grit_lib/state/enum.HeadState.html) |
| `grit_lib::state::InProgressOperation` | enum | An in-progress operation that the repository is in the middle of. | [API](https://docs.rs/grit-lib/latest/grit_lib/state/enum.InProgressOperation.html) |
| `grit_lib::state::RepoState` | struct | Full snapshot of a repository’s state. | [API](https://docs.rs/grit-lib/latest/grit_lib/state/struct.RepoState.html) |
| `grit_lib::state::WtStatusState` | struct | Snapshot of repository state used by git status long-format output (wt-status.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/state/struct.WtStatusState.html) |
| `grit_lib::stripspace` | module | Core logic for git stripspace. | [API](https://docs.rs/grit-lib/latest/grit_lib/stripspace/index.html) |
| `grit_lib::stripspace::Mode` | enum | Processing mode for process. | [API](https://docs.rs/grit-lib/latest/grit_lib/stripspace/enum.Mode.html) |
| `grit_lib::submodule_active` | module | Submodule “active” state (submodule.c is_submodule_active parity). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_active/index.html) |
| `grit_lib::submodule_config` | module | Submodule registration and activation (Git submodule.c parity for tooling). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config/index.html) |
| `grit_lib::submodule_config::SubmoduleRegistration` | struct | Maps a submodule work tree path (as in the index / .gitmodules) to its submodule section name. | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config/struct.SubmoduleRegistration.html) |
| `grit_lib::submodule_config_cache` | module | Submodule configuration cache (Git submodule-config.c subset for test-tool). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config_cache/index.html) |
| `grit_lib::submodule_config_cache::SubmoduleConfigCache` | struct | Cache of parsed .gitmodules blobs (by blob OID) plus path/name indexes. | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config_cache/struct.SubmoduleConfigCache.html) |
| `grit_lib::submodule_config_cache::SubmoduleInfo` | struct | Resolved submodule identity for test output (Submodule name: 'x' for path 'y'). | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_config_cache/struct.SubmoduleInfo.html) |
| `grit_lib::submodule_gitdir` | module | Submodule gitdir paths when extensions.submodulePathConfig is enabled. | [API](https://docs.rs/grit-lib/latest/grit_lib/submodule_gitdir/index.html) |
| `grit_lib::terminal` | module | Cross-platform terminal capability detection for ANSI color output. | [API](https://docs.rs/grit-lib/latest/grit_lib/terminal/index.html) |
| `grit_lib::textconv_cache` | module | Git-compatible diff.<driver>.cachetextconv storage under refs/notes/textconv/<driver>. | [API](https://docs.rs/grit-lib/latest/grit_lib/textconv_cache/index.html) |
| `grit_lib::transfer` | module | Embedder-facing transfer (fetch / push) result & option types, plus the negotiation-driven pack builder. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/index.html) |
| `grit_lib::transfer::CloneReflog` | struct | Identity and message for clone reflog entries written by the library. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.CloneReflog.html) |
| `grit_lib::transfer::FetchOptions` | struct | Options controlling a fetch. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.FetchOptions.html) |
| `grit_lib::transfer::FetchOutcome` | struct | The structured result of a fetch, ready for the embedder’s ref-store apply. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.FetchOutcome.html) |
| `grit_lib::transfer::PackBuildOptions` | struct | Options controlling build_pack. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PackBuildOptions.html) |
| `grit_lib::transfer::PushOptions` | struct | Options controlling a push. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushOptions.html) |
| `grit_lib::transfer::PushOutcome` | struct | The structured result of a push. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushOutcome.html) |
| `grit_lib::transfer::PushRefSpec` | struct | A single ref update requested by a push. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.PushRefSpec.html) |
| `grit_lib::transfer::RefUpdate` | struct | The resolved outcome of one reference during a fetch. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/struct.RefUpdate.html) |
| `grit_lib::transfer::TagMode` | enum | Which tags to fetch alongside the requested refs. | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/enum.TagMode.html) |
| `grit_lib::transfer::UpdateMode` | enum | How a single reference resolved during a fetch (or would resolve in a push). | [API](https://docs.rs/grit-lib/latest/grit_lib/transfer/enum.UpdateMode.html) |
| `grit_lib::transport` | module | Embedder-facing transport abstraction for the Git wire protocols. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/index.html) |
| `grit_lib::transport::Advertisement` | struct | The captured ref/capability advertisement for a v0/v1 connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.Advertisement.html) |
| `grit_lib::transport::ConnectOptions` | struct | Options controlling the transport handshake. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.ConnectOptions.html) |
| `grit_lib::transport::Connection` | trait | A live, bidirectional pkt-line connection to a Git service, with the ref/capability advertisement captured during the handshake. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/trait.Connection.html) |
| `grit_lib::transport::GitDaemonConnection` | struct | A live connection to a Git daemon over a duplex TCP socket. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.GitDaemonConnection.html) |
| `grit_lib::transport::GitDaemonTransport` | struct | The native git:// daemon transport. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.GitDaemonTransport.html) |
| `grit_lib::transport::GitDaemonUrl` | struct | Parsed git://host[:port]/path (path includes the leading /). | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.GitDaemonUrl.html) |
| `grit_lib::transport::Service` | enum | The Git service a Connection speaks. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/enum.Service.html) |
| `grit_lib::transport::SshCommand` | enum | How the SshTransport invokes ssh. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/enum.SshCommand.html) |
| `grit_lib::transport::SshConnection` | struct | A live connection to a remote Git service over an ssh subprocess. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshConnection.html) |
| `grit_lib::transport::SshTransport` | struct | The ssh transport: spawn ssh [opts] <host> git-upload-pack '<path>' and expose the child’s stdio as a Connection. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshTransport.html) |
| `grit_lib::transport::SshUrl` | struct | A parsed SSH remote (scp-style host:path, ssh://, or git+ssh://). | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/struct.SshUrl.html) |
| `grit_lib::transport::Transport` | trait | A factory that connects to a remote and performs the protocol handshake. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/trait.Transport.html) |
| `grit_lib::transport::http` | module | Smart-HTTP Git transport over a pluggable HTTP client. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/http/index.html) |
| `grit_lib::transport::http::HttpClient` | trait | The minimal HTTP surface the smart-HTTP transport needs. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/http/trait.HttpClient.html) |
| `grit_lib::transport::http::SmartHttpConnection` | struct | A live smart-HTTP connection: the parsed advertisement plus the context needed to issue the stateless-RPC POST. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/http/struct.SmartHttpConnection.html) |
| `grit_lib::transport::http::SmartHttpTransport` | struct | A smart-HTTP Transport over a pluggable HttpClient. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport/http/struct.SmartHttpTransport.html) |
| `grit_lib::transport_path` | module | Safety checks and path resolution for local transport URLs (matches Git connect.c / path.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/transport_path/index.html) |
| `grit_lib::transport_path::NoDirectoryName` | struct | Error returned by git_url_basename when no directory name can be derived from a URL. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport_path/struct.NoDirectoryName.html) |
| `grit_lib::transport_path::TransportPathError` | enum | Errors returned while validating local transport paths. | [API](https://docs.rs/grit-lib/latest/grit_lib/transport_path/enum.TransportPathError.html) |
| `grit_lib::tree_path_follow` | module | Resolve tree paths with symlink following (get_tree_entry_follow_symlinks). | [API](https://docs.rs/grit-lib/latest/grit_lib/tree_path_follow/index.html) |
| `grit_lib::tree_path_follow::FollowPathFailure` | enum | Failure modes reported as special git cat-file --batch-check lines. | [API](https://docs.rs/grit-lib/latest/grit_lib/tree_path_follow/enum.FollowPathFailure.html) |
| `grit_lib::tree_path_follow::FollowPathResult` | enum | Result of resolving tree_oid:path with symlink following. | [API](https://docs.rs/grit-lib/latest/grit_lib/tree_path_follow/enum.FollowPathResult.html) |
| `grit_lib::unicode_normalization` | module | UTF-8 NFC path normalization for macOS-style filesystems (core.precomposeUnicode). | [API](https://docs.rs/grit-lib/latest/grit_lib/unicode_normalization/index.html) |
| `grit_lib::unicode_normalization::WorktreePathForStaging` | struct | Absolute worktree path and repository-relative index spelling after NFC/NFD resolution. | [API](https://docs.rs/grit-lib/latest/grit_lib/unicode_normalization/struct.WorktreePathForStaging.html) |
| `grit_lib::unpack_objects` | module | unpack-objects: unpack a pack stream into loose objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/index.html) |
| `grit_lib::unpack_objects::PackIndexRecord` | struct | Resolved (oid, pack_offset) pairs for every object slot in a pack byte stream. | [API](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/struct.PackIndexRecord.html) |
| `grit_lib::unpack_objects::UnpackOptions` | struct | Options controlling unpack-objects behaviour. | [API](https://docs.rs/grit-lib/latest/grit_lib/unpack_objects/struct.UnpackOptions.html) |
| `grit_lib::untracked_cache` | module | Git index UNTR (untracked cache) — git/dir.c / read-cache.c. | [API](https://docs.rs/grit-lib/latest/grit_lib/untracked_cache/index.html) |
| `grit_lib::untracked_cache::OidStat` | struct | Git struct stat_data on disk (36 bytes). | [API](https://docs.rs/grit-lib/latest/grit_lib/untracked_cache/struct.OidStat.html) |
| `grit_lib::untracked_cache::UntrackedCache` | struct | Collect untracked paths from a populated untracked cache tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/untracked_cache/struct.UntrackedCache.html) |
| `grit_lib::upload_filter` | module | Upload-pack object filter policy. | [API](https://docs.rs/grit-lib/latest/grit_lib/upload_filter/index.html) |
| `grit_lib::upload_filter::UploadFilterError` | enum | Error returned when an upload-pack filter request is not allowed by config. | [API](https://docs.rs/grit-lib/latest/grit_lib/upload_filter/enum.UploadFilterError.html) |
| `grit_lib::url_rewrite` | module | URL rewrite helpers for url.*.insteadOf / url.*.pushInsteadOf. | [API](https://docs.rs/grit-lib/latest/grit_lib/url_rewrite/index.html) |
| `grit_lib::userdiff` | module | User-defined and built-in diff function-name matching. | [API](https://docs.rs/grit-lib/latest/grit_lib/userdiff/index.html) |
| `grit_lib::userdiff::FuncnameMatcher` | struct | Compiled function-name matcher used for diff hunk headers. | [API](https://docs.rs/grit-lib/latest/grit_lib/userdiff/struct.FuncnameMatcher.html) |
| `grit_lib::whitespace_rule` | module | Git-compatible core.whitespace rules and ws_fix_copy (git/ws.c). | [API](https://docs.rs/grit-lib/latest/grit_lib/whitespace_rule/index.html) |
| `grit_lib::wildmatch` | module | Git-compatible wildmatch pattern matching. | [API](https://docs.rs/grit-lib/latest/grit_lib/wildmatch/index.html) |
| `grit_lib::worktree` | module | Linked worktree registry: discovery, listing, and admin-dir layout. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree/index.html) |
| `grit_lib::worktree::WorktreeEntry` | struct | One row returned by list_worktrees. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree/struct.WorktreeEntry.html) |
| `grit_lib::worktree_cwd` | module | Process current working directory relative to a Git work tree. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_cwd/index.html) |
| `grit_lib::worktree_index` | module | Index and working tree: the index, sparse checkout, attributes, ignore, CRLF. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_index/index.html) |
| `grit_lib::worktree_ref` | module | Per-worktree ref name parsing and storage location (Git parse_worktree_ref / files_ref_path). | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_ref/index.html) |
| `grit_lib::worktree_ref::RefWorktreeType` | enum | How a ref name maps to on-disk storage across worktrees. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_ref/enum.RefWorktreeType.html) |
| `grit_lib::worktree_rules` | module | Per-operation worktree attribute and ignore context. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_rules/index.html) |
| `grit_lib::worktree_rules::WorktreeRules` | struct | Attribute/ignore/conversion state shared across one porcelain operation. | [API](https://docs.rs/grit-lib/latest/grit_lib/worktree_rules/struct.WorktreeRules.html) |
| `grit_lib::write_tree` | module | Build tree objects from index entries (git write-tree core logic). | [API](https://docs.rs/grit-lib/latest/grit_lib/write_tree/index.html) |
| `grit_lib::write_tree::WriteTreeFlags` | struct | Options for cache_tree_update and write_tree_update_index. | [API](https://docs.rs/grit-lib/latest/grit_lib/write_tree/struct.WriteTreeFlags.html) |
| `grit_lib::write_tree::WriteTreePersistence` | enum | How cache_tree_update persists rebuilt tree objects. | [API](https://docs.rs/grit-lib/latest/grit_lib/write_tree/enum.WriteTreePersistence.html) |
| `grit_lib::ws` | module | Git-compatible whitespace rules (core.whitespace, whitespace attribute). | [API](https://docs.rs/grit-lib/latest/grit_lib/ws/index.html) |
| `grit_lib::ws::WhitespaceGitAttr` | enum | Blank lines at end of file (handled at apply layer, not in ws_check). | [API](https://docs.rs/grit-lib/latest/grit_lib/ws/enum.WhitespaceGitAttr.html) |
