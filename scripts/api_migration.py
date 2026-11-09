"""Generate and check the grit-lib 0.5.31 → 0.6 public API migration table."""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "grit-lib" / "public-api.txt"
MAP_PATH = Path(__file__).resolve().parent / "api_migration_map.toml"
DOC_PATH = ROOT / "content" / "docs" / "library" / "migrating-to-0.6.md"
BASE_REF = "v0.5.31"
BEGIN = "<!-- api-migration:begin -->"
END = "<!-- api-migration:end -->"

sys.path.insert(0, str(Path(__file__).resolve().parent))
import public_api as pa  # noqa: E402

# Renamed types: apply to every removed path that mentions the old name.
_TYPE_RENAMES = {
    "grit_lib::porcelain::branch::CreateBranchOptions": "grit_lib::porcelain::branch::BranchCreateOptions",
    "grit_lib::porcelain::branch::DeleteMode": "grit_lib::porcelain::branch::BranchDeleteMode",
    "grit_lib::porcelain::branch::RenameBranchOptions": "grit_lib::porcelain::branch::BranchRenameOptions",
    "grit_lib::porcelain::tag::TagInfo": "grit_lib::porcelain::tag::TagSummary",
}

# Exact old path → replacement (0.6 public path or decision prose).
_EXACT: dict[str, str] = {
    "grit_lib::blame::blame_file": "grit_lib::diff::Diff::blame",
    "grit_lib::blame::resolve_blame_line_range": "grit_lib::diff::Diff::resolve_blame_line_range",
    "grit_lib::blame::resolve_blame_line_range_with_options": "grit_lib::diff::Diff::resolve_blame_line_range_with_options",
    "grit_lib::command_runner::hooks::resolve_hooks_dir": "grit_lib::repo_handles::RepositoryHooks::hooks_dir",
    "grit_lib::command_runner::hooks::run_commit_hook": "grit_lib::repo_handles::RepositoryHooks::run_commit",
    "grit_lib::command_runner::hooks::run_commit_hook_checked": "grit_lib::repo_handles::RepositoryHooks::run_commit_checked",
    "grit_lib::command_runner::hooks::run_hook": "grit_lib::repo_handles::RepositoryHooks::run",
    "grit_lib::command_runner::hooks::run_hook_in_git_dir": "grit_lib::repo_handles::RepositoryHooks::run_in_git_dir",
    "grit_lib::command_runner::hooks::run_hook_opts": "grit_lib::repo_handles::RepositoryHooks::run",
    "grit_lib::command_runner::hooks::run_post_checkout": "grit_lib::repo_handles::RepositoryHooks::run_post_checkout",
    "grit_lib::command_runner::hooks::run_post_merge": "grit_lib::repo_handles::RepositoryHooks::run_post_merge",
    "grit_lib::command_runner::hooks::run_post_rewrite": "grit_lib::repo_handles::RepositoryHooks::run_post_rewrite",
    "grit_lib::command_runner::hooks::run_pre_push_hook": "grit_lib::repo_handles::RepositoryHooks::run_pre_push",
    "grit_lib::command_runner::hooks::run_pre_rebase": "grit_lib::repo_handles::RepositoryHooks::run_pre_rebase",
    "grit_lib::config::add_config_value": "grit_lib::config::ConfigEditor::add_value",
    "grit_lib::config::remove_config_section": "grit_lib::config::ConfigEditor::remove_section",
    "grit_lib::config::rename_config_section": "grit_lib::config::ConfigEditor::rename_section",
    "grit_lib::config::set_config_value": "grit_lib::config::ConfigEditor::set",
    "grit_lib::config::unset_all_config_values": "grit_lib::config::ConfigEditor::unset_all",
    "grit_lib::config::unset_config_value": "grit_lib::config::ConfigEditor::unset",
    "grit_lib::diff::DiffAlgorithm::similar_algorithm": "core::str::FromStr::from_str (DiffAlgorithm)",
    "grit_lib::diff::anchored_unified_diff": "grit_lib::diff::unified_diff",
    "grit_lib::diff::annotate_unified_diff_no_newline_at_eof": "grit_lib::diff::structured (hunk metadata)",
    "grit_lib::diff::count_changes_with_algorithm": "grit_lib::diff::Diff::files",
    "grit_lib::diff::line_diff_unified_hunk_body": "grit_lib::diff::unified_diff",
    "grit_lib::diff::rerere::rerere_clear": "grit_lib::merge::Merge::abort (rerere state)",
    "grit_lib::diff::resolve_indent_heuristic": "grit_lib::diff::DiffOptions::resolve_indent_heuristic",
    "grit_lib::diff::unified_diff_histogram_hunks_only": "grit_lib::diff::unified_diff",
    "grit_lib::diff::unified_diff_histogram_ignore_with_prefix_and_funcname<F": "grit_lib::diff::unified_diff",
    "grit_lib::diff::unified_diff_with_prefix_and_funcname": "grit_lib::diff::unified_diff",
    "grit_lib::diff::unified_diff_with_prefix_and_funcname_and_algorithm": "grit_lib::diff::unified_diff",
    "grit_lib::notes::apply_prepared_notes_rewrite": "grit_lib::repo_handles::RepositoryNotes::apply_rewrite_plan",
    "grit_lib::notes::copy_notes_for_rewrite": "grit_lib::repo_handles::RepositoryNotes::copy_for_rewrite",
    "grit_lib::notes::notes_merge_inner": "grit_lib::repo_handles::RepositoryNotes::merge_refs",
    "grit_lib::notes::prepare_notes_for_rewrite": "grit_lib::repo_handles::RepositoryNotes::prepare_rewrite",
    "grit_lib::notes::read_notes_tree": "grit_lib::repo_handles::RepositoryNotes::read_tree",
    "grit_lib::notes::read_notes_tree_limited": "grit_lib::repo_handles::RepositoryNotes::read_tree_limited",
    "grit_lib::notes::write_notes_commit": "grit_lib::repo_handles::RepositoryNotes::write_tree_commit",
    "grit_lib::repo::InitOptions::environment": "grit_lib::config::environment::RepositoryOptions::environment",
    "grit_lib::repo::InitOptions::with_environment": "grit_lib::repo::InitOptions::with_repository_options",
    "grit_lib::revwalk::rev_list::shallow_boundary_oids": (
        "grit_lib::revwalk::handle::RevWalk::shallow_boundary_oids"
    ),
    "grit_lib::revwalk::rev_parse::load_graft_parents": "removed (crate-internal graft helpers)",
    "grit_lib::revwalk::rev_parse::superproject_work_tree_from_nested_git_modules": "removed (crate-internal submodule helper)",
    "grit_lib::worktree::add_linked_worktree": "grit_lib::repo_handles::RepositoryWorktrees::add",
    "grit_lib::worktree::add_linked_worktree_with_cancel": "grit_lib::repo_handles::RepositoryWorktrees::add_with_cancel",
    "grit_lib::worktree::common_git_dir": "grit_lib::repo::Repository::common_git_dir",
    "grit_lib::worktree::copy_filtered_worktree_config": "removed (crate-internal worktree setup)",
    "grit_lib::worktree::ensure_branch_not_checked_out_elsewhere": "grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_elsewhere",
    "grit_lib::worktree::ensure_branch_not_checked_out_in_any_worktree": "grit_lib::repo_handles::RepositoryWorktrees::ensure_branch_not_checked_out_in_any_worktree",
    "grit_lib::worktree::find_worktree_with_branch_checked_out": "grit_lib::repo_handles::RepositoryWorktrees::find_with_branch_checked_out",
    "grit_lib::worktree::list_worktrees": "grit_lib::repo_handles::RepositoryWorktrees::list",
    "grit_lib::worktree::move_linked_worktree": "grit_lib::repo_handles::RepositoryWorktrees::move_linked",
    "grit_lib::worktree::prune_linked_worktrees": "grit_lib::repo_handles::RepositoryWorktrees::prune",
    "grit_lib::worktree::remove_linked_worktree": "grit_lib::repo_handles::RepositoryWorktrees::remove",
    "grit_lib::worktree::repair_worktrees": "grit_lib::repo_handles::RepositoryWorktrees::repair",
    "grit_lib::worktree::worktree_admin_dir": "grit_lib::repo_handles::RepositoryWorktrees::admin_dir",
    "grit_lib::refs::branch_tracking::apply_push_remote_tracking_updates": "grit_lib::repo_handles::Branches::apply_push_remote_tracking_updates",
    "grit_lib::refs::branch_tracking::compare_with_target_optional": "grit_lib::repo_handles::Branches::compare_with_target",
    "grit_lib::refs::branch_tracking::list_branches": "grit_lib::repo_handles::Branches::list",
    "grit_lib::refs::branch_tracking::upstream_status": "grit_lib::repo_handles::Branches::upstream_status",
    "grit_lib::refs::branch_tracking::upstream_tracking_full_ref": "grit_lib::repo_handles::Branches::upstream_tracking_full_ref",
}

