#!/bin/sh
# Tests for grit status with various file states and porcelain output.

test_description='grit status porcelain output with various file states'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with files' '
	(
	"$REAL_GIT" init --initial-branch=master repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "file one" >file1.txt &&
	echo "file two" >file2.txt &&
	echo "file three" >file3.txt &&
	mkdir -p sub &&
	echo "nested" >sub/nested.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: Clean status
###########################################################################

test_expect_success 'status --porcelain on clean repo shows nothing (except branch)' '
	(
	cd repo &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../actual >../filtered || true &&
	test_must_be_empty ../filtered
	)
'

test_expect_success 'status --short on clean repo shows nothing (except branch)' '
	(
	cd repo &&
	"$GUST_BIN" status --short >../actual &&
	grep -v "^##" ../actual >../filtered || true &&
	test_must_be_empty ../filtered
	)
'

###########################################################################
# Section 3: Untracked files
###########################################################################

test_expect_success 'status shows untracked file' '
	(
	cd repo &&
	echo "new" >untracked.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "?? untracked.txt" ../actual
	)
'

test_expect_success 'status shows multiple untracked files' '
	(
	cd repo &&
	echo "a" >new_a.txt &&
	echo "b" >new_b.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "?? new_a.txt" ../actual &&
	grep "?? new_b.txt" ../actual
	)
'

test_expect_success 'status shows untracked directory' '
	(
	cd repo &&
	mkdir -p newdir &&
	echo "x" >newdir/x.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "newdir" ../actual
	)
'

test_expect_success 'cleanup untracked' '
	(
	cd repo &&
	rm -f untracked.txt new_a.txt new_b.txt &&
	rm -rf newdir
	)
'

###########################################################################
# Section 4: Staged additions
###########################################################################

test_expect_success 'status shows staged new file as A' '
	(
	cd repo &&
	echo "added" >added.txt &&
	"$REAL_GIT" add added.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^A" ../actual | grep "added.txt"
	)
'

test_expect_success 'status shows multiple staged additions' '
	(
	cd repo &&
	echo "add1" >add1.txt &&
	echo "add2" >add2.txt &&
	"$REAL_GIT" add add1.txt add2.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^A" ../actual | grep "add1.txt" &&
	grep "^A" ../actual | grep "add2.txt"
	)
'

test_expect_success 'commit staged files' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "add files"
	)
'

###########################################################################
# Section 5: Modified files (unstaged)
###########################################################################

test_expect_success 'status shows modified unstaged file' '
	(
	cd repo &&
	echo "modified content" >file1.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^ M" ../actual | grep "file1.txt"
	)
'

test_expect_success 'status shows multiple modified files' '
	(
	cd repo &&
	echo "mod2" >file2.txt &&
	echo "mod3" >file3.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "file1.txt" ../actual &&
	grep "file2.txt" ../actual &&
	grep "file3.txt" ../actual
	)
'

test_expect_success 'status modified matches git' '
	(
	cd repo &&
	"$REAL_GIT" status --porcelain >../expected &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../expected | sort >../expected_sorted &&
	grep -v "^##" ../actual | sort >../actual_sorted &&
	test_cmp ../expected_sorted ../actual_sorted
	)
'

test_expect_success 'restore files' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- file1.txt file2.txt file3.txt
	)
'

###########################################################################
# Section 6: Staged modifications
###########################################################################

test_expect_success 'status shows staged modification as M' '
	(
	cd repo &&
	echo "staged mod" >file1.txt &&
	"$REAL_GIT" add file1.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^M" ../actual | grep "file1.txt"
	)
'

test_expect_success 'status staged mod matches git' '
	(
	cd repo &&
	"$REAL_GIT" status --porcelain >../expected &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../expected | sort >../expected_sorted &&
	grep -v "^##" ../actual | sort >../actual_sorted &&
	test_cmp ../expected_sorted ../actual_sorted
	)
'

test_expect_success 'commit staged mod' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "modify file1"
	)
'

###########################################################################
# Section 7: Deleted files
###########################################################################

test_expect_success 'status shows unstaged deletion' '
	(
	cd repo &&
	rm file2.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^ D" ../actual | grep "file2.txt"
	)
'

