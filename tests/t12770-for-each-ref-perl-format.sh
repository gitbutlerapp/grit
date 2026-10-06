#!/bin/sh

test_description='for-each-ref format strings: complex patterns, sorting, counting, and filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with branches and tags' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "initial" >file.txt &&
     grit add file.txt &&
     grit commit -m "first commit" &&
     grit branch alpha &&
     grit branch beta &&
     grit branch gamma &&
     echo "second" >>file.txt &&
     grit add file.txt &&
     grit commit -m "second commit" &&
     grit tag v1.0 &&
     grit tag -a v2.0 -m "release two" &&
     grit branch delta &&
     grit branch feature/login &&
     grit branch feature/signup
    )
'

test_expect_success 'default format shows hash type refname' '
    (cd repo &&
     grit for-each-ref refs/heads/alpha >../actual) &&
    grep "commit" actual &&
    grep "refs/heads/alpha" actual
'

test_expect_success 'format with refname only' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/heads/alpha >../actual) &&
    echo "refs/heads/alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with refname:short' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/beta >../actual) &&
    echo "beta" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with objectname gives full SHA' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
    len=$(wc -c <actual) &&
    test "$len" -ge 40
'

test_expect_success 'format with objecttype for branch is commit' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/heads/alpha >../actual) &&
    echo "commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with subject' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
    echo "second commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with leading literal text' '
    (cd repo &&
     grit for-each-ref --format="ref:%(refname:short)" refs/heads/alpha >../actual) &&
    echo "ref:alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with trailing literal text' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short):end" refs/heads/alpha >../actual) &&
    echo "alpha:end" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with surrounding brackets' '
    (cd repo &&
     grit for-each-ref --format="[%(refname:short)]" refs/heads/beta >../actual) &&
    echo "[beta]" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with two atoms separated by space' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype) %(refname:short)" refs/heads/alpha >../actual) &&
    echo "commit alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with tab separator between atoms' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)	%(refname:short)" refs/heads/alpha >../actual) &&
    printf "commit\talpha\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with three atoms' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype) %(refname:short) %(subject)" refs/heads/main >../actual) &&
    echo "commit main second commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with repeated atom' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)-%(refname:short)" refs/heads/alpha >../actual) &&
    echo "alpha-alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with objectname and refname' '
    (cd repo &&
     hash=$(grit rev-parse alpha) &&
     grit for-each-ref --format="%(objectname) %(refname)" refs/heads/alpha >../actual) &&
    (cd repo && echo "$(grit rev-parse alpha) refs/heads/alpha" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format with just literal text (no atoms)' '
    (cd repo &&
     grit for-each-ref --format="hello" refs/heads/alpha >../actual) &&
    echo "hello" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with empty string produces empty lines' '
    (cd repo &&
     grit for-each-ref --format="" refs/heads/alpha >../actual) &&
    echo "" >expect &&
    test_cmp expect actual
'

test_expect_success 'sort by refname ascending' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --sort=refname refs/heads/ >../actual) &&
    head -1 actual >first &&
    echo "alpha" >expect &&
    test_cmp expect first
'

test_expect_success 'sort by refname descending' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --sort=-refname refs/heads/ >../actual) &&
    head -1 actual >first &&
    tail -1 actual >last &&
    echo "alpha" >expect_last &&
    test_cmp expect_last last
'

test_expect_success 'count limits number of results' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --count=2 refs/heads/ >../actual) &&
    wc -l <actual >count &&
    echo "2" >expect &&
    test_cmp expect count
'

test_expect_success 'count=1 returns single result' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --count=1 refs/heads/ >../actual) &&
    wc -l <actual >count &&
    echo "1" >expect &&
    test_cmp expect count
'

test_expect_success 'count larger than total returns all' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --count=100 refs/heads/ >../actual &&
     grit for-each-ref --format="%(refname:short)" refs/heads/ >../all) &&
    test_cmp all actual
'

test_expect_success 'filter by refs/tags pattern' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/tags/ >../actual) &&
    grep "v1.0" actual &&
    grep "v2.0" actual
'

test_expect_success 'annotated tag has objecttype tag' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v2.0 >../actual) &&
    echo "tag" >expect &&
    test_cmp expect actual
'

test_expect_success 'lightweight tag has objecttype commit' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >../actual) &&
    echo "commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'subject of annotated tag' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/tags/v2.0 >../actual) &&
    echo "release two" >expect &&
    test_cmp expect actual
'

test_expect_success 'filter by feature/ prefix' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/feature/ >../actual) &&
    grep "feature/login" actual &&
    grep "feature/signup" actual &&
    wc -l <actual >count &&
    echo "2" >expect_cnt &&
    test_cmp expect_cnt count
'

test_expect_success 'no results for non-existent pattern' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/nonexistent/ >../actual) &&
    test ! -s actual
'

test_expect_success 'sort with count combined' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" --sort=-refname --count=3 refs/heads/ >../actual) &&
    wc -l <actual >count &&
    echo "3" >expect &&
    test_cmp expect count
'

test_expect_success 'format with pipes as separator' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)|%(refname:short)" refs/heads/gamma >../actual) &&
    echo "commit|gamma" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with colons as separator' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype):%(refname:short):%(subject)" refs/heads/main >../actual) &&
    echo "commit:main:second commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'all branches listed when using refs/heads/' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)" refs/heads/ >../actual) &&
    grep "alpha" actual &&
    grep "beta" actual &&
    grep "gamma" actual &&
    grep "delta" actual &&
    grep "main" actual &&
    grep "feature/login" actual &&
    grep "feature/signup" actual
'

test_expect_success 'format preserves whitespace around atoms' '
    (cd repo &&
     grit for-each-ref --format="  %(refname:short)  " refs/heads/alpha >../actual) &&
    echo "  alpha  " >expect &&
    test_cmp expect actual
'

test_expect_success 'format with multiple literals between atoms' '
    (cd repo &&
     grit for-each-ref --format="type=%(objecttype) name=%(refname:short)" refs/heads/delta >../actual) &&
    echo "type=commit name=delta" >expect &&
    test_cmp expect actual
'

test_expect_success 'sort by objectname' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" --sort=objectname refs/heads/ >../actual) &&
    sort actual >sorted &&
    test_cmp sorted actual
'

test_expect_success 'refs/tags and refs/heads together in refs/' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/ >../actual) &&
    grep "refs/heads/" actual &&
    grep "refs/tags/" actual
'

test_done
