# grit-cli

`grit-cli` provides `grit`, a small opinionated command line interface backed by [`grit-lib`](https://crates.io/crates/grit-lib).

It is not intended to be a drop-in replacement for Git. For Git-compatible command behavior, use the `grit-git` CLI (the `grit-git` crate). `grit` is a simpler interface for workflows built on Grit's Rust implementation.

## Install

```sh
cargo install grit-cli
```

This installs the `grit` executable.

## Commands

`grit` favors one obvious way to do the common thing, plain-language output, and a
status screen that doubles as the home base.

### `grit` / `grit status`

Running `grit` with no arguments shows the dashboard: the current branch, a
shortlog of the commits you're ahead of the target branch by, your staged and
unstaged changes, untracked files, and a hint for what to do next.

```sh
grit
# explicit form / alias:
grit status
grit st
```

```text
On feature/example  ·  2 ahead of origin/main

  abc1234  Add example
  fed9876  Refine output

Staged
  +  new-file.txt                      new
  ~  src/main.rs                       modified

Changed (not staged)
  ~  README.md                         modified

Untracked
  ?  notes.md

→ grit add <file> to stage  ·  grit commit "message" to commit
```

### `grit add`

Stage changes. With no paths, stages **everything** that `grit status` reports as
changed — modifications, deletions, and untracked files alike. Pass paths to
stage a subset. Path arguments are resolved from your **current directory** (like
Git): from a subdirectory, `grit add g` stages `g` in that folder, and
`../other` or absolute paths under the repository work too. A path that matches
nothing is an error (non-zero exit).

```sh
grit add            # stage all changes
grit add src/ a.txt # stage only these paths
```

### `grit commit`

Stage every change and record a new commit. The message can be a positional
argument or `-m`; `-a` is accepted for familiarity and makes the staging step
explicit.

```sh
grit commit "what changed"    # stage everything, then commit
grit commit -m "what changed" # same, with -m
grit commit -am "what changed"
grit commit -a                # stages everything, then asks for a message
```

Author/committer identity comes from `user.name` / `user.email` (honoring the
`GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE` overrides).

### `grit branch`

List branches, or create / delete one.

```sh
grit branch            # list (current marked with *)
grit branch feature    # create "feature" at HEAD (does not switch)
grit branch -d feature # delete "feature" (refuses if not merged into HEAD)
grit branch -D feature # force-delete even when not merged
```

### `grit switch`

Move to another branch, updating the working tree. Refuses to switch with
uncommitted changes, and won't clobber an untracked file the destination needs.

```sh
grit switch main          # switch to an existing branch
grit switch -c feature    # create "feature" and switch to it
grit checkout main        # aliases: checkout, co
```

### `grit merge`

Merge another branch into the current one — fast-forwarding when possible,
otherwise recording a merge commit. Conflicts are reported without leaving a
half-finished merge (resolving them is out of scope for `grit`).

```sh
grit merge feature
```

### `grit fetch` / `grit pull` / `grit push`

Talk to remotes. `grit` keeps these argument-free: they default to `origin` and
the current branch's same-named counterpart — no `-u origin <branch>` ceremony.

```sh
grit fetch          # download refs/objects from origin (or: grit fetch <remote>)
grit pull           # fetch, then fast-forward / merge the upstream in
grit push           # publish the current branch to origin
```

Local (`file://` / path), `git://`, `ssh`, and `http(s)` remotes are supported.

For HTTPS remotes, credentials come from your configured `credential.helper`.
When a push or fetch to a `github.com` HTTPS remote fails authentication, `grit`
offers to sign you in (see `grit auth`) and retries.

### `grit auth`

Sign in to GitHub using the OAuth **Device Flow** and store the resulting token
in Git's credential store, so HTTPS `grit push` / `grit fetch` to github.com just
work. There is no intermediate service — every request goes straight to
github.com.

```sh
grit auth
```

`grit` prints a short code and a URL (`https://github.com/login/device`); you enter
the code in your browser, and `grit` polls GitHub until you authorize, then hands
the token to your `credential.helper`. A credential helper must be configured so
the token can be saved, for example:

```sh
grit config --global credential.helper osxkeychain   # macOS
grit config --global credential.helper libsecret      # Linux
grit config --global credential.helper store          # plaintext file (any OS)
```

The flow uses the client id of a registered GitHub OAuth App (no client secret
is needed for the device flow). Provide it via `$GRIT_GITHUB_CLIENT_ID` or the
`grit.githubClientId` config key:

```sh
grit config --global grit.githubClientId <your-oauth-app-client-id>
```

### `grit shortlog`

Show the current branch, the target branch, and commits that are reachable from
`HEAD` but not from the target branch.

```sh
grit shortlog
# alias:
grit sl
```

Target branch lookup uses the first available value from:

1. `target.branch` in Git config
2. `origin/master`
3. `origin/main`
4. `master`
5. `main`

Example:

```text
On feature/example
Ahead of origin/main by 2 commits
abc1234 Add example
fed9876 Refine output
```
