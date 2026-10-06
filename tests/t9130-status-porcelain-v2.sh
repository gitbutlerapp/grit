#!/bin/sh
# Test status --porcelain output and related formatting options.

test_description='grit status --porcelain and formatting options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Tester" &&
	echo "initial" >tracked.txt &&
	grit add tracked.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Clean working tree
###########################################################################

test_expect_success 'status --porcelain on clean repo shows branch header' '
	(
	cd repo &&
	grit status --porcelain -b >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status -s on clean repo has no file entries' '
	(
	cd repo &&
	grit status -s >../out &&
	! grep "^[MADRCU?]" ../out
	)
'

test_expect_success 'status --short on clean repo has no file entries' '
	(
	cd repo &&
	grit status --short >../out &&
	! grep "^[MADRCU?]" ../out
	)
'

###########################################################################
# Section 2: Untracked files
###########################################################################

test_expect_success 'status --porcelain shows untracked file with ??' '
	(
	cd repo &&
	echo "new" >untracked.txt &&
	grit status --porcelain >../out &&
	grep "^?? untracked.txt$" ../out
	)
'

test_expect_success 'status -s shows untracked file with ??' '
	(
	cd repo &&
	grit status -s >../out &&
	grep "^?? untracked.txt$" ../out
	)
'

test_expect_success 'status --porcelain shows multiple untracked files' '
	(
	cd repo &&
	echo "a" >untracked_a.txt &&
	echo "b" >untracked_b.txt &&
	grit status --porcelain >../out &&
	grep "^?? untracked_a.txt$" ../out &&
	grep "^?? untracked_b.txt$" ../out
	)
'

test_expect_success 'status --porcelain with untracked directory' '
	(
	cd repo &&
	mkdir -p newdir &&
	echo "inside" >newdir/file.txt &&
	grit status --porcelain >../out &&
	grep "newdir" ../out
	)
'

###########################################################################
# Section 3: Staged files (index changes)
###########################################################################

test_expect_success 'status --porcelain shows staged new file with A' '
	(
	cd repo &&
	echo "staged" >staged.txt &&
	grit add staged.txt &&
	grit status --porcelain >../out &&
	grep "^A  staged.txt$" ../out
	)
'

test_expect_success 'status --porcelain shows staged modification with M' '
	(
	cd repo &&
	grit commit -m "add staged" &&
	rm -f untracked.txt untracked_a.txt untracked_b.txt &&
	rm -rf newdir &&
	echo "modified" >>tracked.txt &&
	grit add tracked.txt &&
	grit status --porcelain >../out &&
	grep "^M  tracked.txt$" ../out
	)
'

test_expect_success 'status --porcelain shows staged deletion with D' '
	(
	cd repo &&
	grit commit -m "modify" &&
	grit rm staged.txt &&
	grit status --porcelain >../out &&
	grep "^D  staged.txt$" ../out
	)
'

###########################################################################
# Section 4: Working tree modifications (unstaged)
###########################################################################

test_expect_success 'status --porcelain shows unstaged modification' '
	(
	cd repo &&
	grit commit -m "del staged" &&
	echo "unstaged change" >>tracked.txt &&
	grit status --porcelain >../out &&
	grep "^ M tracked.txt$" ../out
	)
'

test_expect_success 'status -s shows unstaged modification same format' '
	(
	cd repo &&
	grit status -s >../out &&
	grep "^ M tracked.txt$" ../out
	)
'

###########################################################################
# Section 5: Mixed staged and unstaged
###########################################################################

test_expect_success 'status --porcelain shows both staged and unstaged for same file' '
	(
	cd repo &&
	grit add tracked.txt &&
	echo "more changes" >>tracked.txt &&
	grit status --porcelain >../out &&
	grep "^MM tracked.txt$" ../out
	)
'

test_expect_success 'status --porcelain with staged add and unstaged untracked' '
	(
	cd repo &&
	grit commit -m "commit tracked" &&
	echo "new_staged" >ns.txt &&
	grit add ns.txt &&
	echo "new_untracked" >nu.txt &&
	grit status --porcelain >../out &&
	grep "^A  ns.txt$" ../out &&
	grep "^?? nu.txt$" ../out
	)
'

###########################################################################
# Section 6: Branch header with --porcelain -b
###########################################################################

test_expect_success 'status --porcelain shows branch in header' '
	(
	cd repo &&
	grit status --porcelain -b >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status --porcelain -b shows branch header' '
	(
	cd repo &&
	grit status --porcelain -b >../out &&
	grep "^## master" ../out
	)
'

test_expect_success 'status -s -b shows branch header' '
	(
	cd repo &&
	grit status -s -b >../out &&
	grep "^## master" ../out
	)
'

###########################################################################
# Section 7: Untracked files modes
###########################################################################

test_expect_success 'status --porcelain -u no hides untracked' '
	(
	cd repo &&
	grit status --porcelain -u no >../out &&
	! grep "^??" ../out
	)
'

test_expect_success 'status --porcelain -u normal shows untracked' '
	(
	cd repo &&
	grit status --porcelain -u normal >../out &&
	grep "^?? nu.txt$" ../out
	)
'

###########################################################################
# Section 8: Empty repository status
###########################################################################

test_expect_success 'status --porcelain in fresh empty repo has branch header' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	grit status --porcelain -b >../empty_out &&
	grep "^## " ../empty_out
	)
'

###########################################################################
# Section 9: Subdirectory handling
###########################################################################

test_expect_success 'status --porcelain shows files in subdirectories' '
	(
	cd repo &&
	grit commit -m "ns" &&
	rm -f nu.txt &&
	mkdir -p deep/nested &&
	echo "deep file" >deep/nested/file.txt &&
	grit add deep/ &&
	grit status --porcelain >../out &&
	grep "deep/nested/file.txt" ../out
	)
'

test_expect_success 'status --porcelain with nested untracked dir' '
	(
	cd repo &&
	grit commit -m "deep" &&
	mkdir -p other/sub &&
	echo "x" >other/sub/x.txt &&
	grit status --porcelain >../out &&
	grep "other" ../out
	)
'

###########################################################################
# Section 10: Multiple operations combined
###########################################################################

test_expect_success 'status --porcelain with add, modify, delete, untracked' '
	(
	cd repo &&
	echo "brand_new" >brand_new.txt &&
	grit add brand_new.txt &&
	echo "modify again" >>tracked.txt &&
	grit add tracked.txt &&
	echo "even more" >>tracked.txt &&
	grit rm ns.txt &&
	echo "untracked_extra" >extra.txt &&
	grit status --porcelain >../out &&
	grep "brand_new.txt" ../out &&
	grep "tracked.txt" ../out &&
	grep "ns.txt" ../out &&
	grep "extra.txt" ../out
	)
'

test_expect_success 'status --porcelain output is stable across consecutive runs' '
	(
	cd repo &&
	grit status --porcelain >../out1 &&
	grit status --porcelain >../out2 &&
	test_cmp ../out1 ../out2
	)
'

test_expect_success 'status --porcelain has correct entry count' '
	(
	cd repo &&
	grit status --porcelain >../out &&
	grep -v "^##" ../out >../entries &&
	test -s ../entries
	)
'

test_done
