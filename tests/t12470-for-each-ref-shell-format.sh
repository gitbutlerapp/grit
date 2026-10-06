#!/bin/sh

test_description='for-each-ref format strings with literals, multi-atom combos, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository with diverse refs' '
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
     echo "second" >file.txt &&
     grit add file.txt &&
     grit commit -m "second commit" &&
     grit tag v1.0 &&
     grit tag -a v2.0 -m "release two" &&
     grit branch delta &&
     grit branch feature/login &&
     grit branch feature/signup
    )
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

test_expect_success 'format with surrounding literal text' '
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

test_expect_success 'format with two atoms separated by tab' '
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
     grit rev-parse alpha >../expect_hash &&
     grit for-each-ref --format="%(objectname) %(refname:short)" refs/heads/alpha >../actual) &&
    hash=$(cat expect_hash) &&
    echo "$hash alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format objectname is 40 hex chars' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
    len=$(awk "{ print length }" <actual | head -1) &&
    test "$len" -eq 40
'

test_expect_success 'format objectname consists of hex chars only' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/heads/main >../actual) &&
    grep -E "^[0-9a-f]{40}$" actual
'

test_expect_success 'format on tag shows commit type for lightweight' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v1.0 >../actual) &&
    echo "commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format on annotated tag shows tag type' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/tags/v2.0 >../actual) &&
    echo "tag" >expect &&
    test_cmp expect actual
'

test_expect_success 'format refname full vs short for branch' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/heads/alpha >../full &&
     grit for-each-ref --format="%(refname:short)" refs/heads/alpha >../short) &&
    echo "refs/heads/alpha" >expect_full &&
    echo "alpha" >expect_short &&
    test_cmp expect_full full &&
    test_cmp expect_short short
'

test_expect_success 'format refname full vs short for tag' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/tags/v1.0 >../full &&
     grit for-each-ref --format="%(refname:short)" refs/tags/v1.0 >../short) &&
    echo "refs/tags/v1.0" >expect_full &&
    echo "v1.0" >expect_short &&
    test_cmp expect_full full &&
    test_cmp expect_short short
'

test_expect_success 'format subject shows commit message' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/heads/main >../actual) &&
    echo "second commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format subject on old branch shows initial message' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/heads/alpha >../actual) &&
    echo "first commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format subject on annotated tag shows tag message' '
    (cd repo &&
     grit for-each-ref --format="%(subject)" refs/tags/v2.0 >../actual) &&
    echo "release two" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with complex literal: key=value pairs' '
    (cd repo &&
     grit for-each-ref --format="name=%(refname:short) type=%(objecttype)" refs/heads/alpha >../actual) &&
    echo "name=alpha type=commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with special chars in literals' '
    (cd repo &&
     grit for-each-ref --format="%(refname:short)|%(objecttype)|%(subject)" refs/heads/main >../actual) &&
    echo "main|commit|second commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format multiple refs sorted by refname' '
    (cd repo &&
     grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/alpha refs/heads/beta >../actual) &&
    printf "alpha\nbeta\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with --count and pattern combined' '
    (cd repo &&
     grit for-each-ref --count=1 --sort=refname --format="%(refname:short)" refs/heads/ >../actual) &&
    echo "alpha" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with reverse sort' '
    (cd repo &&
     grit for-each-ref --count=1 --sort=-refname --format="%(refname:short)" refs/heads/ >../actual) &&
    test -s actual &&
    ! grep "alpha" actual
'

test_expect_success 'format on feature branches with slash in name' '
    (cd repo &&
     grit for-each-ref --sort=refname --format="%(refname:short)" refs/heads/feature/ >../actual) &&
    printf "feature/login\nfeature/signup\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format full refname on feature branch' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/heads/feature/login >../actual) &&
    echo "refs/heads/feature/login" >expect &&
    test_cmp expect actual
'

test_expect_success 'format on nonexistent pattern returns empty' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" refs/nonexistent/ >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'format with all atoms produces multi-column output' '
    (cd repo &&
     grit for-each-ref --format="%(objectname) %(objecttype) %(refname) %(refname:short) %(subject)" refs/heads/alpha >../actual) &&
    hash=$(grit -C repo rev-parse alpha) &&
    echo "$hash commit refs/heads/alpha alpha first commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format output is deterministic on same ref' '
    (cd repo &&
     grit for-each-ref --format="%(objectname) %(refname:short)" refs/heads/main >../run1 &&
     grit for-each-ref --format="%(objectname) %(refname:short)" refs/heads/main >../run2) &&
    test_cmp run1 run2
'

test_expect_success 'format objectname matches rev-parse for branch' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/heads/main >../fer_hash &&
     grit rev-parse main >../rp_hash) &&
    test_cmp fer_hash rp_hash
'

test_expect_success 'format objectname on tag matches rev-parse for lightweight' '
    (cd repo &&
     grit for-each-ref --format="%(objectname)" refs/tags/v1.0 >../fer_hash &&
     grit rev-parse v1.0 >../rp_hash) &&
    test_cmp fer_hash rp_hash
'

test_expect_success 'format all branches have objecttype commit' '
    (cd repo &&
     grit for-each-ref --format="%(objecttype)" refs/heads/ >../actual) &&
    while read line; do
        test "$line" = "commit" || exit 1
    done <actual
'

test_expect_success 'format with empty format string produces empty lines' '
    (cd repo &&
     grit for-each-ref --format="" refs/heads/alpha >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'format with only literal text produces that text' '
    (cd repo &&
     grit for-each-ref --format="hello" refs/heads/alpha >../actual) &&
    echo "hello" >expect &&
    test_cmp expect actual
'

test_expect_success 'format count of all refs' '
    (cd repo &&
     grit for-each-ref --format="%(refname)" >../actual) &&
    br=$(grep -c "refs/heads/" actual) &&
    tg=$(grep -c "refs/tags/" actual) &&
    test "$br" -eq 7 &&
    test "$tg" -eq 2
'

test_done
