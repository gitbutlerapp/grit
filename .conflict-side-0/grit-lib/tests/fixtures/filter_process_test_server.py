#!/usr/bin/env python3
"""Minimal git-filter-server v2 for Grit integration tests (clean uppercases, smudge lowercases)."""

import os
import sys

STATE = os.environ["GRIT_FILTER_STATE_FILE"]
LARGE = 65516


def pkt_write(payload: bytes) -> None:
    total = len(payload) + 4
    sys.stdout.buffer.write(f"{total:04x}".encode("ascii") + payload)
    sys.stdout.buffer.flush()


def pkt_flush() -> None:
    sys.stdout.buffer.write(b"0000")
    sys.stdout.buffer.flush()


def pkt_read() -> bytes | None:
    hdr = sys.stdin.buffer.read(4)
    if not hdr or hdr == b"0000":
        return None
    n = int(hdr.decode("ascii"), 16)
    if n < 4:
        return b""
    return sys.stdin.buffer.read(n - 4)


def pkt_read_line() -> str | None:
    chunk = pkt_read()
    if chunk is None:
        return None
    return chunk.decode("utf-8", "replace").rstrip("\n")


def read_blob() -> bytes:
    out = bytearray()
    while True:
        chunk = pkt_read()
        if chunk is None:
            break
        out.extend(chunk)
    return bytes(out)


def write_blob(data: bytes) -> None:
    off = 0
    while off < len(data):
        end = min(off + LARGE, len(data))
        pkt_write(data[off:end])
        off = end
    pkt_flush()


with open(STATE, "a", encoding="utf-8") as f:
    f.write(f"{os.getpid()}\n")

assert pkt_read_line() == "git-filter-client"
assert pkt_read_line() == "version=2"
assert pkt_read_line() is None

pkt_write(b"git-filter-server\n")
pkt_write(b"version=2\n")
pkt_flush()

while True:
    line = pkt_read_line()
    if line is None:
        break

for cap in (b"capability=clean\n", b"capability=smudge\n", b"capability=delay\n"):
    pkt_write(cap)
pkt_flush()

while True:
    cmd_line = pkt_read_line()
    if cmd_line is None:
        sys.exit(0)
    cmd = cmd_line.split("=", 1)[1]
    path_line = pkt_read_line()
    assert path_line and path_line.startswith("pathname="), path_line
    while pkt_read_line() is not None:
        pass
    blob = read_blob()
    if cmd == "clean":
        out = blob.upper()
    elif cmd == "smudge":
        out = blob.lower()
    else:
        pkt_write(b"status=error\n")
        pkt_flush()
        continue
    pkt_write(b"status=success\n")
    pkt_flush()
    write_blob(out)
    pkt_write(b"status=success\n")
    pkt_flush()
