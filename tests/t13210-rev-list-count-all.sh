#!/bin/sh

test_description='grit rev-list --count and --all options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo A >a.txt && grit add a.txt && grit commit -m "A" &&
	echo B >b.txt && grit add b.txt && grit commit -m "B" &&
	echo C >c.txt && grit add c.txt && grit commit -m "C"
	)
'

test_expect_success 'rev-list --count HEAD' '
	(cd repo && grit rev-list --count HEAD >../actual) &&
	echo "3" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count --all with single branch' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo "3" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --all lists all commits' '
	(cd repo && grit rev-list --all >../actual) &&
	test_line_count = 3 actual
'

test_expect_success 'rev-list --all same as HEAD on single branch' '
	(cd repo && grit rev-list --all | sort >../actual_all) &&
	(cd repo && grit rev-list HEAD | sort >../actual_head) &&
	test_cmp actual_all actual_head
'

test_expect_success 'setup feature branch' '
	(cd repo &&
	 git checkout -b feature &&
	 echo D >d.txt && grit add d.txt && grit commit -m "D" &&
	 echo E >e.txt && grit add e.txt && grit commit -m "E")
'

test_expect_success 'rev-list --count feature' '
	(cd repo && grit rev-list --count feature >../actual) &&
	echo "5" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count --all includes both branches' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo "5" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --all includes feature commits' '
	(cd repo && grit rev-list --all | sort >../all_sorted) &&
	(cd repo && grit rev-list feature | sort >../feature_sorted) &&
	test_cmp all_sorted feature_sorted
'

test_expect_success 'rev-list --count main' '
	(cd repo && grit rev-list --count main >../actual) &&
	echo "3" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count main..feature' '
	(cd repo && grit rev-list --count main..feature >../actual) &&
	echo "2" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count feature..main is 0' '
	(cd repo && grit rev-list --count feature..main >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'setup second branch from main' '
	(cd repo &&
	 git checkout main &&
	 git checkout -b other &&
	 echo F >f.txt && grit add f.txt && grit commit -m "F" &&
	 echo G >g.txt && grit add g.txt && grit commit -m "G" &&
	 echo H >h.txt && grit add h.txt && grit commit -m "H")
'

test_expect_success 'rev-list --count other' '
	(cd repo && grit rev-list --count other >../actual) &&
	echo "6" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count --all with three branches' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo "8" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --all shows all unique commits' '
	(cd repo && grit rev-list --all >../actual) &&
	test_line_count = 8 actual
'

test_expect_success 'rev-list --all hashes are unique' '
	(cd repo && grit rev-list --all >../actual) &&
	sort actual >sorted &&
	sort -u actual >unique &&
	test_cmp sorted unique
'

test_expect_success 'rev-list --count main..other' '
	(cd repo && grit rev-list --count main..other >../actual) &&
	echo "3" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count other..feature' '
	(cd repo && grit rev-list --count other..feature >../actual) &&
	echo "2" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count feature..other' '
	(cd repo && grit rev-list --count feature..other >../actual) &&
	echo "3" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --all contains main HEAD' '
	(cd repo && grit rev-list --all >../all_hashes) &&
	(cd repo && grit rev-parse main >../main_hash) &&
	main=$(cat main_hash) &&
	grep "$main" all_hashes
'

test_expect_success 'rev-list --all contains feature HEAD' '
	(cd repo && grit rev-list --all >../all_hashes) &&
	(cd repo && grit rev-parse feature >../feature_hash) &&
	feat=$(cat feature_hash) &&
	grep "$feat" all_hashes
'

test_expect_success 'rev-list --all contains other HEAD' '
	(cd repo && grit rev-list --all >../all_hashes) &&
	(cd repo && grit rev-parse other >../other_hash) &&
	oth=$(cat other_hash) &&
	grep "$oth" all_hashes
'

test_expect_success 'rev-list --max-count=1 --all shows one commit' '
	(cd repo && grit rev-list --max-count=1 --all >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'rev-list --count HEAD..HEAD is 0' '
	(cd repo && grit rev-list --count HEAD..HEAD >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'setup: add more commits to main' '
	(cd repo &&
	 git checkout main &&
	 echo I >i.txt && grit add i.txt && grit commit -m "I" &&
	 echo J >j.txt && grit add j.txt && grit commit -m "J")
'

test_expect_success 'rev-list --count main after additional commits' '
	(cd repo && grit rev-list --count main >../actual) &&
	echo "5" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --count --all after additional commits' '
	(cd repo && grit rev-list --count --all >../actual) &&
	echo "10" >expect &&
	test_cmp expect actual
'

test_expect_success 'rev-list --all --max-count=5' '
	(cd repo && grit rev-list --all --max-count=5 >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --all --skip=5' '
	(cd repo && grit rev-list --all --skip=5 >../actual) &&
	test_line_count = 5 actual
'

test_expect_success 'rev-list --count consistent with list length' '
	(cd repo && grit rev-list --count main >../count_out) &&
	(cd repo && grit rev-list main >../list_out) &&
	count=$(cat count_out) &&
	lines=$(wc -l <list_out) &&
	test "$count" = "$lines"
'

test_expect_success 'rev-list --count consistent for --all' '
	(cd repo && grit rev-list --count --all >../count_out) &&
	(cd repo && grit rev-list --all >../list_out) &&
	count=$(cat count_out) &&
	lines=$(wc -l <list_out) &&
	test "$count" = "$lines"
'

test_expect_success 'rev-list HEAD outputs full 40-char hashes' '
	(cd repo && grit rev-list HEAD >../actual) &&
	while read hash; do
		test ${#hash} = 40 || return 1
	done <actual
'

test_done
