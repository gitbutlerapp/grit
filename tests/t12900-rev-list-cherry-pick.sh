#!/bin/sh

test_description='grit cherry and cherry-pick: finding and applying unpicked commits'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: linear base with side branch' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo base >base.txt && grit add base.txt && grit commit -m "base" &&
	echo m1 >m1.txt && grit add m1.txt && grit commit -m "main-1" &&
	echo m2 >m2.txt && grit add m2.txt && grit commit -m "main-2" &&
	base=$(grit rev-parse HEAD~2) &&
	git branch side "$base" &&
	git checkout side &&
	echo s1 >s1.txt && grit add s1.txt && grit commit -m "side-1" &&
	echo s2 >s2.txt && grit add s2.txt && grit commit -m "side-2" &&
	echo s3 >s3.txt && grit add s3.txt && grit commit -m "side-3" &&
	git checkout main
	)
'

test_expect_success 'cherry shows + for unmerged side commits' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^+" actual >plus_lines &&
	test_line_count = 3 plus_lines
'

test_expect_success 'cherry -v shows subject alongside hash' '
	(cd repo && grit cherry -v main side >../actual) &&
	grep "side-1" actual &&
	grep "side-2" actual &&
	grep "side-3" actual
'

test_expect_success 'cherry output format is +/- SPACE hash' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^[+-] [0-9a-f]\{7,\}$" actual >matched &&
	test_line_count = 3 matched
'

test_expect_success 'cherry -v output format is +/- SPACE hash SPACE subject' '
	(cd repo && grit cherry -v main side >../actual) &&
	grep "^[+-] [0-9a-f]\{7,\} " actual >matched &&
	test_line_count = 3 matched
'

test_expect_success 'cherry side main shows main-only commits as +' '
	(cd repo && grit cherry side main >../actual) &&
	grep "^+" actual >plus_lines &&
	test_line_count = 2 plus_lines
'

test_expect_success 'cherry-pick applies a single commit' '
	(cd repo &&
	git checkout main &&
	side_tip=$(grit rev-parse side) &&
	grit cherry-pick "$side_tip" >../actual 2>&1) &&
	(cd repo && grit log -n1 --format="%s" >../actual) &&
	echo "side-3" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick creates new commit with same message' '
	(cd repo && grit log -n1 --format="%s" >../actual) &&
	echo "side-3" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick commit has different hash than original' '
	(cd repo && grit rev-parse HEAD >../picked_hash) &&
	(cd repo && grit rev-parse side >../orig_hash) &&
	! test_cmp orig_hash picked_hash
'

test_expect_success 'cherry shows - for picked commit after cherry-pick' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^-" actual >minus_lines &&
	test_line_count = 1 minus_lines
'

test_expect_success 'cherry still shows + for remaining unpicked commits' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^+" actual >plus_lines &&
	test_line_count = 2 plus_lines
'

test_expect_success 'setup: cherry-pick another commit' '
	(cd repo &&
	side2=$(grit rev-parse side~1) &&
	grit cherry-pick "$side2")
'

test_expect_success 'cherry shows 2 minus after 2 cherry-picks' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^-" actual >minus_lines &&
	test_line_count = 2 minus_lines
'

test_expect_success 'cherry shows 1 plus for remaining unpicked' '
	(cd repo && grit cherry main side >../actual) &&
	grep "^+" actual >plus_lines &&
	test_line_count = 1 plus_lines
'

test_expect_success 'cherry-pick all side commits makes all show -' '
	(cd repo &&
	side1=$(grit rev-parse side~2) &&
	grit cherry-pick "$side1") &&
	(cd repo && grit cherry main side >../actual) &&
	grep "^-" actual >minus_lines &&
	test_line_count = 3 minus_lines
'

test_expect_success 'cherry shows no + after all side commits picked' '
	(cd repo && grit cherry main side >../actual) &&
	! grep "^+" actual
'

test_expect_success 'setup second repo for limit tests' '
	(
	grit init repo2 && cd repo2 &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo base >base.txt && grit add base.txt && grit commit -m "base" &&
	echo f1 >f1.txt && grit add f1.txt && grit commit -m "common-1" &&
	limit=$(grit rev-parse HEAD) &&
	echo f2 >f2.txt && grit add f2.txt && grit commit -m "common-2" &&
	branch_point=$(grit rev-parse HEAD) &&
	git branch br "$branch_point" &&
	echo m1 >m1.txt && grit add m1.txt && grit commit -m "main-only" &&
	git checkout br &&
	echo b1 >b1.txt && grit add b1.txt && grit commit -m "br-only"
	)
