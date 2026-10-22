---
title: grit is now the simple CLI
slug: grit-is-now-the-simple-cli
date: 2026-06-20
author: schacon
summary: The grit binary you install is now the simple, library-first CLI. The fully git-compatible mirror moves to grit-git.
---

We've changed what the `grit` binary is. If you install grit today — via the shell
script, the PowerShell script, or `cargo install grit-cli` — you now get a small,
simple Git client built directly on top of the `grit-lib` library. The fully
git-compatible, command-for-command mirror of `git` is still here, but it now lives
under a different name: `grit-git`.

## What changed

Until now, the installed `grit` binary was the compatibility mirror: a faithful
reimplementation of the `git` command line, plumbing and porcelain included, built to
eventually pass Git's own test suite. That binary hasn't gone anywhere — it has simply
been renamed to `grit-git`.

The name `grit` now points at what used to be the `grit-simple` experiment (briefly
shipped as `gs`). It's a deliberately small, opinionated CLI that shows off what the
`grit-lib` library can do without trying to reproduce every corner of Git's surface
area.

In short:

- `grit` — the simple, library-first CLI. This is what the installers give you.
- `grit-git` — the full git-compatible mirror, our benchmark against upstream Git.

## Why make the swap

Compatibility is still the project's benchmark, and `grit-git` is how we measure it.
But the more interesting story for most people is `grit-lib`: a Git engine you can
embed, reason about, and build on without shelling out to `git`. The simple `grit` CLI
is the most direct way to see that library in action, so it makes sense for it to be
the thing you get by default.

If you want the compatibility mirror, build or run `grit-git` from the workspace. If
you just want to try grit, the install command hasn't changed — you'll now land on the
simpler, friendlier CLI.
