# Goodbye grit-git

**Date:** 2026-10-07

> We removed the Git-compatible grit-git CLI and the ported test harness. Grit is now two things: a complete Git library and a simpler CLI built on it.

Yesterday we wrote about [a new focus](https://grit-scm.com/blog/a-new-site-and-a-new-focus/index.md) for Grit: speed, and a library worth linking, with `grit-git` kept around as a compatibility test bed. Today we went further. `grit-git` is gone.

## What we removed

`grit-git` was the command-for-command reimplementation of `git`: every subcommand, every flag, every message, built to pass Git's own test suite. It got remarkably far, passing more than 41,700 of the 42,001 upstream tests we ran. Along with it, we removed:

- the ported upstream shell test suite and the scripts and dashboards that drove it
- the vendored copy of Git's source tree
- the benchmark scripts, which measured `grit-git` against `git`

Nothing was moved over. The code is still in the history if anyone wants it, but we're not maintaining it.

## Why

Passing Git's test suite was a great way to start. It gave us a concrete target and showed that Grit reads and writes repositories correctly. But a test suite written for a command line pulls you toward reproducing that command line: its flags, its exact error text, its exit codes, and twenty years of subcommands that almost nobody uses. That is a lot of code to carry, and very little of it makes the library better.

It also kept a GPL crate at the center of the project. `grit-git` was GPL because matching Git's output meant reusing Git's wording. With it gone, all of Grit is MIT licensed.

## What Grit is now

Grit is now exactly two things:

- **`grit-lib`** is a Git library for Rust that covers most of what core Git does: the object database and packs, refs, the index, diffing, revision walking, merging, config, fetch and push. It never shells out to `git`, and we're working toward a library that's easy to embed: typed errors, no global state, nothing printed to the terminal. Almost all of our work goes here.
- **`grit-cli`** is the `grit` command. It's a simpler Git client that works on any repository and talks to any remote, but its interface is its own. Running `grit` with no arguments shows where you are and what to do next, and its commands can print `--json` for scripts.

Compatibility still matters, but it now means what users actually depend on: repositories on disk and the wire protocol. A repository Grit writes has to be one Git is happy with, and Grit has to fetch from and push to any Git server. We check that with Rust tests that run against the real `git` binary, not by matching Git's command-line output.

## Serving repositories

One thing `grit-git` did that we wanted to keep was the server side of fetch and push. So `grit` now has two plumbing commands, `grit upload-pack` and `grit receive-pack`, backed by a new server module in `grit-lib`. Any Git client can clone from, fetch from and push to a repository served this way, over ssh or through `grit-http-server` for smart HTTP:

```
git clone --upload-pack='grit upload-pack' ssh://host/srv/project.git
```

They support protocol v0, v1 and v2 and the common `receive.*` safety settings. Shallow clones, partial-clone filters and server-side hooks are still to come.

## What's next

The [roadmap](https://github.com/gitbutlerapp/grit/blob/main/ROADMAP.md) is the same, minus the work that only existed to support `grit-git`: a CI gate for every change, a benchmark baseline against Git, a real documentation site, and then making the library fast and clean. The first cleanup is pruning library code that only `grit-git` ever called.

If you want to try it:

```
curl -fsSL https://grit-scm.com/install | sh
cargo add grit-lib
```
