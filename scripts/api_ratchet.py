#!/usr/bin/env python3
"""API quality ratchet for grit-lib: counts that may only go down.

Compares rustdoc JSON (pinned nightly, ``http-ureq`` feature; JSON at
``target/api-ratchet-doc/grit_lib.json``) and ``grit-lib/public-api.txt``
against ``grit-lib/api-ratchet.toml``.

Subcommands: ``check`` (CI gate), ``update`` (rewrite the baseline), ``report``
(consolidation offenders grouped by top-level public module; ``--module diff`` filters),
and ``audit`` (regenerate or check the Audit section in ``grit-lib/API.md``).
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tomllib
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from typing import Any, Iterable

from public_api import parse_pins
from public_fn_coverage import ensure_rustdoc_json

ROOT = Path(__file__).resolve().parents[1]
GRIT_LIB = ROOT / "grit-lib"
PUBLIC_API_TXT = GRIT_LIB / "public-api.txt"
API_MD_PATH = GRIT_LIB / "API.md"
BASELINE_PATH = GRIT_LIB / "api-ratchet.toml"
AUDIT_BEGIN = "<!-- api-audit:begin -->"
AUDIT_END = "<!-- api-audit:end -->"
# Isolated from `target/doc/grit_lib.json` (coverage, other tools) and from the
# hidden-items rustdoc used by `public_api.py` check-test-items so timestamp
# freshness cannot latch the wrong feature set or RUSTDOCFLAGS.
DEFAULT_RUSTDOC_JSON = ROOT / "target" / "api-ratchet-doc" / "grit_lib.json"

METRIC_KEYS = (
    "pub_repo_constructors",
    "pub_repo_constructor_offenders",
    "public_api_lines",
    "pub_fn_impl_suffix",
    "pub_fn_with_in_name",
    "pub_fn_too_many_arguments_allow",
    "pub_fn_two_plus_bare_bool",
    "pub_fn_result_string_err",
    "pub_free_fn_repository_first",
    "pub_free_fn_git_dir_path_first",
    "pub_free_fn_config_write",
    "pub_items_multiple_paths",
    "pub_libc_types",
    "pub_unsafe_fn",
    "pub_cancel_types",
    "pub_progress_interfaces",
    "pub_refname_validators",
    "pub_fn_refname_str",
    "pub_fn_pathspec_strings",
    "pub_fn_configset_with_repo",
    "umbrella_error_variants",
    "pub_error_catch_all_variants",
    "pub_error_string_tuple_variants",
    "pub_error_enum_not_non_exhaustive",
    "pub_area_error_wraps_umbrella",
    "error_message_construction_sites",
    "doc_example_free_fn_calls",
    "pub_deprecated_items",
)

# Metrics `report` groups by top-level public module (the API 0.6 consolidation targets).
REPORT_METRIC_KEYS = (
    "pub_fn_with_in_name",
    "pub_fn_too_many_arguments_allow",
    "pub_fn_two_plus_bare_bool",
    "pub_free_fn_repository_first",
    "pub_free_fn_git_dir_path_first",
)

CONSUMER_PATHS = (
    "grit-cli",
    "grit-utils",
    "grit-examples",
    "grit-protocol",
    "grit-http-server",
    "grit-test-support",
    "grit-lib/tests",
)

# Offender lists included in the generated audit (counts-only metrics omitted).
RESIDUAL_METRIC_KEYS = tuple(
    key
    for key in METRIC_KEYS
    if key
    not in (
        "public_api_lines",
        "pub_repo_constructors",
        "umbrella_error_variants",
        "error_message_construction_sites",
    )
)

PUBLIC_MOD_RE = re.compile(r"^pub mod grit_lib::([a-z_0-9]+)$")
PUBLIC_FN_MOD_RE = re.compile(r"^pub fn grit_lib::([a-z_0-9]+)::([^(\s]+)\(")
PUBLIC_API_REPO_FIRST_RE = re.compile(
    r"^pub fn grit_lib::([a-z_0-9]+)::[^(\s]+\("
    r"(?:&grit_lib::repo::Repository|grit_lib::repo::Repository)",
)
REPOSITORY_ACCESSOR_RE = re.compile(
    r"^pub fn grit_lib::repo::Repository::([a-z_0-9]+)\(&self\) -> (.+)$",
)

CANONICAL_MODULES_PATH = GRIT_LIB / "canonical-modules.toml"
ROOT_MODULE_LABEL = "(crate root)"
UNKNOWN_MODULE_LABEL = "(unknown)"

# Fence info tokens rustdoc accepts on a Rust doctest (any other language tag is not Rust).
RUST_FENCE_TOKENS = frozenset(
    {
        "rust",
        "ignore",
        "no_run",
        "should_panic",
        "compile_fail",
        "test_harness",
        "standalone_crate",
    },
)
FENCE_RE = re.compile(r"^\s*(```+|~~~+)\s*(.*?)\s*$")
CALL_RE = re.compile(r"(?<![\w.:!])((?:[A-Za-z_]\w*::)*)([a-z_]\w*)\s*\(")
NON_CALL_KEYWORDS = frozenset(
    {"if", "while", "for", "match", "return", "loop", "in", "as", "let", "move", "fn"},
)

ERROR_CATCH_ALL_VARIANTS = frozenset({"Message", "Other", "Generic", "Custom", "Msg"})
UMBRELLA_ERROR_PATH = "grit_lib::error::Error"

CANCEL_FAMILY_NAMES = frozenset({"Cancel", "CancelToken"})

REFNAME_PARAM_NAMES = frozenset({"name", "refname", "ref_name", "target_ref"})
REFNAME_FN_NAME_RE = re.compile(r"refname|ref_name|ref_format", re.IGNORECASE)
REFNAME_VALIDATOR_PATH_MARK = "::check_ref_format::"

REFNAME_STR_MODULE_PREFIXES = (
    "grit_lib::refs::",
    "grit_lib::refs::store::",
    "grit_lib::repo_handles::",
    "grit_lib::revwalk::",
)

PATHSPEC_PARAM_NAMES = frozenset({"pathspec", "pathspecs", "paths", "specs"})
RESOLVED_PATHSPECS_TYPE = "grit_lib::worktree::pathspec::ResolvedPathspecs"
RESOLVED_PATHSPECS_STRING_FIELDS = frozenset({"sources", "specs"})

REFNAME_VALIDATOR_MODULE_PREFIX = "grit_lib::refs::"

GIT_DIR_PARAM_NAMES = frozenset(
    {"git_dir", "gitdir", "git_directory", "repo_git_dir", "common_dir"},
)

# Legacy free functions that wrote a repo config file via `git_dir: Option<&Path>` (may be empty
# after consolidation onto [`ConfigEditor`](grit_lib::config::ConfigEditor)).
CONFIG_WRITE_FREE_FN_NAMES = frozenset(
    {
        "set_config_value",
        "add_config_value",
        "unset_config_value",
        "unset_all_config_values",
        "rename_config_section",
        "remove_config_section",
    },
)

CONFIG_WRITE_GIT_DIR_PARAM_NAMES = frozenset({"git_dir", "git_directory"})

# Config file mutation helpers on [`ConfigEditor`](grit_lib::config::ConfigEditor) (current path).
CONFIG_EDITOR_WRITE_METHODS = frozenset(
    {
        "set",
        "add_value",
        "unset",
        "unset_all",
        "rename_section",
        "remove_section",
    },
)

TOO_MANY_ARGS_RE = re.compile(r"allow\s*\(\s*clippy::too_many_arguments\s*\)")

STRING_RUST_TYPES = frozenset({"String", "str", "alloc::string::String"})

REPOSITORY_TYPE_SUFFIX = "::Repository"
REPOSITORY_TYPE_PLAIN = "Repository"
PATH_TYPE_PLAIN = "Path"


@dataclass
class MetricResult:
    """One ratchet metric: count plus human-readable offenders for check output."""

    count: int
    offenders: list[str] = field(default_factory=list)
    # Top-level public module per offender (parallel to `offenders`), for `report`.
    modules: list[str] = field(default_factory=list)


@dataclass
class RatchetSnapshot:
    """All metric counts and offenders from the current tree."""

    metrics: dict[str, MetricResult]

    def counts(self) -> dict[str, int]:
        return {key: self.metrics[key].count for key in METRIC_KEYS}


def rustdoc_env() -> dict[str, str]:
    return {"RUSTDOCFLAGS": ""}


def ensure_inputs(*, nightly: str, rustdoc_json: Path) -> None:
    ensure_rustdoc_json(
        nightly=nightly,
        output=rustdoc_json,
        env=rustdoc_env(),
        features="http-ureq",
    )


def load_rustdoc(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def index_entry(index: dict[str, Any], item_id: int | str) -> dict[str, Any] | None:
    return index.get(str(item_id)) or index.get(item_id)  # type: ignore[arg-type]


def type_display(type_node: Any) -> str:
    """Render a rustdoc type node to a stable string for matching."""
    if type_node is None:
        return ""
    if isinstance(type_node, str):
        return type_node
    if not isinstance(type_node, dict):
        return json.dumps(type_node, sort_keys=True)

    if "primitive" in type_node:
        return str(type_node["primitive"])
    if "generic" in type_node:
        return str(type_node["generic"])
    if "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        path = str(rp.get("path", ""))
        args = rp.get("args")
        inner = _format_generic_args(args)
        if inner:
            return f"{path}<{inner}>"
        return path
    if "qualified_path" in type_node:
        qp = type_node["qualified_path"]
        self_ty = type_display(qp.get("self_type"))
        name = qp.get("name", "")
        return f"{self_ty}::{name}" if self_ty else str(name)
    if "borrowed_ref" in type_node:
        br = type_node["borrowed_ref"]
        mut = "mut " if br.get("is_mutable") else ""
        return f"&{mut}{type_display(br.get('type'))}"
    if "raw_pointer" in type_node:
        rp = type_node["raw_pointer"]
        mut = "mut " if rp.get("is_mutable") else ""
        return f"*{mut}{type_display(rp.get('type'))}"
    if "dyn_trait" in type_node:
        return "dyn"
    if "type" in type_node:
        return type_display(type_node["type"])
    return json.dumps(type_node, sort_keys=True)


def _format_generic_args(args: Any) -> str:
    if not args:
        return ""
    if isinstance(args, list):
        return ",".join(type_display({"type": a} if "type" not in a else a) for a in args)
    if isinstance(args, dict):
        ab = args.get("angle_bracketed")
        if ab:
            parts = []
            for arg in ab.get("args", []):
                if "type" in arg:
                    parts.append(type_display(arg["type"]))
                else:
                    parts.append(type_display(arg))
            return ",".join(parts)
    return ""


def resolve_type_node(
    type_node: Any,
    index: dict[str, Any],
    *,
    seen_aliases: set[str] | None = None,
) -> Any:
    """Follow public type aliases to their underlying type (cycle-safe)."""
    if seen_aliases is None:
        seen_aliases = set()
    if not isinstance(type_node, dict):
        return type_node
    if "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        alias_id = rp.get("id")
        if alias_id is not None:
            sid = str(alias_id)
            if sid not in seen_aliases:
                item = index_entry(index, sid)
                inner = (item or {}).get("inner") or {}
                if item and item.get("visibility") == "public" and "type_alias" in inner:
                    seen_aliases.add(sid)
                    aliased = inner["type_alias"].get("type")
                    return resolve_type_node(aliased, index, seen_aliases=seen_aliases)
    if "borrowed_ref" in type_node:
        br = type_node["borrowed_ref"]
        resolved_inner = resolve_type_node(
            br.get("type"),
            index,
            seen_aliases=seen_aliases,
        )
        if resolved_inner is not br.get("type"):
            return {
                "borrowed_ref": {
                    **br,
                    "type": resolved_inner,
                }
            }
    if "type" in type_node:
        inner = resolve_type_node(type_node["type"], index, seen_aliases=seen_aliases)
        if inner is not type_node["type"]:
            return {**type_node, "type": inner}
    return type_node


def result_error_type(
    output: Any,
    index: dict[str, Any],
) -> str | None:
    """If ``output`` is ``Result<_, E>`` (after alias resolution), return ``E``."""
    if output is None:
        return None
    resolved = resolve_type_node(output, index)
    if not isinstance(resolved, dict):
        return None
    rp = resolved.get("resolved_path")
    if not isinstance(rp, dict):
        return None
    path = str(rp.get("path", ""))
    if "Result" not in path:
        return None
    args = rp.get("args")
    if isinstance(args, dict):
        ab = args.get("angle_bracketed", {})
        arg_list = ab.get("args", [])
        if len(arg_list) >= 2:
            err = arg_list[1]
            if "type" in err:
                return type_display(err["type"])
            return type_display(err)
    if isinstance(args, list) and len(args) >= 2:
        return type_display(args[1])
    return None


def _resolved_path_name(type_node: Any) -> str | None:
    if not isinstance(type_node, dict):
        return None
    if "resolved_path" in type_node:
        return str(type_node["resolved_path"].get("path", ""))
    if "type" in type_node:
        return _resolved_path_name(type_node["type"])
    return None


def is_exact_repository_ref(type_node: Any) -> bool:
    """True only for ``&Repository`` or ``&mut Repository`` (not options or related types)."""
    if not isinstance(type_node, dict) or "borrowed_ref" not in type_node:
        return False
    inner = type_node["borrowed_ref"].get("type")
    path = _resolved_path_name(inner)
    if not path:
        return False
    if path == REPOSITORY_TYPE_PLAIN or path.endswith(REPOSITORY_TYPE_SUFFIX):
        return "Options" not in path
    return False


def is_exact_path_ref(type_node: Any) -> bool:
    """True only for ``&Path`` / ``&mut Path`` (``std::path::Path`` or plain ``Path``)."""
    if not isinstance(type_node, dict) or "borrowed_ref" not in type_node:
        return False
    inner = type_node["borrowed_ref"].get("type")
    path = _resolved_path_name(inner)
    if not path:
        return False
    return path == PATH_TYPE_PLAIN or path.endswith("::Path")


def build_associated_function_ids(index: dict[str, Any]) -> frozenset[str]:
    """Function ids declared inside public impl or trait blocks (incl. static fns)."""
    associated: set[str] = set()
    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        inner = item.get("inner") or {}
        for key in ("impl", "trait"):
            if key not in inner:
                continue
            for member_id in inner[key].get("items") or []:
                member = index_entry(index, member_id)
                if member and "function" in (member.get("inner") or {}):
                    associated.add(str(member_id))
    return frozenset(associated)


def build_function_impl_for(index: dict[str, Any]) -> dict[str, str]:
    """Map public function item id -> ``type_display`` of the impl's ``for`` type."""
    mapping: dict[str, str] = {}
    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        inner = item.get("inner") or {}
        if "impl" not in inner:
            continue
        for_path = impl_for_type_path(item)
        if not for_path:
            continue
        for member_id in inner["impl"].get("items") or []:
            member = index_entry(index, member_id)
            if member and "function" in (member.get("inner") or {}):
                mapping[str(member_id)] = for_path
    return mapping