# porcelain free fn → handle method (same function name on handle unless noted).
_PORCELAIN_HANDLE: dict[str, tuple[str, str]] = {
    "grit_lib::porcelain::add::": ("grit_lib::index::IndexFile", "repo.index()"),
    "grit_lib::porcelain::branch::": ("grit_lib::repo_handles::Branches", "repo.branches()"),
    "grit_lib::porcelain::checkout::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::checkout_reflog::": ("grit_lib::repo_handles::Checkout", "repo.checkout().switch"),
    "grit_lib::porcelain::clean::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::commit::": ("grit_lib::commits::Commits", "repo.commits()"),
    "grit_lib::porcelain::hunk_select::": ("grit_lib::index::IndexFile", "repo.index()"),
    "grit_lib::porcelain::paths::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::pathspec_match::": ("grit_lib::index::IndexFile", "repo.index()"),
    "grit_lib::porcelain::reset::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::restore::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::stage_tracked::": ("grit_lib::index::IndexFile", "repo.index()"),
    "grit_lib::porcelain::staging::": ("grit_lib::index::IndexFile", "repo.index()"),
    "grit_lib::porcelain::status::": ("grit_lib::status::Status", "repo.status()"),
    "grit_lib::porcelain::switch::": ("grit_lib::repo_handles::Checkout", "repo.checkout()"),
    "grit_lib::porcelain::tag::": ("grit_lib::repo_handles::Tags", "repo.tags()"),
    "grit_lib::porcelain::workspace_snapshot::": (
        "grit_lib::repo_handles::RepositoryWorkspaceSnapshot",
        "repo.workspace_snapshot()",
    ),
}

