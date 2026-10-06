---
title: A new site, and a new focus
slug: a-new-site-and-a-new-focus
date: 2026-10-06
author: schacon
summary: Grit got far enough on Git's own test suite that the next problem isn't compatibility. It's speed and a library worth linking. Here's what changes.
---

If you've been here before, you'll notice the site looks different. The new homepage is laid out as a commit graph: each section is a commit, the library work branches off and merges back in, and the whole thing ends at the root commit. It's a small visual joke, but it's also a fair summary of where the project is. We've reached a point where it makes sense to look back at the history and decide what the next branch is.

## Where we are

Grit started with one goal: reimplement Git in Rust well enough to pass Git's own test suite. As of today, the compatibility CLI, `grit-git`, passes more than 41,700 of the 42,001 upstream tests we run.

That number is real, but it hides a lot. Getting there meant workarounds, slow paths, and faithful support for every odd subcommand and legacy interface Git has picked up over twenty years. Passing the tests showed that the on-disk formats and wire protocols are right. It didn't make Grit a library you'd want to put in your product, or a client you'd want to use every day.

## The new focus

The project is now organized around three crates with distinct jobs:

- **`grit-lib`** is a clean, linkable Git library that any Rust project can use. This is the heart of the project.
- **`grit-cli`** is the `grit` binary: a Git client with a modern command line. It's what the installer gives you.
- **`grit-git`** stays a command-for-command Git mirror, but its job is now to be our **compatibility test bed**, not our product.

The upstream harness doesn't go away. It becomes a regression gate: in-scope tests must not get worse, and nobody gets to weaken a test to make a change pass.

## Performance comes first

The library and CLI need to be as fast as we can make them, and we want to prove it with measurements, not claims. That means benchmarks for core operations in `grit-lib`, plus real-world scenarios compared directly against `git`. Those scenarios cover large repositories, deep history, big packs, many refs and network operations.

Some of the planned work:

- Fix the super-linear hot paths where we reload attributes and config inside per-file loops.
- Move to a single hashing abstraction with hardware-accelerated SHA-1. We're deliberately not implementing sha1dc.
- Rework the object database read path: no reading whole packs into memory, a delta-base cache, faster index lookups.
- Add reachability bitmaps, and parallelize status, checkout, index-pack and delta search.

Most targets are written as ratios against Git, usually within 1.2× or better on the same machine and the same repository.

## A library worth linking

A library that prints to stdout, calls `process::exit`, reads environment variables deep inside, or keeps state in process-wide globals isn't really a library. Cleaning that up is explicit roadmap work:

- Caches and config are owned by a `Repository` value.
- The environment is read only at the CLI boundary.
- Errors are typed, and the Git-formatted text is rendered in `grit-git`.
- The library doesn't shell out.

We're also adding pluggable backends: an object database abstraction like the one Git core is introducing, and ref backends for loose, packed and reftable refs.

Alongside that, the relevant upstream tests get converted into Rust tests against the library API, covering core behavior and edge cases rather than command-line UX. Every public interface gets coverage.

## What we're dropping

Some parts of Git are rarely used and expensive to carry. Archive, the email workflow (`am`, `format-patch`, `send-email`, `imap-send`, `request-pull`) and the foreign-VCS bridges get no CLI and no further work. Where removing them simplifies `grit-lib`, they come out of the library too, and the matching harness files are marked out of scope with a reason.

## The CLI contract

Every `grit` command should work well for three audiences. By default it prints clean output for people. With `--json` it prints stable, documented output for scripts. With `--markdown` it prints output that's easy for agents to read. Today's commands already support `--json`; `--markdown` and published schemas are next.

## Following along

The full plan is in [ROADMAP.md](https://github.com/gitbutlerapp/grit/blob/main/ROADMAP.md), and the live version is on the [Grit Factory dashboard](https://maint.grit-scm.com/roadmap). The items are worked one at a time, in order. The first few are a CI gate for every change, a benchmark baseline against Git, and a real documentation site that has to stay current with every change.

If you want to try it:

```
curl -fsSL https://grit-scm.com/install | sh
cargo add grit-lib
```

Compatibility got us here. Speed and a clean library are what come next.
