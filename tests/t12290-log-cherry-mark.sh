#!/bin/sh
test_description='cherry: find commits not yet applied upstream'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with diverged branches' '
    (
    grit init repo && cd repo &&
    git config user.email "t@t.com" &&
    git config user.name "T" &&
    echo base >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "base" &&
    git checkout -b topic &&
    echo topic1 >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "topic change 1" &&
    echo topic2 >other.txt &&
    grit add other.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "add other file" &&
    echo topic3 >third.txt &&
    grit add third.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "add third file" &&
    git checkout main &&
    echo topic1 >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "cherry-picked topic change 1" &&
    echo main-unique >main-only.txt &&
    grit add main-only.txt &&
    GIT_AUTHOR_DATE="1700005000 +0000" GIT_COMMITTER_DATE="1700005000 +0000" \
    grit commit -m "main unique"
    )
'

test_expect_success 'cherry lists topic commits vs main' '
    (cd repo && grit cherry main topic >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'cherry marks equivalent commit with minus' '
    (cd repo && grit cherry main topic >../actual) &&
    grep "^-" actual >minus_lines &&
    test_line_count = 1 minus_lines
'

test_expect_success 'cherry marks unique commits with plus' '
    (cd repo && grit cherry main topic >../actual) &&
    grep "^+" actual >plus_lines &&
    test_line_count = 2 plus_lines
'

test_expect_success 'cherry output has correct hash format' '
    (cd repo && grit cherry main topic >../actual) &&
    while read sign hash rest; do
        echo "$hash" | grep -q "^[0-9a-f]\{7,40\}$" || exit 1
    done <actual
'

test_expect_success 'cherry -v shows subject line' '
    (cd repo && grit cherry -v main topic >../actual) &&
    grep "topic change 1" actual &&
    grep "add other file" actual &&
    grep "add third file" actual
'

test_expect_success 'cherry -v equivalent has minus and subject' '
    (cd repo && grit cherry -v main topic >../actual) &&
    grep "^- " actual | grep "topic change 1"
'

test_expect_success 'cherry -v unique has plus and subject' '
    (cd repo && grit cherry -v main topic >../actual) &&
    grep "^+ .*add other file" actual &&
    grep "^+ .*add third file" actual
'

test_expect_success 'cherry with reversed args' '
    (cd repo && grit cherry topic main >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'cherry reversed: cherry-picked commit is minus' '
    (cd repo && grit cherry topic main >../actual) &&
    grep "^-" actual >minus &&
    test_line_count = 1 minus
'

test_expect_success 'cherry reversed: unique main commit is plus' '
    (cd repo && grit cherry topic main >../actual) &&
    grep "^+" actual >plus &&
    test_line_count = 1 plus
'

test_expect_success 'setup: identical branches have no unique commits' '
    (cd repo && git checkout -b identical main) &&
    true
'

test_expect_success 'cherry on identical branches produces all minus' '
    (cd repo && grit cherry main identical >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'setup: add commits only on topic2' '
    (cd repo && git checkout -b topic2 main &&
    echo new1 >new1.txt &&
    grit add new1.txt &&
    GIT_AUTHOR_DATE="1700006000 +0000" GIT_COMMITTER_DATE="1700006000 +0000" \
    grit commit -m "topic2 commit 1" &&
    echo new2 >new2.txt &&
    grit add new2.txt &&
    GIT_AUTHOR_DATE="1700007000 +0000" GIT_COMMITTER_DATE="1700007000 +0000" \
    grit commit -m "topic2 commit 2")
'

test_expect_success 'cherry: all unique when no cherry-picks done' '
    (cd repo && grit cherry main topic2 >../actual) &&
    grep "^+" actual >plus &&
    test_line_count = 2 plus
'

test_expect_success 'cherry: no minus when no cherry-picks done' '
    (cd repo && grit cherry main topic2 >../actual) &&
    grep "^-" actual >minus || true &&
    test_line_count = 0 minus
'

test_expect_success 'cherry -v on topic2 shows subjects' '
    (cd repo && grit cherry -v main topic2 >../actual) &&
    grep "topic2 commit 1" actual &&
    grep "topic2 commit 2" actual
'

test_expect_success 'setup: cherry-pick one topic2 commit to main' '
    (cd repo && git checkout main &&
    PICK=$(/usr/bin/git log --format="%H" -1 topic2~1) &&
    grit cherry-pick "$PICK")
'

test_expect_success 'cherry: one minus after cherry-pick' '
    (cd repo && grit cherry main topic2 >../actual) &&
    grep "^-" actual >minus &&
    test_line_count = 1 minus
'

test_expect_success 'cherry: one plus remains after cherry-pick' '
    (cd repo && grit cherry main topic2 >../actual) &&
    grep "^+" actual >plus &&
    test_line_count = 1 plus
'

test_expect_success 'cherry -v after cherry-pick shows correct subjects' '
    (cd repo && grit cherry -v main topic2 >../actual) &&
    grep "^- .*topic2 commit 1" actual &&
    grep "^+ .*topic2 commit 2" actual
'

test_expect_success 'cherry with explicit HEAD argument' '
    (cd repo && git checkout topic &&
    grit cherry main HEAD >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'cherry without HEAD defaults to current branch' '
    (cd repo && git checkout topic &&
    grit cherry main >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'cherry with HEAD matches cherry without HEAD' '
    (cd repo && git checkout topic &&
    grit cherry main >../without_head &&
    grit cherry main HEAD >../with_head) &&
    test_cmp without_head with_head
'

test_expect_success 'setup: branch with single commit' '
    (cd repo && git checkout main &&
    git checkout -b single &&
    echo single >single.txt &&
    grit add single.txt &&
    GIT_AUTHOR_DATE="1700008000 +0000" GIT_COMMITTER_DATE="1700008000 +0000" \
    grit commit -m "single commit")
'

test_expect_success 'cherry on single-commit branch' '
    (cd repo && grit cherry main single >../actual) &&
    test_line_count = 1 actual &&
    grep "^+" actual
'

test_expect_success 'cherry -v on single-commit branch' '
    (cd repo && grit cherry -v main single >../actual) &&
    test_line_count = 1 actual &&
    grep "^+ .*single commit" actual
'

test_expect_success 'cherry output lines match commit count on topic' '
    (cd repo && grit cherry main topic >../cherry_out) &&
    CHERRY_COUNT=$(wc -l <cherry_out) &&
    (cd repo && grit rev-list topic ^main >../rev_out) &&
    REV_COUNT=$(wc -l <rev_out) &&
    test "$CHERRY_COUNT" = "$REV_COUNT"
'

test_expect_success 'cherry hashes match rev-list hashes' '
    (cd repo && grit cherry main topic2 >../cherry_out) &&
    (cd repo && grit rev-list topic2 ^main >../rev_out) &&
    awk "{print \$2}" cherry_out | sort >cherry_sorted &&
    sort rev_out >rev_sorted &&
    # abbreviated vs full: check prefix match
    while read abbrev <&3 && read full <&4; do
        case "$full" in
        "$abbrev"*) : ok ;;
        *) exit 1 ;;
        esac
    done 3<cherry_sorted 4<rev_sorted
'

test_expect_success 'cherry plus/minus counts add up to total' '
    (cd repo && grit cherry main topic >../actual) &&
    TOTAL=$(wc -l <actual) &&
    PLUS=$(grep -c "^+" actual) &&
    MINUS=$(grep -c "^-" actual) &&
    test "$((PLUS + MINUS))" = "$TOTAL"
'

test_expect_success 'cherry -v output has three fields per line' '
    (cd repo && grit cherry -v main topic >../actual) &&
    while IFS= read -r line; do
        set -- $line &&
        test "$#" -ge 3 || exit 1
    done <actual
'

test_expect_success 'cherry sign is only + or -' '
    (cd repo && grit cherry main topic >../actual) &&
    while read sign rest; do
        case "$sign" in
        +|-) : ok ;;
        *) exit 1 ;;
        esac
    done <actual
'

test_expect_success 'cherry -v sign is only + or -' '
    (cd repo && grit cherry -v main topic >../actual) &&
    while read sign rest; do
        case "$sign" in
        +|-) : ok ;;
        *) exit 1 ;;
        esac
    done <actual
'

test_done
