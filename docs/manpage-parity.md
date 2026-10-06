# Manpage/Behavior Parity Checklist (v1 Plumbing)

This is a living checklist for reviewing `grit` against upstream Git docs and observed behavior.  
Status values are intentionally conservative until reviewed command-by-command.

| Command | Upstream doc | Behavior areas to verify vs docs/tests | Reviewed vs `grit` |
|---|---|---|---|
| `init` | [git-init](https://git-scm.com/docs/git-init) | Repo directory layout (`.git` / bare), templates, `HEAD` initialization, `--initial-branch`, `--shared`, `--separate-git-dir`, re-init messaging | ☐ Not reviewed |
| `hash-object` | [git-hash-object](https://git-scm.com/docs/git-hash-object) | stdin/file input modes, type selection, `-w` write semantics, `--stdin-paths`, `--path`, `--literally`, error handling for invalid paths/types | ☐ Not reviewed |
| `cat-file` | [git-cat-file](https://git-scm.com/docs/git-cat-file) | `-t`/`-s`/`-p`/existence modes, object type rendering, tag dereference behavior, batch modes (`--batch*`), malformed object/error output | ☐ Not reviewed |
| `update-index` | [git-update-index](https://git-scm.com/docs/git-update-index) | add/remove/update paths, `--cacheinfo`/`--index-info`, refresh/racy stat handling, flag toggles (`assume-unchanged`, `skip-worktree` as applicable), index format compatibility | ☐ Not reviewed |
| `ls-files` | [git-ls-files](https://git-scm.com/docs/git-ls-files) | default index listing, status filters (`-o/-i/-m/-d/-k/-u`), stage output (`-s`), excludes/pathspec behavior, formatting (`-z`, `--format`, `--deduplicate`) | ☐ Not reviewed |
| `write-tree` | [git-write-tree](https://git-scm.com/docs/git-write-tree) | tree construction from index, sort/mode correctness, `--prefix`, `--missing-ok`, cache-tree interactions (if present), failure cases for unresolved entries | ☐ Not reviewed |
| `ls-tree` | [git-ls-tree](https://git-scm.com/docs/git-ls-tree) | tree traversal depth/options (`-r/-d/-t`), path restriction semantics, output variants (`--name-only`, `--long`, `--format`), quoting/escaping behavior | ☐ Not reviewed |
| `read-tree` | [git-read-tree](https://git-scm.com/docs/git-read-tree) | single-tree reads, `-m` merge modes (2-way/3-way), `-u` worktree updates, `--reset`, `--prefix`, conflict staging semantics, D/F edge cases | ☐ Not reviewed |
| `checkout-index` | [git-checkout-index](https://git-scm.com/docs/git-checkout-index) | checkout modes (`-a`, path list/stdin), force/dry-run/quiet behavior, `--prefix`, `--stage`, temp output (`--temp`/`--tmpdir`), symlink/platform behavior | ☐ Not reviewed |
| `commit-tree` | [git-commit-tree](https://git-scm.com/docs/git-commit-tree) | commit object headers (tree/parent/author/committer), message sources (`-m`, `-F`, stdin), encoding/signing flags, timestamp/env handling, stdout hash output | ☐ Not reviewed |
| `update-ref` | [git-update-ref](https://git-scm.com/docs/git-update-ref) | create/update/delete refs, old-value verification, deref/no-deref behavior, batch stdin protocol, reflog writes, invalid refname/error semantics | ☐ Not reviewed |

## Review notes

- When a command is reviewed, change its status from `☐ Not reviewed` to one of:
  - `☑ Reviewed (matches in-scope behavior)`
  - `△ Reviewed (differences noted)`
- Record significant deltas and follow-up tasks here as short bullets, grouped by command.
