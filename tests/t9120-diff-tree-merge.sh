#!/bin/sh
# Test diff-tree with merges and various output formats.

test_description='grit diff-tree with merges and output options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Real git for operations grit does not support (merge, log --merges).
REAL_GIT=/usr/bin/git

###########################################################################
# Setup: create a history with merges
###########################################################################

test_expect_success 'setup linear history and save SHAs' '
	(
	grit init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "test@test.com" &&
	$REAL_GIT config user.name "Tester" &&
	echo "base" >base.txt &&
	grit add base.txt &&
	grit commit -m "initial" &&
	grit rev-parse HEAD >../SHA_INITIAL &&
	echo "second line" >>base.txt &&
	grit add base.txt &&
	grit commit -m "modify base" &&
	grit rev-parse HEAD >../SHA_MODIFY
	)
'

test_expect_success 'setup feature branch and merge' '
	(
	cd repo &&
	$REAL_GIT checkout -b feature &&
	echo "feature work" >feature.txt &&
	grit add feature.txt &&
	grit commit -m "add feature" &&
	grit rev-parse HEAD >../SHA_FEATURE &&
	$REAL_GIT checkout master &&
	echo "main work" >main.txt &&
	grit add main.txt &&
	grit commit -m "add main" &&
	grit rev-parse HEAD >../SHA_MAIN &&
	$REAL_GIT merge feature --no-edit &&
	grit rev-parse HEAD >../SHA_MERGE
	)
'

###########################################################################
# Section 1: Basic diff-tree between two commits
###########################################################################

test_expect_success 'diff-tree between initial and modify shows base.txt' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_INITIAL) $(cat ../SHA_MODIFY) >out &&
	grep "base.txt" out
	)
'

test_expect_success 'diff-tree shows status letter A for new file' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_MODIFY) $(cat ../SHA_FEATURE) >out &&
	grep "A" out &&
	grep "feature.txt" out
	)
'

test_expect_success 'diff-tree between identical commits produces no output' '
	(
	cd repo &&
	sha=$(cat ../SHA_MODIFY) &&
	grit diff-tree $sha $sha >out &&
	test_must_be_empty out
	)
'

###########################################################################
# Section 2: diff-tree with single commit argument
###########################################################################

test_expect_success 'diff-tree with single non-merge commit compares to parent' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_MODIFY) >out &&
	grep "base.txt" out
	)
'

test_expect_success 'diff-tree with single merge commit shows combined diff' '
	(
	cd repo &&
	grit diff-tree $(cat ../SHA_MERGE) >out &&
	grep "feature.txt" out
	)
'

###########################################################################
# Section 3: diff-tree with -r flag
###########################################################################

test_expect_success 'diff-tree -r shows recursive listing for nested files' '
	(
	cd repo &&
	mkdir -p sub/dir &&
	echo "nested" >sub/dir/nested.txt &&
	grit add sub/ &&
	grit commit -m "add nested" &&
	parent=$(grit rev-parse HEAD~1) &&
	grit diff-tree -r $parent HEAD >out &&
	grep "sub/dir/nested.txt" out
	)
'

test_expect_success 'diff-tree -r with two trees shows all changed files' '
	(
	cd repo &&
	echo "another" >another.txt &&
	grit add another.txt &&
	grit commit -m "add another" &&
	grit diff-tree -r HEAD~2 HEAD >out &&
	grep "sub/dir/nested.txt" out &&
	grep "another.txt" out
	)
'

###########################################################################
# Section 4: diff-tree with --name-only
###########################################################################

test_expect_success 'diff-tree --name-only shows just filenames' '
	(
	cd repo &&
	grit diff-tree --name-only HEAD~1 HEAD >out &&
	grep "^another.txt$" out
	)
'

test_expect_success 'diff-tree -r --name-only with nested files' '
	(
	cd repo &&
	grit diff-tree -r --name-only HEAD~2 HEAD >out &&
	grep "^sub/dir/nested.txt$" out &&
	grep "^another.txt$" out
	)
'

###########################################################################
# Section 5: diff-tree with --name-status
###########################################################################

test_expect_success 'diff-tree --name-status shows A for addition' '
	(
	cd repo &&
	grit diff-tree --name-status HEAD~1 HEAD >out &&
	grep "A" out &&
	grep "another.txt" out
	)
'

test_expect_success 'diff-tree --name-status shows D for deletion' '
	(
	cd repo &&
	grit rm another.txt &&
	grit commit -m "remove another" &&
	grit diff-tree --name-status HEAD~1 HEAD >out &&
	grep "D" out &&
	grep "another.txt" out
	)
'

