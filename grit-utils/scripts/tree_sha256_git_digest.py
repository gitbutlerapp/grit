#!/usr/bin/env python3
"""Git tree-sha256 reproduction: one ls-tree -r -z and one cat-file --batch per repo."""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys


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


class BatchBlobReader:
    def __init__(self, git: str, repo_root: str, env: dict[str, str]) -> None:
        self._proc = subprocess.Popen(
            [git, "cat-file", "--batch"],
            cwd=repo_root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env=env,
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


def git_objects_dir(git: str, repo_root: str) -> str:
    git_dir = subprocess.check_output(
        [git, "rev-parse", "--git-dir"], cwd=repo_root, text=True
    ).strip()
    if not os.path.isabs(git_dir):
        git_dir = os.path.join(repo_root, git_dir)
    return os.path.join(git_dir, "objects")


def git_env(extra_alternate: str | None) -> dict[str, str]:
    env = os.environ.copy()
    if extra_alternate:
        prev = env.get("GIT_ALTERNATE_OBJECT_DIRECTORIES", "")
        env["GIT_ALTERNATE_OBJECT_DIRECTORIES"] = (
            extra_alternate if not prev else prev + os.pathsep + extra_alternate
        )
    return env


def compute_digest(git: str, commit: str, extra_alternate: str | None = None) -> str:
    repo_root = os.getcwd()
    env = git_env(extra_alternate)
    tree = subprocess.check_output(
        [git, "rev-parse", commit + "^{tree}"], cwd=repo_root, text=True, env=env
    ).strip()
    listing = subprocess.check_output(
        [git, "ls-tree", "-r", "-z", "--full-name", tree], cwd=repo_root, env=env
    )
    records: list[bytes] = []
    blobs: list[tuple[bytes, str]] = []
    gitlinks: list[tuple[bytes, str]] = []
    for typ, oid, name in parse_ls_tree(listing):
        if typ == b"blob":
            blobs.append((name, oid))
        elif typ == b"commit":
            gitlinks.append((name, oid))
    batch = BatchBlobReader(git, repo_root, env)
    try:
        for path, oid in blobs:
            records.append(file_record(batch.sha256(oid), path))
        for path, oid in gitlinks:
            sub_wt = os.path.join(repo_root, path.decode())
            if not os.path.isdir(sub_wt):
                raise RuntimeError(f"submodule not checked out: {path!r}")
            parent_objects = git_objects_dir(git, repo_root)
            prev = os.getcwd()
            try:
                os.chdir(sub_wt)
                sub_digest = compute_digest(git, oid, parent_objects)
            finally:
                os.chdir(prev)
            records.append(submodule_record(sub_digest, path))
    finally:
        batch.close()
    records.sort()
    return hashlib.sha256(b"".join(records)).hexdigest()


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: tree_sha256_git_digest.py <git-binary> <rev>")
    git, rev = sys.argv[1], sys.argv[2]
    sys.stdout.write(compute_digest(git, rev))


if __name__ == "__main__":
    main()
