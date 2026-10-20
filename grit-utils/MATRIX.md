# Daily benchmark matrix gaps

Tracked gaps between common Git workflows and current `grit` flags. Matrix rows stay in the
registry with `pending: true` until the library/CLI exposes the capability.

| Scenario id | Git equivalent | Grit today | Notes |
| --- | --- | --- | --- |
| `log/author@git-git` | `git log --author=<pattern>` | no author filter on `grit log` | Roadmap: author/date filters on revwalk; use `log --group-by author` when landed |