def is_str_or_string_type(type_node: Any) -> bool:
    """True for ``&str``, ``&mut str``, or ``String`` (after one layer of borrow)."""
    if isinstance(type_node, dict) and "borrowed_ref" in type_node:
        inner = type_node["borrowed_ref"].get("type")
        if isinstance(inner, dict) and inner.get("primitive") == "str":
            return True
    if isinstance(type_node, dict) and type_node.get("primitive") == "str":
        return True
    path = _resolved_path_name(type_node)
    if path in STRING_RUST_TYPES or (path and path.endswith("::String")):
        return True
    return False


def is_pathspec_string_collection(type_node: Any) -> bool:
    """``&[String]``, ``Vec<String>``, or ``&[&str]``."""
    display = type_display(type_node)
    if "&[&str]" in display or "&[ &str ]" in display.replace(" ", ""):
        return True
    if "Vec<String>" in display or "Vec<alloc::string::String>" in display:
        return True
    if "&[String]" in display or "&[alloc::string::String]" in display:
        return True
    if not isinstance(type_node, dict):
        return False
    if "borrowed_ref" in type_node:
        return is_pathspec_string_collection(type_node["borrowed_ref"].get("type"))
    if "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        path = str(rp.get("path", ""))
        if path.endswith("::Vec") or path == "Vec":
            args = rp.get("args")
            inner = _first_generic_arg_type(args)
            if inner and is_str_or_string_type(inner):
                return True
        if path.endswith("::slice") or path == "slice" or "[T]" in display:
            inner = _first_generic_arg_type(rp.get("args"))
            if inner and is_str_or_string_type(inner):
                return True
    return False


def _first_generic_arg_type(args: Any) -> Any | None:
    if isinstance(args, dict):
        ab = args.get("angle_bracketed", {})
        arg_list = ab.get("args", [])
        if arg_list:
            first = arg_list[0]
            return first.get("type") if isinstance(first, dict) and "type" in first else first
    if isinstance(args, list) and args:
        first = args[0]
        return first.get("type") if isinstance(first, dict) and "type" in first else first
    return None


def type_is_bool(type_node: Any) -> bool:
    if isinstance(type_node, dict) and "type" in type_node:
        type_node = type_node["type"]
    return isinstance(type_node, dict) and type_node.get("primitive") == "bool"


def _type_node_is_unit(type_node: Any) -> bool:
    if type_node is None:
        return False
    if isinstance(type_node, dict):
        if type_node.get("primitive") == "unit":
            return True
        if "tuple" in type_node and not type_node["tuple"]:
            return True
        if "type" in type_node:
            return _type_node_is_unit(type_node["type"])
    return type_display(type_node) in {"()", "unit"}


def returns_bool_or_unit_result(type_node: Any, index: dict[str, Any]) -> bool:
    if type_is_bool(type_node):
        return True
    if isinstance(type_node, dict) and "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        if "Result" in str(rp.get("path", "")):
            ok_ty = _first_generic_arg_type(rp.get("args"))
            if ok_ty is not None:
                ok_inner = ok_ty.get("type") if isinstance(ok_ty, dict) and "type" in ok_ty else ok_ty
                if _type_node_is_unit(ok_inner):
                    return True
    resolved = resolve_type_node(type_node, index)
    if not isinstance(resolved, dict):
        return False
    rp = resolved.get("resolved_path")
    if not isinstance(rp, dict) or "Result" not in str(rp.get("path", "")):
        return False
    ok_ty = _first_generic_arg_type(rp.get("args"))
    if ok_ty is None:
        return False
    ok_inner = ok_ty.get("type") if isinstance(ok_ty, dict) and "type" in ok_ty else ok_ty
    return _type_node_is_unit(ok_inner)


def _cancel_trait_path_tail(trait_node: Any) -> str:
    """Return the last path segment for a dyn-trait entry (rustdoc shape varies)."""
    if not isinstance(trait_node, dict):
        return ""
    inner = trait_node.get("trait")
    if isinstance(inner, dict) and "path" in inner:
        return str(inner["path"]).split("::")[-1]
    rp = trait_node.get("resolved_path")
    if isinstance(rp, dict) and "path" in rp:
        return str(rp["path"]).split("::")[-1]
    disp = type_display(trait_node)
    if "::" in disp:
        return disp.split("::")[-1]
    return disp


def _trait_entry_is_cancel_trait(trait: Any, _index: dict[str, Any]) -> bool:
    """True when a rustdoc-json dyn_trait entry refers to the ``Cancel`` trait."""
    if not isinstance(trait, dict):
        return False
    # Newer rustdoc-json sometimes encodes the trait as ``resolved_path`` at the trait-object slot
    # instead of wrapping it in ``{"trait": {...}}``.
    if "resolved_path" in trait:
        path = str(trait["resolved_path"].get("path", ""))
        if path == "Cancel" or path.endswith("::Cancel"):
            return True
    if "trait" in trait:
        inner = trait["trait"]
        if isinstance(inner, dict):
            path = str(inner.get("path", ""))
            if path == "Cancel" or path.endswith("::Cancel"):
                return True
            # Some rustdoc encodings nest another ``trait`` dict with the path.
            nested = inner.get("trait")
            if isinstance(nested, dict):
                np = str(nested.get("path", ""))
                if np == "Cancel" or np.endswith("::Cancel"):
                    return True
    return _cancel_trait_path_tail(trait) == "Cancel"


def is_cancel_surface_type(type_node: Any, index: dict[str, Any]) -> bool:
    """True for cancellation in the public API: dyn cancel traits or CancelToken/CancelSignal handles."""
    resolved = resolve_type_node(type_node, index)
    if isinstance(resolved, dict) and "dyn_trait" in resolved:
        traits = resolved["dyn_trait"].get("traits") or []
        for trait in traits:
            if _trait_entry_is_cancel_trait(trait, index):
                return True
            tail = _cancel_trait_path_tail(trait)
            if tail in ("Cancel", "CancelToken", "CancelSignal"):
                return True
    if isinstance(resolved, dict) and "borrowed_ref" in resolved:
        return is_cancel_surface_type(resolved["borrowed_ref"].get("type"), index)
    display = type_display(resolved)
    if "CancelToken" in display or "CancelSignal" in display:
        return True
    return "dyn" in display and "Cancel" in display


def is_atomic_bool_watch_type(type_node: Any, index: dict[str, Any]) -> bool:
    resolved = resolve_type_node(type_node, index)
    display = type_display(resolved)
    if "AtomicBool" not in display:
        return False
    if "Arc" in display:
        return True
    if isinstance(resolved, dict) and "borrowed_ref" in resolved:
        return True
    return display.startswith("&") and "AtomicBool" in display


def _type_is_repository_node(resolved: Any) -> bool:
    path = _resolved_path_name(resolved)
    if not path:
        return False
    return path == REPOSITORY_TYPE_PLAIN or path.endswith(REPOSITORY_TYPE_SUFFIX)


def returns_repository(
    type_node: Any,
    index: dict[str, Any],
    *,
    impl_for: str | None = None,
) -> bool:
    if type_node is None:
        return False
    if isinstance(type_node, dict) and type_node.get("generic") == "Self":
        return _type_is_repository_node({"resolved_path": {"path": impl_for or ""}})
    if _type_is_repository_node(type_node):
        return True
    if isinstance(type_node, dict) and "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        path = str(rp.get("path", ""))
        if "Option" in path:
            inner = _first_generic_arg_type(rp.get("args"))
            if inner is not None:
                inner_ty = inner.get("type") if isinstance(inner, dict) and "type" in inner else inner
                if returns_repository(inner_ty, index, impl_for=impl_for):
                    return True
        if "Result" in path:
            ok = _first_generic_arg_type(rp.get("args"))
            if ok is not None:
                ok_ty = ok.get("type") if isinstance(ok, dict) and "type" in ok else ok
                if isinstance(ok_ty, dict) and ok_ty.get("generic") == "Self":
                    if _type_is_repository_node({"resolved_path": {"path": impl_for or ""}}):
                        return True
                if returns_repository(ok_ty, index, impl_for=impl_for):
                    return True
    resolved = resolve_type_node(type_node, index)
    if resolved is type_node:
        return False
    return returns_repository(resolved, index, impl_for=impl_for)


def _result_ok_type(resolved: Any) -> Any | None:
    if not isinstance(resolved, dict):
        return None
    rp = resolved.get("resolved_path")
    if not isinstance(rp, dict) or "Result" not in str(rp.get("path", "")):
        return None
    return _first_generic_arg_type(rp.get("args"))


def impl_for_type_path(item: dict[str, Any]) -> str | None:
    inner = item.get("inner") or {}
    if "impl" not in inner:
        return None
    for_block = inner["impl"].get("for") or inner["impl"].get("type")
    if for_block is None:
        return None
    return type_display(for_block)


def handle_borrows_repository(for_path: str | None) -> bool:
    if not for_path:
        return False
    if for_path == REPOSITORY_TYPE_PLAIN or for_path.endswith(REPOSITORY_TYPE_SUFFIX):
        return True
    if "repo_handles" in for_path:
        return True
    if for_path.endswith("::References") or for_path.endswith("::Diff"):
        return True
    if "IndexHandle" in for_path or "ConfigHandle" in for_path or "Objects" in for_path:
        return True
    return False


