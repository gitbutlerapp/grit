# Development logs

This directory held the development logs and test-run transcripts written while
building grit (phase notes, `ticket-runs/` output, transcripts). They were
removed from the working tree because some filenames contained `:` (timestamps
like `16:30`), which is invalid on Windows/NTFS and aborted `git checkout` on
Windows CI runners.

Nothing was rewritten — the files are still in git history. To restore the whole
`logs/` directory, check it out from the commit just before it was removed:

```sh
git checkout "$(git log --diff-filter=D -1 --format=%H -- logs/)^" -- logs/
```

That finds the commit that deleted the logs and restores the previous version of
`logs/` into your working tree and index.

> Note: on Windows the `:`-named files still can't be written to the filesystem —
> run the restore on macOS/Linux (or under WSL).

To browse the historical files without restoring them:

```sh
C=$(git log --diff-filter=D -1 --format=%H -- logs/)   # the removal commit
git ls-tree -r --name-only "$C^" -- logs/              # list every archived file
git show "$C^:logs/<filename>"                          # print one file
```
