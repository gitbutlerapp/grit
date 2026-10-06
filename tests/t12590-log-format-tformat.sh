#!/bin/sh

test_description='grit log --format with various format specifiers and tformat'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "alice@example.com" && git config user.name "Alice" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
    echo one >file.txt && grit add file.txt && grit commit -m "first commit" &&
    git config user.email "bob@example.com" && git config user.name "Bob" &&
    echo two >file2.txt && grit add file2.txt && grit commit -m "second commit" &&
    git config user.email "charlie@example.com" && git config user.name "Charlie" &&
    echo three >file3.txt && grit add file3.txt && grit commit -m "third commit"
	)
'

test_expect_success 'format %H shows full commit hash' '
    (cd repo && grit log --format="%H" >../actual) &&
    (cd repo && git log --format="%H" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %h shows abbreviated hash' '
    (cd repo && grit log --format="%h" >../actual) &&
    (cd repo && git log --format="%h" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %s shows subject' '
    (cd repo && grit log --format="%s" >../actual) &&
    (cd repo && git log --format="%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %an shows author name' '
    (cd repo && grit log --format="%an" >../actual) &&
    (cd repo && git log --format="%an" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %ae shows author email' '
    (cd repo && grit log --format="%ae" >../actual) &&
    (cd repo && git log --format="%ae" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %cn shows committer name' '
    (cd repo && grit log --format="%cn" >../actual) &&
    (cd repo && git log --format="%cn" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %ce shows committer email' '
    (cd repo && grit log --format="%ce" >../actual) &&
    (cd repo && git log --format="%ce" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %T shows tree hash' '
    (cd repo && grit log --format="%T" >../actual) &&
    (cd repo && git log --format="%T" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %t shows abbreviated tree hash' '
    (cd repo && grit log --format="%t" >../actual) &&
    (cd repo && git log --format="%t" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %P shows parent hashes' '
    (cd repo && grit log --format="%P" >../actual) &&
    (cd repo && git log --format="%P" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %p shows abbreviated parent hashes' '
    (cd repo && grit log --format="%p" >../actual) &&
    (cd repo && git log --format="%p" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %n produces newline' '
    (cd repo && grit log -n 1 --format="%h%n%s" >../actual) &&
    (cd repo && git log -n 1 --format="%h%n%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %% produces literal percent' '
    (cd repo && grit log --format="%%" >../actual) &&
    (cd repo && git log --format="%%" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format combining multiple specifiers' '
    (cd repo && grit log --format="%h|%s|%an" >../actual) &&
    (cd repo && git log --format="%h|%s|%an" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format with literal text' '
    (cd repo && grit log --format="commit: %h by %an" >../actual) &&
    (cd repo && git log --format="commit: %h by %an" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %h %s matches git' '
    (cd repo && grit log --format="%h %s" >../actual) &&
    (cd repo && git log --format="%h %s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format: prefix produces same content as git format:' '
    (cd repo && grit log --format="format:%h %s" >../actual) &&
    (cd repo && git log --format="format:%h %s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'tformat: prefix matches git tformat' '
    (cd repo && grit log --format="tformat:%h %s" >../actual) &&
    (cd repo && git log --format="tformat:%h %s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %H is 40 characters' '
    (cd repo && grit log -n 1 --format="%H" >../actual) &&
    len=$(wc -c <actual) &&
    test "$len" -eq 41
'

test_expect_success 'format with -n 1 shows single commit' '
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "third commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'format with -n 2 shows two commits' '
    (cd repo && grit log -n 2 --format="%s" >../actual) &&
    test "$(wc -l <actual)" -eq 2
'

test_expect_success 'format with --reverse' '
    (cd repo && grit log --reverse --format="%s" >../actual) &&
    (cd repo && git log --reverse --format="%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format with --skip' '
    (cd repo && grit log --skip=1 --format="%s" >../actual) &&
    (cd repo && git log --skip=1 --format="%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format root commit has empty parent' '
    (cd repo && grit log --format="%H %P" >../actual) &&
    tail -1 actual >root_line &&
    (cd repo && git log --format="%H %P" >../expect) &&
    tail -1 expect >expect_root &&
    test_cmp expect_root root_line
'

test_expect_success 'format %an differs by commit when authors differ' '
    (cd repo && grit log --format="%an" >../actual) &&
    head -1 actual >first_author &&
    echo "Charlie" >expect &&
    test_cmp expect first_author
'

test_expect_success 'format %ae shows correct email for each commit' '
    (cd repo && grit log --format="%ae" >../actual) &&
    head -1 actual >first_email &&
    echo "charlie@example.com" >expect &&
    test_cmp expect first_email
'

test_expect_success 'format oldest commit shows Alice as author' '
    (cd repo && grit log --format="%an" >../actual) &&
    tail -1 actual >last_author &&
    echo "Alice" >expect &&
    test_cmp expect last_author
'

test_expect_success 'format %h %an %ae combined' '
    (cd repo && grit log --format="%h %an %ae" >../actual) &&
    (cd repo && git log --format="%h %an %ae" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format %H %T %P combined' '
    (cd repo && grit log --format="%H %T %P" >../actual) &&
    (cd repo && git log --format="%H %T %P" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format with --reverse -n 2' '
    (cd repo && grit log --reverse -n 2 --format="%s" >../actual) &&
    (cd repo && git log --reverse -n 2 --format="%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format with separator chars' '
    (cd repo && grit log --format="%h:%an:%s" >../actual) &&
    (cd repo && git log --format="%h:%an:%s" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'format empty string produces empty lines' '
    (cd repo && grit log --format="" >../actual) &&
    (cd repo && git log --format="" >../expect) &&
    test_cmp expect actual
'

test_done
