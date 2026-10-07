---
title: Docs
summary: How to use grit, a simple Git client built on grit-lib. Start with the tutorial, then look up any command or open the library guide.
---

Grit is two things:

- **`grit`** — a Git client with a small, modern command line. It works on any Git repository and talks to any Git remote, but its commands are its own: one obvious way to do the common thing, plain-language output, and `--json` on every command.
- **`grit-lib`** — a fast, linkable Git library for Rust. Everything the CLI does goes through the library; you can embed the same engine in your own tools.

## How these docs are organized

| Section | What you'll find |
| --- | --- |
| **Getting started** | [Install](install/) grit, follow the [tutorial](tutorial/), or try the [library quick start](library-quickstart/). |
| **The grit CLI** | [Global options](global-options/), [scripting with `--json`](scripting/), and a man page for every command. |
| **Library guide** | Rust-oriented guides for `grit-lib` (growing over time), plus the [API reference on docs.rs](https://docs.rs/grit-lib). |
| **Benchmarks** | How `grit` and `grit-lib` compare to system Git on core operations. |

If you're new to the command line, start with the [tutorial](tutorial/). If you're wiring Git into a Rust program, start with the [library quick start](library-quickstart/) and the [library guide overview](library/).

## CLI vs library

Use **`grit`** when you want a day-to-day Git client from the terminal or in scripts (`--json` / `--filter`). Use **`grit-lib`** when you need programmatic access — opening repositories, reading objects and refs, diffing, revwalking, fetch/push, and the rest of Git's core — inside a Rust binary or service.

On-disk formats and the wire protocol stay compatible with Git; the CLI's argv and messages are deliberately *not* a Git mirror.

See [Install](install/) for download options, [Global options](global-options/) for flags shared by every command, and [Scripting with grit](scripting/) for `--json` and `--filter`.
