#!/bin/sh

test_description='grit rev-list walk options: --max-count, --skip, --reverse, --first-parent, ranges'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup linear history' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo A >file.txt && grit add file.txt && grit commit -m "A" &&
	echo B >file.txt && grit add file.txt && grit commit -m "B" &&
	echo C >file.txt && grit add file.txt && grit commit -m "C" &&
	echo D >file.txt && grit add file.txt && grit commit -m "D" &&
	echo E >file.txt && grit add file.txt && grit commit -m "E" &&
	echo F >file.txt && grit add file.txt && grit commit -m "F"
	)
'

test_expect_success 'rev-list HEAD lists all commits' '
	(cd repo && grit rev-list HEAD >../actual) &&
	test_line_count = 6 actual
'

test_expect_success 'rev-list HEAD outputs full hashes' '
	(cd repo && grit rev-list HEAD | head -1 >../actual) &&
	hash=$(cat actual) &&
	test ${#hash} = 40
'

test_expect_success 'rev-list HEAD contains HEAD commit' '
	(cd repo && grit rev-list HEAD >../actual_rev) &&
	(cd repo && grit rev-parse HEAD >../actual_head) &&
	head_hash=$(cat actual_head) &&
	grep "$head_hash" actual_rev
'

test_expect_success 'rev-list --max-count=1 shows one commit' '
	(cd repo && grit rev-list --max-count=1 HEAD >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list --max-count=3 shows three commits' '
	(cd repo && grit rev-list --max-count=3 HEAD >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'rev-list --max-count=0 shows nothing' '
	(cd repo && grit rev-list --max-count=0 HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rev-list --max-count larger than total shows all' '
	(cd repo && grit rev-list --max-count=100 HEAD >../actual) &&
	test_line_count = 6 actual
'

test_expect_success 'rev-list --skip=1 skips latest' '
	(cd repo && grit rev-list --skip=1 HEAD >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --skip=3 skips three' '
	(cd repo && grit rev-list --skip=3 HEAD >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'rev-list --skip=6 skips all' '
	(cd repo && grit rev-list --skip=6 HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rev-list --skip=100 beyond total is empty' '
	(cd repo && grit rev-list --skip=100 HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rev-list --skip=2 --max-count=2' '
	(cd repo && grit rev-list --skip=2 --max-count=2 HEAD >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --reverse reverses order' '
	(cd repo && grit rev-list HEAD >../fwd) &&
	(cd repo && grit rev-list --reverse HEAD >../rev) &&
	test_line_count = 6 rev &&
	head -1 fwd >fwd_first &&
	tail -1 rev >rev_last &&
	test_cmp fwd_first rev_last
'

test_expect_success 'rev-list --reverse flips first and last' '
	(cd repo && grit rev-list --reverse HEAD | head -1 >../actual) &&
	(cd repo && grit rev-list HEAD | tail -1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'rev-list --reverse --max-count=2' '
	(cd repo && grit rev-list --reverse --max-count=2 HEAD >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list --first-parent on linear history same as HEAD' '
	(cd repo && grit rev-list HEAD >../actual_all) &&
	(cd repo && grit rev-list --first-parent HEAD >../actual_fp) &&
	test_cmp actual_all actual_fp
'

test_expect_success 'rev-list main same as HEAD' '
	(cd repo && grit rev-list HEAD >../actual_head) &&
	(cd repo && grit rev-list main >../actual_main) &&
	test_cmp actual_head actual_main
'

test_expect_success 'setup branch for range tests' '
	(cd repo &&
	 git checkout -b feature &&
	 echo G >g.txt && grit add g.txt && grit commit -m "G" &&
	 echo H >h.txt && grit add h.txt && grit commit -m "H" &&
	 git checkout main)
'

test_expect_success 'rev-list feature shows more commits than main' '
	(cd repo && grit rev-list feature >../actual_feature) &&
	(cd repo && grit rev-list main >../actual_main) &&
	feature_count=$(wc -l <actual_feature) &&
	main_count=$(wc -l <actual_main) &&
	test "$feature_count" -gt "$main_count"
'

test_expect_success 'rev-list main..feature shows only feature commits' '
	(cd repo && grit rev-list main..feature >../actual) &&
	test_line_count = 2 actual
'

test_expect_success 'rev-list main..feature contains only feature-exclusive commits' '
	(cd repo && grit rev-list main..feature >../actual) &&
	(cd repo && grit rev-list main >../main_commits) &&
	while read hash; do
		! grep "$hash" main_commits || return 1
	done <actual
'

test_expect_success 'rev-list feature..main is empty' '
	(cd repo && grit rev-list feature..main >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rev-list --count main..feature shows 2' '
	(cd repo && grit rev-list --count main..feature >../actual) &&
	echo "2" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --reverse main..feature flips order' '
	(cd repo && grit rev-list main..feature >../fwd) &&
	(cd repo && grit rev-list --reverse main..feature >../rev) &&
	head -1 fwd >fwd_first &&
	tail -1 rev >rev_last &&
	test_cmp fwd_first rev_last
'

test_expect_success 'rev-list --max-count=1 main..feature' '
	(cd repo && grit rev-list --max-count=1 main..feature >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list --skip=1 main..feature' '
	(cd repo && grit rev-list --skip=1 main..feature >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list HEAD^ shows parent and ancestors' '
	(cd repo && grit rev-list HEAD^ >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list HEAD~1 same as HEAD^' '
	(cd repo && grit rev-list "HEAD^" >../actual_caret) &&
	(cd repo && grit rev-list "HEAD~1" >../actual_tilde) &&
	test_cmp actual_caret actual_tilde
'

test_expect_success 'rev-list --count HEAD shows 6' '
	(cd repo && grit rev-list --count HEAD >../actual) &&
	echo "6" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count feature shows 8' '
	(cd repo && grit rev-list --count feature >../actual) &&
	echo "8" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list outputs unique hashes' '
	(cd repo && grit rev-list HEAD >../actual) &&
	sort actual >sorted &&
	sort -u actual >unique &&
	test_cmp sorted unique
'

test_expect_success 'rev-list consistent across runs' '
	(cd repo && grit rev-list HEAD >../run1) &&
	(cd repo && grit rev-list HEAD >../run2) &&
	test_cmp run1 run2
'

test_expect_success 'rev-list HEAD..HEAD is empty' '
	(cd repo && grit rev-list HEAD..HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rev-list --skip=1 --reverse HEAD' '
	(cd repo && grit rev-list --skip=1 --reverse HEAD >../actual) &&
	test_line_count = 5 actual
'

test_done
