#!/bin/sh
# Tests for diff header format: "diff --git a/X b/X", "index", "--- a/X", "+++ b/X"

test_description='grit diff header format verification'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ===========================================================================
# Setup
# ===========================================================================

test_expect_success 'setup: init repo with initial commit' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	printf "line1\nline2\nline3\n" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit" &&
	c1=$(git rev-parse HEAD) &&
	echo "$c1" >../c1
	)
'

test_expect_success 'setup: create second commit with modification' '
	(
	cd repo &&
	printf "line1\nmodified\nline3\n" >file.txt &&
	git add file.txt &&
	git commit -m "modify file.txt" &&
	c2=$(git rev-parse HEAD) &&
	echo "$c2" >../c2
	)
'

# ===========================================================================
# "diff --git a/X b/X" header line
# ===========================================================================

test_expect_success 'diff output starts with "diff --git a/X b/X"' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^diff --git a/file.txt b/file.txt$" actual
	)
'

test_expect_success 'diff --git header uses a/ and b/ prefixes' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	head -1 actual >first_line &&
	grep "^diff --git a/" first_line &&
	grep " b/" first_line
	)
'

# ===========================================================================
# "index" header line
# ===========================================================================

test_expect_success 'diff output includes "index" line with abbreviated OIDs' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^index [0-9a-f].*\.\.[0-9a-f]" actual
	)
'

test_expect_success 'index line contains mode when unchanged' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^index [0-9a-f].*\.\.[0-9a-f].* 100644$" actual
	)
'

test_expect_success 'index line OIDs match actual blob OIDs' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	index_line=$(grep "^index " actual) &&
	old_abbrev=$(echo "$index_line" | sed "s/^index \([0-9a-f]*\)\.\..*/\1/") &&
	new_abbrev=$(echo "$index_line" | sed "s/^index [0-9a-f]*\.\.\([0-9a-f]*\).*/\1/") &&
	old_blob=$(git rev-parse "$c1":file.txt) &&
	new_blob=$(git rev-parse "$c2":file.txt) &&
	echo "$old_blob" | grep "^$old_abbrev" &&
	echo "$new_blob" | grep "^$new_abbrev"
	)
'

# ===========================================================================
# "--- a/X" and "+++ b/X" header lines
# ===========================================================================

test_expect_success 'diff output includes "--- a/file.txt"' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^--- a/file.txt$" actual
	)
'

test_expect_success 'diff output includes "+++ b/file.txt"' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^+++ b/file.txt$" actual
	)
'

# ===========================================================================
# Header order
# ===========================================================================

test_expect_success 'headers appear in correct order: diff, index, ---, +++' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	diff_line=$(grep -n "^diff --git" actual | head -1 | cut -d: -f1) &&
	index_line=$(grep -n "^index " actual | head -1 | cut -d: -f1) &&
	minus_line=$(grep -n "^--- " actual | head -1 | cut -d: -f1) &&
	plus_line=$(grep -n "^+++ " actual | head -1 | cut -d: -f1) &&
	test "$diff_line" -lt "$index_line" &&
	test "$index_line" -lt "$minus_line" &&
	test "$minus_line" -lt "$plus_line"
	)
'

# ===========================================================================
# @@ hunk header
# ===========================================================================

test_expect_success 'diff includes @@ hunk header' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^@@" actual
	)
'

test_expect_success 'hunk header shows line numbers' '
	(
	cd repo &&
	c1=$(cat ../c1) && c2=$(cat ../c2) &&
	git diff "$c1" "$c2" >actual &&
	grep "^@@ -[0-9]" actual
	)
'

# ===========================================================================
# New file headers
# ===========================================================================

test_expect_success 'setup: add a new file in third commit' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	git add newfile.txt &&
	git commit -m "add newfile.txt" &&
	c3=$(git rev-parse HEAD) &&
	echo "$c3" >../c3
	)
'

test_expect_success 'new file diff shows "new file mode" line' '
	(
	cd repo &&
	c2=$(cat ../c2) && c3=$(cat ../c3) &&
	git diff "$c2" "$c3" >actual &&
	grep "^new file mode" actual
	)
'

test_expect_success 'new file diff shows --- /dev/null' '
	(
	cd repo &&
	c2=$(cat ../c2) && c3=$(cat ../c3) &&
	git diff "$c2" "$c3" >actual &&
	grep "^--- .*/dev/null" actual || grep "^--- a//dev/null" actual
	)
'

test_expect_success 'new file diff shows +++ b/newfile.txt' '
	(
	cd repo &&
	c2=$(cat ../c2) && c3=$(cat ../c3) &&
	git diff "$c2" "$c3" >actual &&
	grep "^+++ b/newfile.txt$" actual
	)
'

test_expect_success 'new file index line starts with 0000000' '
	(
	cd repo &&
	c2=$(cat ../c2) && c3=$(cat ../c3) &&
	git diff "$c2" "$c3" >actual &&
	grep "^index 0000000" actual
	)
'

# ===========================================================================
# Deleted file headers
# ===========================================================================

test_expect_success 'setup: delete file in fourth commit' '
	(
	cd repo &&
	git rm newfile.txt &&
	git commit -m "remove newfile.txt" &&
	c4=$(git rev-parse HEAD) &&
	echo "$c4" >../c4
	)
'

