#!/bin/sh
test_description='rev-parse --show-prefix, --show-toplevel, --git-dir, --is-inside-work-tree, --is-bare-repository, --is-inside-git-dir'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo &&
    cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo hello >file.txt &&
    grit add file.txt &&
    grit commit -m "initial" &&
    mkdir -p sub/deep/nested
	)
'

test_expect_success '--show-prefix at repo root is empty' '
    (cd repo && grit rev-parse --show-prefix >../actual) &&
    printf "\n" >expect &&
    test_cmp expect actual
'

test_expect_success '--show-prefix in sub/ shows sub/' '
    (cd repo/sub && grit rev-parse --show-prefix >../../actual) &&
    echo "sub/" >expect &&
    test_cmp expect actual
'

test_expect_success '--show-prefix in sub/deep/ shows sub/deep/' '
    (cd repo/sub/deep && grit rev-parse --show-prefix >../../../actual) &&
    echo "sub/deep/" >expect &&
    test_cmp expect actual
'

test_expect_success '--show-prefix in sub/deep/nested/' '
    (cd repo/sub/deep/nested && grit rev-parse --show-prefix >../../../../actual) &&
    echo "sub/deep/nested/" >expect &&
    test_cmp expect actual
'

test_expect_success '--show-toplevel at repo root' '
    (cd repo && grit rev-parse --show-toplevel >../actual) &&
    (cd repo && pwd >../expect) &&
    test_cmp expect actual
'

test_expect_success '--show-toplevel from subdirectory' '
    (cd repo/sub/deep && grit rev-parse --show-toplevel >../../../actual) &&
    (cd repo && pwd >../expect) &&
    test_cmp expect actual
'

test_expect_success '--show-toplevel from nested subdirectory' '
    (cd repo/sub/deep/nested && grit rev-parse --show-toplevel >../../../../actual) &&
    (cd repo && pwd >../expect) &&
    test_cmp expect actual
'

test_expect_success '--git-dir at repo root is .git' '
    (cd repo && grit rev-parse --git-dir >../actual) &&
    echo ".git" >expect &&
    test_cmp expect actual
'

test_expect_success '--git-dir from subdirectory' '
    (cd repo/sub/deep && grit rev-parse --git-dir >../../../actual) &&
    # could be relative or absolute; just check it ends with .git
    grep "\.git$" actual
'

test_expect_success '--is-inside-work-tree at root' '
    (cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success '--is-inside-work-tree from subdirectory' '
    (cd repo/sub/deep && grit rev-parse --is-inside-work-tree >../../../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success '--is-bare-repository is false for normal repo' '
    (cd repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success '--is-inside-git-dir is false at worktree root' '
    (cd repo && grit rev-parse --is-inside-git-dir >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success '--is-inside-git-dir is false from subdirectory' '
    (cd repo/sub && grit rev-parse --is-inside-git-dir >../../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD resolves to 40-char hash' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD from subdirectory works' '
    (cd repo/sub/deep && grit rev-parse HEAD >../../../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD is same from root and subdirectory' '
    (cd repo && grit rev-parse HEAD >../from_root) &&
    (cd repo/sub/deep && grit rev-parse HEAD >../../../from_sub) &&
    test_cmp from_root from_sub
'

test_expect_success 'rev-parse --short HEAD from subdirectory' '
    (cd repo/sub && grit rev-parse --short HEAD >../../actual) &&
    grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'rev-parse --verify HEAD from subdirectory' '
    (cd repo/sub && grit rev-parse --verify HEAD >../../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse main from subdirectory' '
    (cd repo/sub && grit rev-parse main >../../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'setup bare repo' '
    grit init --bare bare.git
'

test_expect_success '--is-bare-repository is true for bare repo' '
    (cd bare.git && grit rev-parse --is-bare-repository >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success '--is-inside-work-tree is false for bare repo' '
    (cd bare.git && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success '--git-dir for bare repo is .' '
    (cd bare.git && grit rev-parse --git-dir >../actual) &&
    echo "." >expect &&
    test_cmp expect actual
'

test_expect_success 'setup additional subdirectories' '
    (cd repo && mkdir -p a/b/c/d/e)
'

test_expect_success '--show-prefix deeply nested' '
    (cd repo/a/b/c/d/e && grit rev-parse --show-prefix >../../../../../../actual) &&
    echo "a/b/c/d/e/" >expect &&
    test_cmp expect actual
'

test_expect_success '--show-toplevel deeply nested' '
    (cd repo/a/b/c/d/e && grit rev-parse --show-toplevel >../../../../../../actual) &&
    (cd repo && pwd >../expect) &&
    test_cmp expect actual
'

test_expect_success '--show-prefix and --is-inside-work-tree separately from sub' '
    (cd repo/sub && grit rev-parse --show-prefix >../../prefix_out) &&
    echo "sub/" >expect &&
    test_cmp expect prefix_out &&
    (cd repo/sub && grit rev-parse --is-inside-work-tree >../../inside_out) &&
    echo "true" >expect2 &&
    test_cmp expect2 inside_out
'

test_expect_success '--git-dir and --is-bare-repository separately' '
    (cd repo && grit rev-parse --git-dir >../gitdir_out) &&
    echo ".git" >expect &&
    test_cmp expect gitdir_out &&
    (cd repo && grit rev-parse --is-bare-repository >../bare_out) &&
    echo "false" >expect2 &&
    test_cmp expect2 bare_out
'

test_expect_success 'rev-parse --verify with invalid ref fails' '
    (cd repo && ! grit rev-parse --verify nonexistent 2>/dev/null)
'

test_expect_success 'rev-parse --quiet --verify with invalid ref fails silently' '
    (cd repo && ! grit rev-parse --quiet --verify nonexistent 2>../stderr_out) &&
    test_must_be_empty stderr_out
'

test_expect_success 'setup second commit for parent resolution' '
    (cd repo &&
     echo world >file2.txt &&
     grit add file2.txt &&
     grit commit -m "second")
'

test_expect_success 'rev-parse HEAD~1 resolves parent' '
    (cd repo && grit rev-parse HEAD~1 >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse HEAD^ resolves parent same as HEAD~1' '
    (cd repo && grit rev-parse HEAD^ >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^{commit} resolves to same as HEAD' '
    (cd repo && grit rev-parse "HEAD^{commit}" >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_done
