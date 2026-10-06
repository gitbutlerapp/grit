#!/bin/sh

test_description='grit rev-parse resolving refs, tags, HEAD, and various peel operators'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo && (
    cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo one >file.txt && grit add file.txt && grit commit -m "first" &&
    echo two >file2.txt && grit add file2.txt && grit commit -m "second" &&
    echo three >file3.txt && grit add file3.txt && grit commit -m "third" &&
    git tag v1.0 HEAD~2 &&
    git tag v2.0 HEAD~1 &&
    git tag v3.0 &&
    git branch feature
    )
'

test_expect_success 'rev-parse HEAD resolves to commit hash' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    (cd repo && git rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD output is 40-char hex' '
    (cd repo && grit rev-parse HEAD >../actual) &&
    len=$(wc -c <actual) &&
    test "$len" -eq 41
'

test_expect_success 'rev-parse main resolves same as HEAD' '
    (cd repo && grit rev-parse main >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse feature resolves same as main' '
    (cd repo && grit rev-parse feature >../actual) &&
    (cd repo && grit rev-parse main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse v1.0 resolves to first commit' '
    (cd repo && grit rev-parse v1.0 >../actual) &&
    (cd repo && git rev-parse v1.0 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse v2.0 resolves to second commit' '
    (cd repo && grit rev-parse v2.0 >../actual) &&
    (cd repo && git rev-parse v2.0 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse v3.0 resolves to third commit' '
    (cd repo && grit rev-parse v3.0 >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^1 resolves to parent' '
    (cd repo && grit rev-parse "HEAD^1" >../actual) &&
    (cd repo && git rev-parse "HEAD^1" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^ resolves same as HEAD^1' '
    (cd repo && grit rev-parse "HEAD^" >../actual) &&
    (cd repo && grit rev-parse "HEAD^1" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^{commit} peels to commit' '
    (cd repo && grit rev-parse "HEAD^{commit}" >../actual) &&
    (cd repo && git rev-parse "HEAD^{commit}" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse HEAD^{tree} peels to tree' '
    (cd repo && grit rev-parse "HEAD^{tree}" >../actual) &&
    (cd repo && git rev-parse "HEAD^{tree}" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse main^{commit} matches HEAD^{commit}' '
    (cd repo && grit rev-parse "main^{commit}" >../actual) &&
    (cd repo && grit rev-parse "HEAD^{commit}" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse v1.0^{commit} peels tag to commit' '
    (cd repo && grit rev-parse "v1.0^{commit}" >../actual) &&
    (cd repo && git rev-parse "v1.0^{commit}" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify HEAD succeeds' '
    (cd repo && grit rev-parse --verify HEAD >../actual) &&
    (cd repo && git rev-parse --verify HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify main succeeds' '
    (cd repo && grit rev-parse --verify main >../actual) &&
    (cd repo && git rev-parse --verify main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify v1.0 succeeds' '
    (cd repo && grit rev-parse --verify v1.0 >../actual) &&
    (cd repo && git rev-parse --verify v1.0 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --verify nonexistent fails' '
    (cd repo && test_must_fail grit rev-parse --verify nonexistent)
'

test_expect_success 'rev-parse --verify nonexistent exits non-zero' '
    (cd repo && ! grit rev-parse --verify nonexistent 2>/dev/null)
'

test_expect_success 'rev-parse multiple refs' '
    (cd repo && grit rev-parse HEAD main v1.0 >../actual) &&
    (cd repo && git rev-parse HEAD main v1.0 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse multiple refs line count' '
    (cd repo && grit rev-parse HEAD main v1.0 >../actual) &&
    wc -l <actual >count &&
    echo 3 >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'rev-parse --show-toplevel' '
    (cd repo && grit rev-parse --show-toplevel >../actual) &&
    (cd repo && git rev-parse --show-toplevel >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir' '
    (cd repo && grit rev-parse --git-dir >../actual) &&
    echo ".git" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree' '
    (cd repo && grit rev-parse --is-inside-work-tree >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-git-dir from worktree' '
    (cd repo && grit rev-parse --is-inside-git-dir >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-bare-repository on non-bare' '
    (cd repo && grit rev-parse --is-bare-repository >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from repo root is empty' '
    (cd repo && grit rev-parse --show-prefix >../actual) &&
    (cd repo && git rev-parse --show-prefix >../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --show-prefix from subdir' '
    (cd repo && mkdir -p sub/dir && cd sub/dir &&
    grit rev-parse --show-prefix >../../../actual) &&
    echo "sub/dir/" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --git-dir from subdir' '
    (cd repo/sub/dir && grit rev-parse --git-dir >../../../actual) &&
    (cd repo/sub/dir && git rev-parse --git-dir >../../../expect) &&
    test_cmp expect actual
'

test_expect_success 'rev-parse --is-inside-work-tree from subdir' '
    (cd repo/sub/dir && grit rev-parse --is-inside-work-tree >../../../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'rev-parse tag resolves differently from branch' '
    (cd repo && grit rev-parse v1.0 >../tag_hash &&
    grit rev-parse main >../branch_hash) &&
    ! test_cmp tag_hash branch_hash
'

test_expect_success 'rev-parse HEAD^{tree} differs from HEAD^{commit}' '
    (cd repo && grit rev-parse "HEAD^{tree}" >../tree_hash &&
    grit rev-parse "HEAD^{commit}" >../commit_hash) &&
    ! test_cmp tree_hash commit_hash
'

test_done
