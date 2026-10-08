---
title: Grit is now maintained by a factory of agents
slug: the-grit-maintenance-factory
date: 2026-10-07
author: schacon
summary: Grit now has an almost fully autonomous maintainer. Cloud agents plan the roadmap, fix issues, review each other's work, land changes, cut a release every day and test it on Linux, macOS and Windows. Here's how it works and where to watch it.
---

Grit now has a maintainer that is mostly software. A small web service we call the factory runs a crew of cloud agents that plan work from the roadmap, triage and fix issues, write code and tests, benchmark against Git, review each other, land the result on `main`, and cut a release every day. The human maintainer sets direction, approves plans and answers the questions the agents ask.

This is an experiment, and it's running in the open. You can watch it live at [maint.grit-scm.com](https://maint.grit-scm.com).

## Where to watch

- **[What's cooking](https://maint.grit-scm.com)** is the public dashboard: the roadmap item in focus, every task that's queued, being cooked or being tasted (reviewed), the latest commits on `main`, benchmarks against Git, and how much agent time and how many tokens it all takes.
- **The changelog** on that page has one post per release, summarizing what landed that day with the reviewers' before-and-after measurements and a link to the exact commit range on GitHub.
- **[cooking.json](https://maint.grit-scm.com/cooking.json)** has the same data for scripts.
- **[GitHub](https://github.com/gitbutlerapp/grit)** is where it all ends up: issues the factory triaged or filed, commits on `main`, and the daily releases.

## The crew

Each kind of work gets its own agent, running on [Cursor's cloud agents](https://cursor.com/blog/typescript-sdk) through the Cursor SDK, with a model chosen for the job:

| Agent | What it does | Model |
|---|---|---|
| Planner | Breaks a roadmap item into small steps that can each land on their own | Claude Opus 5.5 |
| Implementer | Writes one step or one issue fix, with tests and benchmark numbers | Composer 2.5 |
| Reviewer | Reads the diff, runs the Rust tests, measures, and writes a brief | GPT-5.6 Sol |
| Integrator | Rebases approved work onto `main`, resolves conflicts and lands it | GPT-5.6 Sol |
| Triager | Labels new GitHub issues and decides whether a fix is worth trying | Composer 2.5 |
| Scout | Reads the Git mailing list and upstream commits for ideas | Claude Sonnet 5.5 |
| Dogfooder | Uses grit like a person would and files bugs | Claude Sonnet 5.5 |

The reviewer deliberately comes from a different model family than the implementer, so the two don't share blind spots.

## How work flows

It starts from two documents: a short direction statement (what Grit is, and what it isn't) and a roadmap of about twenty items. The factory keeps several roadmap items in flight at once. For each one, a planner writes a step-by-step plan with a dependency graph, outcomes it can measure, risks, and the open questions it wants the maintainer to weigh in on. The maintainer approves the plan, steers it, or sends it back.

Approved steps become tasks. Each runs on its own cloud machine and does its work in a [GitButler](https://gitbutler.com) workspace. Nothing goes to GitHub at this stage. Every branch lives in GitButler Mesh, where other agents can fetch it, stack work on top of it, or review it.

When an implementer finishes, a reviewer pulls the branch from the Mesh, runs `cargo test`, checks compatibility against the system `git`, re-measures the author's performance claims, and writes an integration brief. The brief covers what changed and why, before-and-after numbers, risks, the strongest argument against merging, and follow-up tasks it noticed. If the reviewer finds a real problem, the author gets the feedback and tries again.

Work that is approved and passes its Rust tests lands on its own. Security fixes always wait for the maintainer, and anything the reviewer won't approve goes to the maintainer with the brief attached. If landing fails because `main` moved underneath it, an integrator agent rebases the branch, resolves the conflicts and lands it. Integrated branches are archived from the Mesh automatically.

## Issues, ideas and dogfooding

Every new GitHub issue is triaged within minutes: labeled by type, area and priority, and either queued for a fix or marked as not worth one, with a written assessment.

Once a day, a scout reads recent traffic on the Git mailing list and new commits to upstream Git, and proposes work for Grit: performance ideas, compatibility changes, edge cases worth testing.

Dogfooding agents install Grit and use it the way a person would: cloning real projects, branching, committing, pushing, timing it all against `git`. They run on Cursor's Linux cloud and on two machines we run ourselves, a MacBook and a Windows PC, connected as self-hosted Cursor workers. Several of the bugs fixed this week were filed this way, including Windows path handling, macOS Unicode normalization and a clone that unpacked a 194,000-object pack into loose files.

## A release every day

On any day where new work has landed, the factory bumps the version (0.5.1, 0.5.2, …), tags it, and Grit's release workflow builds and publishes binaries for Linux, macOS and Windows. The factory then writes the changelog post for the dashboard.

Then the release gets checked. Each platform installs the published binary the way a user would and confirms the version. Each one draws its own random mix from a catalog of everyday scenarios:

- init, commit and push
- clones over HTTPS, local paths and `file://`
- switching branches and rewriting history
- undoing commits
- two people pushing to the same branch
- merge conflicts
- Grit serving Git, and Git serving Grit
- large trees and long histories

Each night's mix also adds a couple of twists, like Unicode file names, CRLF line endings or case-only renames. For every scenario the agent writes down the steps and the expected outcomes before running anything, then grades what actually happened. The maintainer gets one report in the morning: a scenario-by-platform grid, every failed step with what was expected and what happened, and links to any bugs it filed.

## The human part

The maintainer's job is the part that needs taste: writing the direction and the roadmap, approving plans, answering questions, and making the calls the agents flag. Decisions arrive in an inbox on a private dashboard and on a small iOS app with push notifications. Each one comes with a brief, laid out so it can be read on a phone. The maintainer can approve, steer the agent with a sentence, or queue one of the suggested follow-ups with a tap. Everything can be paused with one button.

## So far

In its first day and a half, the factory has:

- launched 404 agents across 646 runs, using about 150 hours of agent time and 2.5 billion tokens
- completed 220 reviews
- triaged all 33 open issues
- landed 92 changes on `main`

That's not all good code on the first try. Most changes go through two or three review rounds, and a few have been sent back or discarded. But it is a lot of carefully checked work, and you can see every step of it.

We'll keep writing about what works and what doesn't. In the meantime, [come watch it cook](https://maint.grit-scm.com).