def has_configset_param(inputs: list[tuple[str, Any]], index: dict[str, Any]) -> bool:
    for _name, ty in inputs:
        display = type_display(resolve_type_node(ty, index))
        if "ConfigSet" in display:
            return True
    return False


def has_repository_first_param(inputs: list[tuple[str, Any]]) -> bool:
    if not inputs:
        return False
    _name, ty = inputs[0]
    return is_exact_repository_ref(ty) or (
        _resolved_path_name(ty) == REPOSITORY_TYPE_PLAIN
        or (_resolved_path_name(ty) or "").endswith(REPOSITORY_TYPE_SUFFIX)
    )


def trait_name_from_item(item: dict[str, Any]) -> str | None:
    return item.get("name")


def trait_has_progress_sink_supertrait(trait_inner: dict[str, Any]) -> bool:
    for bound in trait_inner.get("bounds") or []:
        tb = bound.get("trait_bound") or {}
        trait = tb.get("trait") or {}
        path = str(trait.get("path", ""))
        if path == "ProgressSink" or path.endswith("::ProgressSink"):
            return True
    decl = trait_inner.get("decl") or {}
    for bound in decl.get("supertraits") or []:
        if "ProgressSink" in type_display(bound):
            return True
    return False


def _dyn_fnmut_input_types(type_node: Any) -> list[Any]:
    """Extract FnMut callback input types from ``dyn FnMut(...)`` rustdoc nodes."""
    candidates: list[Any] = [type_node]
    if isinstance(type_node, dict) and "borrowed_ref" in type_node:
        candidates.append(type_node["borrowed_ref"].get("type"))
    out: list[Any] = []
    for node in candidates:
        if not isinstance(node, dict) or "dyn_trait" not in node:
            continue
        for entry in node["dyn_trait"].get("traits") or []:
            trait = entry.get("trait") or {}
            if "FnMut" not in str(trait.get("path", "")):
                continue
            args = trait.get("args") or {}
            paren = args.get("parenthesized") or {}
            out.extend(paren.get("inputs") or [])
            angle = args.get("angle_bracketed") or {}
            fn_args = _first_generic_arg_type({"angle_bracketed": angle})
            if fn_args is not None:
                if isinstance(fn_args, dict) and "tuple" in fn_args:
                    for entry in fn_args["tuple"]:
                        ty = entry.get("type") if isinstance(entry, dict) else entry
                        if ty is not None:
                            out.append(ty)
                else:
                    out.append(fn_args)
    return out


def fn_param_is_fnmut_with_progress_arg(type_node: Any, index: dict[str, Any]) -> bool:
    for inp in _dyn_fnmut_input_types(type_node):
        if "Progress" in type_display(inp):
            return True
    resolved = resolve_type_node(type_node, index)
    if not isinstance(resolved, dict):
        return False
    rp = resolved.get("resolved_path")
    if not isinstance(rp, dict):
        return False
    path = str(rp.get("path", ""))
    if "FnMut" not in path:
        return False
    args = rp.get("args")
    fn_args = _first_generic_arg_type(args)
    if fn_args is None:
        return False
    if isinstance(fn_args, dict) and "tuple" in fn_args:
        for entry in fn_args["tuple"]:
            ty = entry.get("type") if isinstance(entry, dict) else entry
            if ty and "Progress" in type_display(ty):
                return True
        return False
    return "Progress" in type_display(fn_args)


def module_path_allowed(path: str, prefixes: tuple[str, ...]) -> bool:
    return any(path.startswith(prefix) for prefix in prefixes)


def item_has_public_path_prefix(
    item_id: str,
    path_map: dict[str, list[str]],
    fallback_path: str,
    prefix: str,
) -> bool:
    paths = path_map.get(item_id) or []
    if not paths and fallback_path:
        paths = [fallback_path]
    return any(p.startswith(prefix) for p in paths)


def item_defined_in_refs(
    item_id: str,
    path_map: dict[str, list[str]],
    fallback_path: str,
    item: dict[str, Any],
) -> bool:
    """True when exported under ``grit_lib::refs::`` or defined under ``src/refs/``."""
    if item_has_public_path_prefix(item_id, path_map, fallback_path, "grit_lib::refs::"):
        return True
    span = item.get("span") or {}
    filename = str(span.get("filename", "")).replace("\\", "/")
    if "/refs/" in filename or filename.endswith("/refs/mod.rs"):
        return True
    if filename.endswith("/check_ref_format.rs"):
        return True
    candidates = [
        Path(filename),
        GRIT_LIB / "src" / Path(filename).name,
        GRIT_LIB / "src" / filename.replace("grit-lib/src/", ""),
    ]
    for path in candidates:
        try:
            rel = path.resolve().relative_to((GRIT_LIB / "src").resolve())
        except (ValueError, OSError):
            continue
        parts = rel.parts
        if parts and parts[0] == "refs":
            return True
    return False


def has_with_in_name(name: str) -> bool:
    """True for ``_with_`` variant names (``open_with_options``), not ``with_<field>`` setters."""
    return name.find("_with_") > 0


def is_bare_bool(type_node: Any) -> bool:
    if isinstance(type_node, dict) and "type" in type_node:
        type_node = type_node["type"]
    if not isinstance(type_node, dict):
        return False
    return type_node.get("primitive") == "bool"


def function_inputs(fn: dict[str, Any]) -> list[tuple[str, Any]]:
    """Parameter list from rustdoc JSON (``function.sig.inputs`` or top-level ``function.inputs``)."""
    sig = fn.get("sig") or {}
    raw: Any = sig.get("inputs")
    if raw is None:
        raw = fn.get("inputs") or []
    out: list[tuple[str, Any]] = []
    for entry in raw:
        if isinstance(entry, (list, tuple)) and len(entry) == 2:
            out.append((str(entry[0]), entry[1]))
        elif isinstance(entry, dict) and "name" in entry and "type" in entry:
            out.append((str(entry["name"]), entry["type"]))
    return out


def is_method(fn: dict[str, Any]) -> bool:
    inputs = function_inputs(fn)
    if not inputs:
        return False
    return inputs[0][0] in {"self", "mut self"}


def is_unsafe_fn(fn: dict[str, Any]) -> bool:
    header = fn.get("header") or {}
    return bool(header.get("is_unsafe"))


def item_has_deprecation(item: dict[str, Any]) -> bool:
    """True when rustdoc marks the item deprecated (top-level `deprecation` on the item).

    Rustdoc JSON puts `#[deprecated]` on `Item.deprecation`, not in `attrs`.
    """
    return item.get("deprecation") is not None


def count_pub_deprecated_items(index: dict[str, Any]) -> int:
    total = 0
    for item in index.values():
        if not isinstance(item, dict):
            continue
        if item.get("crate_id") != 0 or item.get("visibility") != "public":
            continue
        if item_has_deprecation(item):
            total += 1
    return total


def attrs_contain_too_many_args(attrs: Iterable[Any]) -> bool:
    for attr in attrs:
        if isinstance(attr, dict):
            text = attr.get("other") or attr.get("text") or ""
        else:
            text = str(attr)
        if TOO_MANY_ARGS_RE.search(text):
            return True
    return False


def read_source_attr_lines(span: dict[str, Any], needle: str) -> bool:
    filename = span.get("filename")
    begin = span.get("begin")
    if not filename or not begin:
        return False
    raw = Path(str(filename))
    candidates = [
        raw,
        GRIT_LIB / "src" / raw.name,
        GRIT_LIB / "src" / str(filename).replace("grit-lib/src/", ""),
        ROOT / str(filename),
    ]
    path = next((p for p in candidates if p.is_file()), None)
    if path is None:
        return False
    lines = path.read_text(encoding="utf-8").splitlines()
    start_line = int(begin[0]) - 1
    for idx in range(max(0, start_line - 8), start_line):
        if needle in lines[idx]:
            return True
    return False


def has_too_many_arguments_allow(item: dict[str, Any]) -> bool:
    if attrs_contain_too_many_args(item.get("attrs") or []):
        return True
    span = item.get("span") or {}
    return read_source_attr_lines(span, "clippy::too_many_arguments")


def mentions_libc(item: dict[str, Any]) -> bool:
    return "libc::" in json.dumps(item)


def iter_public_functions(index: dict[str, Any]) -> Iterable[tuple[str, dict[str, Any], dict[str, Any]]]:
    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        if item.get("visibility") != "public":
            continue
        inner = item.get("inner") or {}
        if "function" not in inner:
            continue
        yield str(item_id), item, inner["function"]


def iter_public_trait_methods(
    index: dict[str, Any],
    paths_map: dict[str, Any],
) -> Iterable[tuple[str, dict[str, Any], dict[str, Any], str]]:
    """Trait methods on public traits (rustdoc marks them ``default``, not ``public``)."""
    for trait_id, trait_item in index.items():
        if trait_item.get("crate_id") != 0 or trait_item.get("visibility") != "public":
            continue
        trait_inner = (trait_item.get("inner") or {}).get("trait")
        if not trait_inner:
            continue
        trait_path = public_path_for_item(index, paths_map, str(trait_id))
        for member_id in trait_inner.get("items") or []:
            member = index_entry(index, member_id)
            if member is None:
                continue
            if member.get("visibility") not in {"public", "default"}:
                continue
            member_inner = member.get("inner") or {}
            if "function" not in member_inner:
                continue
            member_name = member.get("name") or "?"
            if trait_path and not trait_path.startswith("id:"):
                method_path = f"{trait_path}::{member_name}"
            else:
                method_path = public_path_for_item(index, paths_map, str(member_id))
            yield str(member_id), member, member_inner["function"], method_path


def struct_field_member_ids(struct_inner: dict[str, Any]) -> list[str]:
    """Field item ids from rustdoc ``inner.struct.kind`` (plain or tuple struct)."""
    kind = struct_inner.get("kind")
    if not isinstance(kind, dict):
        return []
    ids: list[str] = []
    plain = kind.get("plain")
    if isinstance(plain, dict):
        ids.extend(str(i) for i in plain.get("fields") or [])
    tuple_fields = kind.get("tuple")
    if isinstance(tuple_fields, list):
        ids.extend(str(i) for i in tuple_fields)
    return ids


def type_node_from_struct_field(field_inner: dict[str, Any]) -> Any | None:
    """Turn ``inner.struct_field`` into a type node for ``type_display`` / matchers."""
    sf = field_inner.get("struct_field")
    if not isinstance(sf, dict):
        return None
    if any(key in sf for key in ("resolved_path", "primitive", "borrowed_ref", "generic", "dyn_trait")):
        return sf
    if "type" in sf:
        return sf["type"]
    return sf


def iter_public_struct_fields(
    index: dict[str, Any],
) -> Iterable[tuple[str, str, str, Any]]:
    """Yield ``(struct_path, field_name, field_id, type_node)`` for public fields."""
    for struct_id, struct_item in index.items():
        if struct_item.get("crate_id") != 0 or struct_item.get("visibility") != "public":
            continue
        struct_inner = (struct_item.get("inner") or {}).get("struct")
        if not struct_inner:
            continue
        struct_name = struct_item.get("name") or "?"
        for field_id in struct_field_member_ids(struct_inner):
            field_item = index_entry(index, field_id)
            if field_item is None or field_item.get("visibility") != "public":
                continue
            field_name = field_item.get("name")
            if field_name is None:
                field_name = field_id
            field_type = type_node_from_struct_field(field_item.get("inner") or {})
            if field_type is None:
                continue
            yield struct_id, struct_name, str(field_name), field_type


def public_path_for_item(
    index: dict[str, Any],
    paths_map: dict[str, Any],
    item_id: str,
) -> str:
    meta = paths_map.get(item_id)
    if meta and meta.get("path"):
        return "::".join(meta["path"])
    item = index_entry(index, item_id)
    if not item:
        return f"id:{item_id}"
    span = item.get("span") or {}
    filename = span.get("filename", "?")
    begin = span.get("begin", ["?", "?"])
    name = item.get("name") or "?"
    return f"{name} ({filename}:{begin[0]})"


