#!/usr/bin/env python3
"""Move outcome types + HumanRender/MarkdownRender from a command file to render/<name>.rs."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CMD_ROOT = REPO / "grit-cli" / "src" / "commands"
RENDER_ROOT = REPO / "grit-cli" / "src" / "render"


def command_path(name: str) -> Path:
    if name == "diff":
        return CMD_ROOT / "diff" / "mod.rs"
    return CMD_ROOT / f"{name}.rs"


def render_name(name: str) -> str:
    return {"serve_http": "serve", "merge_base_cmd": "merge_base"}.get(name, name)


def find_render_span(lines: list[str]) -> tuple[int, int] | None:
    hr_lines = [i for i, l in enumerate(lines) if "impl HumanRender" in l]
    if not hr_lines:
        return None
    hr = hr_lines[0]
    md_last = max(i for i, l in enumerate(lines) if "impl MarkdownRender" in l)

    result_markers = [
        i for i in range(hr) if "/// Result of" in lines[i] or "/// Result of `" in lines[i]
    ]
    if result_markers:
        start = result_markers[-1] - 1
        while start > 0:
            prev = lines[start - 1]
            if prev.startswith("///") or prev.startswith("#[derive") or prev.strip() == "":
                start -= 1
                continue
            if re.match(r"^pub (struct|enum) \w+", prev):
                start -= 1
                continue
            break
        start += 1 if start > 0 else 0
    else:
        start = hr
        while start > 0:
            prev = lines[start - 1]
            if re.match(r"^(pub )?fn \w", prev):
                break
            start -= 1

    while start > 0 and re.match(r"^const \w", lines[start - 1]):
        start -= 1

    depth = 0
    end = md_last
    for k in range(md_last, len(lines)):
        depth += lines[k].count("{") - lines[k].count("}")
        if depth <= 0 and k > md_last:
            end = k + 1
            break
    else:
        end = len(lines)

    # Include trailing blank line
    while end < len(lines) and lines[end].strip() == "":
        end += 1
    return start, end


def extra_imports(block: str) -> str:
    imports: list[str] = []
    if "JsonSchema" in block:
        imports.append("use schemars::JsonSchema;")
    if "Serialize" in block:
        imports.append("use serde::Serialize;")
    if "BTreeMap" in block:
        imports.append("use std::collections::BTreeMap;")
    if "Write" in block or "dyn Write" in block:
        imports.append("use std::io::Write;")
    if "Result<" in block or "-> Result" in block:
        imports.append("use anyhow::Result;")
    if "outln!" in block:
        imports.append("use crate::output::outln;")
    if "HumanRender" in block or "MarkdownRender" in block or "RenderContext" in block:
        imports.append("use crate::render::{HumanRender, MarkdownRender, RenderContext};")
    if "markdown::" in block:
        imports.append("use crate::markdown;")
    if "PathDisplayOptions" in block:
        imports.append("use crate::ui::PathDisplayOptions;")
    if "short_hex" in block:
        imports.append("use grit_lib::objects::short_hex;")
    if "ConflictKindPhrase" in block:
        imports.append("use grit_lib::operation::ConflictKindPhrase;")
    if "format_inline_code" in block:
        imports.append("use crate::render::list::format_inline_code;")
    if "paint(" in block:
        imports.append("use crate::progress::{format_bytes, format_duration, paint, BOLD, DIM};")
    if "filesystem_path_for_user" in block:
        imports.append("use crate::ui::filesystem_path_for_user;")
    if "CommitJson" in block:
        imports.append("use crate::output::CommitJson;")
    if "ObjectId" in block:
        imports.append("use grit_lib::objects::ObjectId;")
    # dedupe preserving order
    seen = set()
    out = []
    for line in imports:
        if line not in seen:
            seen.add(line)
            out.append(line)
    return "\n".join(out) + ("\n" if out else "")


def make_render_module(cmd: str, block: str) -> str:
    title = f"grit {cmd.replace('_', ' ')}"
    header = f"//! `{title}` outcomes and rendering.\n\n"
    return header + extra_imports(block) + "\n" + block


def patch_command(path: Path, cmd: str, render_stem: str, start: int, end: int, dry_run: bool) -> None:
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    block = "".join(lines[start:end])

    new_lines = lines[:start] + [f"pub use crate::render::{render_stem}::*;\n\n"] + lines[end:]

    # Drop now-unused imports in command file (minimal)
    joined = "".join(new_lines)
    for pat in [
        r"use crate::output::\{[^}]*HumanRender[^}]*\};\n",
        r"use crate::output::\{outln, HumanRender, MarkdownRender, RenderContext\};\n",
        r"use crate::output::\{outln, CommitJson, HumanRender, MarkdownRender, OutputMode, RenderContext\};\n",
        r"use std::io::Write;\n",
        r"use crate::markdown;\n",
    ]:
        joined = re.sub(pat, "", joined, count=1)

    render_path = RENDER_ROOT / f"{render_stem}.rs"
    render_body = make_render_module(cmd, block.rstrip("\n") + "\n")

    if dry_run:
        print(f"Would write {render_path} ({len(render_body)} bytes), strip {start}-{end} from {path}")
        return

    render_path.write_text(render_body)
    path.write_text(joined)

    mod_path = RENDER_ROOT / "mod.rs"
    mod_line = f"pub mod {render_stem};"
    mod_text = mod_path.read_text()
    if mod_line not in mod_text:
        # insert alphabetically after first mod line block
        insert = f"pub mod {render_stem};\n"
        # find position: after `pub mod ` lines sorted
        mods = re.findall(r"^pub mod (\w+);", mod_text, re.M)
        if render_stem not in mods:
            mods.append(render_stem)
            mods.sort()
            # rebuild mod section is hard; append before blank after clone if exists
            if f"pub mod {render_stem};" not in mod_text:
                anchor = "pub mod clone;"
                if anchor in mod_text:
                    mod_text = mod_text.replace(anchor, f"pub mod {render_stem};\n{anchor}", 1)
                else:
                    mod_text = mod_text.replace("pub mod auth;", f"pub mod auth;\npub mod {render_stem};", 1)
                mod_path.write_text(mod_text)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("command")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    path = command_path(args.command)
    if not path.is_file():
        print(f"missing {path}", file=sys.stderr)
        sys.exit(1)
    lines = path.read_text().splitlines(keepends=True)
    span = find_render_span(lines)
    if not span:
        print("no HumanRender block", file=sys.stderr)
        sys.exit(1)
    render_stem = render_name(args.command)
    patch_command(path, args.command, render_stem, span[0], span[1], args.dry_run)


if __name__ == "__main__":
    main()