_PORCELAIN_FN_REMAP: dict[str, str] = {
    "stage_on_index": "stage",
    "commit_with_patch": "create",
    "commit_with_scope": "create",
    "commit_worktree": "create",
    "create_commit": "create",
    "default_commit_scope": "default_scope",
    "pending_commit_message": "pending_message",
    "plan_commit": "plan",
    "prepare_commit_bytes": "prepare_bytes",
    "write_commit_object": "write_object",
    "preview_reset": "preview_reset",
    "reset_to_commit": "reset",
    "conflict_style_from_config": "conflict_style",
    "status": "run",
    "status_diff_entry_matches_pathspec_typed": "diff_entry_matches_pathspec",
    "collect_untracked_and_ignored": "untracked",
    "collect_untracked_and_ignored_with_cache": "untracked",
    "collect_untracked_and_ignored_with_rules_and_cancel": "untracked_with_meta",
    "expand_untracked_for_staging": "expand_untracked_for_staging",
    "expand_untracked_for_staging_with_rules": "expand_untracked_for_staging",
    "dir_is_nested_submodule_worktree": "removed (status internals)",
    "checkout_between_trees_with_progress": "checkout",
    "checkout_hard_to_tree_from_head_tree": "checkout_hard_to_tree",
    "clean_untracked_with_cancel": "clean",
    "create_branch": "create",
    "delete_branch": "delete",
    "list_branches": "list_simple",
    "rename_branch": "rename",
    "resolve_create_tracking": "resolve_create_tracking",
    "stage_tracked_modifications_in_index": "stage_tracked_modifications_on",
    "stage_tracked_modifications_in_index_with_thresholds": "stage_tracked_modifications_on",
    "stage_tracked_modifications_with_thresholds": "stage_tracked_modifications",
    "stage_worktree_changes": "stage_worktree_paths",
    "list_unstaged_hunk_ids": "list_unstaged_hunk_ids",
    "resolve_selection": "resolve_hunk_selection",
    "checkout_between_trees": "checkout",
    "clean_untracked": "clean",
    "restore_paths": "restore",
    "diff_options_for_hunk_select": "removed (use DiffOptions)",
    "reverse_apply_patches_to_worktree": "removed (use IndexFile::restore_hunks)",
    "switch_repository": "switch",
    "resolve_create_branch": "resolve_create_branch",
    "create_tag": "create",
    "delete_tag": "delete",
    "list_tags": "list",
    "list_tags_with_detail": "list",
    "tag_ref_metadata": "ref_metadata",
    "verify_tag_by_name": "verify",
    "capture_workspace_snapshot": "capture",
    "pin_workspace_snapshot": "pin",
    "restore_workspace_snapshot": "restore",
    "unpin_workspace_snapshot": "unpin",
    "append_head_checkout_reflog": "removed (used by Checkout::switch)",
    "checkout_reflog_identity": "removed (used by Checkout::switch)",
    "create_branch_ref_with_reflog": "removed (used by Checkout::switch)",
    "finish_switch_ref_updates": "removed (used by Checkout::switch)",
    "is_already_detached_at": "removed (used by Checkout::switch)",
    "is_already_on_branch": "removed (used by Checkout::switch)",
    "ensure_pathspec_matches_known_in_repository": "ensure_pathspec_matches_known",
    "pathspec_matches_known_in_repository": "pathspec_matches_known",
}


