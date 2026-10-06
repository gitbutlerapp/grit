#!/bin/sh
test_description='rev-parse --default, --verify, --short, revision resolution'
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
    echo world >file2.txt &&
    grit add file2.txt &&
    grit commit -m "second" &&
    echo foo >file3.txt &&
    grit add file3.txt &&
    grit commit -m "third"
	)
'

test_expect_success 'rev-parse HEAD resolves to 40-char hash' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse main resolves same as HEAD' '
    (cd repo && grit rev-parse main >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify HEAD' '
    (cd repo && grit rev-parse --verify HEAD >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify main' '
    (cd repo && grit rev-parse --verify main >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify fails on invalid ref' '
    (cd repo && ! grit rev-parse --verify nonexistent 2>/dev/null)
'

test_expect_success 'rev-parse --quiet --verify fails silently on invalid ref' '
    (cd repo && ! grit rev-parse --quiet --verify nonexistent 2>../stderr_out) &&
    test_must_be_empty stderr_out
'

test_expect_success 'rev-parse --quiet --verify succeeds on valid ref' '
    (cd repo && grit rev-parse --quiet --verify HEAD >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --short HEAD produces 7-char hash' '
    (cd repo && grit rev-parse --short HEAD >../actual) &&
    grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'rev-parse --short HEAD matches beginning of full hash' '
    FULL=$(cd repo && grit rev-parse HEAD) &&
    SHORT=$(cd repo && grit rev-parse --short HEAD) &&
    case "$FULL" in
    ${SHORT}*) true ;;
    *) false ;;
    esac
'

test_expect_success 'rev-parse --short=4 HEAD produces 4-char hash' '
    (cd repo && grit rev-parse --short=4 HEAD >../actual) &&
    grep "^[0-9a-f]\{4\}$" actual
'

test_expect_success 'rev-parse HEAD~0 is same as HEAD' '
    (cd repo && grit rev-parse HEAD~0 >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~1 is parent of HEAD' '
    (cd repo && grit rev-parse HEAD~1 >../actual) &&
    (cd repo && grit log -n1 --skip=1 --format="%H" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD~2 is grandparent of HEAD' '
    (cd repo && grit rev-parse HEAD~2 >../actual) &&
    (cd repo && grit log -n1 --skip=2 --format="%H" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^ is same as HEAD~1' '
    (cd repo && grit rev-parse HEAD^ >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^{commit} is same as HEAD' '
    (cd repo && grit rev-parse "HEAD^{commit}" >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^{tree} resolves to tree hash' '
    (cd repo && grit rev-parse "HEAD^{tree}" >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual &&
    TREE_HASH=$(cat actual) &&
    HEAD_HASH=$(cd repo && grit rev-parse HEAD) &&
    test "$TREE_HASH" != "$HEAD_HASH"
'

test_expect_success 'rev-parse with tag' '
    (cd repo && grit tag v1.0) &&
    (cd repo && grit rev-parse v1.0 >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse tag resolves same as --verify tag' '
    (cd repo && grit rev-parse v1.0 >../plain) &&
    (cd repo && grit rev-parse --verify v1.0 >../verified) &&
    test_cmp plain verified
'

test_expect_success 'rev-parse main~1' '
    (cd repo && grit rev-parse main~1 >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse main~2' '
    (cd repo && grit rev-parse main~2 >../actual) &&
    (cd repo && grit rev-parse HEAD~2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse tag~1' '
    (cd repo && grit rev-parse v1.0~1 >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --short main' '
    (cd repo && grit rev-parse --short main >../actual) &&
    (cd repo && grit rev-parse --short HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse full hash as input' '
    FULL=$(cd repo && grit rev-parse HEAD) &&
    (cd repo && grit rev-parse $FULL >../actual) &&
    echo "$FULL" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse short hash as input' '
    FULL=$(cd repo && grit rev-parse HEAD) &&
    SHORT=$(echo "$FULL" | cut -c1-7) &&
    (cd repo && grit rev-parse $SHORT >../actual) &&
    echo "$FULL" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup branch and second tag' '
    PARENT=$(cd repo && grit rev-parse HEAD~1) &&
    GRANDPARENT=$(cd repo && grit rev-parse HEAD~2) &&
    (cd repo && grit branch feature $PARENT) &&
    (cd repo && grit tag v0.9 $GRANDPARENT)
'

test_expect_success 'rev-parse feature resolves to HEAD~1' '
    (cd repo && grit rev-parse feature >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse v0.9 resolves to HEAD~2' '
    (cd repo && grit rev-parse v0.9 >../actual) &&
    (cd repo && grit rev-parse HEAD~2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse feature~1 resolves to HEAD~2' '
    (cd repo && grit rev-parse feature~1 >../actual) &&
    (cd repo && grit rev-parse HEAD~2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse feature^ resolves to HEAD~2' '
    (cd repo && grit rev-parse feature^ >../actual) &&
    (cd repo && grit rev-parse HEAD~2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify fails on ambiguous prefix (if applicable)' '
    (cd repo && grit rev-parse --verify HEAD >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'rev-parse --short=5 HEAD' '
    (cd repo && grit rev-parse --short=5 HEAD >../actual) &&
    grep "^[0-9a-f]\{5\}$" actual
'

test_expect_success 'rev-parse --short=40 HEAD outputs full hash' '
    (cd repo && grit rev-parse --short=40 HEAD >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-toplevel works after branch creation' '
    (cd repo && grit rev-parse --show-toplevel >../actual) &&
    (cd repo && pwd >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD from different branches is correct' '
    (cd repo && grit rev-parse HEAD >../main_head) &&
    (cd repo && grit checkout feature) &&
    (cd repo && grit rev-parse HEAD >../feature_head) &&
    ! test_cmp main_head feature_head &&
    (cd repo && grit checkout main)
'

test_done
