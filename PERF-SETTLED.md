# Settled performance

Speed and memory questions that are closed: scenarios judged fast enough, and approaches accepted as the right trade-off. Read this before speed or memory work, and don't reopen an entry unless it is now materially worse (about 25% slower or fatter) than the numbers recorded here. The factory's speed loop skips the benchmark ids (`command/case@repo`) in the first column.

A difference is worth working on only when a person would notice it: grit takes over ~100 ms and is at least ~50 ms (and 10%) slower than git, or uses ~200 MB more peak memory than git (and 1.5× as much), or fails where git works.

| scenario or behaviour | verdict | grit | git | why | evidence | date |
| --- | --- | --- | --- | --- | --- | --- |
| `branch/list@many-refs` | fast enough | 1.7 ms | 1.3 ms | the remaining ~0.4 ms is process startup; five speed rounds after the first 6× win found nothing worth taking | factory #2906, #4184, #5562, #6005, #6793 | 2026-11-02 |
| `push/refspec@git-git` | fast enough | 989 ms | 1051 ms | grit now beats git. About half the wall time is the server (`git http-backend` receive-pack). The rest is the date-ordered have walk over ~75k commits, which git makes too because the new commits are dated before the remote's history. It now reads only packed header blocks, and what's left is mostly zlib setting up its Huffman tables per commit. Going further needs a commit-graph in the client, or a stopping rule that can send larger packs | factory #6936 | 2026-11-03 |