def snapshot_text_at(ref: str) -> str:
    """Return committed public-api snapshot text at a git ref."""
    return subprocess.check_output(
        ["git", "show", f"{ref}:grit-lib/public-api.txt"],
        text=True,
        cwd=ROOT,
    )


def paths_in_snapshot(text: str) -> set[str]:
    """Collect every grit_lib path referenced in a snapshot."""
    paths: set[str] = set()
    for line in text.splitlines():
        for path in pa.item_paths_from_snapshot_line(line):
            paths.add(path)
    return paths


def _is_migration_path(path: str) -> bool:
    """Drop signature fragments mistakenly parsed from snapshot lines."""
    if "<" in path or ">" in path or "(" in path:
        return False
    return path.startswith("grit_lib::")


def removed_paths(*, base_ref: str = BASE_REF, current: Path = SNAPSHOT) -> list[str]:
    """Sorted list of paths present at base_ref but not in current snapshot."""
    old = paths_in_snapshot(snapshot_text_at(base_ref))
    new = paths_in_snapshot(current.read_text(encoding="utf-8"))
    return sorted(p for p in (old - new) if _is_migration_path(p))


def apply_type_renames(path: str) -> str:
    out = path
    for old, new in _TYPE_RENAMES.items():
        out = out.replace(old, new)
    return out


_GENERIC_FRAGMENT = re.compile(r"<[^>]+>")


def canonical_api_path(path: str) -> str:
    """Strip lifetime/generic fragments so snapshot paths compare to migration map paths."""
    return _GENERIC_FRAGMENT.sub("", path)


def is_prose_replacement(replacement: str) -> bool:
    """True when the replacement is intentional prose, not a rustdoc path."""
    if replacement in {"", "removed, no replacement", "—"}:
        return True
    if replacement.startswith("removed "):
        return True
    if not replacement.startswith("grit_lib::"):
        return True
    return False


def replacement_api_path(replacement: str) -> str:
    """Grit-lib path prefix before an optional `` (annotation)`` suffix."""
    if " (" in replacement:
        return replacement.split(" (", 1)[0]
    return replacement


_DECLARED_FN = re.compile(r"^pub (?:async )?fn (grit_lib::.+?)(?:\(|\s+->)")


def declared_public_paths(text: str) -> set[str]:
    """Collect declared ``grit_lib::…`` item paths from snapshot lines (incl. lifetimes)."""
    out: set[str] = set()
    for line in text.splitlines():
        s = line.strip()
        fn_match = _DECLARED_FN.match(s)
        if fn_match:
            out.add(canonical_api_path(fn_match.group(1).strip()))
            continue
        for kind in ("struct", "enum", "type", "const", "trait", "mod", "static"):
            m = re.match(rf"^pub {kind} (grit_lib::\S+)", s)
            if m:
                raw = m.group(1).split(":")[0]
                out.add(canonical_api_path(raw.rstrip(":")))
                break
    return out


def current_public_api_paths() -> set[str]:
    text = SNAPSHOT.read_text(encoding="utf-8")
    paths = declared_public_paths(text)
    for path in paths_in_snapshot(text):
        if path.startswith("grit_lib::"):
            paths.add(canonical_api_path(path))
    return paths


def replacement_exists(replacement: str, snapshot_paths: set[str] | None = None) -> bool:
    """Return whether a replacement names a path in the working-tree public API."""
    if is_prose_replacement(replacement):
        return True
    paths = snapshot_paths if snapshot_paths is not None else current_public_api_paths()
    target = canonical_api_path(replacement_api_path(replacement))
    for path in paths:
        if canonical_api_path(path) == target:
            return True
    return False


def invalid_replacements(
    mapping: dict[str, str],
    snapshot_paths: set[str] | None = None,
) -> list[tuple[str, str]]:
    paths = snapshot_paths if snapshot_paths is not None else current_public_api_paths()
    bad: list[tuple[str, str]] = []
    for old, replacement in mapping.items():
        if replacement in ("removed, no replacement",):
            continue
        if not replacement_exists(replacement, paths):
            bad.append((old, replacement))
    return bad