test_expect_success 'diff-tree --name-status shows M for modification' '
	(
	cd repo &&
	echo "more base" >>base.txt &&
	grit add base.txt &&
	grit commit -m "modify base again" &&
	grit diff-tree --name-status HEAD~1 HEAD >out &&
	grep "M" out &&
	grep "base.txt" out
	)
'

###########################################################################
# Section 6: diff-tree with --stat
###########################################################################

test_expect_success 'diff-tree --stat shows diffstat' '
	(
	cd repo &&
	grit diff-tree --stat HEAD~1 HEAD >out &&
	grep "base.txt" out &&
	grep "1 file changed" out
	)
'

test_expect_success 'diff-tree --stat for multi-file commit' '
	(
	cd repo &&
	echo "x" >x.txt &&
	echo "y" >y.txt &&
	grit add x.txt y.txt &&
	grit commit -m "two files" &&
	grit diff-tree --stat HEAD~1 HEAD >out &&
	grep "x.txt" out &&
	grep "y.txt" out &&
	grep "2 files changed" out
	)
'

###########################################################################
# Section 7: diff-tree with -p (patch)
###########################################################################

test_expect_success 'diff-tree -p shows unified diff headers' '
	(
	cd repo &&
	grit diff-tree -p HEAD~1 HEAD >out &&
	grep "^diff --git" out &&
	grep "^@@" out
	)
'

test_expect_success 'diff-tree -p shows added lines with plus prefix' '
	(
	cd repo &&
	grit diff-tree -p HEAD~1 HEAD >out &&
	grep "^+x$" out
	)
'

test_expect_success 'diff-tree -p for deletion shows minus prefix' '
	(
	cd repo &&
	grit rm y.txt &&
	grit commit -m "del y" &&
	grit diff-tree -p HEAD~1 HEAD >out &&
	grep "^-y$" out
	)
'

###########################################################################
# Section 8: diff-tree merge commit analysis
###########################################################################

test_expect_success 'diff-tree on merge shows introduced files' '
	(
	cd repo &&
	merge=$(cat ../SHA_MERGE) &&
	grit diff-tree $merge >out &&
	grep "feature.txt" out
	)
'

test_expect_success 'diff-tree between merge parents shows differences' '
	(
	cd repo &&
	merge=$(cat ../SHA_MERGE) &&
	p1=$(grit rev-parse ${merge}^1) &&
	p2=$(grit rev-parse ${merge}^2) &&
	grit diff-tree $p1 $p2 >out &&
	test -s out
	)
'

test_expect_success 'diff-tree -p on merge shows patch' '
	(
	cd repo &&
	merge=$(cat ../SHA_MERGE) &&
	grit diff-tree -p $merge >out &&
	grep "feature.txt" out
	)
'

test_expect_success 'diff-tree --stat on merge shows stat' '
	(
	cd repo &&
	merge=$(cat ../SHA_MERGE) &&
	grit diff-tree --stat $merge >out &&
	grep "feature.txt" out
	)
'

###########################################################################
# Section 9: diff-tree across branches
###########################################################################

test_expect_success 'setup second branch' '
	(
	cd repo &&
	$REAL_GIT checkout -b other &&
	echo "other content" >other.txt &&
	grit add other.txt &&
	grit commit -m "other branch" &&
	$REAL_GIT checkout master
	)
'

test_expect_success 'diff-tree between branch tips' '
	(
	cd repo &&
	master_tip=$(grit rev-parse master) &&
	other_tip=$(grit rev-parse other) &&
	grit diff-tree $master_tip $other_tip >out &&
	grep "other.txt" out
	)
'

test_expect_success 'diff-tree --name-only between branches' '
	(
	cd repo &&
	master_tip=$(grit rev-parse master) &&
	other_tip=$(grit rev-parse other) &&
	grit diff-tree --name-only $master_tip $other_tip >out &&
	grep "^other.txt$" out
	)
'

test_expect_success 'diff-tree --stat between branches' '
	(
	cd repo &&
	master_tip=$(grit rev-parse master) &&
	other_tip=$(grit rev-parse other) &&
	grit diff-tree --stat $master_tip $other_tip >out &&
	grep "other.txt" out
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'diff-tree on root commit' '
	(
	cd repo &&
	root=$(cat ../SHA_INITIAL) &&
	grit diff-tree $root >out &&
	cat out
	)
'

test_expect_success 'diff-tree -r between widely separated commits' '
	(
	cd repo &&
	first=$(cat ../SHA_INITIAL) &&
	last=$(grit rev-parse HEAD) &&
	grit diff-tree -r $first $last >out &&
	test -s out
	)
'

test_done
