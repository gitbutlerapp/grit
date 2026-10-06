#!/bin/sh
test_description='rev-parse: --verify, --short, --show-toplevel, --git-dir, object suffixes'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo one >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "first" &&
    echo two >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "second" &&
    echo three >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "third" &&
    echo four >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "fourth"
    )
'

test_expect_success 'rev-parse HEAD returns full 40-char hash' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    HASH=$(cat actual) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse HEAD~0 equals HEAD' '
    (cd repo && grit rev-parse HEAD >../head &&
    grit rev-parse HEAD~0 >../head0) &&
    test_cmp head head0
'

test_expect_success 'rev-parse HEAD~1 returns parent' '
    (cd repo && grit rev-parse HEAD~1 >../parent) &&
    HASH=$(cat parent) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse HEAD~1 differs from HEAD' '
    (cd repo && grit rev-parse HEAD >../head &&
    grit rev-parse HEAD~1 >../parent) &&
    ! test_cmp head parent
'

test_expect_success 'rev-parse HEAD~2 returns grandparent' '
    (cd repo && grit rev-parse HEAD~2 >../gp) &&
    HASH=$(cat gp) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse HEAD~3 returns root commit' '
    (cd repo && grit rev-parse HEAD~3 >../root) &&
    ROOT=$(cat root) &&
    FIRST=$(cd repo && grit rev-list --reverse HEAD | head -1) &&
    test "$ROOT" = "$FIRST"
'

test_expect_success 'rev-parse --short HEAD returns abbreviated hash' '
    (cd repo && grit rev-parse --short HEAD >../actual) &&
    ABBREV=$(cat actual) &&
    LEN=${#ABBREV} &&
    test "$LEN" -ge 4 &&
    test "$LEN" -le 12
'

test_expect_success 'rev-parse --short is prefix of full hash' '
    (cd repo && grit rev-parse HEAD >../full &&
    grit rev-parse --short HEAD >../short) &&
    FULL=$(cat full) &&
    SHORT=$(cat short) &&
    case "$FULL" in
    "$SHORT"*) : ok ;;
    *) exit 1 ;;
    esac
'

test_expect_success 'rev-parse --verify HEAD succeeds' '
    (cd repo && grit rev-parse --verify HEAD >../actual) &&
    HASH=$(cat actual) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse --verify HEAD matches rev-parse HEAD' '
    (cd repo && grit rev-parse HEAD >../plain &&
    grit rev-parse --verify HEAD >../verified) &&
    test_cmp plain verified
'

test_expect_success 'rev-parse --verify HEAD~1 succeeds' '
    (cd repo && grit rev-parse --verify HEAD~1 >../actual) &&
    HASH=$(cat actual) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse --verify nonexistent fails' '
    (cd repo && ! grit rev-parse --verify nonexistent 2>/dev/null)
'

test_expect_success 'rev-parse HEAD^{commit} returns commit hash' '
    (cd repo && grit rev-parse HEAD^{commit} >../actual) &&
    (cd repo && grit rev-parse HEAD >../head) &&
    test_cmp head actual
'

test_expect_success 'rev-parse HEAD^0 equals HEAD^{commit}' '
    (cd repo && grit rev-parse HEAD^0 >../hat0 &&
    grit rev-parse HEAD^{commit} >../hatcommit) &&
    test_cmp hat0 hatcommit
'

test_expect_success 'rev-parse HEAD^{tree} returns tree hash' '
    (cd repo && grit rev-parse HEAD^{tree} >../actual) &&
    HASH=$(cat actual) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse HEAD^{tree} differs from HEAD' '
    (cd repo && grit rev-parse HEAD >../head &&
    grit rev-parse HEAD^{tree} >../tree) &&
    ! test_cmp head tree
'

test_expect_success 'rev-parse HEAD^{tree} matches log %T' '
    (cd repo && grit rev-parse HEAD^{tree} >../tree_rp &&
    grit log -n 1 --format="%T" >../tree_log) &&
    test_cmp tree_rp tree_log
'

test_expect_success 'rev-parse --show-toplevel returns repo root' '
    (cd repo && grit rev-parse --show-toplevel >../actual) &&
    TOPLEVEL=$(cat actual) &&
    test -d "$TOPLEVEL/.git"
'

test_expect_success 'rev-parse --git-dir returns .git' '
    (cd repo && grit rev-parse --git-dir >../actual) &&
    echo ".git" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-bare-repository returns false' '
    (cd repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree returns true' '
    (cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix at root is empty' '
    (cd repo && grit rev-parse --show-prefix >../actual) &&
    echo "" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix in subdirectory' '
    (cd repo && mkdir -p sub/deep &&
    cd sub/deep && grit rev-parse --show-prefix >../../../actual) &&
    echo "sub/deep/" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-toplevel from subdirectory' '
    (cd repo && mkdir -p sub2 && cd sub2 &&
    grit rev-parse --show-toplevel >../../actual) &&
    (cd repo && grit rev-parse --show-toplevel >../expected) &&
    test_cmp expected actual
'

test_expect_success 'rev-parse --git-dir from subdirectory' '
    (cd repo && mkdir -p sub3 && cd sub3 &&
    grit rev-parse --git-dir >../../actual) &&
    grep "\.git$" actual
'

test_expect_success 'setup bare repo' '
    grit init --bare bare-repo
'

test_expect_success 'rev-parse --is-bare-repository in bare repo' '
    (cd bare-repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree in bare repo' '
    (cd bare-repo && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir in bare repo' '
    (cd bare-repo && grit rev-parse --git-dir >../actual) &&
    echo "." >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse multiple refs' '
    (cd repo && grit rev-parse HEAD HEAD~1 >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'rev-parse multiple refs are different' '
    (cd repo && grit rev-parse HEAD HEAD~1 >../actual) &&
    FIRST=$(head -1 actual) &&
    SECOND=$(tail -1 actual) &&
    test "$FIRST" != "$SECOND"
'

test_expect_success 'rev-parse HEAD~1^{tree} returns tree' '
    (cd repo && grit rev-parse HEAD~1^{tree} >../actual) &&
    HASH=$(cat actual) &&
    echo "$HASH" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success 'rev-parse HEAD~1^{tree} differs from HEAD^{tree}' '
    (cd repo && grit rev-parse HEAD^{tree} >../tree_head &&
    grit rev-parse HEAD~1^{tree} >../tree_parent) &&
    ! test_cmp tree_head tree_parent
'

test_expect_success 'rev-parse --short HEAD~1 returns abbreviation' '
    (cd repo && grit rev-parse --short HEAD~1 >../actual) &&
    ABBREV=$(cat actual) &&
    LEN=${#ABBREV} &&
    test "$LEN" -ge 4 &&
    test "$LEN" -le 12
'

test_expect_success 'rev-parse main resolves to HEAD' '
    (cd repo && grit rev-parse main >../main_hash &&
    grit rev-parse HEAD >../head_hash) &&
    test_cmp main_hash head_hash
'

test_done