test_expect_success 'status shows staged deletion as D' '
	(
	cd repo &&
	"$REAL_GIT" rm file2.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^D" ../actual | grep "file2.txt"
	)
'

test_expect_success 'status staged deletion matches git' '
	(
	cd repo &&
	"$REAL_GIT" status --porcelain >../expected &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../expected | sort >../expected_sorted &&
	grep -v "^##" ../actual | sort >../actual_sorted &&
	test_cmp ../expected_sorted ../actual_sorted
	)
'

test_expect_success 'commit deletion' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "remove file2"
	)
'

###########################################################################
# Section 8: Mixed states
###########################################################################

test_expect_success 'status with mixed states: staged + unstaged + untracked' '
	(
	cd repo &&
	echo "stage me" >staged_new.txt &&
	"$REAL_GIT" add staged_new.txt &&
	echo "modify" >file1.txt &&
	echo "untracked" >untracked_mix.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^A" ../actual | grep "staged_new.txt" &&
	grep "M" ../actual | grep "file1.txt" &&
	grep "??" ../actual | grep "untracked_mix.txt"
	)
'

test_expect_success 'status mixed matches git (file entries)' '
	(
	cd repo &&
	"$REAL_GIT" status --porcelain >../expected &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../expected | sort >../expected_sorted &&
	grep -v "^##" ../actual | sort >../actual_sorted &&
	test_cmp ../expected_sorted ../actual_sorted
	)
'

test_expect_success 'cleanup mixed state' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- file1.txt &&
	"$REAL_GIT" reset HEAD staged_new.txt &&
	rm -f staged_new.txt untracked_mix.txt
	)
'

###########################################################################
# Section 9: Nested file changes
###########################################################################

test_expect_success 'status shows nested modified file' '
	(
	cd repo &&
	echo "nested mod" >sub/nested.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "sub/nested.txt" ../actual
	)
'

test_expect_success 'status shows nested new file' '
	(
	cd repo &&
	echo "new nested" >sub/new_nested.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "sub/" ../actual
	)
'

test_expect_success 'cleanup nested' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- sub/nested.txt &&
	rm -f sub/new_nested.txt
	)
'

###########################################################################
# Section 10: Branch header in porcelain
###########################################################################

test_expect_success 'status --porcelain shows branch header' '
	(
	cd repo &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^## master" ../actual
	)
'

test_expect_success 'status on new branch shows correct branch' '
	(
	cd repo &&
	"$REAL_GIT" checkout -b test-branch &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep "^## test-branch" ../actual
	)
'

test_expect_success 'switch back to master' '
	(
	cd repo &&
	"$REAL_GIT" checkout master
	)
'

###########################################################################
# Section 11: Status after various operations
###########################################################################

test_expect_success 'status after file mode change' '
	(
	cd repo &&
	chmod +x file3.txt &&
	"$GUST_BIN" status --porcelain >../actual &&
	"$REAL_GIT" status --porcelain >../expected &&
	grep -v "^##" ../expected | sort >../expected_sorted &&
	grep -v "^##" ../actual | sort >../actual_sorted &&
	test_cmp ../expected_sorted ../actual_sorted
	)
'

test_expect_success 'cleanup mode change' '
	(
	cd repo &&
	chmod -x file3.txt
	)
'

###########################################################################
# Section 12: Status short format
###########################################################################

test_expect_success 'status --short matches --porcelain for file entries' '
	(
	cd repo &&
	echo "change" >file1.txt &&
	"$GUST_BIN" status --porcelain >../porcelain &&
	"$GUST_BIN" status --short >../short &&
	grep -v "^##" ../porcelain | sort >../p_sorted &&
	grep -v "^##" ../short | sort >../s_sorted &&
	test_cmp ../p_sorted ../s_sorted
	)
'

test_expect_success 'status --short shows M for modified' '
	(
	cd repo &&
	"$GUST_BIN" status --short >../actual &&
	grep "M" ../actual | grep "file1.txt"
	)
'

test_expect_success 'cleanup' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- file1.txt
	)
'

test_expect_success 'final: clean status after all tests' '
	(
	cd repo &&
	"$GUST_BIN" status --porcelain >../actual &&
	grep -v "^##" ../actual >../filtered || true &&
	test_must_be_empty ../filtered
	)
'

test_done
