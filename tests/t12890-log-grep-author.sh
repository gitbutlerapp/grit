#!/bin/sh

test_description='grit log filtering: skip, max-count, reverse, first-parent, multi-rev'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history' '
	(
	grit init repo && cd repo &&
	git config user.email "bob@example.com" && git config user.name "Bob" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo a >a.txt && grit add a.txt && grit commit -m "commit-A" &&
	echo b >b.txt && grit add b.txt && grit commit -m "commit-B" &&
	echo c >c.txt && grit add c.txt && grit commit -m "commit-C" &&
	echo d >d.txt && grit add d.txt && grit commit -m "commit-D" &&
	echo e >e.txt && grit add e.txt && grit commit -m "commit-E"
	)
'

test_expect_success 'log shows all 5 commits' '
	(cd repo && grit log --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log -n3 shows exactly 3 commits' '
	(cd repo && grit log -n3 --format="%s" >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'log -n3 shows most recent 3' '
	(cd repo && grit log -n3 --format="%s" >../actual) &&
	head -1 actual >first &&
	echo "commit-E" >expect &&
	test_cmp expect first
'

test_expect_success 'log --skip=2 omits newest 2' '
	(cd repo && grit log --skip=2 --format="%s" >../actual) &&
	test_line_count = 3 actual &&
	head -1 actual >first &&
	echo "commit-C" >expect &&
	test_cmp expect first
'

test_expect_success 'log --skip=2 -n1 shows exactly commit-C' '
	(cd repo && grit log --skip=2 -n1 --format="%s" >../actual) &&
	echo "commit-C" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip=4 shows only root commit' '
	(cd repo && grit log --skip=4 --format="%s" >../actual) &&
	test_line_count = 1 actual &&
	echo "commit-A" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip=5 produces empty output' '
	(cd repo && grit log --skip=5 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log --skip=100 produces empty output' '
	(cd repo && grit log --skip=100 --format="%s" >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'log --reverse shows oldest first' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	head -1 actual >first &&
	echo "commit-A" >expect &&
	test_cmp expect first
'

test_expect_success 'log --reverse last line is newest' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	tail -1 actual >last &&
	echo "commit-E" >expect &&
	test_cmp expect last
'

test_expect_success 'log --reverse preserves count' '
	(cd repo && grit log --reverse --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log --first-parent on linear history matches regular log' '
	(cd repo && grit log --format="%H" >../regular) &&
	(cd repo && grit log --first-parent --format="%H" >../firstp) &&
	test_cmp regular firstp
'

test_expect_success 'log --format with multiple placeholders' '
	(cd repo && grit log -n1 --format="%an <%ae>" >../actual) &&
	echo "Bob <bob@example.com>" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --format=%H is 40 hex chars' '
	(cd repo && grit log -n1 --format="%H" >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'log --format=%h is 7 chars' '
	(cd repo && grit log -n1 --format="%h" >../actual) &&
	grep "^[0-9a-f]\{7\}$" actual
'

test_expect_success 'log --oneline count matches --format count' '
	(cd repo && grit log --oneline --no-decorate >../oneline) &&
	(cd repo && grit log --format="%s" >../formatted) &&
	test_line_count = 5 oneline &&
	test_line_count = 5 formatted
'

test_expect_success 'log --max-count=2 same as -n2' '
	(cd repo && grit log --max-count=2 --format="%H" >../maxcount) &&
	(cd repo && grit log -n2 --format="%H" >../nflag) &&
	test_cmp maxcount nflag
'

test_expect_success 'setup branch for merge' '
	(cd repo &&
	base=$(grit rev-parse HEAD~2) &&
	git branch side "$base" &&
	git checkout side &&
	echo s1 >s1.txt && grit add s1.txt && grit commit -m "side-1" &&
	echo s2 >s2.txt && grit add s2.txt && grit commit -m "side-2" &&
	git checkout main)
'

test_expect_success 'log side shows correct count' '
	(cd repo && grit log side --format="%s" >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'log side tip is side-2' '
	(cd repo && grit log side -n1 --format="%s" >../actual) &&
	echo "side-2" >expect &&
	test_cmp expect actual
'

test_expect_success 'log main tip is commit-E' '
	(cd repo && grit log main -n1 --format="%s" >../actual) &&
	echo "commit-E" >expect &&
	test_cmp expect actual
'

test_expect_success 'log --skip=1 on side' '
	(cd repo && grit log side --skip=1 -n1 --format="%s" >../actual) &&
	echo "side-1" >expect &&
	test_cmp expect actual
'

test_expect_success 'log format tree hash exists' '
	(cd repo && grit log -n1 --format="%T" >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'log format parent hash exists' '
	(cd repo && grit log -n1 --format="%P" >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual
'

test_expect_success 'log format committer matches author for simple commits' '
	(cd repo && grit log -n1 --format="%an" >../author) &&
	(cd repo && grit log -n1 --format="%cn" >../committer) &&
	test_cmp author committer
'

test_expect_success 'log format committer email matches author email' '
	(cd repo && grit log -n1 --format="%ae" >../author_email) &&
	(cd repo && grit log -n1 --format="%ce" >../committer_email) &&
	test_cmp author_email committer_email
'

test_expect_success 'log --reverse --skip=3 shows last 2 reversed' '
	(cd repo && grit log --reverse --skip=3 --format="%s" >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'log --skip with --reverse starts from correct offset' '
	(cd repo && grit log --reverse --skip=4 --format="%s" >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'log -n1 --skip=0 shows HEAD' '
	(cd repo && grit log -n1 --skip=0 --format="%H" >../actual) &&
	(cd repo && grit rev-parse HEAD >../expect) &&
	test_cmp expect actual
'

test_expect_success 'log consecutive skips traverse history' '
	(cd repo && grit log --skip=0 -n1 --format="%s" >../s0) &&
	(cd repo && grit log --skip=1 -n1 --format="%s" >../s1) &&
	(cd repo && grit log --skip=2 -n1 --format="%s" >../s2) &&
	echo "commit-E" >e0 && echo "commit-D" >e1 && echo "commit-C" >e2 &&
	test_cmp e0 s0 && test_cmp e1 s1 && test_cmp e2 s2
'

test_expect_success 'log format with literal text' '
	(cd repo && grit log -n1 --format="hash=%H" >../actual) &&
	grep "^hash=[0-9a-f]\{40\}$" actual
'

test_expect_success 'log format with newline separator' '
	(cd repo && grit log -n1 --format="%an%n%ae" >../actual) &&
	test_line_count = 2 actual
'

test_done
