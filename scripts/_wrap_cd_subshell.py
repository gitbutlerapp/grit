#!/usr/bin/env python3
"""Wrap test bodies that `cd` into a subdir in a subshell `( ... )`.

These ported harness files assume cwd resets to the trash root before each
test, but test-lib.sh persists cwd across top-level test blocks (matching
upstream git/t). The setup test cd's into a subdir and stays there, so every
later block's bare `cd repo` fails. Wrapping each cd-using body in a subshell
contains the cwd change, so the parent stays at the trash root and every test
starts fresh — restoring the reset semantics the files were written for.
"""
import re
import sys

START = re.compile(r"^test_expect_(success|failure)\b.*'\s*$")


def odd_quotes(line: str) -> bool:
    return line.count("'") % 2 == 1


def process(path: str) -> int:
    with open(path) as f:
        lines = f.readlines()

    out = []
    i = 0
    changed = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        # A multiline block opener: starts with test_expect_*, ends with a lone
        # opening quote (odd number of quotes on the line => body follows).
        if START.match(line) and odd_quotes(line):
            # collect body until a line that is exactly a single quote
            j = i + 1
            body = []
            while j < n and lines[j].rstrip("\n").strip() != "'":
                body.append(lines[j])
                j += 1
            if j >= n:
                # no closer found; emit as-is
                out.append(line)
                i += 1
                continue
            closer = lines[j]
            has_cd = any(b.lstrip().startswith("cd ") for b in body)
            already_wrapped = body and body[0].lstrip().rstrip("\n") == "("
            if has_cd and not already_wrapped:
                out.append(line)
                out.append("\t(\n")
                out.extend(body)
                out.append("\t)\n")
                out.append(closer)
                changed += 1
            else:
                out.append(line)
                out.extend(body)
                out.append(closer)
            i = j + 1
            continue
        out.append(line)
        i += 1

    if changed:
        with open(path, "w") as f:
            f.writelines(out)
    return changed


if __name__ == "__main__":
    total = 0
    for p in sys.argv[1:]:
        c = process(p)
        print(f"{p}: wrapped {c} blocks")
        total += c
    print(f"TOTAL wrapped: {total}")
