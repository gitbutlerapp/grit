#!/bin/sh

test_description='for-each-ref count, sort, format, and pattern filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with branches and tags' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "initial" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch alpha &&
     grit branch beta &&
     grit branch gamma &&
     grit branch delta &&
     grit tag v1.0 &&
     grit tag v2.0 &&
     grit tag v3.0
    )
'

test_expect_success 'for-each-ref lists all refs' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" >../actual) &&
    cat actual | wc -l >count &&
    test "$(cat count)" -ge 7
'

test_expect_success 'for-each-ref with --count=1 returns one ref' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(refname)" >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'for-each-ref with --count=3 returns three refs' '
    (cd repo &&
     grit for-each-ref --count=3 --format="%(refname)" >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'for-each-ref with --count=0 returns nothing' '
    (cd repo &&
     grit for-each-ref --count=0 --format="%(refname)" >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'for-each-ref count larger than total returns all' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" >../all &&
     grit for-each-ref --count=100 --format="%(refname)" >../actual) &&
    test_cmp all actual
'

test_expect_success 'for-each-ref pattern filters to branches' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/heads/ >../actual) &&
    grep "refs/heads/" actual &&
    ! grep "refs/tags/" actual
'

test_expect_success 'for-each-ref pattern filters to tags' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/tags/ >../actual) &&
    grep "refs/tags/" actual &&
    ! grep "refs/heads/" actual
'

test_expect_success 'for-each-ref --sort=refname sorts alphabetically' '
    (cd repo &&
     grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >../actual) &&
    sort actual >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref --sort=-refname sorts reverse' '
    (cd repo &&
     grit for-each-ref --sort=-refname --format="%(refname:short)" refs/heads/ >../actual) &&
    sort -r actual >expect_rev &&
    test_cmp expect_rev actual
'

test_expect_success 'for-each-ref format %(objectname) shows full hash' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(objectname)" refs/heads/ >../actual) &&
    len=$(wc -c <actual | tr -d " ") &&
    test "$len" -ge 40
'

test_expect_success 'for-each-ref format %(objectname) is 40 chars' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(objectname)" refs/heads/ >../actual) &&
    len=$(awk "{ print length }" <actual | head -1) &&
    test "$len" -eq 40
'

test_expect_success 'for-each-ref format %(refname:short) strips prefix' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/alpha >../actual) &&
    echo "alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref format %(objecttype) shows commit' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(objecttype)" refs/heads/ >../actual) &&
    echo "commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref --count with --sort limits after sort' '
    (cd repo &&
     grit for-each-ref --count=2 --sort=refname --format="%(refname:short)" refs/heads/ >../actual) &&
    test_line_count = 2 actual &&
    head -1 actual >first &&
    echo "alpha" >expect &&
    test_cmp expect first
'

test_expect_success 'for-each-ref --count with --sort=-refname gets last entries' '
    (cd repo &&
     grit for-each-ref --count=1 --sort=-refname --format="%(refname:short)" refs/heads/ >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'setup additional refs for more tests' '
    (cd repo &&
     echo "v2" >file.txt &&
     grit add file.txt &&
     grit commit -m "second" &&
     grit tag -a v4.0 -m "annotated tag v4" &&
     grit branch feature/one &&
     grit branch feature/two &&
     grit branch release/1.0
    )
'

test_expect_success 'for-each-ref pattern with glob matches subset' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/feature/ >../actual) &&
    test_line_count = 2 actual &&
    grep "feature/one" actual &&
    grep "feature/two" actual
'

test_expect_success 'for-each-ref with multiple patterns' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/heads/alpha refs/heads/beta >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'for-each-ref format %(subject) on branch shows message' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
    test -s actual
'

test_expect_success 'for-each-ref format with multiple atoms' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype) %(refname:short)" refs/heads/alpha >../actual) &&
    echo "commit alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref format %(subject) shows commit message' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
    echo "second" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref on annotated tag shows tag type' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v4.0 >../actual) &&
    echo "tag" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref on lightweight tag shows commit type' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >../actual) &&
    echo "commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref objectname matches rev-parse' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/heads/main >../actual &&
     grit rev-parse main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref --count=2 on tags' '
    (cd repo &&
     grit for-each-ref --count=2 --format="%(refname:short)" refs/tags/ >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'for-each-ref empty pattern matches all' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" >../actual) &&
    grep "refs/heads/" actual &&
    grep "refs/tags/" actual
'

test_expect_success 'for-each-ref pattern that matches nothing returns empty' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/nonexistent/ >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'for-each-ref --count=1 with pattern that matches nothing' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(refname)" refs/nonexistent/ >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'for-each-ref format with literal text' '
    (cd repo &&
     grit for-each-ref --format="ref:%(refname:short)" refs/heads/alpha >../actual) &&
    echo "ref:alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref sorted output is deterministic' '
    (cd repo &&
     grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >../actual1 &&
     grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >../actual2) &&
    test_cmp actual1 actual2
'

test_expect_success 'for-each-ref release branch via pattern' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/release/ >../actual) &&
    echo "release/1.0" >expect &&
    test_cmp expect actual
'

test_expect_success 'for-each-ref --count combined with pattern limits correctly' '
    (cd repo &&
     grit for-each-ref --count=1 --format="%(refname:short)" refs/heads/feature/ >../actual) &&
    test_line_count = 1 actual
'

test_done
