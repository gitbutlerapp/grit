#!/usr/bin/env python3
"""Recompute a tree-sha256 digest using git ls-tree and a single cat-file --batch session per repo."""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys


def git_out(git: str, cwd: str, *args: str) -> bytes:
    return subprocess.check_output([git, *args], cwd=cwd)


def parse_ls_tree(data: bytes):
    pos = 0
    while pos < len(data):
        nul = data.find(b"\0", pos)
        if nul == -1:
            break
        entry = data[pos:nul]
        pos = nul + 1
        sp = entry.find(b" ")
        rest = entry[sp + 1 :]
        sp2 = rest.find(b" ")
        typ = rest[:sp2]
        rest2 = rest[sp2 + 1 :]
        tab = rest2.find(b"\t")
        oid = rest2[:tab].decode()
        name = rest2[tab + 1 :]
        yield typ, oid, name
    return


class BatchBlobReader:
    def __init__(self, git: str, cwd: str) -> None:
        self._proc = subprocess.Popen(
            [git, "cat-file", "--batch"],
            cwd=cwd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
        )
        self._cache: dict[str, str] = {}

    def sha256(self, oid: str) -> str:
        if oid in self._cache:
            return self._cache[oid]
        assert self._proc.stdin and self._proc.stdout
        self._proc.stdin.write(f"{oid}\n".encode())
        self._proc.stdin.flush()
        header = self._proc.stdout.readline()
        parts = header.split()
        if len(parts) < 3 or parts[1] != b"blob":
            raise RuntimeError(f"unexpected cat-file header for {oid}: {header!r}")
        size = int(parts[2])
        data = self._proc.stdout.read(size + 1)
        digest = hashlib.sha256(data[:size]).hexdigest()
        self._cache[oid] = digest
        return digest

    def close(self) -> None:
        if self._proc.stdin:
            self._proc.stdin.close()
        self._proc.wait()


def file_record(hex_digest: str, path: bytes) -> bytes:
    return hex_digest.encode() + b" " + path + b"\0"


def submodule_record(hex_digest: str, path: bytes) -> bytes:
    rec = hex_digest.encode() + b" " + path
    if not path.endswith(b"/"):
        rec += b"/"
    return rec + b"\0"


def compute_digest(git: str, cwd: str, commit: str) -> str:
    tree = git_out(git, cwd, "rev-parse", f"{commit}^{{tree}}").decode().strip()
    records: list[bytes] = []
    batch = BatchBlobReader(git, cwd)
    try:

        def walk(tree_oid: str, prefix: bytes) -> None:
            listing = git_out(git, cwd, "ls-tree", "-z", tree_oid)
            for typ, oid, name in parse_ls_tree(listing):
                path = prefix + name
                if typ == b"tree":
                    walk(oid, path + b"/")
                elif typ == b"blob":
                    hex_d = batch.sha256(oid)
                    records.append(file_record(hex_d, path))
                elif typ == b"commit":
                    sub_wt = os.path.join(cwd, name.decode())
                    if os.path.isdir(sub_wt):
                        sub_digest = compute_digest(git, sub_wt, oid)
                        records.append(submodule_record(sub_digest, path))
                    else:
                        raise RuntimeError(f"submodule not checked out: {path!r}")

        walk(tree, b"")
    finally:
        batch.close()
    records.sort()
    return hashlib.sha256(b"".join(records)).hexdigest()


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: tree_sha256_git_digest.py <git-binary> <rev>")
    git, rev = sys.argv[1], sys.argv[2]
    cwd = os.getcwd()
    digest = compute_digest(git, cwd, rev)
    sys.stdout.write(digest)


if __name__ == "__main__":
    main()
