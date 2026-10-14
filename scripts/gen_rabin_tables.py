import re
from pathlib import Path

text = Path("/tmp/git-scratch/diff-delta.c").read_text()
out = Path("/workspace/grit-lib/src/delta_rabin_tables.rs")
lines = ["// Rabin tables from Git diff-delta (format constants).\n"]
for name in ["T", "U"]:
    m = re.search(
        rf"static const unsigned int {name}\[256\] = \{{([^}}]+)\}};", text, re.S
    )
    vals = [int(x.strip(), 0) for x in m.group(1).split(",") if x.strip()]
    lines.append(f"pub(crate) const {name}: [u32; 256] = [\n")
    for i in range(0, 256, 8):
        chunk = ", ".join(f"0x{v:08x}" for v in vals[i : i + 8])
        lines.append(f"    {chunk},\n")
    lines.append("];\n\n")
out.write_text("".join(lines))