def suggest_replacement(old: str) -> str | None:
    """Return the 0.6 replacement for one removed path when rules cover it."""
    if old in _EXACT:
        return _EXACT[old]

    renamed = apply_type_renames(old)
    if renamed != old and renamed not in removed_paths():
        return renamed

    for prefix, (handle, _via) in _PORCELAIN_HANDLE.items():
        if not old.startswith(prefix):
            continue
        tail = old[len(prefix) :]
        if "::" in tail and not tail.split("::")[-1][0].isupper():
            # Associated fn on type under prefix — apply type renames only.
            return apply_type_renames(old)

        fn = tail.split("::")[-1] if "::" not in tail else tail
        if fn in _PORCELAIN_FN_REMAP:
            mapped = _PORCELAIN_FN_REMAP[fn]
            if mapped.startswith("removed"):
                return mapped
            return f"{handle}::{mapped}"
        # Types at prefix+tail
        if tail and tail[0].isupper() and "::" not in tail:
            return apply_type_renames(f"{prefix}{tail}")
        if "::" in tail:
            return apply_type_renames(old)
        return f"{handle}::{fn}"

    return None


def load_map(path: Path = MAP_PATH) -> dict[str, str]:
    """Parse api_migration_map.toml (simple ``old = \"new\"`` lines)."""
    if not path.is_file():
        return {}
    mapping: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip().strip('"')
        value = value.strip().strip('"')
        mapping[key] = value
    return mapping


def write_map(path: Path = MAP_PATH) -> dict[str, str]:
    """Regenerate the hand-maintained map from rules plus explicit overrides."""
    removed = removed_paths()
    snapshot_paths = current_public_api_paths()
    mapping: dict[str, str] = dict(_EXACT)
    for old in removed:
        if old in mapping:
            continue
        suggestion = suggest_replacement(old)
        if suggestion is not None:
            mapping[old] = suggestion
    invalid = invalid_replacements(
        {old: mapping[old] for old in removed if old in mapping},
        snapshot_paths,
    )
    if invalid:
        sys.stderr.write("replacement paths missing from current public API:\n")
        for old, replacement in invalid:
            sys.stderr.write(f"  {old} -> {replacement}\n")
        raise SystemExit(1)
    lines = [
        "# Hand-maintained grit-lib public API path migrations (0.5.31 → 0.6).",
        "# Regenerate with: python3 scripts/api_migration.py write-map",
        "",
    ]
    for old in removed:
        replacement = mapping.get(old)
        if replacement is None:
            replacement = "removed, no replacement"
        escaped_old = old.replace("\\", "\\\\").replace('"', '\\"')
        escaped_new = replacement.replace("\\", "\\\\").replace('"', '\\"')
        lines.append(f'"{escaped_old}" = "{escaped_new}"')
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return {old: mapping.get(old, "removed, no replacement") for old in removed}


def _deprecated_note_from_lines(lines: list[str], start: int) -> str:
    """Read a #[deprecated(note = …)] that may span multiple lines (1-based start)."""
    chunk = "\n".join(lines[max(0, start - 1) : min(len(lines), start + 8)])
    match = re.search(r'note\s*=\s*"([^"]+)"', chunk, re.DOTALL)
    return match.group(1) if match else ""