def count_error_message_construction_sites() -> int:
    """Occurrences of ``Error::Message(`` under ``grit-lib/src`` (migration ratchet)."""
    total = 0
    src_root = GRIT_LIB / "src"
    if not src_root.is_dir():
        return 0
    for path in src_root.rglob("*.rs"):
        total += path.read_text(encoding="utf-8").count("Error::Message(")
    return total


def item_is_non_exhaustive(item: dict[str, Any]) -> bool:
    for attr in item.get("attrs") or []:
        if isinstance(attr, dict):
            text = attr.get("other") or attr.get("text") or ""
        else:
            text = str(attr)
        if "non_exhaustive" in text:
            return True
    return False


def enum_variant_nodes(enum_inner: dict[str, Any], index: dict[str, Any]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for entry in enum_inner.get("variants", []):
        vid = entry.get("id") if isinstance(entry, dict) else entry
        variant = index_entry(index, vid) if vid is not None else None
        if variant:
            out.append(variant)
    return out


def variant_field_types(variant: dict[str, Any], index: dict[str, Any]) -> list[Any]:
    inner = variant.get("inner") or {}
    if "variant" not in inner:
        return []
    vinner = inner["variant"]
    kind = vinner.get("kind")
    types: list[Any] = []
    if kind == "struct":
        for field in vinner.get("fields") or []:
            if isinstance(field, dict) and "type" in field:
                types.append(field["type"])
    elif kind == "tuple":
        for field in vinner.get("fields") or []:
            if isinstance(field, dict) and "type" in field:
                types.append(field["type"])
    elif isinstance(kind, dict) and ("tuple" in kind or "struct" in kind):
        field_ids = kind["tuple"] if "tuple" in kind else (kind["struct"] or {}).get("fields", [])
        for fid in field_ids:
            if fid is None:
                continue
            field = index_entry(index, fid)
            if not field:
                continue
            finner = field.get("inner") or {}
            if "type" in finner:
                types.append(finner["type"])
            elif "struct_field" in finner:
                sf = finner["struct_field"]
                if isinstance(sf, dict) and "type" in sf:
                    types.append(sf["type"])
                else:
                    types.append(sf)
    return types


def variant_is_tuple_kind(variant: dict[str, Any]) -> bool:
    """True when rustdoc describes a tuple variant (legacy inline or modern field-id encoding)."""
    inner = variant.get("inner") or {}
    if "variant" not in inner:
        return False
    kind = inner["variant"].get("kind")
    return kind == "tuple" or (isinstance(kind, dict) and "tuple" in kind)


def variant_is_string_tuple_payload(
    variant: dict[str, Any],
    index: dict[str, Any],
) -> bool:
    """True when a tuple variant's sole payload is ``String`` (not named struct fields)."""
    if not variant_is_tuple_kind(variant):
        return False
    field_types = variant_field_types(variant, index)
    return len(field_types) == 1 and type_display(field_types[0]) in STRING_RUST_TYPES


def variant_carries_catch_all_string(
    variant: dict[str, Any],
    index: dict[str, Any],
) -> bool:
    """True when a public `*Error` variant is a catch-all carrying a `String` (any rustdoc tuple shape)."""
    name = variant.get("name") or ""
    if name not in ERROR_CATCH_ALL_VARIANTS:
        return False
    field_types = variant_field_types(variant, index)
    return any(type_display(ty) in STRING_RUST_TYPES for ty in field_types)


UMBRELLA_ERROR_SOURCE_PATHS = frozenset(
    {UMBRELLA_ERROR_PATH, "crate::error::Error", "crate::Error", "error::Error"}
)


def resolved_path_is_umbrella(rp: dict[str, Any], paths_map: dict[str, Any]) -> bool:
    """True when a rustdoc ``resolved_path`` names ``grit_lib::error::Error`` itself.

    The item id is authoritative when the ``paths`` table knows it; the written path is
    only consulted for ids rustdoc did not record. Other types that happen to be called
    ``Error`` (``std::io::Error``, ``regex::Error``) or merely end in ``Error`` are not
    the umbrella.
    """
    rid = rp.get("id")
    if rid is not None:
        meta = paths_map.get(str(rid))
        if meta is not None:
            return meta.get("crate_id") == 0 and "::".join(meta.get("path", [])) == UMBRELLA_ERROR_PATH
    return rp.get("path", "") in UMBRELLA_ERROR_SOURCE_PATHS


def field_type_is_umbrella_error(type_node: Any, paths_map: dict[str, Any] | None = None) -> bool:
    """True when ``type_node`` holds the umbrella error anywhere in its type tree.

    Covers the umbrella directly and inside generic arguments, tuples, slices, arrays and
    references (``Box<Error>``, ``Vec<(String, Error)>``, ``Option<Box<Error>>``).
    """
    paths_map = paths_map or {}
    if isinstance(type_node, dict):
        rp = type_node.get("resolved_path")
        if isinstance(rp, dict) and resolved_path_is_umbrella(rp, paths_map):
            return True
        return any(field_type_is_umbrella_error(v, paths_map) for v in type_node.values())
    if isinstance(type_node, list):
        return any(field_type_is_umbrella_error(v, paths_map) for v in type_node)
    return False


def collect_error_ratchet_metrics(
    index: dict[str, Any],
    paths_map: dict[str, Any],
) -> dict[str, MetricResult]:
    umbrella_variants = 0
    catch_all: list[str] = []
    string_tuple: list[str] = []
    not_non_exhaustive: list[str] = []
    wraps_umbrella: list[str] = []

    for item_id, item in index.items():
        if item.get("crate_id") != 0 or item.get("visibility") != "public":
            continue
        name = item.get("name") or ""
        inner = item.get("inner") or {}
        if "enum" not in inner or not name.endswith("Error"):
            continue
        path = public_path_for_item(index, paths_map, str(item_id))
        enum_inner = inner["enum"]
        if name == "Error" and "error" in path:
            umbrella_variants = len(enum_variant_nodes(enum_inner, index))
        if not item_is_non_exhaustive(item):
            not_non_exhaustive.append(f"{path} :: {name}")
        for variant in enum_variant_nodes(enum_inner, index):
            vname = variant.get("name") or "?"
            vpath = f"{path} :: {vname}"
            if variant_carries_catch_all_string(variant, index):
                catch_all.append(vpath)
            if variant_is_string_tuple_payload(variant, index):
                string_tuple.append(vpath)
            if any(
                field_type_is_umbrella_error(fty, paths_map)
                for fty in variant_field_types(variant, index)
            ):
                wraps_umbrella.append(vpath)
                continue
            vinner = variant.get("inner") or {}
            if "variant" in vinner:
                for field in vinner["variant"].get("fields") or []:
                    if not isinstance(field, dict):
                        continue
                    fty = field.get("type")
                    if fty and field_type_is_umbrella_error(fty, paths_map):
                        wraps_umbrella.append(vpath)
                        break

    msg_sites = count_error_message_construction_sites()
    return {
        "umbrella_error_variants": MetricResult(umbrella_variants),
        "pub_error_catch_all_variants": MetricResult(len(catch_all), catch_all),
        "pub_error_string_tuple_variants": MetricResult(len(string_tuple), string_tuple),
        "pub_error_enum_not_non_exhaustive": MetricResult(
            len(not_non_exhaustive),
            not_non_exhaustive,
        ),
        "pub_area_error_wraps_umbrella": MetricResult(len(wraps_umbrella), wraps_umbrella),
        "error_message_construction_sites": MetricResult(msg_sites),
    }



def collect_public_paths(payload: dict[str, Any]) -> dict[str, list[str]]:
    """Map rustdoc item id -> every public path reaching it (modules, reexports, globs)."""
    index = payload["index"]
    by_id: dict[str, list[str]] = defaultdict(list)

    root_id: str | None = None
    for pid, meta in payload.get("paths", {}).items():
        if meta.get("path") == ["grit_lib"]:
            root_id = str(pid)
            break
    if root_id is None:
        return by_id

    def record(item_id: int | str, parts: tuple[str, ...]) -> None:
        if not parts or parts[0] != "grit_lib":
            return
        path = "::".join(parts)
        existing = by_id[str(item_id)]
        if path not in existing:
            existing.append(path)

    def export_module_contents(
        module_id: str,
        prefix: tuple[str, ...],
        expanding: set[str],
    ) -> None:
        if module_id in expanding:
            return
        expanding.add(module_id)
        try:
            item = index_entry(index, module_id)
            if item is None or item.get("visibility") != "public":
                return
            inner = item.get("inner") or {}
            if "module" not in inner:
                return
            for child_id in inner["module"].get("items", []):
                child = index_entry(index, child_id)
                if child is None or child.get("visibility") != "public":
                    continue
                child_inner = child.get("inner") or {}
                if "use" in child_inner:
                    use = child_inner["use"]
                    name = use.get("name") or child.get("name")
                    target_id = use.get("id")
                    if use.get("is_glob"):
                        if target_id is not None:
                            export_module_contents(str(target_id), prefix, expanding)
                    elif name and target_id is not None:
                        new_prefix = prefix + (name,)
                        record(target_id, new_prefix)
                        target = index_entry(index, target_id)
                        if target and "module" in (target.get("inner") or {}):
                            walk_module(str(target_id), new_prefix, expanding)
                elif "module" in child_inner:
                    name = child.get("name")
                    if name:
                        sub_prefix = prefix + (name,)
                        record(child_id, sub_prefix)
                        walk_module(str(child_id), sub_prefix, expanding)
                else:
                    name = child.get("name")
                    if name:
                        record(child_id, prefix + (name,))
        finally:
            expanding.discard(module_id)

    def walk_module(
        module_id: str,
        prefix: tuple[str, ...],
        expanding: set[str],
    ) -> None:
        item = index_entry(index, module_id)
        if item is None or item.get("visibility") != "public":
            return
        inner = item.get("inner") or {}
        if "module" not in inner:
            return
        for child_id in inner["module"].get("items", []):
            child = index_entry(index, child_id)
            if child is None or child.get("visibility") != "public":
                continue
            child_inner = child.get("inner") or {}
            if "use" in child_inner:
                use = child_inner["use"]
                name = use.get("name") or child.get("name")
                target_id = use.get("id")
                if use.get("is_glob"):
                    if target_id is not None:
                        export_module_contents(str(target_id), prefix, expanding)
                elif name and target_id is not None:
                    new_prefix = prefix + (name,)
                    record(target_id, new_prefix)
                    target = index_entry(index, target_id)
                    if target and "module" in (target.get("inner") or {}):
                        walk_module(str(target_id), new_prefix, expanding)
            elif "module" in child_inner:
                name = child.get("name")
                if name:
                    walk_module(str(child_id), prefix + (name,), expanding)
            else:
                name = child.get("name")
                if name:
                    record(child_id, prefix + (name,))

    walk_module(root_id, ("grit_lib",), set())
    return by_id


def load_canonical_module_map(path: Path | None = None) -> dict[str, str]:
    """Flat implementation module -> top-level public module, from ``canonical-modules.toml``."""
    source = path or CANONICAL_MODULES_PATH
    if not source.is_file():
        return {}
    table = tomllib.loads(source.read_text(encoding="utf-8")).get("map", {})
    return {
        str(flat): str(canonical).split("::")[0]
        for flat, canonical in table.items()
        if isinstance(canonical, str) and canonical
    }


def top_module_of_path(path: str) -> str | None:
    """Top-level module of a ``grit_lib::…`` item path; crate-root items get a fixed label."""
    parts = path.split("::")
    if not parts or parts[0] != "grit_lib":
        return None
    if len(parts) <= 2:
        return ROOT_MODULE_LABEL
    return parts[1]


def build_member_owner(index: dict[str, Any]) -> dict[str, str]:
    """Map impl/trait member item id -> id of the type (or trait) that owns it."""
    owners: dict[str, str] = {}
    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        inner = item.get("inner") or {}
        owner: str | None = None
        members: list[Any] = []
        if "impl" in inner:
            for_node = inner["impl"].get("for") or {}
            rp = for_node.get("resolved_path") if isinstance(for_node, dict) else None
            if isinstance(rp, dict) and rp.get("id") is not None:
                owner = str(rp["id"])
            members = inner["impl"].get("items") or []
        elif "trait" in inner:
            owner = str(item_id)
            members = inner["trait"].get("items") or []
        if owner is None:
            continue
        for member_id in members:
            owners.setdefault(str(member_id), owner)
    return owners


class ModuleResolver:
    """Resolve an item id to the top-level public module a caller imports it from.

    Order: the shortest public path (prelude excluded), then the owning type for impl and
    trait members, then rustdoc's defining path or source file mapped through
    ``canonical-modules.toml`` (flat implementation modules are private).
    """

    def __init__(
        self,
        payload: dict[str, Any],
        public_paths: dict[str, list[str]],
        canonical: dict[str, str] | None = None,
    ) -> None:
        self.index: dict[str, Any] = payload.get("index", {})
        self.paths_map: dict[str, Any] = payload.get("paths", {})
        self.public_paths = public_paths
        self.canonical = load_canonical_module_map() if canonical is None else canonical
        self.owners = build_member_owner(self.index)
        self.reexported_flat = self._reexported_flat_modules()

    def _defining_flat(self, item_id: Any) -> str | None:
        meta = self.paths_map.get(str(item_id)) or {}
        parts = list(meta.get("path") or [])
        if len(parts) < 2 or parts[0] != "grit_lib" or meta.get("crate_id") not in (0, None):
            return None
        return parts[1]

    def _reexported_flat_modules(self) -> dict[str, str]:
        """Flat module -> public top-level module that re-exports its items (``pub use``).

        Public modules are seeded from their public path; private implementation modules
        inherit the top-level module of the first public (or already-resolved) module that
        re-exports them, so a glob ``pub use crate::history_root::*`` under ``revwalk``
        files ``history_root`` items under ``revwalk``.
        """
        resolved: dict[str, str] = {}
        modules = [
            (str(item_id), item)
            for item_id, item in self.index.items()
            if item.get("crate_id") == 0 and "module" in (item.get("inner") or {})
        ]

        def module_top(module_id: str, item: dict[str, Any]) -> str | None:
            meta = self.paths_map.get(module_id) or {}
            if item.get("visibility") == "public" and meta.get("path"):
                return top_module_of_path("::".join(meta["path"]) + "::_")
            flat = self._defining_flat(module_id) or item.get("name")
            if not flat:
                return None
            return self.canonical.get(flat) or resolved.get(flat)

        changed = True
        while changed:
            changed = False
            for module_id, item in modules:
                top = module_top(module_id, item)
                if not top or top == ROOT_MODULE_LABEL:
                    continue
                for child_id in item["inner"]["module"].get("items") or []:
                    child = index_entry(self.index, child_id) or {}
                    use = (child.get("inner") or {}).get("use")
                    if not use or child.get("visibility") != "public":
                        continue
                    segments = str(use.get("source", "")).split("::")
                    if len(segments) < 2 or segments[0] != "crate":
                        continue
                    target_flat = segments[1]
                    if target_flat in resolved or target_flat in self.canonical:
                        continue
                    resolved[target_flat] = top
                    changed = True
        return resolved

    def _canonical_flat(self, flat: str) -> str:
        return self.canonical.get(flat) or self.reexported_flat.get(flat, flat)

    def _from_public_paths(self, item_id: str) -> str | None:
        candidates = [
            p
            for p in self.public_paths.get(item_id) or []
            if p != "grit_lib::prelude" and not p.startswith("grit_lib::prelude::")
        ]
        if not candidates:
            return None
        return top_module_of_path(min(candidates, key=lambda p: (p.count("::"), p)))

    def _from_defining_path(self, item_id: str) -> str | None:
        meta = self.paths_map.get(item_id)
        if not meta or not meta.get("path") or meta.get("crate_id") not in (0, None):
            return None
        parts = list(meta["path"])
        if not parts or parts[0] != "grit_lib":
            return None
        if len(parts) <= 2:
            return ROOT_MODULE_LABEL
        return self._canonical_flat(parts[1])

    def _from_span(self, item_id: str) -> str | None:
        item = index_entry(self.index, item_id) or {}
        filename = str((item.get("span") or {}).get("filename", "")).replace("\\", "/")
        marker = "grit-lib/src/"
        if marker not in filename:
            return None
        rel = Path(filename.split(marker, 1)[1])
        parts = [p for p in rel.parts if p not in ("", ".", "..")]
        if not parts:
            return None
        first = parts[0]
        if first in ("lib.rs", "main.rs"):
            return ROOT_MODULE_LABEL
        if first.endswith(".rs"):
            first = first[: -len(".rs")]
        return self._canonical_flat(first)

    def label_of(self, item_id: str, fallback: str) -> str:
        """``Owner (file:line)`` for members without their own rustdoc path."""
        sid = str(item_id)
        if (self.paths_map.get(sid) or {}).get("path"):
            return fallback
        owner = self.owners.get(sid)
        owner_meta = (self.paths_map.get(owner) or {}) if owner is not None else {}
        item = index_entry(self.index, sid) or {}
        if not owner_meta.get("path") or not item.get("name"):
            return fallback
        span = item.get("span") or {}
        begin = span.get("begin") or ["?"]
        owner_path = "::".join(owner_meta["path"])
        return f"{owner_path} ({span.get('filename', '?')}:{begin[0]})"

    def module_of(self, item_id: str) -> str:
        sid = str(item_id)
        found = self._from_public_paths(sid)
        if found:
            return found
        owner = self.owners.get(sid)
        if owner is not None:
            found = self._from_public_paths(owner) or self._from_defining_path(owner)
            if found:
                return found
        return self._from_defining_path(sid) or self._from_span(sid) or UNKNOWN_MODULE_LABEL


def rust_code_blocks(docs: str) -> list[str]:
    """Rust doctest bodies in a rustdoc Markdown string (hidden ``# `` lines included)."""
    blocks: list[str] = []
    lines = docs.splitlines()
    i = 0
    while i < len(lines):
        m = FENCE_RE.match(lines[i])
        if not m:
            i += 1
            continue
        fence, info = m.group(1), m.group(2)
        tokens = [t.strip() for t in re.split(r"[,\s]+", info) if t.strip()]
        is_rust = all(t in RUST_FENCE_TOKENS or t.startswith("edition") for t in tokens)
        body: list[str] = []
        i += 1
        while i < len(lines):
            close = FENCE_RE.match(lines[i])
            if close and close.group(1).startswith(fence[0]) and len(close.group(1)) >= len(fence) and not close.group(2):
                break
            body.append(lines[i])
            i += 1
        i += 1
        if not is_rust:
            continue
        code = []
        for line in body:
            stripped = line.lstrip()
            if stripped == "#":
                continue
            if stripped.startswith("# "):
                line = line.replace("# ", "", 1)
            code.append(line)
        blocks.append("\n".join(code))
    return blocks


def _call_arguments(code: str, open_paren: int) -> list[str]:
    """Top-level comma-separated arguments of the call whose ``(`` is at ``open_paren``."""
    depth = 0
    args: list[str] = []
    current: list[str] = []
    for ch in code[open_paren:]:
        if ch in "([{":
            depth += 1
            if depth == 1:
                continue
        elif ch in ")]}":
            depth -= 1
            if depth == 0:
                break
        if depth == 1 and ch == ",":
            args.append("".join(current).strip())
            current = []
            continue
        current.append(ch)
    tail = "".join(current).strip()
    if tail:
        args.append(tail)
    return args


def count_repo_free_fn_calls(code: str, repo_free_fn_names: frozenset[str]) -> list[str]:
    """Free-function calls in example ``code`` that take a repository or its git dir.

    A call counts when its callee names a public free function whose first parameter is
    ``&Repository`` or a git-dir ``&Path``, or when any argument passes ``….git_dir()``.
    Method calls, associated functions (``Type::f``), macros and ``fn`` definitions do not.
    """
    found: list[str] = []
    for m in CALL_RE.finditer(code):
        prefix, name = m.group(1), m.group(2)
        if name in NON_CALL_KEYWORDS:
            continue
        segments = [s for s in prefix.split("::") if s]
        if segments and segments[-1][:1].isupper():
            continue
        before = code[: m.start()].rstrip()
        if before.endswith("fn") and (len(before) == 2 or not before[-3].isalnum()):
            continue
        args = _call_arguments(code, m.end() - 1)
        if name in repo_free_fn_names or any("git_dir()" in arg for arg in args):
            found.append(f"{prefix}{name}")
    return found


def handle_type_ids(index: dict[str, Any], paths_map: dict[str, Any]) -> list[str]:
    """``Repository`` plus each handle type its ``&self`` accessors return (borrowing types)."""
    repo_ids = [
        str(item_id)
        for item_id, item in index.items()
        if item.get("crate_id") == 0
        and item.get("name") == REPOSITORY_TYPE_PLAIN
        and "struct" in (item.get("inner") or {})
        and (paths_map.get(str(item_id)) or {}).get("path", ["grit_lib", "repo", "Repository"])[-1]
        == REPOSITORY_TYPE_PLAIN
    ]
    found: list[str] = list(repo_ids)
    for repo_id in repo_ids:
        for impl_id in (index_entry(index, repo_id) or {}).get("inner", {}).get("struct", {}).get("impls") or []:
            impl = (index_entry(index, impl_id) or {}).get("inner", {}).get("impl") or {}
            if impl.get("trait"):
                continue
            for member_id in impl.get("items") or []:
                member = index_entry(index, member_id) or {}
                fn = (member.get("inner") or {}).get("function")
                if member.get("visibility") != "public" or not fn:
                    continue
                if len(function_inputs(fn)) != 1 or not is_method(fn):
                    continue
                output = (fn.get("sig") or {}).get("output")
                rp = output.get("resolved_path") if isinstance(output, dict) else None
                if not isinstance(rp, dict) or rp.get("id") is None:
                    continue
                target = index_entry(index, rp["id"]) or {}
                if target.get("crate_id") != 0 or "struct" not in (target.get("inner") or {}):
                    continue
                generic_args = ((rp.get("args") or {}).get("angle_bracketed") or {}).get("args") or []
                if not any(isinstance(a, dict) and "lifetime" in a for a in generic_args):
                    continue
                if str(rp["id"]) not in found:
                    found.append(str(rp["id"]))
    return found


def collect_doc_example_free_fn_calls(
    payload: dict[str, Any],
    repo_free_fn_names: frozenset[str],
) -> MetricResult:
    """Calls in crate-root and handle-type rustdoc examples to repo/git-dir free functions."""
    index = payload.get("index", {})
    paths_map = payload.get("paths", {})
    sources: list[tuple[str, str]] = []
    root_id = payload.get("root")
    if root_id is not None:
        root = index_entry(index, root_id) or {}
        sources.append(("grit_lib", root.get("docs") or ""))
    for type_id in handle_type_ids(index, paths_map):
        type_item = index_entry(index, type_id) or {}
        type_path = public_path_for_item(index, paths_map, type_id)
        sources.append((type_path, type_item.get("docs") or ""))
        for impl_id in (type_item.get("inner") or {}).get("struct", {}).get("impls") or []:
            impl = (index_entry(index, impl_id) or {}).get("inner", {}).get("impl") or {}
            if impl.get("trait"):
                continue
            for member_id in impl.get("items") or []:
                member = index_entry(index, member_id) or {}
                if member.get("visibility") != "public":
                    continue
                sources.append((f"{type_path}::{member.get('name', '?')}", member.get("docs") or ""))
    offenders: list[str] = []
    for where, docs in sources:
        for block in rust_code_blocks(docs):
            for callee in count_repo_free_fn_calls(block, repo_free_fn_names):
                offenders.append(f"{where} :: {callee}")
    return MetricResult(len(offenders), offenders)


def compute_metrics(
    *,
    public_api_text: str,
    payload: dict[str, Any],
) -> RatchetSnapshot:
    index = payload["index"]
    paths_map = payload.get("paths", {})

    api_lines = [line for line in public_api_text.splitlines() if line.strip()]

    repo_constructor_re = re.compile(
        r"^pub fn grit_lib::repo::Repository::(init|open|discover)\("
    )
    repo_constructor_names: set[str] = set()
    for line in api_lines:
        m = repo_constructor_re.match(line)
        if m:
            repo_constructor_names.add(m.group(1))
    repo_constructors = len(repo_constructor_names)

    impl_suffix: list[str] = []
    with_in_name: list[str] = []
    too_many_args: list[str] = []
    two_plus_bool: list[str] = []
    result_string: list[str] = []
    repo_first: list[str] = []
    git_dir_first: list[str] = []
    config_write_surface: list[str] = []
    unsafe_fns: list[str] = []
    libc_offenders: list[str] = []
    associated_fn_ids = build_associated_function_ids(index)
    fn_impl_for = build_function_impl_for(index)
    path_map = collect_public_paths(payload)
    resolver = ModuleResolver(payload, path_map)
    report_modules: dict[str, list[str]] = {key: [] for key in REPORT_METRIC_KEYS}
    repo_free_fn_names: set[str] = set()

    def path_in_refname_str_modules(item_id: str, fallback: str) -> bool:
        if module_path_allowed(fallback, REFNAME_STR_MODULE_PREFIXES):
            return True
        for prefix in REFNAME_STR_MODULE_PREFIXES:
            if item_has_public_path_prefix(item_id, path_map, fallback, prefix):
                return True
        return False

    cancel_offenders: list[str] = []
    progress_offenders: list[str] = []
    refname_validator_offenders: list[str] = []
    refname_str_offenders: list[str] = []
    pathspec_str_offenders: list[str] = []
    configset_repo_offenders: list[str] = []
    repo_constructor_offenders: list[str] = []

    for item_id, item in index.items():
        if item.get("crate_id") != 0 or item.get("visibility") != "public":
            continue
        path = public_path_for_item(index, paths_map, str(item_id))
        name = item.get("name") or "?"
        inner = item.get("inner") or {}

        if name in CANCEL_FAMILY_NAMES:
            cancel_offenders.append(f"{path} :: {name}")

        if "trait" in inner:
            trait_inner = inner["trait"]
            if (
                name.endswith("Progress")
                or name == "ProgressSink"
                or trait_has_progress_sink_supertrait(trait_inner)
            ):
                progress_offenders.append(f"{path} :: trait {name}")

        if "struct" in inner and name.endswith("Progress"):
            progress_offenders.append(f"{path} :: struct {name}")

    for struct_id, struct_name, field_name, field_ty in iter_public_struct_fields(index):
        struct_path = public_path_for_item(index, paths_map, str(struct_id))
        if is_cancel_surface_type(field_ty, index):
            cancel_offenders.append(f"{struct_path} :: field {field_name}")
        if is_atomic_bool_watch_type(field_ty, index):
            cancel_offenders.append(f"{struct_path} :: field {field_name}")
        if (
            struct_path.endswith("::ResolvedPathspecs")
            and field_name in RESOLVED_PATHSPECS_STRING_FIELDS
            and is_pathspec_string_collection(field_ty)
        ):
            pathspec_str_offenders.append(f"{struct_path} :: field {field_name}")

    def record_refname_str_params(
        item_id: str,
        item: dict[str, Any],
        fn: dict[str, Any],
        *,
        path: str | None = None,
    ) -> None:
        item_path = path or public_path_for_item(index, paths_map, item_id)
        if not path_in_refname_str_modules(item_id, item_path):
            return
        for param_name, param_ty in function_inputs(fn):
            if param_name in REFNAME_PARAM_NAMES and is_str_or_string_type(param_ty):
                refname_str_offenders.append(
                    f"{item_path} :: {item.get('name', '?')}({param_name})",
                )

    for item_id, item, fn in iter_public_functions(index):
        path = public_path_for_item(index, paths_map, item_id)
        name = item.get("name") or "?"

        if name.endswith("_impl"):
            impl_suffix.append(f"{path} :: {name}")
        module = resolver.module_of(item_id)
        label = resolver.label_of(item_id, path)
        if has_with_in_name(name):
            with_in_name.append(f"{label} :: {name}")
            report_modules["pub_fn_with_in_name"].append(module)

        if has_too_many_arguments_allow(item):
            too_many_args.append(f"{label} :: {name}")
            report_modules["pub_fn_too_many_arguments_allow"].append(module)

        inputs = function_inputs(fn)
        bool_count = sum(1 for _param, ty in inputs if is_bare_bool(ty))
        if bool_count >= 2:
            two_plus_bool.append(f"{label} :: {name}")
            report_modules["pub_fn_two_plus_bare_bool"].append(module)

        err_ty = result_error_type(fn.get("sig", {}).get("output"), index)
        if err_ty in STRING_RUST_TYPES:
            result_string.append(f"{path} :: {name}")

        if is_unsafe_fn(fn):
            unsafe_fns.append(f"{path} :: {name}")

        if mentions_libc(item):
            libc_offenders.append(f"{path} :: {name}")

        is_free_fn = item_id not in associated_fn_ids and not is_method(fn)
        if is_free_fn and inputs:
            param_name, param_ty = inputs[0]
            if is_exact_repository_ref(param_ty):
                repo_first.append(f"{path} :: {name}")
                report_modules["pub_free_fn_repository_first"].append(module)
                repo_free_fn_names.add(name)
            if is_exact_path_ref(param_ty) and (
                param_name in GIT_DIR_PARAM_NAMES
                or param_name.endswith("_git_dir")
                or param_name.endswith("_git_directory")
            ):
                git_dir_first.append(f"{path} :: {name}")
                report_modules["pub_free_fn_git_dir_path_first"].append(module)
                repo_free_fn_names.add(name)
            if (
                is_free_fn
                and path.startswith("grit_lib::config::")
                and name in CONFIG_WRITE_FREE_FN_NAMES
                and len(inputs) >= 3
            ):
                gd_name, gd_ty = inputs[2]
                if (
                    gd_name in CONFIG_WRITE_GIT_DIR_PARAM_NAMES
                    and type_display(resolve_type_node(gd_ty, index)).startswith("Option")
                ):
                    config_write_surface.append(f"{path} :: {name}")

        for param_name, param_ty in inputs:
            if is_cancel_surface_type(param_ty, index):
                cancel_offenders.append(f"{path} :: {name}({param_name})")
            if is_atomic_bool_watch_type(param_ty, index):
                cancel_offenders.append(f"{path} :: {name}({param_name})")
            if fn_param_is_fnmut_with_progress_arg(param_ty, index):
                progress_offenders.append(f"{path} :: {name}({param_name})")
            if param_name in PATHSPEC_PARAM_NAMES and is_pathspec_string_collection(
                param_ty,
            ):
                pathspec_str_offenders.append(f"{path} :: {name}({param_name})")

        validator_name = REFNAME_FN_NAME_RE.search(name) or (
            REFNAME_VALIDATOR_PATH_MARK in path
        )
        if (
            is_free_fn
            and item_defined_in_refs(item_id, path_map, path, item)
            and validator_name
            and returns_bool_or_unit_result(fn.get("sig", {}).get("output"), index)
        ):
            refname_validator_offenders.append(f"{path} :: {name}")

        record_refname_str_params(item_id, item, fn)

        if has_configset_param(inputs, index):
            method = is_method(fn)
            repo_ctx = False
            if method:
                for_path = fn_impl_for.get(item_id)
                repo_ctx = handle_borrows_repository(for_path)
            elif has_repository_first_param(inputs):
                repo_ctx = True
            if repo_ctx:
                configset_repo_offenders.append(f"{path} :: {name}")

        output = fn.get("sig", {}).get("output")
        impl_for = fn_impl_for.get(item_id)
        if item_id in associated_fn_ids and returns_repository(
            output,
            index,
            impl_for=impl_for,
        ):
            repo_constructor_offenders.append(f"{path} :: {name}")
        if "rev_parse" in path and path.endswith("::discover_optional"):
            repo_constructor_offenders.append(f"{path} :: {name}")

    for item_id, item, fn, method_path in iter_public_trait_methods(index, paths_map):
        record_refname_str_params(item_id, item, fn, path=method_path)

    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        inner = item.get("inner") or {}
        if "impl" not in inner:
            continue
        for_path = impl_for_type_path(item)
        if for_path not in {
            "grit_lib::config::ConfigEditor",
            "ConfigEditor",
        } and not for_path.endswith("::config::ConfigEditor"):
            continue
        for member_id in inner["impl"].get("items") or []:
            member = index_entry(index, member_id)
            if member is None or member.get("visibility") != "public":
                continue
            member_inner = member.get("inner") or {}
            if "function" not in member_inner:
                continue
            fname = member.get("name") or "?"
            if fname not in CONFIG_EDITOR_WRITE_METHODS:
                continue
            mpath = public_path_for_item(index, paths_map, str(member_id))
            config_write_surface.append(f"{mpath} :: {fname}")

    for item_id, item in index.items():
        if item.get("crate_id") != 0:
            continue
        inner = item.get("inner") or {}
        if "impl" not in inner:
            continue
        for_path = impl_for_type_path(item)
        if not for_path or (
            "RepositoryLocation" not in for_path and not for_path.endswith("::RepositoryLocation")
        ):
            continue
        for member_id in inner["impl"].get("items") or []:
            member = index_entry(index, member_id)
            if not member or member.get("visibility") != "public":
                continue
            member_inner = member.get("inner") or {}
            if "function" not in member_inner:
                continue
            fname = member.get("name") or "?"
            if fname.startswith("discover"):
                mpath = public_path_for_item(index, paths_map, str(member_id))
                repo_constructor_offenders.append(f"{mpath} :: {fname}")

    # Also scan non-function public items for libc in signatures? Task says "public signatures"
    for item_id, item in index.items():
        if item.get("crate_id") != 0 or item.get("visibility") != "public":
            continue
        inner = item.get("inner") or {}
        if "function" in inner:
            continue
        if mentions_libc(item):
            path = public_path_for_item(index, paths_map, str(item_id))
            libc_offenders.append(f"{path} :: {item.get('name', '?')}")

    multi_paths: list[str] = []
    for item_id, paths in sorted(path_map.items(), key=lambda kv: kv[0]):
        non_prelude = [
            p
            for p in paths
            if p != "grit_lib::prelude" and not p.startswith("grit_lib::prelude::")
        ]
        if len(non_prelude) <= 1:
            continue
        multi_paths.append(f"id {item_id}: " + " | ".join(sorted(non_prelude)))

    metrics = {
        "pub_repo_constructors": MetricResult(repo_constructors),
        "public_api_lines": MetricResult(len(api_lines)),
        "pub_fn_impl_suffix": MetricResult(len(impl_suffix), impl_suffix),
        "pub_fn_with_in_name": MetricResult(
            len(with_in_name),
            with_in_name,
            report_modules["pub_fn_with_in_name"],
        ),
        "pub_fn_too_many_arguments_allow": MetricResult(
            len(too_many_args),
            too_many_args,
            report_modules["pub_fn_too_many_arguments_allow"],
        ),
        "pub_fn_two_plus_bare_bool": MetricResult(
            len(two_plus_bool),
            two_plus_bool,
            report_modules["pub_fn_two_plus_bare_bool"],
        ),
        "pub_fn_result_string_err": MetricResult(len(result_string), result_string),
        "pub_free_fn_repository_first": MetricResult(
            len(repo_first),
            repo_first,
            report_modules["pub_free_fn_repository_first"],
        ),
        "pub_free_fn_git_dir_path_first": MetricResult(
            len(git_dir_first),
            git_dir_first,
            report_modules["pub_free_fn_git_dir_path_first"],
        ),
        "doc_example_free_fn_calls": collect_doc_example_free_fn_calls(
            payload,
            frozenset(repo_free_fn_names),
        ),
        "pub_free_fn_config_write": MetricResult(
            len(config_write_surface),
            config_write_surface,
        ),
        "pub_items_multiple_paths": MetricResult(len(multi_paths), multi_paths),
        "pub_libc_types": MetricResult(len(sorted(set(libc_offenders))), sorted(set(libc_offenders))),
        "pub_unsafe_fn": MetricResult(len(unsafe_fns), unsafe_fns),
        "pub_cancel_types": MetricResult(
            len(sorted(set(cancel_offenders))),
            sorted(set(cancel_offenders)),
        ),
        "pub_progress_interfaces": MetricResult(
            len(sorted(set(progress_offenders))),
            sorted(set(progress_offenders)),
        ),
        "pub_refname_validators": MetricResult(
            len(sorted(set(refname_validator_offenders))),
            sorted(set(refname_validator_offenders)),
        ),
        "pub_fn_refname_str": MetricResult(
            len(sorted(set(refname_str_offenders))),
            sorted(set(refname_str_offenders)),
        ),
        "pub_fn_pathspec_strings": MetricResult(
            len(sorted(set(pathspec_str_offenders))),
            sorted(set(pathspec_str_offenders)),
        ),
        "pub_fn_configset_with_repo": MetricResult(
            len(sorted(set(configset_repo_offenders))),
            sorted(set(configset_repo_offenders)),
        ),
        "pub_repo_constructor_offenders": MetricResult(
            len(sorted(set(repo_constructor_offenders))),
            sorted(set(repo_constructor_offenders)),
        ),
        "pub_deprecated_items": MetricResult(
            count_pub_deprecated_items(index),
        ),
        **collect_error_ratchet_metrics(index, paths_map),
    }
    return RatchetSnapshot(metrics=metrics)


def load_baseline(path: Path) -> dict[str, int]:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    counts_table = data.get("counts", data)
    counts: dict[str, int] = {}
    for key in METRIC_KEYS:
        if key in counts_table:
            counts[key] = int(counts_table[key])
    return counts


_STABLE_V_TAG_RE = re.compile(r"^v\d+\.\d+\.\d+$")


def _parse_version_tag(tag: str) -> tuple[int, int, int] | None:
    """Parse a stable release tag (vMAJOR.MINOR.PATCH only; no prerelease suffix)."""
    if not _STABLE_V_TAG_RE.fullmatch(tag):
        return None
    parts = tag[1:].split(".")
    if len(parts) < 2:
        return None
    try:
        major = int(parts[0])
        minor = int(parts[1])
        patch = int(parts[2]) if len(parts) > 2 else 0
    except ValueError:
        return None
    return (major, minor, patch)


def reachable_v06_plus_tag(repo_root: Path | None = None) -> str | None:
    """Return the newest merged v* tag with version >= 0.6.0, or None."""
    root = repo_root or ROOT
    try:
        out = subprocess.check_output(
            [
                "git",
                "-C",
                str(root),
                "tag",
                "--merged",
                "HEAD",
                "--list",
                "v*",
                "--sort=-v:refname",
            ],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    for line in out.splitlines():
        tag = line.strip()
        if not tag:
            continue
        parsed = _parse_version_tag(tag)
        if parsed is None:
            continue
        major, minor, _patch = parsed
        if major > 0 or minor >= 6:
            return tag
    return None


def write_baseline(path: Path, snapshot: RatchetSnapshot) -> None:
    today = date.today().isoformat()
    preserved_meta: dict[str, str] = {}
    if path.is_file():
        existing = tomllib.loads(path.read_text(encoding="utf-8"))
        meta_table = existing.get("meta", {})
        if isinstance(meta_table, dict):
            for key, value in meta_table.items():
                if key != "updated" and value is not None:
                    preserved_meta[str(key)] = str(value)
    if reachable_v06_plus_tag() is not None:
        preserved_meta.pop("semver_baseline", None)
    lines = [
        "# grit-lib public API quality counts — ratchet baseline",
        "# Regenerate with: make api-ratchet",
        "",
        "[meta]",
        f'updated = "{today}"',
    ]
    for key in sorted(preserved_meta):
        val = preserved_meta[key]
        if val.startswith('"') or val.isdigit():
            lines.append(f"{key} = {val}")
        else:
            lines.append(f'{key} = "{val}"')
    lines.extend(
        [
        "",
        "[counts]",
        ]
    )
    for key in METRIC_KEYS:
        lines.append(f"{key} = {snapshot.metrics[key].count}")
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def compare(
    baseline: dict[str, int],
    snapshot: RatchetSnapshot,
) -> tuple[bool, list[str]]:
    failures: list[str] = []
    current = snapshot.counts()
    for key in METRIC_KEYS:
        base = baseline.get(key)
        cur = current.get(key, 0)
        if base is None:
            failures.append(f"  {key}: missing from baseline (current {cur})")
            continue
        if cur > base:
            failures.append(f"  {key}: increased {base} -> {cur}")
            offenders = snapshot.metrics[key].offenders
            for line in offenders[:20]:
                failures.append(f"      {line}")
            if len(offenders) > 20:
                failures.append(f"      ... and {len(offenders) - 20} more")
        elif cur < base:
            failures.append(
                f"  {key}: decreased {base} -> {cur} (run `make api-ratchet` and commit)"
            )
    return (not failures, failures)


def cmd_check(args: argparse.Namespace) -> int:
    if not BASELINE_PATH.is_file():
        print(f"error: missing baseline {BASELINE_PATH}", file=sys.stderr)
        return 1
    pins = parse_pins()
    ensure_inputs(nightly=pins["nightly_date"], rustdoc_json=args.rustdoc_json)
    payload = load_rustdoc(args.rustdoc_json)
    public_api_text = PUBLIC_API_TXT.read_text(encoding="utf-8")
    snapshot = compute_metrics(public_api_text=public_api_text, payload=payload)
    baseline = load_baseline(BASELINE_PATH)
    ok, failures = compare(baseline, snapshot)
    if ok:
        print("api-ratchet: all metrics match baseline.")
        return 0
    print("api-ratchet check failed:\n", file=sys.stderr)
    for line in failures:
        print(line, file=sys.stderr)
    return 1


def group_offenders_by_module(
    snapshot: RatchetSnapshot,
    keys: Iterable[str] = REPORT_METRIC_KEYS,
) -> dict[str, dict[str, list[str]]]:
    """``{module: {metric: [offender, …]}}`` for the given metrics, offenders sorted."""
    grouped: dict[str, dict[str, list[str]]] = defaultdict(lambda: defaultdict(list))
    for key in keys:
        metric = snapshot.metrics[key]
        modules = metric.modules or [UNKNOWN_MODULE_LABEL] * len(metric.offenders)
        for module, offender in zip(modules, metric.offenders):
            grouped[module][key].append(offender)
    return {
        module: {key: sorted(offs) for key, offs in by_key.items()}
        for module, by_key in grouped.items()
    }


def render_report(snapshot: RatchetSnapshot, *, module: str | None = None) -> str:
    """Per-module offender listing for the report metrics (optionally one module)."""
    grouped = group_offenders_by_module(snapshot)
    if module is not None:
        grouped = {m: v for m, v in grouped.items() if m == module}
    totals = {key: sum(len(by_key.get(key, [])) for by_key in grouped.values()) for key in REPORT_METRIC_KEYS}

    def module_total(name: str) -> int:
        return sum(len(offs) for offs in grouped[name].values())

    order = sorted(grouped, key=lambda m: (-module_total(m), m))
    scope = f"module `{module}`" if module is not None else f"{len(order)} modules"
    lines = [f"api-ratchet report: {sum(totals.values())} offenders in {scope}", ""]
    lines.append("Totals:")
    for key in REPORT_METRIC_KEYS:
        lines.append(f"  {key} = {totals[key]}")
    if module is not None and not grouped:
        lines.extend(["", f"No offenders in module `{module}`."])
        return "\n".join(lines) + "\n"
    for name in order:
        by_key = grouped[name]
        summary = ", ".join(f"{key} {len(by_key[key])}" for key in REPORT_METRIC_KEYS if by_key.get(key))
        lines.extend(["", f"## {name} ({module_total(name)}: {summary})"])
        for key in REPORT_METRIC_KEYS:
            offenders = by_key.get(key)
            if not offenders:
                continue
            lines.append(f"  {key} ({len(offenders)})")
            lines.extend(f"    {line}" for line in offenders)
    return "\n".join(lines) + "\n"


def cmd_report(args: argparse.Namespace) -> int:
    pins = parse_pins()
    ensure_inputs(nightly=pins["nightly_date"], rustdoc_json=args.rustdoc_json)
    payload = load_rustdoc(args.rustdoc_json)
    public_api_text = PUBLIC_API_TXT.read_text(encoding="utf-8")
    snapshot = compute_metrics(public_api_text=public_api_text, payload=payload)
    sys.stdout.write(render_report(snapshot, module=args.module))
    return 0


def cmd_update(args: argparse.Namespace) -> int:
    pins = parse_pins()
    ensure_inputs(nightly=pins["nightly_date"], rustdoc_json=args.rustdoc_json)
    payload = load_rustdoc(args.rustdoc_json)
    public_api_text = PUBLIC_API_TXT.read_text(encoding="utf-8")
    snapshot = compute_metrics(public_api_text=public_api_text, payload=payload)
    write_baseline(BASELINE_PATH, snapshot)
    print(f"Wrote {BASELINE_PATH.relative_to(ROOT)}")
    for key in METRIC_KEYS:
        print(f"  {key} = {snapshot.metrics[key].count}")
    return 0


def load_baseline_meta(path: Path = BASELINE_PATH) -> dict[str, str]:
    if not path.is_file():
        return {}
    meta = tomllib.loads(path.read_text(encoding="utf-8")).get("meta", {})
    return {str(k): str(v) for k, v in meta.items()} if isinstance(meta, dict) else {}


def public_api_nonempty_lines(public_api_text: str) -> list[str]:
    return [line for line in public_api_text.splitlines() if line.strip()]


def top_level_module_spans(lines: list[str]) -> list[tuple[str, int, int]]:
    """``(module_name, start_line_index, end_line_index)`` from ``public-api.txt``."""
    mods: list[tuple[str, int]] = []
    for index, line in enumerate(lines):
        match = PUBLIC_MOD_RE.match(line)
        if match:
            mods.append((match.group(1), index))
    spans: list[tuple[str, int, int]] = []
    for index, (name, start) in enumerate(mods):
        end = mods[index + 1][1] if index + 1 < len(mods) else len(lines)
        spans.append((name, start, end))
    return spans


def approx_module_repo_first_and_with(lines: list[str], module: str, start: int, end: int) -> tuple[int, int]:
    repo_first = 0
    with_in = 0
    for line in lines[start:end]:
        fn_match = PUBLIC_FN_MOD_RE.match(line)
        if not fn_match or fn_match.group(1) != module:
            continue
        if PUBLIC_API_REPO_FIRST_RE.match(line):
            repo_first += 1
        if "_with_" in fn_match.group(2):
            with_in += 1
    return repo_first, with_in


def rg_file_count(pattern: str, paths: Iterable[str]) -> int:
    """Number of distinct files under ``paths`` that match ``pattern``.

    Uses ripgrep when installed; otherwise ``grep -Erl`` (extended regex, matching
    ripgrep-style patterns such as ``\\.accessor\\(``). GitHub-hosted runners do not
    ship ``rg``; see ``scripts/check_canonical_layout.py``.
    """
    files: set[str] = set()
    use_rg = shutil.which("rg") is not None
    for tree in paths:
        target = ROOT / tree
        if not target.exists():
            continue
        if use_rg:
            cmd = ["rg", "-l", pattern, str(target)]
        else:
            # -E: same metacharacters as ripgrep for patterns we emit (``\\.foo\\(``).
            cmd = ["grep", "-Erl", "--include=*.rs", pattern, str(target)]
        result = subprocess.run(
            cmd,
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            files.update(line.strip() for line in result.stdout.splitlines() if line.strip())
        elif result.returncode == 1:
            continue
        else:
            detail = (result.stderr or result.stdout or "").strip()
            raise RuntimeError(
                f"file search failed ({' '.join(cmd)!r}): exit {result.returncode}"
                + (f": {detail}" if detail else "")
            )
    return len(files)


def external_module_files(module: str) -> int:
    return rg_file_count(f"grit_lib::{module}", CONSUMER_PATHS)


def external_accessor_files(accessor: str) -> int:
    return rg_file_count(rf"\.{re.escape(accessor)}\(", CONSUMER_PATHS)


ACCESSOR_VALUE_TYPE_MARKERS = (
    "objects::ObjectId",
    "offset_date_time::OffsetDateTime",
    "parallel::ThreadPolicy",
    "repo::DiscoveryInfo",
    "command_runner::hooks::HookStdio",
    "config::environment::Environment",
)


def unwrap_result_ok_type(return_type: str) -> str:
    """Inner ok-type of ``Result<…>`` wrappers in ``public-api.txt`` spellings."""
    text = return_type.strip()
    marker = "Result<"
    idx = text.find(marker)
    if idx < 0:
        return text
    start = idx + len(marker)
    depth = 0
    for pos in range(start, len(text)):
        ch = text[pos]
        if ch == "<":
            depth += 1
            continue
        if ch == ">":
            if depth == 0:
                return text[start:pos].strip()
            depth -= 1
            continue
        if ch == "," and depth == 0:
            return text[start:pos].strip()
    return text[start:].strip()


def is_repository_handle_accessor(return_type: str) -> bool:
    """True when ``Repository::accessor(&self)`` returns a domain handle (not a value getter)."""
    lowered = return_type.strip()
    ok_ty = unwrap_result_ok_type(lowered)

    if ok_ty in {"()", "unit"} or ok_ty.endswith("Result<()>") or "Result<()>" in lowered:
        return False
    if ok_ty in {"bool", "i64", "u64", "usize"} or lowered.endswith("-> bool"):
        return False
    if "path::Path" in lowered or ok_ty.startswith("&str") or "Option<&str>" in lowered:
        return False
    if any(marker in ok_ty for marker in ACCESSOR_VALUE_TYPE_MARKERS):
        return False
    if ok_ty.startswith("alloc::rcs::arc::Arc") or "&dyn " in ok_ty or ok_ty.startswith("&dyn"):
        return False
    if ok_ty.startswith("&grit_lib::prelude::Odb"):
        return False

    # Borrowed repo handles (``Type<'_>``) are the supported accessor shape.
    if "<'_>" in ok_ty and "grit_lib::" in ok_ty:
        return True
    if "grit_lib::diagnostics::DiagnosticsHandle" in ok_ty:
        return True
    return False


def shorten_rust_type(display: str) -> str:
    """Stable short label for accessor return types in audit tables."""
    display = display.strip()
    ok_ty = unwrap_result_ok_type(display)
    idx = ok_ty.find("grit_lib::")
    if idx < 0:
        return ok_ty
    rest = ok_ty[idx:]
    depth = 0
    for i, ch in enumerate(rest):
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth -= 1
            if depth == 0:
                return rest[: i + 1]
    match = re.match(r"grit_lib::[a-zA-Z0-9_:]+", rest)
    return match.group(0) if match else rest


def repository_accessors(public_api_lines: list[str]) -> list[tuple[str, str]]:
    """Unique ``(accessor, return_type)`` for public ``Repository`` methods that return handles."""
    by_name: dict[str, str] = {}
    for line in public_api_lines:
        match = REPOSITORY_ACCESSOR_RE.match(line)
        if not match:
            continue
        name, ret = match.group(1), match.group(2)
        if not is_repository_handle_accessor(ret):
            continue
        short = shorten_rust_type(ret)
        existing = by_name.get(name)
        if existing is None:
            by_name[name] = short
        elif existing != short:
            by_name[name] = short if len(short) <= len(existing) else existing
    return sorted(by_name.items(), key=lambda item: item[0])


def count_type_mentions(public_api_lines: list[str], type_fragment: str) -> int:
    return sum(1 for line in public_api_lines if type_fragment in line)


def module_offender_totals(snapshot: RatchetSnapshot) -> dict[str, int]:
    grouped = group_offenders_by_module(snapshot, REPORT_METRIC_KEYS)
    return {module: sum(len(offs) for offs in by_key.values()) for module, by_key in grouped.items()}


def collect_residual_rows(snapshot: RatchetSnapshot) -> list[tuple[str, str]]:
    rows: list[tuple[str, str]] = []
    for key in RESIDUAL_METRIC_KEYS:
        metric = snapshot.metrics[key]
        for offender in sorted(metric.offenders):
            rows.append((key, offender))
    rows.sort(key=lambda pair: (pair[0], pair[1]))
    return rows


def render_audit_markdown(
    *,
    snapshot: RatchetSnapshot,
    public_api_lines: list[str],
    public_api_file_lines: int,
    meta: dict[str, str],
) -> str:
    """Markdown body for ``API.md`` between ``api-audit`` markers (deterministic)."""
    api_line_count = len(public_api_lines)
    updated = meta.get("updated", "unknown")
    lines: list[str] = [
        f"Snapshot: [`api-ratchet.toml`](api-ratchet.toml) `meta.updated` **{updated}**; "
        f"[`public-api.txt`](public-api.txt) **{api_line_count:,}** non-empty lines "
        f"(`wc -l` on the file: **{public_api_file_lines:,}**).",
        "",
        "Regenerate this block with `make api-audit`; CI runs `make api-audit-check` "
        "in the **public-api** job next to `make api-ratchet-check`.",
        "",
        "### Summary — ratchet counts",
        "",
        "| Metric | Value |",
        "| --- | ---: |",
    ]
    for key in METRIC_KEYS:
        lines.append(f"| `{key}` | {snapshot.metrics[key].count:,} |")

    offender_by_module = module_offender_totals(snapshot)
    lines.extend(
        [
            "",
            "### Top-level public modules",
            "",
            "Top-level modules match `grep -E '^pub mod grit_lib::[a-z_0-9]+$' grit-lib/public-api.txt`. "
            "**External files** counts files under consumer trees that reference "
            "`grit_lib::<module>` (`rg -l`). **Offenders** is the consolidation-total from "
            "`make api-ratchet-report` metrics in that module; **repo-first** / **`_with_`** "
            "are approximated from `public-api.txt` in the module span.",
            "",
            "| Module | Lines | External files | Offenders | repo-first | `_with_` |",
            "| --- | ---: | ---: | ---: | ---: | ---: |",
        ]
    )
    for name, start, end in top_level_module_spans(public_api_lines):
        span_lines = end - start
        repo_first, with_in = approx_module_repo_first_and_with(public_api_lines, name, start, end)
        offenders = offender_by_module.get(name, 0)
        external = external_module_files(name)
        lines.append(
            f"| `{name}` | {span_lines:,} | {external:,} | {offenders:,} | {repo_first:,} | {with_in:,} |",
        )

    lines.extend(
        [
            "",
            "### `Repository` handle accessors",
            "",
            "Listed accessors are `Repository::<name>(&self) -> …` entries in `public-api.txt` "
            "whose return type is a handle (not `Path`, `str`, or a scalar). **Lines** counts "
            "public-api entries mentioning the shortened return type. **External** counts consumer "
            "files calling `.<accessor>(` (`rg -l`).",
            "",
            "| Accessor | Return type | Lines | External `.accessor()` files |",
            "| --- | --- | ---: | ---: |",
        ]
    )
    for accessor, ret_type in sorted(repository_accessors(public_api_lines), key=lambda row: row[0]):
        type_needle = ret_type.split("<", 1)[0]
        line_count = count_type_mentions(public_api_lines, type_needle)
        external = external_accessor_files(accessor)
        lines.append(f"| `{accessor}` | `{ret_type}` | {line_count:,} | {external:,} |")

    lines.extend(
        [
            "",
            "### Residuals frozen in 0.6",
            "",
            "Remaining ratchet offenders at the snapshot above. These counts may only go down "
            "until **0.6.0** freezes the supported surface; see [`api-ratchet.toml`](api-ratchet.toml).",
            "",
            "| Metric | Offender |",
            "| --- | --- |",
        ]
    )
    for metric, offender in collect_residual_rows(snapshot):
        lines.append(f"| `{metric}` | `{offender}` |")

    return "\n".join(lines).rstrip() + "\n"


def read_audit_marked_section(path: Path = API_MD_PATH) -> str | None:
    if not path.is_file():
        return None
    text = path.read_text(encoding="utf-8")
    begin = text.find(AUDIT_BEGIN)
    end = text.find(AUDIT_END)
    if begin < 0 or end < 0 or end <= begin:
        return None
    body_start = begin + len(AUDIT_BEGIN)
    return text[body_start:end].lstrip("\n")


def write_audit_marked_section(body: str, path: Path = API_MD_PATH) -> None:
    text = path.read_text(encoding="utf-8")
    begin = text.find(AUDIT_BEGIN)
    end = text.find(AUDIT_END)
    if begin < 0 or end < 0 or end <= begin:
        raise ValueError(f"missing {AUDIT_BEGIN} / {AUDIT_END} in {path}")
    body_start = begin + len(AUDIT_BEGIN)
    end_marker_start = end
    new_text = text[:body_start] + "\n" + body.rstrip() + "\n" + text[end_marker_start:]
    path.write_text(new_text, encoding="utf-8")


def audit_snapshot(
    args: argparse.Namespace,
) -> tuple[RatchetSnapshot, list[str], int, dict[str, str]]:
    pins = parse_pins()
    ensure_inputs(nightly=pins["nightly_date"], rustdoc_json=args.rustdoc_json)
    payload = load_rustdoc(args.rustdoc_json)
    public_api_text = PUBLIC_API_TXT.read_text(encoding="utf-8")
    public_api_lines = public_api_nonempty_lines(public_api_text)
    file_lines = len(public_api_text.splitlines())
    snapshot = compute_metrics(public_api_text=public_api_text, payload=payload)
    meta = load_baseline_meta()
    return snapshot, public_api_lines, file_lines, meta


def cmd_audit(args: argparse.Namespace) -> int:
    snapshot, public_api_lines, file_lines, meta = audit_snapshot(args)
    body = render_audit_markdown(
        snapshot=snapshot,
        public_api_lines=public_api_lines,
        public_api_file_lines=file_lines,
        meta=meta,
    )
    if args.audit_check:
        committed = read_audit_marked_section()
        if committed is None:
            print(f"error: missing audit markers in {API_MD_PATH}", file=sys.stderr)
            return 1
        if committed != body:
            print("api-audit check failed: API.md audit section drifted", file=sys.stderr)
            print("run `make api-audit` and commit grit-lib/API.md", file=sys.stderr)
            return 1
        print("api-audit: section matches generator output.")
        return 0
    if args.audit_write:
        write_audit_marked_section(body)
        print(f"Wrote audit section in {API_MD_PATH.relative_to(ROOT)}")
        return 0
    sys.stdout.write(body)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="grit-lib API quality ratchet")
    parser.add_argument(
        "--rustdoc-json",
        type=Path,
        default=DEFAULT_RUSTDOC_JSON,
        help="path to grit_lib.json from pinned nightly rustdoc",
    )
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check", help="fail if any metric increased or decreased without baseline update")
    sub.add_parser("update", help="rewrite grit-lib/api-ratchet.toml from current tree")
    report = sub.add_parser(
        "report",
        help="list consolidation offenders grouped by top-level public module",
    )
    report.add_argument(
        "--module",
        help="only show one top-level module (for example `diff` or `porcelain`)",
    )
    audit = sub.add_parser(
        "audit",
        help="render API audit Markdown (stdout), or check/write grit-lib/API.md markers",
    )
    audit_mode = audit.add_mutually_exclusive_group()
    audit_mode.add_argument(
        "--check",
        dest="audit_check",
        action="store_true",
        help="fail if API.md audit markers drift from generator output",
    )
    audit_mode.add_argument(
        "--write",
        dest="audit_write",
        action="store_true",
        help="rewrite the audit block in grit-lib/API.md",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command == "check":
        return cmd_check(args)
    if args.command == "update":
        return cmd_update(args)
    if args.command == "report":
        return cmd_report(args)
    if args.command == "audit":
        return cmd_audit(args)
    parser.error(f"unknown command {args.command!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
