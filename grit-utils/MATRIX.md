# Daily benchmark functionality matrix
Overview of **grit-lib** coverage, **`grit` commands**, common workflows, and **`grit-bench daily`** scenario ids (`<command>/<case>@<repo>`). The factory runs the full matrix on [maint.grit-scm.com/cooking/bench](https://maint.grit-scm.com/cooking/bench); headline ids also appear on the cooking page.
## How to read this
- **Daily id** — stable registry key in `grit-utils` (`HEADLINE_SCENARIOS`, `MATRIX_SCENARIOS`, `REMOTE_MATRIX_SCENARIOS`).
- **Headline** — one of the 17 cooking-page rows when `headline: true` in the registry.
- **Legacy suite ids** — older committed `grit-bench` JSON used short names (for example `status-clean`, `campaign-*`). Daily ids supersede them; the headline `name` field keeps the short slug where applicable.

## odb
**grit-lib:** `grit_lib::odb`, `grit_lib::objects`, `grit_lib::hash`
**grit commands:** *(Criterion `odb_backend`; no `grit-bench daily` row yet)*
**Common paths:** Loose read/write, pack idx lookup, existence checks (micro-benchmarks).
**Daily scenario ids:** *(none yet — see micro-benchmarks / maintenance rows above)*

## packs
**grit-lib:** `grit_lib::odb` (pack/idx/MIDX), `grit_lib::pack`
**grit commands:** *(Criterion pack ingest; daily via `gc` / `repack` scenarios)*
**Common paths:** Pack generation and idx parse (micro-benchmarks; production paths hit `gc repack`).
**Daily scenario ids:** *(none yet — see micro-benchmarks / maintenance rows above)*

## refs
**grit-lib:** `grit_lib::refs`, `grit_lib::reflog`
**grit commands:** `grit branch`, `grit tag`, `grit reflog`, `grit gc pack-refs`
**Common paths:** List/create/delete branches and tags; pack-refs; many-ref fixtures.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `gc/pack-refs@synth-10k` | Pack loose tags via grit gc pack-refs vs git pack-refs |

## index
**grit-lib:** `grit_lib::index`
**grit commands:** `grit diff --staged`, staging via `grit add` / `grit restore`
**Common paths:** Staged vs HEAD diff on large trees.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `diff/staged@deep-tree` | Staged diff on deep-tree: grit diff --staged vs git diff --cached |
| `diff/staged@synth-100k` | Index vs HEAD diff: grit diff --staged vs git diff --cached |

## worktree
**grit-lib:** `grit_lib::worktree`, `grit_lib::index`, `grit_lib::status`
**grit commands:** `grit status`, `grit add`, `grit commit`, `grit restore`, `grit reset`, `grit rm`, `grit mv`, `grit clean`, `grit init`, `grit switch`, `grit stash`
**Common paths:** Clean/dirty status, stage all or paths, commit, restore, reset, rm/mv, clean untracked, init, switch, stash push/pop.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `add/all@synth-100k` | Stage all changes: grit add vs git add -A (after ~1% modified) |
| `add/update@synth-100k` | Stage tracked changes: grit add -u vs git add -u (after ~1% modified) |
| `add/all-ignorecase@synth-100k` | Stage all changes with macOS path config (core.ignorecase, core.precomposeunicode): grit add vs git add -A |
| `add/update-ignorecase@synth-100k` | Stage tracked changes with macOS path config (core.ignorecase, core.precomposeunicode): grit add -u vs git add -u |
| `add/dry-run@synth-10k` | Preview staging: grit add --dry-run vs git add -n -A |
| `add/patch-file@synth-10k` | Apply worktree diff to index: grit add --patch-file vs git apply --cached |
| `add/hunks@large-file` | Stage half of 500 hunks in a 50k-line file: grit add --hunks vs git apply --cached |
| `add/patch-file@large-file` | Apply prebuilt half-hunk patch to index: grit add --patch-file vs git apply --cached |
| `add/lines@large-file` | Stage first 25k new-side lines in a 50k-line file: grit add --lines vs git apply --cached |
| `add/modified@big-blobs` | Stage ~1% modified paths: grit add -- <paths> vs git add -- <paths> |
| `add/modified@synth-10k` | Stage modified files vs git add -A |
| `clean/only-ignored@synth-10k` | Preview ignored untracked removal: grit clean --only-ignored vs git clean -n -X |
| `clean/untracked@synth-10k` | Remove untracked files: grit clean -f vs git clean -fd |
| `commit/small@small` | Commit on small synth: grit commit bench vs git add+commit |
| `commit/small@synth-100k` | Commit after staging ~1% paths: grit commit bench vs git add+commit |
| `commit/small@synth-10k` | Commit staged changes vs git commit |
| `commit/staged@synth-100k` | Commit index only: grit commit --staged vs git commit (after partial stage) |
| `commit/dry-run@synth-10k` | Preview commit: grit commit --dry-run vs git commit --dry-run |
| `commit/paths@synth-10k` | Partial commit: grit commit -- \<paths\> vs git commit --only |
| `init/branch@small` | Initialize repo with branch name: grit init -b trunk vs git init -b trunk |
| `init/empty@small` | Initialize empty repo: grit init vs git init in an empty directory |
| `mv/file@synth-10k` | Rename tracked file: grit mv <src> <dst> vs git mv <src> <dst> |
| `reset/hard@synth-10k` | Hard reset dirty tree: grit reset --hard HEAD vs git reset --hard HEAD |
| `reset/hard-delete-heavy@wide-48k` | Hard reset to a commit deleting 2000 files in 20 dirs of a 48k-file tree and back: grit reset --hard vs git reset --hard |
| `reset/keep@synth-10k` | Reset branch with unrelated local edits: grit reset --keep HEAD~1 vs git reset --keep HEAD~1 |
| `reset/mixed@synth-10k` | Mixed reset dirty tree: grit reset HEAD vs git reset --mixed HEAD |
| `reset/sparse@sparse-120k` | Sparse reset out and back, 300 visible files and 400 excluded directories |
| `reset/sparse@sparse-240k` | Sparse reset out and back, 300 visible files and 400 excluded directories |
| `reset/sparse@sparse-unique-120k` | Sparse reset out and back, 300 visible files and 400 excluded directories |
| `reset/undo-commit@git-git` | Undo the last commit on pinned git.git (deep history, many tags): grit reset --soft HEAD~1 vs git reset --soft HEAD~1 |
| `restore/hunks@large-file` | Discard half of 500 hunks in a 50k-line file: grit restore --hunks vs git apply -R |
| `restore/paths@synth-10k` | Restore ~1% paths from index: grit restore -- <paths> vs git restore -- <paths> |
| `rm/dir@synth-10k` | Remove tracked directory: grit rm -r <dir> vs git rm -r <dir> |
| `rm/dry-run@synth-10k` | Preview remove directory: grit rm --dry-run <dir> vs git rm -n -r <dir> |
| `stash/pop-sparse@wide-120k` | Pop 3k edits in a mostly unchanged 120k-file tree vs git stash pop |
| `stash/pop-index@synth-10k` | Restore index from stash: grit stash pop --index vs git stash pop --index |
| `stash/push-paths@synth-10k` | Stash ~1% pathspec: grit stash push -- \<paths\> vs git stash push -- \<paths\> |
| `stash/push-pop@synth-10k` | Stash push and pop vs git stash |
| `status/clean@deep-tree` | Clean worktree on deep-tree: grit status vs git status -s |
| `status/clean@small` | Clean worktree on small synth: grit status vs git status -s |
| `status/clean@synth-100k` | Clean worktree: grit status vs git status -s |
| `status/clean@synth-10k` | Clean worktree status vs git status -s |
| `status/dirty@deep-tree` | Dirty worktree on deep-tree: grit status vs git status -s |
| `status/dirty@small` | Dirty worktree on small synth: grit status vs git status -s |
| `status/dirty@synth-100k` | Dirty worktree (~1% modified): grit status vs git status -s |
| `status/dirty@synth-10k` | Dirty worktree status vs git status -s |
| `status/paths@synth-100k` | Path-limited status: grit status -- data/d0000 vs git status -s -- data/d0000 |
| `status/renamed@synth-10k` | Staged rename in status: grit status vs git status -s after git mv |
| `switch/branch@synth-10k` | Switch branch vs git switch |
| `switch/sparse@sparse-120k` | Sparse switch out and back, 300 visible files and 400 excluded directories |
| `switch/sparse@sparse-240k` | Sparse switch out and back, 300 visible files and 400 excluded directories |
| `switch/sparse@sparse-unique-120k` | Sparse switch out and back, 300 visible files and 400 excluded directories |

## diff
**grit-lib:** `grit_lib::diff`
**grit commands:** `grit diff`
**Common paths:** Unstaged worktree diff across repo shapes.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `diff/worktree@big-blobs` | Unstaged worktree diff on big-blobs: grit diff vs git diff |
| `diff/worktree@deep-tree` | Unstaged worktree diff on deep-tree: grit diff vs git diff |
| `diff/worktree@synth-100k` | Unstaged worktree diff (~1% modified): grit diff vs git diff |
| `diff/worktree@synth-10k` | Worktree diff vs git diff |
| `diff/stat@synth-100k` | Unstaged diff stat (~1% modified): grit diff --stat vs git diff --stat |

## revwalk
**grit-lib:** `grit_lib::revwalk`, `grit_lib::revision`, `grit_lib::objects`
**grit commands:** `grit log`, `grit show`, `grit revs`, `grit merge-base`, `grit resolve`, `grit cat`, `grit reflog`, `grit shortlog`, `grit patch-id`
**Common paths:** Bounded log, full history, path log, show, rev count, merge-base, resolve, cat blob, reflog, ahead shortlog, patch-id.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `cat/blob@git-git` | Print one blob at HEAD:<path>: grit cat <oid> vs git cat-file -p <oid> (oid from git rev-parse HEAD:<path>) |
| `log/all@git-git` | Full reachable history: grit log (paged) vs git log --oneline |
| `log/oneline-100@git-git` | Last 100 commits one line each vs git log --oneline |
| `log/json-100@git-git` | Last 100 commits as JSON vs git log -100 with verbose `--format` |
| `log/json-filter@git-git` | Commit oids via grit log --json --filter '.commits[].oid' vs git log -50 --format=%H |
| `log/json-n10-loose-tags@git-git` | Bounded metadata with loose tags, including decorations and full messages |
| `log/json-n10-packed-tags@git-git` | Same bounded metadata and tag tips with packed refs |
| `log/stat-n50@git-git` | Last 50 commits with stat: grit log --limit 50 --stat vs git log --stat -50 |
| `log/oneline-100@synth-100k` | Last 100 commits: grit log --limit 100 vs git log --oneline -100 |
| `log/limit-path@git-git` | Bounded path log without commit-graph: grit log -n 200 -- Documentation vs git log (issue #1517) |
| `log/path@git-git` | History for one path: grit log -- <path> vs git log --oneline -- <path> |
| `merge-base/branches@git-git` | Merge base of two branches: grit merge-base <a> <b> vs git merge-base <a> <b> |
| `patch-id/commit@git-git` | Stable patch-id for HEAD: grit patch-id HEAD vs git patch-id --stable HEAD |
| `reflog/list@synth-10k` | Bounded HEAD reflog (10k entries): grit reflog -n 100 --json vs git reflog -n 100 |
| `resolve/head-n@git-git` | Resolve HEAD: grit resolve HEAD vs git rev-parse HEAD |
| `resolve/change-id@git-git` | Resolve change-id prefix: grit resolve \<cid\> vs git log --all --format=%H |
| `revs/count@git-git` | Count commits at HEAD: grit revs --count HEAD vs git rev-list --count HEAD |
| `revs/count-path@git-git` | Literal path count without commit-graph: grit revs --count 7e34d81 -- Documentation/git-log.txt vs git rev-list |
| `revs/count-range-commit-graph@git-git` | Count HEAD~20..HEAD with a verified Git-written commit-graph; catches full-ancestry regressions in small ranges |
| `shortlog/ahead@git-git` | Ahead-of-target commits: grit shortlog vs git log <target>..HEAD --oneline |
| `shortlog/behind@git-git` | Behind-target listing: grit shortlog --behind -n 20 vs git rev-list --left-right --count and git log -n 20 HEAD..target |
| `show/head@big-blobs` | Show HEAD on big-blobs: grit show vs git show |
| `show/head@git-git` | Show HEAD commit vs git show |
| `show/head@synth-10k` | Show HEAD commit: grit show vs git show |

## blame
**grit-lib:** `grit_lib::blame`
**grit commands:** `grit blame`
**Common paths:** Single-file blame on git.git and deep-tree.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `blame/file@deep-tree` | Blame one file on deep-tree: grit blame <path> vs git blame --porcelain <path> |
| `blame/file@git-git` | Blame one tracked file vs git blame |

## branching
**grit-lib:** `grit_lib::merge`, `grit_lib::pick`, `grit_lib::history`
**grit commands:** `grit branch`, `grit switch`, `grit merge`, `grit pick`, `grit revert`, `grit history`, `grit tag`, `grit stash`
**Common paths:** List/create/delete branches, tags, switch, merge, pick, revert, history reword, stash list.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `branch/create@many-refs` | Create one branch vs git branch |
| `branch/delete@many-refs` | Delete one branch vs git branch -D |
| `branch/rename@many-refs` | Rename one branch vs git branch -m |
| `branch/list@many-refs` | List branches vs git branch |
| `branch/list-json@many-refs` | List branches with JSON vs git branch -vv |
| `branch/list@many-refs-loose` | List branches on loose many-refs fixture vs git branch |
| `branch/merged@many-refs` | Branch --merged vs git branch --merged |
| `branch/contains@git-git-10k-refs` | Bounded human branch listing: HEAD~100, 100 matches, 10,000 unique packed tips |
| `branch/contains-json@git-git-10k-refs` | Same bounded branch listing with JSON and full metadata |
| `branch/merged-ref@many-refs` | Branch --merged ref query vs git for-each-ref on many-refs |
| `history/reword@synth-10k` | History reword vs git history reword or scripted rebase |
| `merge/content-both-sides@wide-20k` | Merge a branch where both sides edited all 20k files (one content merge per path) vs git merge |
| `merge/clean@synth-100k` | Merge branch on 100k-file synthetic vs git merge |
| `merge/clean@synth-10k` | Merge branch vs git merge |
| `pick/one@synth-100k` | Cherry-pick one commit on 100k-file synthetic vs git cherry-pick |
| `pick/one@synth-10k` | Cherry-pick one commit vs git cherry-pick |
| `rebase/onto@synth-10k` | Rebase onto upstream vs git rebase on synth-10k history fixture |
| `rebase/onto@git-git` | Rebase onto HEAD~10 vs git rebase on pinned git.git (conflict-free replay) |
| `revert/one@synth-10k` | Revert one commit vs git revert --no-edit |
| `stash/list@synth-10k` | List stash entries vs git stash list |
| `switch/branch@synth-100k` | Switch branch on 100k-file synthetic vs git switch |
| `switch/delete-heavy@wide-48k` | Switch to a branch deleting 2000 files in 20 dirs of a 48k-file tree and back: grit switch vs git switch |
| `switch/large-blob@big-asset` | Switch away from a 300 MB loose text blob and back, writing it to the work tree: grit switch vs git switch |
| `tag/contains@git-git` | Tag --contains ref query vs git tag -l on git.git (commit-graph) |
| `tag/create@many-refs` | Create lightweight tag vs git tag |
| `tag/list-sort-version@git-git` | Tag list --sort=version vs git tag -l --sort=version:refname on git.git |
| `tag/list@many-refs` | List tags vs git tag -l |

## remotes
**grit-lib:** `grit_lib::transport`, `grit_lib::remote`
**grit commands:** `grit fetch`, `grit pull`, `grit push`, `grit clone`, `grit remote`, `grit ls-remote` (via `remote refs`)
**Common paths:** Incremental fetch/push, FF pull, local and HTTP clone, ls-remote, no-op fetch.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `clone/http@synth-10k` | Clone synth-10k over grit smart HTTP vs git clone |
| `clone/local@synth-10k` | Clone over file:// (not git clone --local hardlink shortcut) vs git clone |
| `clone/full@git-git` | Full clone of git/git over smart HTTP (git http-backend) vs git clone; tracks peak RSS of large-pack ingest |
| `clone/bare@synth-10k` | Bare clone synth-10k vs `git clone --bare --no-local file://` |
| `clone/checkout@synth-100k` | Local-path clone with full checkout of 100k files (both tools link the pack, so the checkout dominates) vs git clone |
| `clone/shallow@git-git` | Shallow clone (`--depth 1`) over file:// vs git clone --depth 1 |
| `clone/blobless@git-git` | Blobless partial clone over file://: grit `clone --blobs none` vs `git clone --filter=blob:none` |
| `reset/blobless@git-git` | Materialize a fresh blobless no-checkout clone with reset --hard HEAD over smart HTTP |
| `clone/shallow-depth1@synth-10k` | Shallow clone over file://: grit clone --shallow vs git clone --depth 1 |
| `clone/shallow-depth1-http@synth-10k` | Shallow clone over grit smart HTTP: grit clone --shallow vs git clone --depth 1 |
| `fetch/incremental-http@git-git` | Incremental fetch over grit smart HTTP (git-git bare remote) |
| `fetch/deepen@git-git` | Deepen a depth-1 client by 50 commits vs git fetch --deepen 50 |
| `fetch/dry-run@git-git` | Fetch dry-run reports updates without writing refs or objects |
| `fetch/incremental@git-git` | Incremental fetch vs git fetch |
| `fetch/one-branch@git-git` | Fetch one branch refspec vs git fetch origin main |
| `fetch/noop@git-git` | No-op fetch when client already matches remote (file:// git-git) |
| `fetch/noop-local@many-refs` | No-op local fetch with 3,000 packed branches and 2,000 tags |
| `fetch/noop-local-no-tags@many-refs` | The same local many-ref fetch with automatic tags disabled |
| `ls-remote/all@many-refs` | List all refs on many-refs bare remote vs git ls-remote |
| `remote/show@many-refs` | Remote show on many-refs client vs `git remote show origin` |
| `remote/rename@many-refs` | Rename remote with ~10k packed tracking refs vs git remote rename |
| `pull/fast-forward@git-git` | Fast-forward pull vs git pull --ff-only |
| `pull/rebase@synth-10k` | Rebase pull (3 local onto 5 upstream) vs git pull --rebase |
| `pull/check@git-git` | Pull --check reports fast-forward possibility without changing HEAD |
| `push/incremental-http@git-git` | Incremental push over grit smart HTTP (git-git bare remote) |
| `push/refspec@git-git` | Explicit refspec push HEAD:refs/heads/bench vs git push |
| `push/incremental@git-git` | Incremental push vs git push |
| `push/force-with-lease@git-git` | Amended commit push with `--force-with-lease` vs git |
| `remote/list@small` | List remotes vs git remote |
| `remote/remove@many-refs` | Remove origin with 10k packed tracking refs vs git remote remove |

## maintenance
**grit-lib:** `grit_lib::maintenance`, `grit_lib::fsck`, `grit_lib::bundle`
**grit commands:** `grit gc`, `grit fsck`, `grit repack`
**Common paths:** gc, pack-refs, repack, commit-graph, fsck.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `fsck/full@big-blobs` | Full fsck on big-blobs fixture vs git fsck |
| `fsck/full@git-git` | Full fsck on git.git worktree vs git fsck |
| `gc/default@synth-10k` | Full grit gc vs git gc on synth-10k with loose objects |
| `repack/commit-graph@git-git` | grit gc commit-graph vs git commit-graph write on git.git |
| `repack/full@git-git` | grit gc repack vs git repack -ad on git.git worktree |

## config
**grit-lib:** `grit_lib::config`
**grit commands:** `grit config`
**Common paths:** Get/set one key.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `config/get@small` | Read one config key vs git config |
| `config/get-origin@small` | Read config with provenance: grit config --json \<key\> vs git config --show-origin --show-scope --get \<key\> |
| `config/set@small` | Set one config key vs git config |

## ignore
**grit-lib:** `grit_lib::ignore`
**grit commands:** `grit ignore check`
**Common paths:** Bulk check-ignore parity.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `ignore/check-1000@synth-10k` | Check 1000 paths against ignore rules vs git check-ignore |

## hooks
**grit-lib:** `grit_lib::hooks`
**grit commands:** `grit hooks`
**Common paths:** List hook scripts.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `hooks/list@small` | List hook scripts vs listing .git/hooks |

## notes
**grit-lib:** `grit_lib::notes`
**grit commands:** `grit notes`
**Common paths:** Add/list commit notes.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `notes/add@synth-10k` | Add commit note vs git notes add |
| `notes/list@synth-10k` | List notes vs git notes list |
| `notes/list-n100@synth-10k` | List 100 seeded notes vs git notes list |

## bundles
**grit-lib:** `grit_lib::bundle`
**grit commands:** `grit bundle`
**Common paths:** Create and verify bundle files.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `bundle/create@synth-10k` | Create bundle file vs git bundle create |
| `bundle/create-all@git-git` | Bundle every ref of a packed deep history vs git bundle create --all |
| `bundle/verify@synth-10k` | Verify bundle vs git bundle verify |

## worktrees
**grit-lib:** `grit_lib::worktree`
**grit commands:** `grit worktree`
**Common paths:** Add/remove linked worktrees.
**Daily scenario ids:**
| id | notes |
| --- | --- |
| `worktree/add@synth-10k` | Add linked worktree vs git worktree add |
| `worktree/list@synth-10k` | List ~20 linked worktrees: grit worktree list --json vs git worktree list --porcelain |
| `worktree/remove@synth-10k` | Remove linked worktree vs git worktree remove |

## Exemptions

Commands present in `grit` without a daily registry row (plumbing, agent-only, or covered indirectly):

| Command | Reason |
| --- | --- |
| `auth` | Credential helper plumbing; not a repo workflow benchmark. |
| `update` | Self-update; no git data plane. |
| `completions` | Shell completion generator. |
| `manager` | Internal process supervisor for smart HTTP. |
| `upload-pack` | Wire-protocol side of fetch; covered by `fetch`/`clone` scenarios. |
| `receive-pack` | Wire-protocol side of push; covered by `push` scenarios. |
| `help` | Meta command. |
| `skill` | Agent documentation export; no timed git work. |
| `repack` | Top-level repack; daily matrix uses `grit gc repack` (`repack/full@git-git`). |
| `ahead` | Alias of `shortlog`; covered by `shortlog/ahead@git-git`. |

## Gaps

Common paths not yet in the daily matrix or blocked on roadmap CLI:

- **`grit log --since` / `--until`** — date filters beyond deep-history matrix rows are not duplicated in daily census yet.
- **`grit rebase` / `grit history rebase`** — Top-level rebase is a later roadmap item; no daily row until the history engine exposes it.
- **`grit commit --dry-run`** — Preview only; no daily row (covered indirectly by commit scenarios).
- **Partial clone `--blobs`** — Fetch/clone blob filters land with roadmap partial-clone work.
- **ODB/pack micro-ops** — Criterion `odb_backend` / pack ingest — not duplicated in daily census yet.