def _scan_deprecated_fn_notes(ref: str) -> dict[str, str]:
    """Map bare item names at ref to their deprecated note text."""
    try:
        raw = subprocess.check_output(
            ["git", "grep", "-n", r"#\[deprecated", ref, "--", "grit-lib/src"],
            text=True,
            cwd=ROOT,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return {}
    file_cache: dict[str, list[str]] = {}
    out: dict[str, str] = {}
    for grep_line in raw.splitlines():
        match = re.match(r"^(?:[^:]+:)?grit-lib/src/(.+\.rs):(\d+):", grep_line)
        if not match:
            continue
        rel, lineno = match.group(1), int(match.group(2))
        if rel not in file_cache:
            file_cache[rel] = subprocess.check_output(
                ["git", "show", f"{ref}:grit-lib/src/{rel}"],
                text=True,
                cwd=ROOT,
            ).splitlines()
        body = file_cache[rel]
        note = _deprecated_note_from_lines(body, lineno)
        for i in range(lineno, min(lineno + 24, len(body) + 1)):
            decl = body[i - 1]
            item = re.search(
                r"pub\s+(?:async\s+)?(?:fn|struct|enum|type|const)\s+(\w+)",
                decl,
            )
            if item:
                out[item.group(1)] = note
                break
    return out


def deprecation_notes_at(ref: str, removed: list[str] | None = None) -> dict[str, str]:
    """Attach v0.5.31 deprecated notes to removed public API paths by item name."""
    old_paths = paths_in_snapshot(snapshot_text_at(ref))
    by_suffix: dict[str, list[str]] = {}
    for path in old_paths:
        by_suffix.setdefault(path.split("::")[-1], []).append(path)

    fn_notes = _scan_deprecated_fn_notes(ref)
    matched: dict[str, str] = {}
    for fn_name, note in fn_notes.items():
        if not note:
            continue
        for path in by_suffix.get(fn_name, []):
            matched[path] = note

    if removed is None:
        return matched
    return {path: matched.get(path, "") for path in removed}


def render_table(mapping: dict[str, str], notes: dict[str, str]) -> str:
    rows = ["| Removed path (v0.5.31) | Replacement (0.6) | Notes |", "| --- | --- | --- |"]
    for old in sorted(mapping):
        replacement = mapping[old]
        note = notes.get(old, "")
        if replacement == "removed, no replacement":
            replacement = "—"
        rows.append(f"| `{old}` | `{replacement}` | {note} |")
    return "\n".join(rows)


def update_doc(*, check: bool = False) -> tuple[bool, str]:
    mapping = load_map()
    removed = removed_paths()
    unmapped = [p for p in removed if mapping.get(p) in (None, "", "removed, no replacement")]
    if unmapped and check:
        return False, "unmapped removals:\n" + "\n".join(f"  {p}" for p in unmapped)

    notes = deprecation_notes_at(BASE_REF, removed)
    table = render_table({p: mapping.get(p, "removed, no replacement") for p in removed}, notes)

    if not DOC_PATH.is_file():
        return False, f"missing doc {DOC_PATH}"

    text = DOC_PATH.read_text(encoding="utf-8")
    if BEGIN not in text or END not in text:
        return False, f"missing {BEGIN} / {END} markers in {DOC_PATH}"

    pattern = re.compile(
        re.escape(BEGIN) + r".*?" + re.escape(END),
        re.DOTALL,
    )
    new_text = pattern.sub(f"{BEGIN}\n\n{table}\n\n{END}", text, count=1)
    if check:
        if new_text != text:
            return False, "migration table is out of date (run without --check)"
        if unmapped:
            return False, "unmapped removals in map"
        return True, "ok"
    DOC_PATH.write_text(new_text, encoding="utf-8")
    return (not unmapped, "updated" if not unmapped else f"updated with {len(unmapped)} unmapped")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        nargs="?",
        choices=("write-map", "update-doc", "check"),
        default="update-doc",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="For update-doc/check: fail when map or generated table drifts.",
    )
    args = parser.parse_args(argv)

    if args.command == "write-map":
        mapping = write_map()
        missing = [k for k, v in mapping.items() if v == "removed, no replacement"]
        if missing:
            sys.stderr.write(
                f"warning: {len(missing)} paths still lack rules (marked removed, no replacement)\n"
            )
            for path in missing:
                sys.stderr.write(f"  {path}\n")
            return 1
        print(f"Wrote {MAP_PATH.relative_to(ROOT)} ({len(mapping)} entries)")
        return 0

    if args.command == "check":
        ok_map = True
        removed = removed_paths()
        mapping = load_map()
        unmapped = [p for p in removed if mapping.get(p) in (None, "", "removed, no replacement")]
        if unmapped:
            ok_map = False
            sys.stderr.write("api_migration_map.toml is missing entries:\n")
            for path in unmapped:
                sys.stderr.write(f"  {path}\n")
        bad_replacements = invalid_replacements(
            {p: mapping[p] for p in removed if p in mapping},
        )
        if bad_replacements:
            ok_map = False
            sys.stderr.write("api_migration_map.toml has invalid replacement paths:\n")
            for old, replacement in bad_replacements:
                sys.stderr.write(f"  {old} -> {replacement}\n")
        ok_doc, msg = update_doc(check=True)
        if not ok_doc:
            ok_map = False
            sys.stderr.write(msg + "\n")
        if not ok_map:
            return 1
        print("api migration check passed")
        return 0

    ok, msg = update_doc(check=args.check)
    if args.check and not ok:
        sys.stderr.write(msg + "\n")
        return 1
    print(msg)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