test_expect_success 'deleted file diff shows "deleted file mode" line' '
	(
	cd repo &&
	c3=$(cat ../c3) && c4=$(cat ../c4) &&
	git diff "$c3" "$c4" >actual &&
	grep "^deleted file mode" actual
	)
'

test_expect_success 'deleted file diff shows --- a/newfile.txt' '
	(
	cd repo &&
	c3=$(cat ../c3) && c4=$(cat ../c4) &&
	git diff "$c3" "$c4" >actual &&
	grep "^--- a/newfile.txt$" actual
	)
'

test_expect_success 'deleted file diff shows +++ /dev/null' '
	(
	cd repo &&
	c3=$(cat ../c3) && c4=$(cat ../c4) &&
	git diff "$c3" "$c4" >actual &&
	grep "^+++ .*/dev/null" actual || grep "^+++ b//dev/null" actual
	)
'

# ===========================================================================
# Mode change headers
# ===========================================================================

test_expect_success 'setup: change file mode' '
	(
	cd repo &&
	chmod +x file.txt &&
	git add file.txt &&
	git commit -m "make file.txt executable" &&
	c5=$(git rev-parse HEAD) &&
	echo "$c5" >../c5
	)
'

test_expect_success 'mode change diff shows old mode and new mode' '
	(
	cd repo &&
	c4=$(cat ../c4) && c5=$(cat ../c5) &&
	git diff "$c4" "$c5" >actual &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual
	)
'

# ===========================================================================
# Multiple file diff
# ===========================================================================

test_expect_success 'setup: modify multiple files' '
	(
	cd repo &&
	echo "aaa" >a.txt &&
	echo "zzz" >z.txt &&
	git add a.txt z.txt &&
	git commit -m "add a.txt and z.txt" &&
	c6=$(git rev-parse HEAD) &&
	echo "aaa modified" >a.txt &&
	echo "zzz modified" >z.txt &&
	git add a.txt z.txt &&
	git commit -m "modify both" &&
	c7=$(git rev-parse HEAD) &&
	echo "$c6" >../c6 &&
	echo "$c7" >../c7
	)
'

test_expect_success 'multi-file diff has separate diff --git headers' '
	(
	cd repo &&
	c6=$(cat ../c6) && c7=$(cat ../c7) &&
	git diff "$c6" "$c7" >actual &&
	grep "^diff --git a/a.txt b/a.txt$" actual &&
	grep "^diff --git a/z.txt b/z.txt$" actual
	)
'

test_expect_success 'multi-file diff has separate --- and +++ for each file' '
	(
	cd repo &&
	c6=$(cat ../c6) && c7=$(cat ../c7) &&
	git diff "$c6" "$c7" >actual &&
	grep "^--- a/a.txt$" actual &&
	grep "^+++ b/a.txt$" actual &&
	grep "^--- a/z.txt$" actual &&
	grep "^+++ b/z.txt$" actual
	)
'

test_expect_success 'multi-file diff files appear in alphabetical order' '
	(
	cd repo &&
	c6=$(cat ../c6) && c7=$(cat ../c7) &&
	git diff "$c6" "$c7" >actual &&
	a_line=$(grep -n "^diff --git a/a.txt" actual | cut -d: -f1) &&
	z_line=$(grep -n "^diff --git a/z.txt" actual | cut -d: -f1) &&
	test "$a_line" -lt "$z_line"
	)
'

# ===========================================================================
# Diff of file in subdirectory
# ===========================================================================

test_expect_success 'setup: file in subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/nested.txt &&
	git add sub/deep/nested.txt &&
	git commit -m "add nested file" &&
	c8=$(git rev-parse HEAD) &&
	echo "nested modified" >sub/deep/nested.txt &&
	git add sub/deep/nested.txt &&
	git commit -m "modify nested file" &&
	c9=$(git rev-parse HEAD) &&
	echo "$c8" >../c8 &&
	echo "$c9" >../c9
	)
'

test_expect_success 'subdirectory file diff --git header uses full path' '
	(
	cd repo &&
	c8=$(cat ../c8) && c9=$(cat ../c9) &&
	git diff "$c8" "$c9" >actual &&
	grep "^diff --git a/sub/deep/nested.txt b/sub/deep/nested.txt$" actual
	)
'

test_expect_success 'subdirectory file --- and +++ use full path' '
	(
	cd repo &&
	c8=$(cat ../c8) && c9=$(cat ../c9) &&
	git diff "$c8" "$c9" >actual &&
	grep "^--- a/sub/deep/nested.txt$" actual &&
	grep "^+++ b/sub/deep/nested.txt$" actual
	)
'

# ===========================================================================
# Working tree diff headers (diff-files style)
# ===========================================================================

test_expect_success 'working tree diff shows correct headers' '
	(
	cd repo &&
	echo "dirty" >>file.txt &&
	git diff >actual &&
	grep "^diff --git a/file.txt b/file.txt$" actual &&
	grep "^--- a/file.txt$" actual &&
	grep "^+++ b/file.txt$" actual &&
	git checkout -- file.txt
	)
'

test_expect_success 'staged diff shows correct headers' '
	(
	cd repo &&
	echo "staged change" >>file.txt &&
	git add file.txt &&
	git diff --cached >actual &&
	grep "^diff --git a/file.txt b/file.txt$" actual &&
	grep "^--- a/file.txt$" actual &&
	grep "^+++ b/file.txt$" actual &&
	git checkout -- file.txt
	)
'

test_done
