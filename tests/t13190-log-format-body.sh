#!/bin/sh

test_description='grit log --format body, subject, and multi-line message formatting'

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
	echo first >file.txt && grit add file.txt &&
	grit commit -m "first subject" -m "first body line" &&
	echo second >file2.txt && grit add file2.txt &&
	grit commit -m "second subject" -m "second body text" &&
	echo third >file3.txt && grit add file3.txt &&
	grit commit -m "third subject" &&
	echo fourth >file4.txt && grit add file4.txt &&
	grit commit -m "fourth subject" -m "multi line body"
	)
'

test_expect_success 'format %s shows subject line' '
	(cd repo && grit log -n 1 --format="%s" >../actual) &&
	echo "fourth subject" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %s for all commits' '
	(cd repo && grit log --format="%s" >../actual) &&
	cat >expect <<-EOF &&
	fourth subject
	third subject
	second subject
	first subject
	EOF
	test_cmp expect actual
'

test_expect_success 'format %b shows body for commit with body' '
	(cd repo && grit log -n 1 --format="%b" >../actual) &&
	grep "multi line body" actual
'

test_expect_success 'format %b is empty for commit without body' '
	(cd repo && grit log --skip=1 -n 1 --format="%b" >../actual) &&
	! grep "." actual
'

test_expect_success 'format %an shows author name' '
	(cd repo && grit log -n 1 --format="%an" >../actual) &&
	echo "T" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %ae shows author email' '
	(cd repo && grit log -n 1 --format="%ae" >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %cn shows committer name' '
	(cd repo && grit log -n 1 --format="%cn" >../actual) &&
	echo "T" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %ce shows committer email' '
	(cd repo && grit log -n 1 --format="%ce" >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %H shows full commit hash' '
	(cd repo && grit log -n 1 --format="%H" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'format %h shows abbreviated commit hash' '
	(cd repo && grit log -n 1 --format="%h" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'format %T shows full tree hash' '
	(cd repo && grit log -n 1 --format="%T" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'format %t shows abbreviated tree hash' '
	(cd repo && grit log -n 1 --format="%t" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'format %P shows parent full hash' '
	(cd repo && grit log -n 1 --format="%P" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'format %p shows parent abbreviated hash' '
	(cd repo && grit log -n 1 --format="%p" >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 7
'

test_expect_success 'format %P is empty for root commit' '
	(cd repo && grit log --format="%P" | tail -1 >../actual) &&
	echo >expect &&
	test_cmp expect actual
'

test_expect_success 'format %p is empty for root commit' '
	(cd repo && grit log --format="%p" | tail -1 >../actual) &&
	echo >expect &&
	test_cmp expect actual
'

test_expect_success 'format %n gives newline' '
	(cd repo && grit log -n 1 --format="%n" >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'format combined %h %s' '
	(cd repo && grit log -n 1 --format="%h %s" >../actual) &&
	grep "fourth subject" actual
'

test_expect_success 'format combined %an <%ae>' '
	(cd repo && grit log -n 1 --format="%an <%ae>" >../actual) &&
	echo "T <t@t.com>" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %s with -n 2' '
	(cd repo && grit log -n 2 --format="%s" >../actual) &&
	cat >expect <<-EOF &&
	fourth subject
	third subject
	EOF
	test_cmp expect actual
'

test_expect_success 'format %H matches rev-parse HEAD' '
	(cd repo && grit log -n 1 --format="%H" >../actual_log) &&
	(cd repo && grit rev-parse HEAD >../actual_rp) &&
	test_cmp actual_log actual_rp
'

test_expect_success 'format %h is prefix of %H' '
	(cd repo && grit log -n 1 --format="%h" >../actual_short) &&
	(cd repo && grit log -n 1 --format="%H" >../actual_full) &&
	short=$(cat actual_short) &&
	full=$(cat actual_full) &&
	case "$full" in
	"$short"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'format %t is prefix of %T' '
	(cd repo && grit log -n 1 --format="%t" >../actual_short) &&
	(cd repo && grit log -n 1 --format="%T" >../actual_full) &&
	short=$(cat actual_short) &&
	full=$(cat actual_full) &&
	case "$full" in
	"$short"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'format %p is prefix of %P' '
	(cd repo && grit log -n 1 --format="%p" >../actual_short) &&
	(cd repo && grit log -n 1 --format="%P" >../actual_full) &&
	short=$(cat actual_short) &&
	full=$(cat actual_full) &&
	case "$full" in
	"$short"*) true ;;
	*) false ;;
	esac
'

test_expect_success 'format %cd shows committer date' '
	(cd repo && grit log -n 1 --format="%cd" >../actual) &&
	test -s actual
'

test_expect_success 'format %s consistent across runs' '
	(cd repo && grit log --format="%s" >../actual1) &&
	(cd repo && grit log --format="%s" >../actual2) &&
	test_cmp actual1 actual2
'

test_expect_success 'format %H consistent across runs' '
	(cd repo && grit log --format="%H" >../actual1) &&
	(cd repo && grit log --format="%H" >../actual2) &&
	test_cmp actual1 actual2
'

test_expect_success 'format %b for first commit shows body' '
	(cd repo && grit log --skip=3 -n 1 --format="%b" >../actual) &&
	grep "first body line" actual
'

test_expect_success 'format %b for second commit shows body' '
	(cd repo && grit log --skip=2 -n 1 --format="%b" >../actual) &&
	grep "second body text" actual
'

test_expect_success 'format %s --reverse starts from oldest' '
	(cd repo && grit log --reverse --format="%s" | head -1 >../actual) &&
	echo "first subject" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %an same for all commits' '
	(cd repo && grit log --format="%an" | sort -u >../actual) &&
	echo "T" >expect &&
	test_cmp expect actual
'

test_expect_success 'format %ae same for all commits' '
	(cd repo && grit log --format="%ae" | sort -u >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'format tformat:%h works like %h' '
	(cd repo && grit log --format="tformat:%h" >../actual_t) &&
	(cd repo && grit log --format="%h" >../actual_p) &&
	test_cmp actual_t actual_p
'

test_expect_success 'format with literal text' '
	(cd repo && grit log -n 1 --format="commit: %h" >../actual) &&
	grep "^commit: " actual
'

test_expect_success 'format %H shows different hash per commit' '
	(cd repo && grit log --format="%H" >../actual) &&
	sort actual >sorted &&
	sort -u actual >unique &&
	test_cmp sorted unique
'

test_done
