#!/bin/sh

test_description='grit rev-parse --is-bare-repository and related queries in bare and non-bare repos'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup non-bare repo' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo hello >file.txt && grit add file.txt && grit commit -m "initial"
    )
'

test_expect_success 'setup bare repo' '
    grit init --bare bare.git
'

test_expect_success 'non-bare: --is-bare-repository is false' '
    (cd repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'bare: --is-bare-repository is true' '
    (cd bare.git && grit rev-parse --is-bare-repository >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --is-inside-work-tree is true' '
    (cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bare: --is-inside-work-tree is false' '
    (cd bare.git && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --is-inside-git-dir is false' '
    (cd repo && grit rev-parse --is-inside-git-dir >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --git-dir is .git' '
    (cd repo && grit rev-parse --git-dir >../actual) &&
    echo ".git" >expect &&
    test_cmp expect actual
'

test_expect_success 'bare: --git-dir is .' '
    (cd bare.git && grit rev-parse --git-dir >../actual) &&
    echo "." >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --show-toplevel matches git' '
    (cd repo && grit rev-parse --show-toplevel >../actual) &&
    (cd repo && git rev-parse --show-toplevel >../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --show-prefix at root is empty' '
    (cd repo && grit rev-parse --show-prefix >../actual) &&
    (cd repo && git rev-parse --show-prefix >../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --show-prefix from subdir' '
    (cd repo && mkdir -p a/b/c && cd a/b/c &&
    grit rev-parse --show-prefix >../../../../actual) &&
    echo "a/b/c/" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --git-dir from subdir' '
    (cd repo/a/b/c && grit rev-parse --git-dir >../../../../actual) &&
    (cd repo/a/b/c && git rev-parse --git-dir >../../../../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --is-inside-work-tree from subdir' '
    (cd repo/a/b/c && grit rev-parse --is-inside-work-tree >../../../../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --is-bare-repository from subdir' '
    (cd repo/a/b/c && grit rev-parse --is-bare-repository >../../../../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --is-inside-git-dir from subdir' '
    (cd repo/a/b/c && grit rev-parse --is-inside-git-dir >../../../../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --show-toplevel from subdir' '
    (cd repo/a/b/c && grit rev-parse --show-toplevel >../../../../actual) &&
    (cd repo/a/b/c && git rev-parse --show-toplevel >../../../../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: HEAD resolves in subdir' '
    (cd repo/a/b/c && grit rev-parse HEAD >../../../../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: --verify HEAD in subdir' '
    (cd repo/a/b/c && grit rev-parse --verify HEAD >../../../../actual) &&
    (cd repo && grit rev-parse --verify HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'bare: HEAD not valid without commits' '
    (cd bare.git && test_must_fail grit rev-parse HEAD 2>/dev/null)
'

test_expect_success 'setup: clone into bare to get commits' '
    /usr/bin/git clone --bare repo bare-with-commits.git 2>/dev/null
'

test_expect_success 'bare with commits: --is-bare-repository is true' '
    (cd bare-with-commits.git && grit rev-parse --is-bare-repository >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bare with commits: HEAD resolves' '
    (cd bare-with-commits.git && grit rev-parse HEAD >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'bare with commits: --verify HEAD' '
    (cd bare-with-commits.git && grit rev-parse --verify HEAD >../actual) &&
    (cd repo && grit rev-parse --verify HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'bare with commits: --git-dir is .' '
    (cd bare-with-commits.git && grit rev-parse --git-dir >../actual) &&
    echo "." >expect &&
    test_cmp expect actual
'

test_expect_success 'bare with commits: --is-inside-work-tree is false' '
    (cd bare-with-commits.git && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup: second non-bare with more commits' '
    grit init repo2 && cd repo2 &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo a >a.txt && grit add a.txt && grit commit -m "r2-first" &&
    echo b >b.txt && grit add b.txt && grit commit -m "r2-second" &&
    git branch dev &&
    cd ..
'

test_expect_success 'repo2: --is-bare-repository is false' '
    (cd repo2 && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'repo2: HEAD resolves' '
    (cd repo2 && grit rev-parse HEAD >../actual) &&
    (cd repo2 && git rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'repo2: branch ref resolves' '
    (cd repo2 && grit rev-parse dev >../actual) &&
    (cd repo2 && git rev-parse dev >../expect) &&
    test_cmp expect actual
'

test_expect_success 'repo2: --show-toplevel' '
    (cd repo2 && grit rev-parse --show-toplevel >../actual) &&
    (cd repo2 && git rev-parse --show-toplevel >../expect) &&
    test_cmp expect actual
'

test_expect_success 'non-bare: multiple query flags at once' '
    (cd repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual &&
    (cd repo && grit rev-parse --is-inside-work-tree >../actual2) &&
    echo "true" >expect2 &&
    test_cmp expect2 actual2
'

test_expect_success 'bare: multiple query flags' '
    (cd bare.git && grit rev-parse --is-bare-repository >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual &&
    (cd bare.git && grit rev-parse --is-inside-work-tree >../actual2) &&
    echo "false" >expect2 &&
    test_cmp expect2 actual2
'

test_done
