---
title: Introducing Grit
slug: introducing-grit
date: 2026-06-05
author: schacon
summary: A short note on rebuilding Git in Rust, as a library, with compatibility as the benchmark.
---

Grit is a from-scratch reimplementation of Git in Rust. The goal is not to make a Git-like tool with a nicer surface area, but to build a compatible Git engine that can eventually pass the upstream Git test suite.

## Why rebuild Git?

Git is everywhere, but much of its behavior is encoded in a large C codebase that grew around a command-line program. That history is part of what makes Git powerful, but it also makes Git hard to embed, hard to experiment with, and hard to reason about as a set of reusable library components.

Grit started from a simple question: what would Git look like if it were designed today as an idiomatic Rust library first, with the CLI as a thin wrapper around that library?

## What we want from it

The project is focused on compatibility. Passing Git's own tests gives us a concrete target and lets us use agents to give us a solid starting point.

## Why now?

A library-oriented Git implementation opens the door to better developer tools, servers, agents, and experiments that need Git semantics without shelling out to `git`. Grit is an attempt to make that foundation small, explicit, testable, and embeddable.
