#!/bin/sh
# Test diff-files for detecting deleted, modified, and mode-changed files
# in the working tree vs the index.

test_description='grit diff-files with deletions and working tree changes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with several files' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "alpha" >alpha.txt &&
	echo "beta" >beta.txt &&
	echo "gamma" >gamma.txt &&
	echo "delta" >delta.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/nested.txt &&
	echo "inner" >sub/inner.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Clean working tree
###########################################################################

test_expect_success 'diff-files on clean tree is empty' '
	(
	cd repo &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 2: Single file deletion
###########################################################################

test_expect_success 'diff-files detects deleted file' '
	(
	cd repo &&
	rm alpha.txt &&
	grit diff-files >out &&
	grep "alpha.txt" out
	)
'

test_expect_success 'diff-files shows D status for deleted file' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "D	alpha.txt" out
	)
'

test_expect_success 'diff-files output has colon-prefixed raw format' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "^:100644" out
	)
'

test_expect_success 'diff-files shows old blob hash for deleted file' '
	(
	cd repo &&
	grit diff-files >out &&
	hash=$(cat out | awk "{print \$3}") &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'staging deletion clears diff-files' '
	(
	cd repo &&
	grit add alpha.txt &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

test_expect_success 'restore file and commit deletion' '
	(
	cd repo &&
	grit commit -m "remove alpha"
	)
'

###########################################################################
# Section 3: Multiple file deletions
###########################################################################

test_expect_success 'diff-files detects multiple deletions' '
	(
	cd repo &&
	rm beta.txt gamma.txt &&
	grit diff-files >out &&
	grep "beta.txt" out &&
	grep "gamma.txt" out
	)
'

test_expect_success 'both deleted files show D status' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "D	beta.txt" out &&
	grep "D	gamma.txt" out
	)
'

test_expect_success 'non-deleted file not in diff-files' '
	(
	cd repo &&
	grit diff-files >out &&
	! grep "delta.txt" out
	)
'

test_expect_success 'staging one deletion reduces diff-files output' '
	(
	cd repo &&
	grit add beta.txt &&
	grit diff-files >out &&
	! grep "beta.txt" out &&
	grep "gamma.txt" out
	)
'

test_expect_success 'staging all and committing clears diff-files' '
	(
	cd repo &&
	grit add gamma.txt &&
	grit commit -m "remove beta and gamma" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 4: Nested file deletion
###########################################################################

test_expect_success 'diff-files detects nested file deletion' '
	(
	cd repo &&
	rm sub/deep/nested.txt &&
	grit diff-files >out &&
	grep "sub/deep/nested.txt" out
	)
'

test_expect_success 'nested deletion shows D status' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "D	sub/deep/nested.txt" out
	)
'

test_expect_success 'sibling file not affected' '
	(
	cd repo &&
	grit diff-files >out &&
	! grep "sub/inner.txt" out
	)
'

test_expect_success 'stage and commit nested deletion' '
	(
	cd repo &&
	grit add sub/deep/nested.txt &&
	grit commit -m "remove nested" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 5: File modification (not deletion)
###########################################################################

test_expect_success 'diff-files detects content modification' '
	(
	cd repo &&
	echo "modified" >delta.txt &&
	grit diff-files >out &&
	grep "delta.txt" out
	)
'

test_expect_success 'modified file shows M status' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "M	delta.txt" out
	)
'

test_expect_success 'staging modification clears it from diff-files' '
	(
	cd repo &&
	grit add delta.txt &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

test_expect_success 'commit modification' '
	(
	cd repo &&
	grit commit -m "modify delta"
	)
'

###########################################################################
# Section 6: Mixed deletion and modification
###########################################################################

test_expect_success 'diff-files shows both deletion and modification' '
	(
	cd repo &&
	rm sub/inner.txt &&
	echo "delta v3" >delta.txt &&
	grit diff-files >out &&
	grep "D	sub/inner.txt" out &&
	grep "M	delta.txt" out
	)
'

test_expect_success 'diff-files output has two entries for mixed changes' '
	(
	cd repo &&
	grit diff-files >out &&
	count=$(wc -l <out) &&
	test "$count" -eq 2
	)
'

test_expect_success 'stage and commit mixed changes' '
	(
	cd repo &&
	grit add sub/inner.txt delta.txt &&
	grit commit -m "delete inner, modify delta" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 7: Mode changes
###########################################################################

test_expect_success 'diff-files detects chmod +x' '
	(
	cd repo &&
	chmod +x delta.txt &&
	grit diff-files >out &&
	grep "delta.txt" out
	)
'

test_expect_success 'diff-files mode change shows M status' '
	(
	cd repo &&
	grit diff-files >out &&
	grep "M	delta.txt" out
	)
'

test_expect_success 'restore mode clears diff-files' '
	(
	cd repo &&
	chmod -x delta.txt &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 8: Delete and re-create file
###########################################################################

test_expect_success 'delete file and recreate with same content' '
	(
	cd repo &&
	content=$(cat delta.txt) &&
	rm delta.txt &&
	echo "$content" >delta.txt &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

test_expect_success 'delete file and recreate with different content' '
	(
	cd repo &&
	rm delta.txt &&
	echo "completely new" >delta.txt &&
	grit diff-files >out &&
	grep "delta.txt" out
	)
'

test_expect_success 'stage recreated file' '
	(
	cd repo &&
	grit add delta.txt &&
	grit commit -m "update delta content" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 9: All files deleted
###########################################################################

test_expect_success 'diff-files when all tracked files deleted' '
	(
	cd repo &&
	rm delta.txt &&
	grit diff-files >out &&
	grep "D	delta.txt" out
	)
'

test_expect_success 'stage all deletions' '
	(
	cd repo &&
	grit add delta.txt &&
	grit commit -m "remove last file" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 10: Fresh files after empty state
###########################################################################

test_expect_success 'add new files after all deleted' '
	(
	cd repo &&
	echo "fresh1" >f1.txt &&
	echo "fresh2" >f2.txt &&
	grit add f1.txt f2.txt &&
	grit commit -m "fresh files" &&
	grit diff-files >out &&
	test_must_be_empty out
	)
'

test_expect_success 'modify and delete new files detected' '
	(
	cd repo &&
	echo "changed" >f1.txt &&
	rm f2.txt &&
	grit diff-files >out &&
	grep "M	f1.txt" out &&
	grep "D	f2.txt" out
	)
'

test_done
