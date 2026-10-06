#!/bin/sh
test_description='log --format shortlog-style placeholders and combinations'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (
    cd repo &&
    git config user.email "alice@example.com" &&
    git config user.name "Alice Smith" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
    echo one >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700000000 +0000" GIT_COMMITTER_DATE="1700000000 +0000" \
    grit commit -m "first commit" &&
    echo two >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
    grit commit -m "second commit" &&
    echo three >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
    grit commit -m "third commit" &&
    git config user.email "bob@example.com" &&
    git config user.name "Bob Jones" &&
    echo four >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700003000 +0000" GIT_COMMITTER_DATE="1700003000 +0000" \
    grit commit -m "fourth commit" &&
    echo five >file.txt &&
    grit add file.txt &&
    GIT_AUTHOR_DATE="1700004000 +0000" GIT_COMMITTER_DATE="1700004000 +0000" \
    grit commit -m "fifth commit"
    )
'

test_expect_success 'format %H shows full hash' '
    (cd repo && grit log --format="%H" >../actual) &&
    while read hash; do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done <actual &&
    test_line_count = 5 actual
'

test_expect_success 'format %h shows abbreviated hash' '
    (cd repo && grit log --format="%h" >../actual) &&
    while read hash; do
        len=$(echo "$hash" | wc -c) &&
        test "$len" -le 15 || exit 1
    done <actual &&
    test_line_count = 5 actual
'

test_expect_success 'format %s shows subject line' '
    (cd repo && grit log --format="%s" >../actual) &&
    head -1 actual >first_line &&
    echo "fifth commit" >expect &&
    test_cmp expect first_line
'

test_expect_success 'format %s lists all subjects newest first' '
    (cd repo && grit log --format="%s" >../actual) &&
    cat >expect <<-\EOF &&
	fifth commit
	fourth commit
	third commit
	second commit
	first commit
	EOF
    test_cmp expect actual
'

test_expect_success 'format %an shows author name' '
    (cd repo && grit log --format="%an" >../actual) &&
    head -1 actual >first &&
    echo "Bob Jones" >expect &&
    test_cmp expect first
'

test_expect_success 'format %ae shows author email' '
    (cd repo && grit log --format="%ae" >../actual) &&
    head -1 actual >first &&
    echo "bob@example.com" >expect &&
    test_cmp expect first
'

test_expect_success 'format %cn shows committer name' '
    (cd repo && grit log --format="%cn" >../actual) &&
    head -1 actual >first &&
    echo "Bob Jones" >expect &&
    test_cmp expect first
'

test_expect_success 'format %ce shows committer email' '
    (cd repo && grit log --format="%ce" >../actual) &&
    head -1 actual >first &&
    echo "bob@example.com" >expect &&
    test_cmp expect first
'

test_expect_success 'format %T shows tree hash (full)' '
    (cd repo && grit log --format="%T" >../actual) &&
    while read hash; do
        echo "$hash" | grep -q "^[0-9a-f]\{40\}$" || exit 1
    done <actual
'

test_expect_success 'format %t shows tree hash (abbrev)' '
    (cd repo && grit log --format="%t" >../actual) &&
    while read hash; do
        len=$(echo "$hash" | wc -c) &&
        test "$len" -le 15 || exit 1
    done <actual
'

test_expect_success 'format %P shows parent hashes' '
    (cd repo && grit log --format="%P" >../actual) &&
    LAST=$(tail -1 actual) &&
    # root commit has no parent, so field is empty string
    test -z "$LAST"
'

test_expect_success 'format %p shows abbreviated parent hashes' '
    (cd repo && grit log --format="%p" >../actual) &&
    LAST=$(tail -1 actual) &&
    test -z "$LAST"
'

test_expect_success 'format %h %s combined output' '
    (cd repo && grit log --format="%h %s" >../actual) &&
    head -1 actual | grep -q "[0-9a-f]\{7\} fifth commit" || exit 1
'

test_expect_success 'format %h %s (%an) combined' '
    (cd repo && grit log --format="%h %s (%an)" >../actual) &&
    head -1 actual | grep -q "(Bob Jones)$" || exit 1
'

test_expect_success 'format %H matches rev-parse HEAD for first commit' '
    (cd repo && grit log -n 1 --format="%H" >../actual_hash) &&
    (cd repo && grit rev-parse HEAD >../expected_hash) &&
    test_cmp expected_hash actual_hash
'

test_expect_success 'format %n inserts newline' '
    (cd repo && grit log -n 1 --format="A%nB" >../actual) &&
    test_line_count = 2 actual &&
    head -1 actual >first &&
    echo "A" >expect &&
    test_cmp expect first
