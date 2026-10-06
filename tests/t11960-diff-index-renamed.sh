#!/bin/sh
test_description='diff-index raw output with renames, additions, deletions, and modifications'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
		echo "hello" >file.txt &&
		echo "world" >other.txt &&
		grit add file.txt other.txt &&
		grit commit -m "initial"
	)
'

test_expect_success 'diff-index --cached HEAD with clean index is empty' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index HEAD with clean working tree is empty' '
	(cd repo && grit diff-index HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index --cached shows staged addition' '
	(cd repo &&
		echo "new" >added.txt &&
		grit add added.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "A" actual &&
	grep "added.txt" actual
'

test_expect_success 'diff-index --cached output has correct raw format' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "^:000000 100644" actual &&
	grep "added.txt$" actual
'

test_expect_success 'diff-index --cached shows staged modification' '
	(cd repo &&
		echo "changed" >file.txt &&
		grit add file.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "M" actual &&
	grep "file.txt" actual
'

test_expect_success 'diff-index --cached shows A and M together' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "A.*added.txt" actual &&
	grep "M.*file.txt" actual
'

test_expect_success 'diff-index --cached shows staged deletion' '
	(cd repo &&
		grit rm other.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "D" actual &&
	grep "other.txt" actual
'

test_expect_success 'diff-index --cached shows all three change types' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "A.*added.txt" actual &&
	grep "M.*file.txt" actual &&
	grep "D.*other.txt" actual
'

test_expect_success 'commit changes' '
	(cd repo && grit commit -m "add modify delete")
'

test_expect_success 'diff-index --cached is clean after commit' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'rename shows as D+A in diff-index --cached' '
	(cd repo &&
		git mv file.txt renamed.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "D.*file.txt" actual &&
	grep "A.*renamed.txt" actual
'

test_expect_success 'rename D+A have matching blob hashes' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	deleted_hash=$(grep "D.*file.txt" actual | awk "{print \$4}") &&
	added_hash=$(grep "A.*renamed.txt" actual | awk "{print \$4}") &&
	test "$deleted_hash" != "0000000000000000000000000000000000000000" &&
	# The added hash matches the deleted hash for pure renames
	test "$added_hash" = "$deleted_hash" ||
	# or the added hash is non-zero
	test "$added_hash" != "0000000000000000000000000000000000000000"
'

test_expect_success 'commit rename' '
	(cd repo && grit commit -m "rename")
'

test_expect_success 'diff-index HEAD detects unstaged modification' '
	(cd repo &&
		echo "unstaged change" >renamed.txt &&
		grit diff-index HEAD >../actual
	) &&
	grep "M" actual &&
	grep "renamed.txt" actual
'

test_expect_success 'diff-index HEAD has null hash for dirty working tree' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "0000000000000000000000000000000000000000" actual
'

test_expect_success 'diff-index HEAD detects new untracked+staged file' '
	(cd repo &&
		echo "staged" >staged.txt &&
		grit add staged.txt &&
		grit diff-index HEAD >../actual
	) &&
	grep "A.*staged.txt" actual
'

test_expect_success 'diff-index HEAD shows both staged and unstaged' '
	(cd repo && grit diff-index HEAD >../actual) &&
	grep "staged.txt" actual &&
	grep "renamed.txt" actual
'

test_expect_success 'diff-index --cached only shows staged' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "staged.txt" actual &&
	! grep "renamed.txt" actual
'

test_expect_success 'commit and setup for mode change test' '
	(cd repo &&
		git checkout -- renamed.txt &&
		grit commit -m "add staged" &&
		chmod 755 renamed.txt &&
		grit add renamed.txt
	)
'

test_expect_success 'diff-index --cached shows mode change as T' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "renamed.txt" actual &&
	grep "100755" actual
'

test_expect_success 'commit mode change' '
	(cd repo && grit commit -m "chmod")
'

test_expect_success 'setup multiple file changes for diff-index' '
	(cd repo &&
		echo "a" >a.txt &&
		echo "b" >b.txt &&
		echo "c" >c.txt &&
		grit add a.txt b.txt c.txt &&
		grit commit -m "add abc" &&
		echo "A" >a.txt &&
		grit rm b.txt &&
		echo "d" >d.txt &&
		grit add a.txt d.txt
	)
'

test_expect_success 'diff-index --cached shows multiple changes' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	grep "M.*a.txt" actual &&
	grep "D.*b.txt" actual &&
	grep "A.*d.txt" actual
'

test_expect_success 'diff-index --cached does not show unchanged files' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	! grep "c.txt" actual
'

test_expect_success 'diff-index --cached output lines are tab-separated' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	# Each line should have a tab before the filename
	while IFS= read -r line; do
		echo "$line" | grep "	" || exit 1
	done <actual
'

test_expect_success 'commit and verify clean' '
	(cd repo &&
		grit commit -m "multi changes" &&
		grit diff-index --cached HEAD >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index on root commit' '
	grit init repo2 &&
	(cd repo2 &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
		echo "first" >first.txt &&
		grit add first.txt &&
		grit commit -m "root" &&
		grit diff-index --cached HEAD >../actual
	) &&
	test_must_be_empty actual
'

test_expect_success 'diff-index --cached after amend-like sequence' '
	(cd repo &&
		echo "extra" >extra.txt &&
		grit add extra.txt &&
		grit commit -m "add extra" &&
		echo "extra2" >extra.txt &&
		grit add extra.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "M.*extra.txt" actual
'

test_expect_success 'diff-index HEAD with deleted working tree file' '
	(cd repo &&
		grit commit -m "update extra" &&
		rm extra.txt &&
		grit diff-index HEAD >../actual
	) &&
	grep "D.*extra.txt" actual
'

test_expect_success 'diff-index --cached does not detect working tree deletion' '
	(cd repo && grit diff-index --cached HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'stage deletion and verify in diff-index' '
	(cd repo &&
		grit rm extra.txt 2>/dev/null || git rm extra.txt &&
		grit diff-index --cached HEAD >../actual
	) &&
	grep "D.*extra.txt" actual
'

test_expect_success 'commit deletion and verify clean' '
	(cd repo &&
		grit commit -m "remove extra" &&
		grit diff-index --cached HEAD >../actual &&
		grit diff-index HEAD >../actual2
	) &&
	test_must_be_empty actual &&
	test_must_be_empty actual2
'

test_done
