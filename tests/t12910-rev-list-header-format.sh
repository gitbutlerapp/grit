#!/bin/sh

test_description='grit rev-list --format: formatting, counting, ordering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "dev@test.org" && git config user.name "Dev" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo one >one.txt && grit add one.txt && grit commit -m "first" &&
	echo two >two.txt && grit add two.txt && grit commit -m "second" &&
	echo three >three.txt && grit add three.txt && grit commit -m "third" &&
	echo four >four.txt && grit add four.txt && grit commit -m "fourth" &&
	echo five >five.txt && grit add five.txt && grit commit -m "fifth"
	)
'

test_expect_success 'rev-list HEAD lists all commits' '
	(cd repo && grit rev-list HEAD >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --count HEAD returns 5' '
	(cd repo && grit rev-list --count HEAD >../actual) &&
	echo 5 >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --max-count=2 limits output' '
	(cd repo && grit rev-list --max-count=2 HEAD >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --skip=3 skips 3 commits' '
	(cd repo && grit rev-list --skip=3 HEAD >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --skip=3 --max-count=1 returns exactly one' '
	(cd repo && grit rev-list --skip=3 --max-count=1 HEAD >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list --reverse reverses order' '
	(cd repo && grit rev-list HEAD >../normal) &&
	(cd repo && grit rev-list --reverse HEAD >../reversed) &&
	head -1 normal >normal_first &&
	tail -1 reversed >reversed_last &&
	test_cmp normal_first reversed_last
'

test_expect_success 'rev-list --reverse first is root commit' '
	(cd repo && grit rev-list --reverse HEAD >../reversed) &&
	head -1 reversed >actual &&
	(cd repo && grit rev-list HEAD >../all) &&
	tail -1 all >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --format=%s shows subjects' '
	(cd repo && grit rev-list --format="%s" HEAD >../actual) &&
	grep "fifth" actual &&
	grep "first" actual
'

test_expect_success 'rev-list --format=%s includes commit prefix lines' '
	(cd repo && grit rev-list --format="%s" HEAD >../actual) &&
	grep "^commit [0-9a-f]\{40\}" actual >commits &&
	test_line_count = 5 commits
'

test_expect_success 'rev-list --format=%H shows full hashes' '
	(cd repo && grit rev-list --format="%H" HEAD >../actual) &&
	grep "^[0-9a-f]\{40\}$" actual >hashes &&
	test_line_count = 5 hashes
'

test_expect_success 'rev-list --format=%H hashes match rev-list output' '
	(cd repo && grit rev-list HEAD >../plain) &&
	(cd repo && grit rev-list --format="%H" HEAD >../formatted) &&
	grep "^[0-9a-f]\{40\}$" formatted | sort >formatted_sorted &&
	sort plain >plain_sorted &&
	test_cmp plain_sorted formatted_sorted
'

test_expect_success 'rev-list --format=%h shows abbreviated hashes' '
	(cd repo && grit rev-list --format="%h" HEAD >../actual) &&
	grep "^[0-9a-f]\{7\}$" actual >short_hashes &&
	test_line_count = 5 short_hashes
'

test_expect_success 'rev-list --format=%s subjects are unique per commit' '
	(cd repo && grit rev-list --format="%s" HEAD >../actual) &&
	grep -v "^commit " actual | sort -u >unique_subjects &&
	test_line_count = 5 unique_subjects
'

test_expect_success 'rev-list --format output alternates commit and format lines' '
	(cd repo && grit rev-list --format="%s" HEAD >../actual) &&
	test_line_count = 10 actual
'

test_expect_success 'rev-list --format=%h abbreviations are 7 chars' '
	(cd repo && grit rev-list --format="%h" HEAD >../actual) &&
	grep "^[0-9a-f]\{7\}$" actual >short &&
	test_line_count = 5 short
'

test_expect_success 'rev-list --format=%H matches commit prefix lines' '
	(cd repo && grit rev-list --format="%H" HEAD >../actual) &&
	grep "^commit " actual | sed "s/^commit //" >commit_lines &&
	grep -v "^commit " actual >format_lines &&
	test_cmp commit_lines format_lines
'

test_expect_success 'rev-list --format with multiple placeholders' '
	(cd repo && grit rev-list --format="%H %s" HEAD >../actual) &&
	grep "[0-9a-f]\{40\} fifth" actual
'

test_expect_success 'rev-list --format=%s --max-count=1 shows one subject' '
	(cd repo && grit rev-list --format="%s" --max-count=1 HEAD >../actual) &&
	grep "fifth" actual &&
	! grep "fourth" actual
'

test_expect_success 'rev-list --all lists all reachable commits' '
	(cd repo && grit rev-list --all >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --count --all matches --all count' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo 5 >expect &&
	test_cmp expect actual
'

test_expect_success 'setup branch for multi-ref tests' '
	(cd repo &&
	base=$(grit rev-parse HEAD~2) &&
	git branch side "$base" &&
	git checkout side &&
	echo s1 >s1.txt && grit add s1.txt && grit commit -m "side-1" &&
	echo s2 >s2.txt && grit add s2.txt && grit commit -m "side-2" &&
	git checkout main)
'

test_expect_success 'rev-list --count --all includes side branch' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo 7 >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list side..main shows main-only commits' '
	(cd repo && grit rev-list side..main >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list main..side shows side-only commits' '
	(cd repo && grit rev-list main..side >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --count side..main' '
	(cd repo && grit rev-list --count side..main >../actual) &&
	echo 2 >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --first-parent on linear history' '
	(cd repo && grit rev-list --first-parent HEAD >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --format=%s on range' '
	(cd repo && grit rev-list --format="%s" side..main >../actual) &&
	grep "fifth" actual &&
	grep "fourth" actual &&
	! grep "third" actual
'

test_expect_success 'rev-list --skip on range' '
	(cd repo && grit rev-list --skip=1 side..main >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list output is valid 40-char hex' '
	(cd repo && grit rev-list HEAD >../actual) &&
	while read line; do
		echo "$line" | grep "^[0-9a-f]\{40\}$" || exit 1
	done <actual
'

test_expect_success 'rev-list --reverse --max-count=2 shows oldest 2 reversed' '
	(cd repo && grit rev-list --reverse --max-count=2 HEAD >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --format=%s --reverse reverses' '
	(cd repo && grit rev-list --format="%s" --reverse HEAD >../actual) &&
	grep "first" actual >first_match &&
	grep "fifth" actual >fifth_match
'

test_done