'

test_expect_success 'cherry with limit arg restricts output' '
	(cd repo2 &&
	limit=$(grit rev-parse main~2) &&
	grit cherry main br "$limit" >../actual) &&
	test_line_count = 1 actual
'

test_expect_success 'cherry verbose with limit shows subject' '
	(cd repo2 &&
	limit=$(grit rev-parse main~2) &&
	grit cherry -v main br "$limit" >../actual) &&
	grep "br-only" actual
'

test_expect_success 'cherry without limit shows all divergent commits' '
	(cd repo2 && grit cherry main br >../actual) &&
	grep "^+" actual >plus &&
	test_line_count = 1 plus
'

test_expect_success 'setup: divergent branches with unique patches' '
	(
	grit init repo3 && cd repo3 &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo base >base.txt && grit add base.txt && grit commit -m "base" &&
	base=$(grit rev-parse HEAD) &&
	git branch alt "$base" &&
	echo main-unique >mu.txt && grit add mu.txt && grit commit -m "main-unique" &&
	git checkout alt &&
	echo alt-unique >au.txt && grit add au.txt && grit commit -m "alt-unique" &&
	echo alt-two >a2.txt && grit add a2.txt && grit commit -m "alt-two"
	)
'

test_expect_success 'cherry main alt shows 2 unmerged' '
	(cd repo3 && grit cherry main alt >../actual) &&
	grep "^+" actual >plus &&
	test_line_count = 2 plus
'

test_expect_success 'cherry -v main alt shows subjects' '
	(cd repo3 && grit cherry -v main alt >../actual) &&
	grep "alt-unique" actual &&
	grep "alt-two" actual
'

test_expect_success 'setup: multiple unique commits' '
	(
	grit init repo4 && cd repo4 &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo base >base.txt && grit add base.txt && grit commit -m "base" &&
	base=$(grit rev-parse HEAD) &&
	git branch feat "$base" &&
	echo x >x.txt && grit add x.txt && grit commit -m "main-x" &&
	git checkout feat &&
	echo y >y.txt && grit add y.txt && grit commit -m "feat-y" &&
	echo z >z.txt && grit add z.txt && grit commit -m "feat-z"
	)
'

test_expect_success 'cherry main feat shows 2 unmerged' '
	(cd repo4 && grit cherry main feat >../actual) &&
	grep "^+" actual >plus &&
	test_line_count = 2 plus
'

test_expect_success 'cherry feat main shows 1 unmerged' '
	(cd repo4 && grit cherry feat main >../actual) &&
	grep "^+" actual >plus &&
	test_line_count = 1 plus
'

test_expect_success 'cherry-pick into feat from main' '
	(cd repo4 &&
	git checkout feat &&
	main_tip=$(grit rev-parse main) &&
	grit cherry-pick "$main_tip") &&
	(cd repo4 && grit log -n1 --format="%s" >../actual) &&
	echo "main-x" >expect &&
	test_cmp expect actual
'

test_expect_success 'after cross-pick cherry feat main shows -' '
	(cd repo4 && grit cherry feat main >../actual) &&
	grep "^-" actual >minus &&
	test_line_count = 1 minus
'

test_expect_success 'cherry-pick preserves file content' '
	(cd repo4 && test -f x.txt) &&
	(cd repo4 && cat x.txt >../actual) &&
	echo "x" >expect &&
	test_cmp expect actual
'

test_expect_success 'cherry-pick on clean worktree succeeds' '
	(
	grit init repo5 && cd repo5 &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo a >a.txt && grit add a.txt && grit commit -m "first" &&
	base=$(grit rev-parse HEAD) &&
	git branch pick-src "$base" &&
	echo b >b.txt && grit add b.txt && grit commit -m "main-b" &&
	git checkout pick-src &&
	echo c >c.txt && grit add c.txt && grit commit -m "src-c" &&
	git checkout main &&
	src=$(grit rev-parse pick-src) &&
	grit cherry-pick "$src" &&
	grit log -n1 --format="%s" >actual &&
	echo "src-c" >expect &&
	test_cmp expect actual
	)
'

test_done