'

test_expect_success 'format with literal text' '
    (cd repo && grit log -n 1 --format="commit: %h" >../actual) &&
    grep -q "^commit: [0-9a-f]" actual
'

test_expect_success '--oneline output format' '
    (cd repo && grit log --oneline >../actual) &&
    test_line_count = 5 actual &&
    head -1 actual | grep -q "fifth commit$"
'

test_expect_success '--oneline shows abbreviated hash' '
    (cd repo && grit log --oneline >../actual) &&
    HEAD_ABBREV=$(cd repo && grit log --format="%h" -n 1) &&
    head -1 actual | grep -q "^$HEAD_ABBREV "
'

test_expect_success 'format with -n 1 limits output' '
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'format with -n 3 shows exactly 3' '
    (cd repo && grit log -n 3 --format="%s" >../actual) &&
    test_line_count = 3 actual
'

test_expect_success 'format with --skip=2' '
    (cd repo && grit log --skip=2 --format="%s" >../actual) &&
    test_line_count = 3 actual &&
    head -1 actual >first &&
    echo "third commit" >expect &&
    test_cmp expect first
'

test_expect_success 'format with --skip and -n combined' '
    (cd repo && grit log --skip=1 -n 2 --format="%s" >../actual) &&
    test_line_count = 2 actual &&
    head -1 actual >first &&
    echo "fourth commit" >expect &&
    test_cmp expect first
'

test_expect_success 'format with --reverse' '
    (cd repo && grit log --reverse --format="%s" >../actual) &&
    head -1 actual >first &&
    echo "first commit" >expect &&
    test_cmp expect first
'

test_expect_success 'format %an shows different authors' '
    (cd repo && grit log --format="%an" >../actual) &&
    grep -c "Alice Smith" actual >alice_count &&
    grep -c "Bob Jones" actual >bob_count &&
    test "$(cat alice_count)" = "3" &&
    test "$(cat bob_count)" = "2"
'

test_expect_success 'format %ae shows different emails' '
    (cd repo && grit log --format="%ae" >../actual) &&
    grep -c "alice@example.com" actual >alice_count &&
    grep -c "bob@example.com" actual >bob_count &&
    test "$(cat alice_count)" = "3" &&
    test "$(cat bob_count)" = "2"
'

test_expect_success 'format %H differs per commit' '
    (cd repo && grit log --format="%H" >../actual) &&
    sort actual >sorted &&
    sort -u actual >unique &&
    test_cmp sorted unique
'

test_expect_success 'format %T differs for commits with different trees' '
    (cd repo && grit log --format="%T" >../actual) &&
    sort -u actual >unique &&
    test_line_count = 5 unique
'

test_expect_success 'format %P non-root has parent' '
    (cd repo && grit log -n 1 --format="%P" >../actual) &&
    PARENT=$(cat actual) &&
    echo "$PARENT" | grep -q "^[0-9a-f]\{40\}$"
'

test_expect_success '--oneline with --reverse' '
    (cd repo && grit log --oneline --reverse >../actual) &&
    head -1 actual | grep -q "first commit$" &&
    tail -1 actual | grep -q "fifth commit$"
'

test_expect_success '--oneline with -n 2' '
    (cd repo && grit log --oneline -n 2 >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'format on specific revision' '
    (cd repo && FIRST=$(grit rev-list --reverse HEAD | head -1) &&
     grit log -n 1 --format="%s" "$FIRST" >../actual) &&
    echo "first commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with -n and --skip simulates range' '
    (cd repo && grit log -n 2 --format="%s" >../actual) &&
    test_line_count = 2 actual &&
    head -1 actual >first &&
    echo "fifth commit" >expect &&
    test_cmp expect first &&
    tail -1 actual >second &&
    echo "fourth commit" >expect &&
    test_cmp expect second
'

test_expect_success 'format %h and %H are consistent' '
    (cd repo && grit log -n 1 --format="%h" >../abbrev) &&
    (cd repo && grit log -n 1 --format="%H" >../full) &&
    ABBREV=$(cat abbrev) &&
    FULL=$(cat full) &&
    case "$FULL" in
    "$ABBREV"*) : ok ;;
    *) exit 1 ;;
    esac
'

test_expect_success 'format %t and %T are consistent' '
    (cd repo && grit log -n 1 --format="%t" >../abbrev) &&
    (cd repo && grit log -n 1 --format="%T" >../full) &&
    ABBREV=$(cat abbrev) &&
    FULL=$(cat full) &&
    case "$FULL" in
    "$ABBREV"*) : ok ;;
    *) exit 1 ;;
    esac
'

test_done
