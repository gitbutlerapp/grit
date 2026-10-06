#!/bin/sh
test_description='log --format date placeholders (%ad, %cd, %ai, %ci)'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with known dates' '
	(
    grit init repo &&
    cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
    echo hello >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "first"
	)
'

test_expect_success 'format %ad shows author date' '
    (cd repo && grit log -n1 --format="%ad" >../actual) &&
    echo "Tue Nov 14 22:13:20 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %cd shows committer date' '
    (cd repo && grit log -n1 --format="%cd" >../actual) &&
    echo "Tue Nov 14 22:13:20 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup second commit with different dates' '
    (cd repo &&
     echo world >file2.txt &&
     grit add file2.txt &&
     GIT_AUTHOR_DATE="1700100000 +0000" GIT_COMMITTER_DATE="1700200000 +0000" \
     grit commit -m "second")
'

test_expect_success 'author and committer dates differ when set differently' '
    (cd repo && grit log -n1 --format="%ad" >../actual_ad) &&
    (cd repo && grit log -n1 --format="%cd" >../actual_cd) &&
    ! test_cmp actual_ad actual_cd
'

test_expect_success 'format %ad for second commit' '
    (cd repo && grit log -n1 --format="%ad" >../actual) &&
    echo "Thu Nov 16 02:00:00 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %cd for second commit' '
    (cd repo && grit log -n1 --format="%cd" >../actual) &&
    echo "Fri Nov 17 05:46:40 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup third commit with timezone offset' '
    (cd repo &&
     sane_unset GIT_AUTHOR_NAME &&
     sane_unset GIT_AUTHOR_EMAIL &&
     sane_unset GIT_COMMITTER_NAME &&
     sane_unset GIT_COMMITTER_EMAIL &&
     echo foo >file3.txt &&
     grit add file3.txt &&
     GIT_AUTHOR_DATE="1700300000 +0500" GIT_COMMITTER_DATE="1700300000 -0300" \
     grit commit -m "third")
'

test_expect_success 'format %ad respects author timezone' '
    (cd repo && grit log -n1 --format="%ad" >../actual) &&
    grep "+0500" actual
'

test_expect_success 'format %cd respects committer timezone' '
    (cd repo && grit log -n1 --format="%cd" >../actual) &&
    grep "\-0300" actual
'

test_expect_success 'format %ad over multiple commits shows all dates' '
    (cd repo && grit log --format="%ad" >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'format %cd over multiple commits shows all dates' '
    (cd repo && grit log --format="%cd" >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'format %ad with --reverse shows oldest first' '
    (cd repo && grit log --reverse --format="%ad" >../actual) &&
    head -1 actual >first_date &&
    echo "Tue Nov 14 22:13:20 2023 +0000" >expect &&
    test_cmp expect first_date
'

test_expect_success 'combined format: date and author' '
    (cd repo && grit log -n1 --format="%an %ad" >../actual) &&
    grep "^T " actual &&
    grep "+0500" actual
'

test_expect_success 'combined format: date and subject' '
    (cd repo && grit log -n1 --format="%ad %s" >../actual) &&
    grep "third$" actual
'

test_expect_success 'format %ad with -n2 limits output' '
    (cd repo && grit log -n2 --format="%ad" >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'format %ad with --skip=1' '
    (cd repo && grit log --skip=1 --format="%ad" >../actual) &&
    test_line_count = 2 actual &&
    head -1 actual >first &&
    echo "Thu Nov 16 02:00:00 2023 +0000" >expect &&
    test_cmp expect first
'

test_expect_success 'setup commits with different author/committer for date tests' '
    (cd repo &&
     echo bar >file4.txt &&
     grit add file4.txt &&
     GIT_AUTHOR_NAME="Alice" GIT_AUTHOR_EMAIL="alice@example.com" \
     GIT_COMMITTER_NAME="Bob" GIT_COMMITTER_EMAIL="bob@example.com" \
     GIT_AUTHOR_DATE="1700400000 +0000" GIT_COMMITTER_DATE="1700500000 +0000" \
     grit commit -m "fourth")
'

test_expect_success 'format %ad shows author date for fourth commit' '
    (cd repo && grit log -n1 --format="%ad" >../actual) &&
    echo "Sun Nov 19 13:20:00 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %cd shows committer date for fourth commit' '
    (cd repo && grit log -n1 --format="%cd" >../actual) &&
    echo "Mon Nov 20 17:06:40 2023 +0000" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %ai shows ISO 8601 author date' '
    (cd repo && grit log -n1 --format="%ai" >../actual) &&
    grep "2023-11-19" actual &&
    grep "+0000" actual
'

test_expect_success 'format %ci shows ISO 8601 committer date' '
    (cd repo && grit log -n1 --format="%ci" >../actual) &&
    grep "2023-11-20" actual &&
    grep "+0000" actual
'

test_expect_success 'format %ai does not include author name' '
    (cd repo && grit log -n1 --format="%ai" >../actual) &&
    ! grep "Alice" actual
'

test_expect_success 'format %ci does not include committer name' '
    (cd repo && grit log -n1 --format="%ci" >../actual) &&
    ! grep "Bob" actual
'

test_expect_success 'dates in oneline mode are not shown' '
    (cd repo && grit log --oneline -n1 >../actual) &&
    ! grep "2023" actual
'

test_expect_success 'format dates with multiple placeholders' '
    (cd repo && grit log -n1 --format="%ad|%cd" >../actual) &&
    grep "|" actual &&
    AD=$(cut -d"|" -f1 <actual) &&
    CD=$(cut -d"|" -f2 <actual) &&
    test "$AD" != "$CD"
'

test_expect_success 'setup commit with negative timezone' '
    (cd repo &&
     echo baz >file5.txt &&
     grit add file5.txt &&
     GIT_AUTHOR_DATE="1700600000 -0800" GIT_COMMITTER_DATE="1700600000 -0800" \
     grit commit -m "fifth")
'

test_expect_success 'format %ad with negative timezone' '
    (cd repo && grit log -n1 --format="%ad" >../actual) &&
    grep "\-0800" actual
'

test_expect_success 'format %cd with negative timezone' '
    (cd repo && grit log -n1 --format="%cd" >../actual) &&
    grep "\-0800" actual
'

test_expect_success 'all five commits have dates' '
    (cd repo && grit log --format="%ad" >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'dates are in reverse chronological order by default' '
    (cd repo && grit log --format="%ad" >../actual) &&
    head -1 actual >newest &&
    tail -1 actual >oldest &&
    grep "\-0800" newest &&
    grep "+0000" oldest
'

test_expect_success 'format %ad and %an together for all commits' '
    (cd repo && grit log --format="%an: %ad" >../actual) &&
    test_line_count = 5 actual
'

test_expect_success 'format date with newline separator' '
    (cd repo && grit log -n1 --format="%ad%n%cd" >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'format date with percent literal' '
    (cd repo && grit log -n1 --format="%ad %%" >../actual) &&
    grep "%" actual
'

test_done
