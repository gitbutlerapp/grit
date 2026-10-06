#!/bin/sh
test_description='log --format author and committer placeholders'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
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
    grit commit -m "initial"
    )
'

test_expect_success 'format %an shows author name' '
    (cd repo && grit log -n1 --format="%an" >../actual) &&
    echo "T" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %ae shows author email' '
    (cd repo && grit log -n1 --format="%ae" >../actual) &&
    echo "t@t.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %cn shows committer name' '
    (cd repo && grit log -n1 --format="%cn" >../actual) &&
    echo "T" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %ce shows committer email' '
    (cd repo && grit log -n1 --format="%ce" >../actual) &&
    echo "t@t.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %s shows subject' '
    (cd repo && grit log -n1 --format="%s" >../actual) &&
    echo "initial" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %H shows full commit hash (40 hex chars)' '
    (cd repo && grit log -n1 --format="%H" >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'format %h shows abbreviated commit hash' '
    (cd repo && grit log -n1 --format="%h" >../actual) &&
    grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'format %T shows full tree hash' '
    (cd repo && grit log -n1 --format="%T" >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'format %t shows abbreviated tree hash' '
    (cd repo && grit log -n1 --format="%t" >../actual) &&
    grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'format %b has no content for commit with no body' '
    (cd repo && grit log -n1 --format="%b" >../actual) &&
    test $(wc -c <actual) -le 1
'

test_expect_success 'format %P for root commit has no hash content' '
    (cd repo && grit log --skip=0 --format="%P" >../actual) &&
    LAST_LINE=$(tail -1 actual) &&
    test -z "$LAST_LINE" || test "$LAST_LINE" = ""
'

test_expect_success 'format %p for root commit has no hash content' '
    (cd repo && grit log --format="%p" >../actual) &&
    LAST_LINE=$(tail -1 actual) &&
    test -z "$LAST_LINE" || test "$LAST_LINE" = ""
'

test_expect_success 'setup second commit with different author and committer' '
    (cd repo &&
     echo world >file2.txt &&
     grit add file2.txt &&
     GIT_AUTHOR_NAME="Alice" GIT_AUTHOR_EMAIL="alice@example.com" \
     GIT_COMMITTER_NAME="Bob" GIT_COMMITTER_EMAIL="bob@example.com" \
     GIT_AUTHOR_DATE="1700001000 +0000" GIT_COMMITTER_DATE="1700001000 +0000" \
     grit commit -m "second commit")
'

test_expect_success 'different author name' '
    (cd repo && grit log -n1 --format="%an" >../actual) &&
    echo "Alice" >expect &&
    test_cmp expect actual
'

test_expect_success 'different author email' '
    (cd repo && grit log -n1 --format="%ae" >../actual) &&
    echo "alice@example.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'different committer name' '
    (cd repo && grit log -n1 --format="%cn" >../actual) &&
    echo "Bob" >expect &&
    test_cmp expect actual
'

test_expect_success 'different committer email' '
    (cd repo && grit log -n1 --format="%ce" >../actual) &&
    echo "bob@example.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %P shows parent hash for non-root commit' '
    (cd repo && grit log -n1 --format="%P" >../actual) &&
    grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'format %p shows abbreviated parent hash' '
    (cd repo && grit log -n1 --format="%p" >../actual) &&
    grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'format %P matches parent commit hash' '
    (cd repo && grit log -n1 --format="%P" >../actual) &&
    (cd repo && grit log -n1 --skip=1 --format="%H" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %p matches abbreviated parent hash' '
    (cd repo && grit log -n1 --format="%p" >../actual) &&
    (cd repo && grit log -n1 --skip=1 --format="%h" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'combined format: author name and email' '
    (cd repo && grit log -n1 --format="%an <%ae>" >../actual) &&
    echo "Alice <alice@example.com>" >expect &&
    test_cmp expect actual
'

test_expect_success 'combined format: committer name and email' '
    (cd repo && grit log -n1 --format="%cn <%ce>" >../actual) &&
    echo "Bob <bob@example.com>" >expect &&
    test_cmp expect actual
'

test_expect_success 'combined format: hash and subject' '
    (cd repo && grit log -n1 --format="%h %s" >../actual) &&
    grep "^[0-9a-f]\{7\} second commit$" actual
'

test_expect_success 'format %n produces newline' '
    (cd repo && grit log -n1 --format="A%nB" >../actual) &&
    printf "A\nB\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %% produces literal percent' '
    (cd repo && grit log -n1 --format="%%" >../actual) &&
    echo "%" >expect &&
    test_cmp expect actual
'

test_expect_success 'multi-line format with author and committer' '
    (cd repo && grit log -n1 --format="%an%n%cn" >../actual) &&
    printf "Alice\nBob\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format over multiple commits' '
    (cd repo && grit log --format="%an" >../actual) &&
    printf "Alice\nT\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %s over multiple commits' '
    (cd repo && grit log --format="%s" >../actual) &&
    printf "second commit\ninitial\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup third commit with yet another author' '
    (cd repo &&
     echo foo >file3.txt &&
     grit add file3.txt &&
     GIT_AUTHOR_NAME="Charlie" GIT_AUTHOR_EMAIL="charlie@example.com" \
     GIT_COMMITTER_NAME="Charlie" GIT_COMMITTER_EMAIL="charlie@example.com" \
     GIT_AUTHOR_DATE="1700002000 +0000" GIT_COMMITTER_DATE="1700002000 +0000" \
     grit commit -m "third commit")
'

test_expect_success 'format author names across three commits' '
    (cd repo && grit log --format="%an" >../actual) &&
    printf "Charlie\nAlice\nT\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format committer names across three commits' '
    (cd repo && grit log --format="%cn" >../actual) &&
    printf "Charlie\nBob\nT\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with -n2 shows only two commits' '
    (cd repo && grit log -n2 --format="%an" >../actual) &&
    printf "Charlie\nAlice\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with --skip=1 skips first commit' '
    (cd repo && grit log --skip=1 --format="%an" >../actual) &&
    printf "Alice\nT\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with --reverse shows oldest first' '
    (cd repo && grit log --reverse --format="%an" >../actual) &&
    printf "T\nAlice\nCharlie\n" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with literal text around placeholders' '
    (cd repo && grit log -n1 --format="Author: %an, Subject: %s" >../actual) &&
    echo "Author: Charlie, Subject: third commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format %H is consistent with rev-parse HEAD' '
    (cd repo && grit log -n1 --format="%H" >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %T is consistent with tree from cat-file' '
    (cd repo && grit log -n1 --format="%T" >../actual) &&
    HEAD=$(cd repo && grit rev-parse HEAD) &&
    (cd repo && grit cat-file -p $HEAD | grep "^tree " | cut -d" " -f2 >../expect) &&
    test_cmp expect actual
'

test_done
