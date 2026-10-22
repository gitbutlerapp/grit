---
title: Two weeks inside the grit factory
slug: two-weeks-of-the-factory
date: 2026-10-20
author: schacon
summary: The agent factory that maintains Grit has been running for two weeks. Here is what happens inside it, who does what, how a change travels from a plan to main, and what 2,656 agents built along the way, with the real data behind every chart.
script: /blog-media/two-weeks-of-the-factory/factory.js?v=2
style: /blog-media/two-weeks-of-the-factory/factory.css?v=2
---

Two weeks ago we [handed Grit's maintenance to a factory of agents](https://grit-scm.com/blog/the-grit-maintenance-factory). A small web service plans work from a roadmap, hands each piece to a cloud agent, has a second agent from a different model family review it, and lands what passes on `main`. A human sets the direction and makes the calls the agents can't.

That post described the design. This one shows what actually happened. Every chart below is drawn from the factory's own records: each agent, each run, each review verdict and each landing, between October 6 and October 20. Hover anything for details, and open the data tables under each chart for the exact numbers.

## Two weeks in numbers

<factory-stats>2,656 agents · 4,314 runs · 2,544 agent-hours · 24.5 billion tokens · 1,601 reviews · 653 changes landed · 15 releases.</factory-stats>

The first roadmap had 21 items, from "CI gate for every integration" to "grit history commands", and all 21 are done. Counting the next roadmap's first four items and a few plans the maintainer asked for directly, the planners wrote 30 plans with 330 steps. On top of those came 198 issue fixes and a steady stream of follow-ups. On the GitHub side that is 691 commits on `main`, about 275,000 lines added and 139,000 removed, and a release every day from v0.5.1 to v0.5.15.

## The heartbeat

This is every agent run of those two weeks, one line each, laid out by role. The orange band is the implementers writing code. The blue band is the reviewers reading it. The black ticks along the bottom are changes landing on `main`, and the marks along the top are releases.

<factory-heartbeat>A timeline of every agent run, one line each, grouped by role, with changes landing on main along the bottom.</factory-heartbeat>

A few things stand out. The factory never stops: even in its quietest hour of the day an average of five agents are at work, and in its busiest about ten, because nothing in the loop waits for a person unless it has to. The blue band follows the orange one all the way across, since every change is reviewed as soon as its author reports it. And landings come in clusters, as a stack of approved steps lands one branch after another.

## The crew

Each kind of work has its own agent with its own instructions and model. Implementation is the bulk of the agent time. Review is the bulk of the agent count, because every change is reviewed at least once and usually two or three times.

| Agent | What it does | Model | Agents | Agent-hours |
|---|---|---|---|---|
| Planner | Breaks a roadmap item into steps that can each land alone, with a dependency graph | Claude Opus 5.5 | 30 | 6 |
| Implementer | Writes one step, issue fix or follow-up, with tests and benchmarks, then lands it | Composer 2.5 | 728 | 1,689 |
| Reviewer | Fetches the branch, runs the tests, re-measures, writes the integration brief | GPT-5.6 Sol | 1,601 | 775 |
| Integrator | Takes over landing when it fails: rebases, resolves conflicts, lands | GPT-5.6 Sol | 32 | 11 |
| Triager | Labels new issues and decides whether a fix is worth an agent | Composer 2.5 | 204 | 15 |
| Dogfooder | Uses Grit like a person on real repos, and checks each release on three platforms | Claude Sonnet 5.5 | 50 | 18 |
| Quality | A standing loop that simplifies code and moves logic from the CLI into the library | Claude Opus 5.5 | 8 | 29 |

Two newer standing loops joined this week, a speed agent that works on the worst benchmark gaps and a daily benchmark run, so they barely show up yet.

## Work over time

The same history as a stacked chart. Switch between how many agents were working on average, how many runs started, and how many changes landed, by hour or by day.

<factory-activity>Agents at work, runs started and changes landed on main, by hour or by day, stacked by role.</factory-activity>

The factory runs up to 15 agents at once, and up to 25 when more ready work is queued than there are free slots. The peaks are those surges: a plan is approved, its independent steps all become ready at once, and the queue drains in parallel. The flat orange block around October 18 is not a surge. It is the worst bug of the two weeks, described below.

## Tokens and models

Two models did almost all the work. Composer 2.5 writes the code, and GPT-5.6 Sol reviews it and repairs failed integrations. Keeping the reviewer in a different model family from the author is deliberate: the two don't share blind spots, and the review pass catches things the author's own tests didn't.

<factory-models>Tokens, agent-hours and agent counts per model, and how the tokens divide between cached reads, fresh input and output.</factory-models>

The token total looks enormous, 24.5 billion, but 98% of it is cached context. An agent working in a large repository rereads the same files and conversation turn after turn, and those reads are cheap. Fresh input and output, the part that's actually new, comes to about 270 million tokens over two weeks.

## How one roadmap item gets built

Here is "grit history commands", one roadmap item, from the moment the planner started to the moment its last step landed, 21 hours later. Five of those hours were the plan waiting for the maintainer to approve it, the dashed line at the top. Each row is one plan step. The top half of a row is the step's author, and the bottom half is the agents that reviewed it. The arrows are stacking: a step building on another step's branch before that branch has landed. Choose another roadmap item from the menu to compare.

<factory-anatomy>A timeline of every agent and run for one roadmap item, with reviews, fixes, landings and stacked branches.</factory-anatomy>

The planner wrote 11 steps: a history-rewrite engine in the library, an atomic way to apply a rewrite, then one `grit history` subcommand per step, and finally benchmarks against `git`. Every step after the first built on an earlier one. A row goes through these handovers:

1. **Build.** An implementer starts on its own cloud machine, creates a GitButler branch, and commits as it goes. GitButler Mesh shares the branch with the other machines; nothing goes to GitHub yet.
2. **Review.** When the author reports its branch, a reviewer on another model pulls it with `but mesh get`, runs the Rust tests, checks the change against the system `git` and writes a brief. Cursor's Bugbot reviews the same diff in parallel.
3. **Fix.** If the reviewer or Bugbot found a real problem, the feedback goes back to the *same* author agent, which still has its context, and it fixes the branch. The hatched segments are those fix rounds. This repeats until the reviewer approves, or the round limit is reached and the tests pass.
4. **Stack.** As soon as a step is approved, the steps that depend on it can start. They apply its branch from the Mesh and stack their own branch on top, without waiting for it to land.
5. **Land.** The author agent gets one more message: rebase onto the latest `main`, rerun the checks CI runs, and land with `but merge`. If that fails, a fresh integrator agent takes over. A step whose dependencies haven't landed yet waits, then lands on its own once they have.

## Stacking branches

Most plan steps don't start from `main`. They start from a branch that another agent built an hour earlier and a reviewer approved minutes ago. That keeps a plan's steps small and parallel without waiting a full review-and-land cycle for every link in the chain.

<factory-stacks>How many plan steps were built directly on main, and how many on top of one or more unmerged branches.</factory-stacks>

235 changes were built on someone else's unmerged work, some on a stack six branches deep. That only works because every branch is fetchable from any machine through the Mesh, and because landing respects the stack. A step can't land before the branches under it, and if a branch underneath is discarded, everything stacked on it is rebuilt without it.

## How work lands

Of the 727 pieces of code work the factory took on, here is where they went: what the first review said, whether the change landed, and who said yes.

<factory-flow>How code work flows: from the first review verdict, to landed, discarded or still in flight, to who approved the landing.</factory-flow>

The first review is not a rubber stamp. It approved only about a quarter of changes as submitted and asked for changes on the rest, and the most common path to `main` was three review rounds. Very few changes were discarded outright, though: 643 of 727 landed. The usual outcome of a rejection is a better version of the same change, not a dead one.

Most landings needed nobody's approval. A change that is approved by its reviewer and passes the Rust tests lands automatically, unless it's a security fix: 391 landed that way. The maintainer said yes to 208 more, and the integration agent to 44 since it started answering merge decisions on October 15. The median change went from task created to landed on `main` in 3.8 hours. Nine out of ten landed within 18 hours.

## The last mile

Landing is its own kind of work, and more of it than you might expect:

- **623 changes were landed by their own author**, which rebased onto a `main` that had usually moved several times since it started, resolved the conflicts and reran CI's checks first.
- **20 needed a separate integrator agent**, after the author's attempt failed on conflicts, a push race, or a rebase that broke the tests.
- **35 waited** in the queue because a branch underneath them hadn't landed yet, and landed on their own once it did.
- **12 were held during a release**: while the daily release is cut and built, `main` is frozen and only fixes for the release itself land.
- **22 main-keeper agents** were launched when CI on `main` went red. Each had one job, getting `main` green again before anything else, and 21 of them landed their fix.

## Who makes the decisions

Agents ask for a decision when something is genuinely not theirs to call: approving a plan, a change that won't pass review, a stuck task, a question about scope. The maintainer's phone has an inbox for those.

<factory-decisions>Who answered the 441 decisions the agents asked for: the maintainer, the integration agent, the inbox agent, or nobody because they resolved on their own.</factory-decisions>

There were 441 decisions in two weeks, about 30 a day, most of them "should this land?" That was too many to read carefully. So more of them now go to agents: an integration agent answers merge decisions as they arrive, and as of this week an inbox agent goes through everything still open once a day. Security fixes still always wait for a person.

## What went wrong

Plenty, and the factory's own records keep the score.

- **Agents lose track of things.** Runs end without reporting a branch, reviews finish without being routed, and a stuck task waits forever. The factory now sweeps for these every minute and has recovered work 121 times.
- **Runaway work.** From October 15 to 18, the sweep that recovers lost work kept queuing the same follow-up task again: 54 copies of "Campaign fixtures and 1.1x scorecard", up to 15 of them running at once. They used 472 agent-hours, nearly a fifth of the two weeks' total, before we caught it. 50 were cancelled. The factory now never queues a second copy of live work, and raises an alarm when one title is created four times in a day.
- **False alarms.** Steps that were simply waiting for an earlier step to land were reported to the maintainer as "stuck" until this week, and six of them were sitting in the inbox when we found it.
- **Discarded foundations.** When a change near the bottom of a stack is discarded, the work stacked on it can't land as it is. It has to be rebuilt without that change, which the factory now does on its own.
- **Outside dependencies.** On the first morning an invalid API key kept agents from launching until we fixed it. Bugbot's API has rejected the team key 263 times, and each time the factory falls back to requesting the review with a comment on the PR.

## What's next

The first roadmap is done. The next one has two themes that run the whole time:

- **Speed and memory.** Every supported command is benchmarked every day across realistic repository shapes, against the system `git`, including peak memory. A speed agent always works on the worst gap, and any regression becomes a fix task automatically. The full daily breakdown is at [maint.grit-scm.com/cooking/bench](https://maint.grit-scm.com/cooking/bench).
- **Quality.** A standing agent is always simplifying something, removing duplication, or moving logic out of the CLI into `grit-lib`. Its work is reviewed by a different model and landed by a separate integrator.

Alongside those, the roadmap turns to the everyday commands. `log`, `diff`, `commit`, `branch`, `push` and the rest get the options developers and agents actually reach for, plus change ids that survive every history rewrite.

You can watch all of it live at [maint.grit-scm.com](https://maint.grit-scm.com).
