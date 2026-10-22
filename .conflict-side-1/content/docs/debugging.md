---
title: Debugging and tracing
summary: Environment knobs for diagnosing grit behavior without changing CLI flags.
---

## Operation tracing (`GRIT_TRACE`)

Set **`GRIT_TRACE=1`** to print coarse **operation spans** from `grit-lib` to **stderr** while a command runs. Each line shows when a span opens and closes, with elapsed time on close. Spans cover repository open, config load, index read/write, status, revwalk setup, diffs, checkout, pack I/O, fetch negotiation, and push — not per-object work.

This is intended for local debugging and factory performance work. There is **no `--trace` flag**; only the environment variable is supported.

```console
$ GRIT_TRACE=1 grit status
grit.repository.open: new
grit.config.load: new
grit.config.load: close time.busy=147µs time.idle=4.82µs
grit.repository.open: close time.busy=297µs time.idle=26.2µs
grit.status.scan: new
grit.index.read: new
grit.status.scan: close time.busy=18.3ms time.idle=3.13µs
…
```

Filter noise with the usual **`RUST_LOG`** / tracing env filter (for example `RUST_LOG=info`).

Library embedders can enable the same spans by building `grit-lib` with the Cargo feature **`tracing`**, installing any `tracing` subscriber, and calling the same code paths. `grit-lib` does not install a global subscriber.

## Network trace (existing)

Network fetch/push tracing still uses repository options and **`GRIT_NET_DEBUG`** (see the [network library guide](../library/network/)). That path is separate from `GRIT_TRACE` operation spans.
