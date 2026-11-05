#!/usr/bin/env python3
"""API quality ratchet for grit-lib: counts that may only go down.

Compares rustdoc JSON (pinned nightly, ``http-ureq`` feature) and
``grit-lib/public-api.txt`` against ``grit-lib/api-ratchet.toml``.
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
DEFAULT_RUSTDOC_JSON = ROOT / "target" / "doc" / "grit_lib.json"

METRIC_KEYS = (
    "public_api_lines",
    "pub_fn_impl_suffix",
    "pub_fn_with_in_name",
    "pub_fn_too_many_arguments_allow",
    "pub_fn_two_plus_bare_bool",
    "pub_fn_result_string_err",
    "pub_free_fn_repository_first",
    "pub_free_fn_git_dir_path_first",
    "pub_items_multiple_paths",
    "pub_libc_types",
    "pub_unsafe_fn",
)

GIT_DIR_PARAM_NAMES = frozenset(
    {"git_dir", "gitdir", "repo_git_dir", "common_dir"},
)

TOO_MANY_ARGS_RE = re.compile(r"allow\s*\(\s*clippy::too_many_arguments\s*\)")

STRING_RUST_TYPES = frozenset({"String", "alloc::string::String"})

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

    impl_suffix: list[str] = []
    with_in_name: list[str] = []
    too_many_args: list[str] = []
    two_plus_bool: list[str] = []
    result_string: list[str] = []
    repo_first: list[str] = []
    git_dir_first: list[str] = []
    unsafe_fns: list[str] = []
    libc_offenders: list[str] = []
    associated_fn_ids = build_associated_function_ids(index)

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
                param_name in GIT_DIR_PARAM_NAMES or param_name.endswith("_git_dir")
            ):
                git_dir_first.append(f"{path} :: {name}")

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

    path_map = collect_public_paths(payload)
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
        "public_api_lines": MetricResult(len(api_lines)),
        "pub_fn_impl_suffix": MetricResult(len(impl_suffix), impl_suffix),
        "pub_fn_with_in_name": MetricResult(len(with_in_name), with_in_name),
        "pub_fn_too_many_arguments_allow": MetricResult(len(too_many_args), too_many_args),
        "pub_fn_two_plus_bare_bool": MetricResult(len(two_plus_bool), two_plus_bool),
        "pub_fn_result_string_err": MetricResult(len(result_string), result_string),
        "pub_free_fn_repository_first": MetricResult(len(repo_first), repo_first),
        "pub_free_fn_git_dir_path_first": MetricResult(len(git_dir_first), git_dir_first),
        "pub_items_multiple_paths": MetricResult(len(multi_paths), multi_paths),
        "pub_libc_types": MetricResult(len(sorted(set(libc_offenders))), sorted(set(libc_offenders))),
        "pub_unsafe_fn": MetricResult(len(unsafe_fns), unsafe_fns),
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
    lines = [
        "# grit-lib public API quality counts — ratchet baseline",
        "# Regenerate with: make api-ratchet",
        "",
        "[meta]",
        f'updated = "{today}"',
        "",
        "[counts]",
    ]
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
