#!/usr/bin/env python3
"""API quality ratchet for grit-lib: counts that may only go down.

Compares rustdoc JSON (pinned nightly, ``http-ureq`` feature; JSON at
``target/api-ratchet-doc/grit_lib.json``) and ``grit-lib/public-api.txt``
against ``grit-lib/api-ratchet.toml``.
"""

from __future__ import annotations

import argparse
import json
import re
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
BASELINE_PATH = GRIT_LIB / "api-ratchet.toml"
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
)

ERROR_CATCH_ALL_VARIANTS = frozenset({"Message", "Other", "Generic", "Custom", "Msg"})
UMBRELLA_ERROR_PATH = "grit_lib::error::Error"

CANCEL_FAMILY_NAMES = frozenset({"CancelToken"})

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


def is_cancel_surface_type(type_node: Any, index: dict[str, Any]) -> bool:
    """True for cancellation in the public API: dyn cancel traits or CancelToken/CancelSignal handles."""
    resolved = resolve_type_node(type_node, index)
    if isinstance(resolved, dict) and "dyn_trait" in resolved:
        traits = resolved["dyn_trait"].get("traits") or []
        for trait in traits:
            if "Cancel" in type_display(trait):
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


def is_bare_bool(type_node: Any) -> bool:
    if isinstance(type_node, dict) and "type" in type_node:
        type_node = type_node["type"]
    if not isinstance(type_node, dict):
        return False
    return type_node.get("primitive") == "bool"


def function_inputs(fn: dict[str, Any]) -> list[tuple[str, Any]]:
    sig = fn.get("sig") or {}
    raw = sig.get("inputs") or []
    out: list[tuple[str, Any]] = []
    for entry in raw:
        if isinstance(entry, (list, tuple)) and len(entry) == 2:
            out.append((str(entry[0]), entry[1]))
    return out


def is_method(fn: dict[str, Any]) -> bool:
    inputs = function_inputs(fn)
    if not inputs:
        return False
    return inputs[0][0] in {"self", "mut self"}


def is_unsafe_fn(fn: dict[str, Any]) -> bool:
    header = fn.get("header") or {}
    return bool(header.get("is_unsafe"))


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
    elif isinstance(kind, dict) and "tuple" in kind:
        for fid in kind["tuple"]:
            field = index_entry(index, fid)
            if not field:
                continue
            finner = field.get("inner") or {}
            if "type" in finner:
                types.append(finner["type"])
            elif "struct_field" in finner:
                sf = finner["struct_field"]
                if "type" in sf:
                    types.append(sf["type"])
                elif "resolved_path" in sf:
                    types.append({"resolved_path": sf["resolved_path"]})
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


def field_type_is_umbrella_error(type_node: Any) -> bool:
    text = type_display(type_node)
    if text == UMBRELLA_ERROR_PATH or text.endswith("::error::Error"):
        return True
    if "Box" in text and "Error" in text:
        return True
    if isinstance(type_node, dict) and "resolved_path" in type_node:
        rp = type_node["resolved_path"]
        path = rp.get("path", "")
        if path in {"Box", "alloc::boxed::Box"} or path.endswith("::Box"):
            inner = _format_generic_args(rp.get("args"))
            if "Error" in inner or "error::Error" in inner:
                return True
        if path.endswith("::Error") or path == "Error":
            return True
    if isinstance(type_node, dict) and "type" in type_node:
        return field_type_is_umbrella_error(type_node["type"])
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
            for fty in variant_field_types(variant, index):
                if field_type_is_umbrella_error(fty):
                    wraps_umbrella.append(f"{vpath} :: tuple")
            vinner = variant.get("inner") or {}
            if "variant" in vinner:
                for field in vinner["variant"].get("fields") or []:
                    if not isinstance(field, dict):
                        continue
                    fty = field.get("type")
                    if fty and field_type_is_umbrella_error(fty):
                        fname = field.get("name") or "?"
                        wraps_umbrella.append(f"{vpath} :: field {fname}")

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
        if "_with_" in name:
            with_in_name.append(f"{path} :: {name}")

        if has_too_many_arguments_allow(item):
            too_many_args.append(f"{path} :: {name}")

        inputs = function_inputs(fn)
        bool_count = sum(1 for _param, ty in inputs if is_bare_bool(ty))
        if bool_count >= 2:
            two_plus_bool.append(f"{path} :: {name}")

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
            if is_exact_path_ref(param_ty) and (
                param_name in GIT_DIR_PARAM_NAMES
                or param_name.endswith("_git_dir")
                or param_name.endswith("_git_directory")
            ):
                git_dir_first.append(f"{path} :: {name}")
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
        "pub_fn_with_in_name": MetricResult(len(with_in_name), with_in_name),
        "pub_fn_too_many_arguments_allow": MetricResult(len(too_many_args), too_many_args),
        "pub_fn_two_plus_bare_bool": MetricResult(len(two_plus_bool), two_plus_bool),
        "pub_fn_result_string_err": MetricResult(len(result_string), result_string),
        "pub_free_fn_repository_first": MetricResult(len(repo_first), repo_first),
        "pub_free_fn_git_dir_path_first": MetricResult(len(git_dir_first), git_dir_first),
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
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command == "check":
        return cmd_check(args)
    if args.command == "update":
        return cmd_update(args)
    parser.error(f"unknown command {args.command!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
