#!/bin/sh

test_description='grit log display options: oneline, graph, decorate, skip, max-count'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo first >file.txt && grit add file.txt && grit commit -m "first commit" &&
	echo second >file2.txt && grit add file2.txt && grit commit -m "second commit" &&
	echo third >file3.txt && grit add file3.txt && grit commit -m "third commit" &&
	echo fourth >file4.txt && grit add file4.txt && grit commit -m "fourth commit" &&
	echo fifth >file5.txt && grit add file5.txt && grit commit -m "fifth commit"
	)
'

test_expect_success 'log --oneline shows abbreviated hash and subject' '
	(cd repo && grit log --oneline >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --oneline lines have short hash then subject' '
	(cd repo && grit log --oneline | head -1 >../actual) &&
	grep "fifth commit" actual
'

test_expect_success 'log --oneline shows all commits' '
	(cd repo && grit log --oneline >../actual) &&
	grep "first commit" actual &&
	grep "second commit" actual &&
	grep "third commit" actual &&
	grep "fourth commit" actual &&
	grep "fifth commit" actual
'

test_expect_success 'log -n 1 shows only one commit' '
	(cd repo && grit log -n 1 --format="%s" >../actual) &&
	echo "fifth commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log -n 2 shows only two commits' '
	(cd repo && grit log -n 2 --format="%s" >../actual) &&
	cat >expect <<-EOF &&
	fifth commit
	fourth commit
	EOF
	test_cmp expect actual
'

test_expect_success 'log -n 0 shows zero or minimal output' '
	(cd repo && grit log -n 0 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log --max-count=3 limits output' '
	(cd repo && grit log --max-count=3 --format="%s" >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'log --max-count=1 same as -n 1' '
	(cd repo && grit log --max-count=1 --format="%s" >../actual_max) &&
	(cd repo && grit log -n 1 --format="%s" >../actual_n) &&
	test_cmp actual_max actual_n
'

test_expect_success 'log --skip=1 skips latest commit' '
	(cd repo && grit log --skip=1 --format="%s" | head -1 >../actual) &&
	echo "fourth commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip=2 skips two commits' '
	(cd repo && grit log --skip=2 --format="%s" >../actual) &&
	test_line_count = 3 actual &&
	head -1 actual >first_line &&
	echo "third commit" >expect &&
	test_cmp expect first_line
'

test_expect_success 'log --skip=5 skips all commits' '
	(cd repo && grit log --skip=5 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log --skip=2 -n 2 combines skip and limit' '
	(cd repo && grit log --skip=2 -n 2 --format="%s" >../actual) &&
	cat >expect <<-EOF &&
	third commit
	second commit
	EOF
	test_cmp expect actual
'

test_expect_success 'log --reverse shows oldest first' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	head -1 actual >first_line &&
	echo "first commit" >expect &&
	test_cmp expect first_line
'

test_expect_success 'log --reverse last line is newest' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	tail -1 actual >last_line &&
	echo "fifth commit" >expect &&
	test_cmp expect last_line
'

test_expect_success 'log --reverse preserves count' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --reverse -n 2 reverses limited set' '
	(cd repo && grit log --reverse -n 2 --format="%s" >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'log --graph produces output' '
	(cd repo && grit log --graph --oneline >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --oneline shows HEAD decoration' '
	(cd repo && grit log --oneline --decorate -n 1 >../actual) &&
	grep "HEAD" actual
'

test_expect_success 'log --no-decorate omits decorations' '
	(cd repo && grit log --no-decorate --oneline -n 1 >../actual) &&
	! grep "HEAD" actual
'

test_expect_success 'log --no-decorate still shows hash and subject' '
	(cd repo && grit log --no-decorate --oneline -n 1 >../actual) &&
	grep "fifth commit" actual
'

test_expect_success 'log --format=%H shows full hashes' '
	(cd repo && grit log --format="%H" -n 1 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'log --format=%h shows short hashes' '
	(cd repo && grit log --format="%h" -n 1 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'log with no arguments shows all commits' '
	(cd repo && grit log --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --first-parent on linear history shows all' '
	(cd repo && grit log --first-parent --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --skip larger than total returns empty' '
	(cd repo && grit log --skip=100 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log -n larger than total shows all' '
	(cd repo && grit log -n 100 --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --oneline with -n 3' '
	(cd repo && grit log --oneline -n 3 >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'log --oneline with --skip=1' '
	(cd repo && grit log --oneline --skip=1 >../actual) &&
	test_line_count = 4 actual
'

test_expect_success 'log --reverse --skip=3 shows last two reversed' '
	(cd repo && grit log --reverse --skip=3 --format="%s" >../actual) &&
	cat >expect <<-EOF &&
	first commit
	second commit
	EOF
	test_cmp expect actual
'

test_expect_success 'log HEAD shows same as log' '
	(cd repo && grit log --format="%H" >../actual_default) &&
	(cd repo && grit log --format="%H" HEAD >../actual_head) &&
	test_cmp actual_default actual_head
'

test_expect_success 'log --skip=4 -n 1 shows first commit' '
	(cd repo && grit log --skip=4 -n 1 --format="%s" >../actual) &&
	echo "first commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --oneline --no-decorate omits refs' '
	(cd repo && grit log --oneline --no-decorate -n 1 >../actual) &&
	! grep "master" actual
'

test_expect_success 'log --graph -n 2 limits graph output' '
	(cd repo && grit log --graph --oneline -n 2 >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'log --format with space separator' '
	(cd repo && grit log --format="%h %s" -n 1 >../actual) &&
	grep "fifth commit" actual
'

test_done
