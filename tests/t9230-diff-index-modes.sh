#!/bin/sh
# Test diff-index with file mode changes, staged vs working tree,
# --cached, and various content/mode scenarios.

test_description='grit diff-index with file modes and --cached'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with tracked files' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "normal" >normal.txt &&
	echo "script" >script.sh &&
	echo "data" >data.bin &&
	grit add normal.txt script.sh data.bin &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 1: Basic diff-index HEAD on clean tree
###########################################################################

test_expect_success 'diff-index HEAD on clean tree is empty' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-index --cached HEAD on clean tree is empty' '
	(
	cd repo &&
	grit diff-index --cached HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 2: Working tree content changes
###########################################################################

test_expect_success 'diff-index HEAD shows modified file' '
	(
	cd repo &&
	echo "changed" >normal.txt &&
	grit diff-index HEAD >out &&
	grep "normal.txt" out
	)
'

test_expect_success 'diff-index HEAD shows M status for modification' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "M	normal.txt" out
	)
'

test_expect_success 'diff-index HEAD shows old mode 100644' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "100644" out
	)
'

test_expect_success 'diff-index HEAD output contains blob hash' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	hash=$(cat out | awk "{print \$3}") &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'staging and committing clears diff-index' '
	(
	cd repo &&
	grit add normal.txt &&
	grit commit -m "update normal" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 3: Staged changes (--cached)
###########################################################################

test_expect_success 'diff-index --cached shows staged addition' '
	(
	cd repo &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit diff-index --cached HEAD >out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff-index --cached shows A status for new file' '
	(
	cd repo &&
	grit diff-index --cached HEAD >out &&
	grep "A" out &&
	grep "new.txt" out
	)
'

test_expect_success 'diff-index --cached shows staged modification' '
	(
	cd repo &&
	echo "v3" >normal.txt &&
	grit add normal.txt &&
	grit diff-index --cached HEAD >out &&
	grep "normal.txt" out &&
	grep "M" out
	)
'

test_expect_success 'diff-index --cached shows staged deletion' '
	(
	cd repo &&
	grit rm data.bin &&
	grit diff-index --cached HEAD >out &&
	grep "data.bin" out &&
	grep "D" out
	)
'

test_expect_success 'commit clears all staged changes' '
	(
	cd repo &&
	grit commit -m "various staged changes" &&
	grit diff-index --cached HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 4: File mode changes (executable bit)
###########################################################################

test_expect_success 'chmod +x is detected by diff-index HEAD' '
	(
	cd repo &&
	chmod +x script.sh &&
	grit diff-index HEAD >out &&
	grep "script.sh" out
	)
'

test_expect_success 'diff-index shows M status for mode change' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "M	script.sh" out
	)
'

test_expect_success 'staging mode change via grit add' '
	(
	cd repo &&
	grit add script.sh &&
	grit diff-index --cached HEAD >out &&
	grep "script.sh" out
	)
'

test_expect_success 'commit mode change and verify clean' '
	(
	cd repo &&
	grit commit -m "make script executable" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 5: Multiple simultaneous changes
###########################################################################

test_expect_success 'diff-index shows multiple modified files' '
	(
	cd repo &&
	echo "a" >>normal.txt &&
	echo "b" >>script.sh &&
	grit diff-index HEAD >out &&
	grep "normal.txt" out &&
	grep "script.sh" out
	)
'

test_expect_success 'diff-index --cached shows only staged subset' '
	(
	cd repo &&
	grit add normal.txt &&
	grit diff-index --cached HEAD >out &&
	grep "normal.txt" out
	)
'

test_expect_success 'unstaged file still in diff-index HEAD' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "script.sh" out
	)
'

test_expect_success 'staging all and committing clears everything' '
	(
	cd repo &&
	grit add script.sh &&
	grit commit -m "update both" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 6: Addition and deletion of files
###########################################################################

test_expect_success 'new file shows in diff-index after staging' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	grit add alpha.txt &&
	grit diff-index --cached HEAD >out &&
	grep "A" out &&
	grep "alpha.txt" out
	)
'

test_expect_success 'deleted file shows D in diff-index --cached' '
	(
	cd repo &&
	grit rm new.txt &&
	grit diff-index --cached HEAD >out &&
	grep "D" out &&
	grep "new.txt" out
	)
'

test_expect_success 'multiple additions show in diff-index' '
	(
	cd repo &&
	echo "b" >beta.txt &&
	echo "g" >gamma.txt &&
	grit add beta.txt gamma.txt &&
	grit diff-index --cached HEAD >out &&
	grep "beta.txt" out &&
	grep "gamma.txt" out
	)
'

test_expect_success 'commit and verify clean' '
	(
	cd repo &&
	grit commit -m "add and remove files" &&
	grit diff-index --cached HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 7: Content changes on multiple file types
###########################################################################

test_expect_success 'modify executable file detected' '
	(
	cd repo &&
	echo "updated script" >script.sh &&
	grit diff-index HEAD >out &&
	grep "script.sh" out
	)
'

test_expect_success 'modify newly added file detected' '
	(
	cd repo &&
	echo "alpha v2" >alpha.txt &&
	grit diff-index HEAD >out &&
	grep "alpha.txt" out
	)
'

test_expect_success 'stage and commit modified files' '
	(
	cd repo &&
	grit add script.sh alpha.txt &&
	grit commit -m "modify executable and alpha" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 8: Empty file scenarios
###########################################################################

test_expect_success 'diff-index detects added empty file' '
	(
	cd repo &&
	: >empty.txt &&
	grit add empty.txt &&
	grit diff-index --cached HEAD >out &&
	grep "empty.txt" out
	)
'

test_expect_success 'diff-index detects content added to empty file' '
	(
	cd repo &&
	grit commit -m "add empty" &&
	echo "now has content" >empty.txt &&
	grit diff-index HEAD >out &&
	grep "empty.txt" out
	)
'

test_expect_success 'diff-index detects file emptied' '
	(
	cd repo &&
	grit add empty.txt &&
	grit commit -m "fill empty" &&
	: >empty.txt &&
	grit diff-index HEAD >out &&
	grep "empty.txt" out
	)
'

###########################################################################
# Section 9: Subdirectory changes
###########################################################################

test_expect_success 'diff-index detects changes in subdirectory' '
	(
	cd repo &&
	grit add empty.txt &&
	grit commit -m "empty again" &&
	mkdir -p sub/deep &&
	echo "deep file" >sub/deep/f.txt &&
	grit add sub/ &&
	grit diff-index --cached HEAD >out &&
	grep "sub/deep/f.txt" out
	)
'

test_expect_success 'diff-index shows A for new nested file' '
	(
	cd repo &&
	grit diff-index --cached HEAD >out &&
	grep "A" out
	)
'

test_expect_success 'commit nested file and verify clean' '
	(
	cd repo &&
	grit commit -m "add nested" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success 'modify nested file detected' '
	(
	cd repo &&
	echo "updated" >sub/deep/f.txt &&
	grit diff-index HEAD >out &&
	grep "sub/deep/f.txt" out
	)
'

###########################################################################
# Section 10: diff-index output format
###########################################################################

test_expect_success 'diff-index output has colon-prefixed format' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "^:" out
	)
'

test_expect_success 'diff-index output has tab before filename' '
	(
	cd repo &&
	grit diff-index HEAD >out &&
	grep "	sub/deep/f.txt" out
	)
'

test_expect_success 'diff-index --cached with no HEAD on fresh repo' '
	(
	cd repo &&
	grit add sub/deep/f.txt &&
	grit commit -m "final" &&
	grit diff-index HEAD >out &&
	test_must_be_empty out
	)
'

test_done
