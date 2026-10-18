# grit-bench results

- **timestamp:** 2026-10-17T06:03:01.869390109Z
- **tools:** grit `grit 0.5.10` vs git `git version 2.43.0`
- **grit commit:** `a415eaaddcb67e35c4d3cf07bde2c19a597cb260`

| scenario | git median (ms) | grit median (ms) | ratio |
| --- | ---: | ---: | ---: |
| worktree-status-clean-synth-l | 1030.1 | 2613.9 | 2.538 |
| worktree-status-dirty-synth-l | 1581.3 | 2605.8 | 1.648 |
| worktree-add-synth-l | 667.2 | 36638.2 | 54.914 |
| worktree-commit-synth-l | 867.2 | 4368.9 | 5.038 |
| worktree-diff-synth-l | 234.6 | 735.5 | 3.135 |
| history-log-first-100 | 397.3 | 24109.6 | 60.683 |
| history-log-all | 471.8 | 5193.9 | 11.009 |
| history-rev-list-count | 318.9 | 956.7 | 2.999 |
| history-blame | 2220.9 | 2219.2 | 0.999 |
| history-show | 382.7 | 32515.7 | 84.969 |
| history-cat-file-batch | 23252.0 | 13093.9 | 0.563 |
| clone-file-prod | 491.2 | 481.9 | 0.981 |
| clone-git-http-prod | 530.7 | 862.8 | 1.626 |
| clone-grit-http-prod | 145264.8 | 144835.8 | 0.997 |
| fetch-incr-file-prod | 33.4 | 20.8 | 0.624 |
| fetch-noop-file-prod | 7.8 | 2.6 | 0.333 |
| fetch-incr-git-http-prod | 52.4 | 62.0 | 1.185 |
| fetch-noop-git-http-prod | 15.8 | 11.9 | 0.754 |
| fetch-incr-grit-http-prod | 246.9 | 581.3 | 2.355 |
| fetch-noop-grit-http-prod | 12.6 | 8.8 | 0.695 |
| push-file-prod | 26.9 | 16.6 | 0.618 |
| push-grit-http-prod | 29.7 | 276089.0 | 9303.600 |
| ls-remote-file-prod | 10.5 | 18.4 | 1.746 |
| ls-remote-git-http-prod | 19.7 | 20.1 | 1.020 |
| ls-remote-grit-http-prod | 53.7 | 53.8 | 1.002 |
| server-clone-http-compare-prod | 554.8 | 143581.8 | 258.797 |
| rev-list-count-git.git | 57.3 | 868.7 | 15.174 |
| rev-list-count-objects-git.git | 16.8 | 834.4 | 49.597 |
| serve-clone-git.git | 604.5 | 0.0 | 0.000 |

