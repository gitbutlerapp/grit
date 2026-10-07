---
title: Tutorial
summary: Fifteen minutes with grit. Create a repository, record some changes, work on a branch, and share it with a remote.
---

This walkthrough covers the commands you'll use every day. It assumes you have [installed grit](../install/) and know roughly what a commit and a branch are. The output shown is what `grit` prints, minus the colors.

## Tell grit who you are

Every commit records an author. Set your name and email once, in your global config:

```
$ grit config --global user.name "Ada Lovelace"
$ grit config --global user.email ada@example.com
```

`grit` reads and writes the same config files as Git, so if you've already set these up for Git, you can skip this step.

## Create a repository

```
$ grit init project
Initialized empty repository in /home/ada/project/.git
$ cd project
```

To work on an existing project instead, copy it with [`grit clone`](../clone/) and skip ahead to the next section:

```
$ grit clone https://github.com/gitbutlerapp/grit.git
```

## Your home base: grit status

Running `grit` with no arguments shows where you are and what's changed. Add a couple of files and look:

```
$ echo "# Notes" > README.md
$ echo "fn main() {}" > main.rs
$ grit
On main — no commits yet

Untracked
  ?  untracked     README.md
  ?  untracked     main.rs

→ grit add <file> to stage
```

The last line always suggests the next step. You'll come back to this screen a lot; [`grit status`](../status/) (or `grit st`) shows the same thing.

## Record a commit

[`grit commit`](../commit/) stages every change in the working tree and records it in one step:

```
$ grit commit "Start the project"
[main 217c6f9] Start the project
2 changes committed
```

There is no separate staging step to remember. [`grit add`](../add/) exists for when you want to stage particular files and check them in `grit status` first, but `grit commit` always records every change.

Look at the history with [`grit log`](../log/):

```
$ grit log
  217c6f9  ada  just now  Start the project
```

## Work on a branch

Create a branch and switch to it with [`grit switch -c`](../switch/):

```
$ grit switch -c feature
Created and switched to branch feature
```

Make a change and look at it with [`grit diff`](../diff/) before committing:

```
$ printf 'fn main() {\n    println!("hi");\n}\n' > main.rs
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

The two number columns are the old and new line numbers. Commit the change:

```
$ grit commit "Say hi"
[feature cf18394] Say hi
1 change committed
```

[`grit show`](../show/) displays a commit, its message and the files it changed. With no argument, it shows the latest commit:

```
$ grit show
branch feature
commit cf18394a62c3f845bd9c44927a5a55e014b2a99d
Author: Ada Lovelace <ada@example.com>
Date:   2026-10-07 10:00:00 +0000

    Say hi

 main.rs |   4 +++-
 1 file changed, 3 insertions(+), 1 deletion(-)
```

## Merge it back

Switch back to `main` and [merge](../merge/) the branch in. Nothing else has happened on `main`, so `grit` just moves `main` forward:

```
$ grit switch main
Switched to branch main
$ grit merge feature
Fast-forwarded feature → cf18394
```

The branch is done, so [delete it](../branch/):

```
$ grit branch -d feature
Deleted branch feature (was cf18394).
```

`grit branch -d` refuses to delete a branch whose commits haven't been merged into the branch you're on. Use `-D` when you really mean it.

## Share it with a remote

A remote is another copy of the repository, usually on a server. Add one called `origin` with [`grit remote add`](../remote/), then [push](../push/):

```
$ grit remote add origin https://github.com/ada/project.git
Added remote origin → https://github.com/ada/project.git
$ grit push
  pushed main → origin refs/heads/main
```

`grit push` sends the current branch to a branch with the same name on `origin`, creating it if needed. There are no upstream flags to set. For GitHub over HTTPS, [`grit auth`](../auth/) signs you in, and `grit` offers to run it if a push fails for lack of credentials.

To get other people's work, run [`grit pull`](../pull/). It fetches from the remote and brings your branch up to date, fast-forwarding when it can and recording a merge commit when both sides have new commits:

```
$ grit pull
Merged origin/main into the current branch (acd1d4b)
```

If someone pushed before you, `grit push` is rejected and tells you what to do:

```
$ grit push
  rejected origin refs/heads/main: not a fast-forward — run `grit pull` first
```

## Tag a release

[`grit tag`](../tag/) marks the current commit, and `grit push --tags` publishes your tags:

```
$ grit tag v0.1
Created tag v0.1
$ grit push --tags
  pushed --tags → origin refs/tags/v0.1
```

## When things conflict

`grit merge`, `grit pull` and `grit pick` never leave a half-finished merge behind. If the two sides change the same lines, `grit` lists the conflicting files and leaves your branch and working tree as they were. To resolve the conflict, run the merge with `git`, fix the files and commit. Conflict resolution in `grit` itself is on the roadmap.

`grit` also refuses to switch branches, merge or pull while you have uncommitted changes, so work in progress can't get mixed into a merge. Commit first.

## Scripting

Every command takes `--json` and prints a single JSON object, which is handy for scripts and agents. `--filter` picks out the part you need:

```
$ grit status --json --filter '{branch, clean}'
{
  "branch": "main",
  "clean": true
}
```

See [Scripting with grit](../scripting/) for details, and each command's page for its JSON fields.

## Where to go next

Every command has a man page with its options and examples. Start from the [command list](../#commands).
