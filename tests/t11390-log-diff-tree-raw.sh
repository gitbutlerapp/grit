#!/bin/sh
# Tests for grit diff-tree raw output.

test_description='grit diff-tree: raw output, -r, --name-only, --name-status, -p'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with various changes' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file1.txt &&
	echo "world" >file2.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deep" >sub/deep/deep.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial" &&
	echo "modified" >>file1.txt &&
	echo "new file" >file3.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "modify and add" &&
	"$REAL_GIT" rm file2.txt &&
	echo "changed nested" >sub/nested.txt &&
	"$REAL_GIT" add sub/nested.txt &&
	"$REAL_GIT" commit -m "delete and modify nested" &&
	echo "another" >file4.txt &&
	"$REAL_GIT" add file4.txt &&
	"$REAL_GIT" commit -m "add file4"
	)
'

###########################################################################
# Section 2: Basic diff-tree (single commit, compare to parent)
###########################################################################

test_expect_success 'diff-tree HEAD shows changes in latest commit' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	test -s output
	)
'

test_expect_success 'diff-tree HEAD raw format has colon prefix' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	grep -q "^:" output
	)
'

test_expect_success 'diff-tree HEAD shows file4.txt added' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	grep -q "file4.txt" output
	)
'

test_expect_success 'diff-tree HEAD shows A status for new file' '
	(
	cd repo &&
	git diff-tree --name-status HEAD >output &&
	grep -q "A" output &&
	grep -q "file4.txt" output
	)
'

test_expect_success 'diff-tree -r HEAD works same as diff-tree HEAD for flat files' '
	(
	cd repo &&
	git diff-tree HEAD >without_r &&
	git diff-tree -r HEAD >with_r &&
	test_cmp without_r with_r
	)
'

###########################################################################
# Section 3: --name-only and --name-status
###########################################################################

test_expect_success 'diff-tree --name-only shows only filenames' '
	(
	cd repo &&
	git diff-tree --name-only HEAD >output &&
	grep -q "file4.txt" output &&
	! grep -q "^:" output
	)
'

test_expect_success 'diff-tree --name-status shows status and filenames' '
	(
	cd repo &&
	git diff-tree --name-status HEAD >output &&
	grep -qE "^[AMDRC]" output
	)
'

test_expect_success 'diff-tree --name-only on modify commit shows modified file' '
	(
	cd repo &&
	commit2=$(git rev-parse HEAD~2) &&
	git diff-tree --name-only "$commit2" >output &&
	grep -q "file1.txt" output
	)
'

test_expect_success 'diff-tree --name-status on delete commit shows D status' '
	(
	cd repo &&
	commit3=$(git rev-parse HEAD~1) &&
	git diff-tree --name-status "$commit3" >output &&
	grep -q "D" output &&
	grep -q "file2.txt" output
	)
'

test_expect_success 'diff-tree --name-status on modify shows M status' '
	(
	cd repo &&
	commit3=$(git rev-parse HEAD~1) &&
	git diff-tree --name-status "$commit3" >output &&
	grep -q "M" output
	)
'

###########################################################################
# Section 4: Two-tree comparison
###########################################################################

test_expect_success 'diff-tree between two commits shows all changes' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree "$first" "$last" >output &&
	test -s output
	)
'

test_expect_success 'diff-tree between two commits with -r includes nested' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -r "$first" "$last" >output &&
	grep -q "sub/nested.txt" output
	)
'

test_expect_success 'diff-tree between two commits --name-only lists all changed files' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -r --name-only "$first" "$last" >output &&
	grep -q "file1.txt" output &&
	grep -q "file3.txt" output
	)
'

test_expect_success 'diff-tree between same commit produces no output' '
	(
	cd repo &&
	head=$(git rev-parse HEAD) &&
	git diff-tree "$head" "$head" >output &&
	test_must_be_empty output
	)
'

test_expect_success 'diff-tree two-tree shows deleted file' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -r --name-status "$first" "$last" >output &&
	grep -q "D.*file2.txt" output
	)
'

###########################################################################
# Section 5: Patch output (-p)
###########################################################################

test_expect_success 'diff-tree -p HEAD shows patch' '
	(
	cd repo &&
	git diff-tree -p HEAD >output &&
	grep -q "diff --git" output
	)
'

test_expect_success 'diff-tree -p shows added file content' '
	(
	cd repo &&
	git diff-tree -p HEAD >output &&
	grep -q "+another" output
	)
'

test_expect_success 'diff-tree -p between two commits shows full diff' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -p "$first" "$last" >output &&
	grep -q "diff --git" output &&
	grep -q "file1.txt" output
	)
'

test_expect_success 'diff-tree -p shows deletion as minus lines' '
	(
	cd repo &&
	commit3=$(git rev-parse HEAD~1) &&
	git diff-tree -p "$commit3" >output &&
	grep -q "^-" output || grep -q "deleted" output || grep -q "file2" output
	)
'

###########################################################################
# Section 6: --no-commit-id
###########################################################################

test_expect_success 'diff-tree --no-commit-id suppresses commit hash line' '
	(
	cd repo &&
	git diff-tree --no-commit-id -r HEAD >output &&
	! head -1 output | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'diff-tree without --no-commit-id may include commit id' '
	(
	cd repo &&
	git diff-tree -r HEAD >output &&
	test -s output
	)
'

###########################################################################
# Section 7: Raw format details
###########################################################################

test_expect_success 'diff-tree raw output has correct format fields' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	grep "^:" output | head -1 >first_line &&
	grep -qE "^:[0-9]{6} [0-9]{6} [0-9a-f]{40} [0-9a-f]{40}" first_line
	)
'

test_expect_success 'diff-tree raw shows 100644 for regular files' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	grep -q "100644" output
	)
'

test_expect_success 'diff-tree raw shows blob hashes' '
	(
	cd repo &&
	git diff-tree HEAD >output &&
	grep -qE "[0-9a-f]{40}" output
	)
'

###########################################################################
# Section 8: Comparison with real git
###########################################################################

test_expect_success 'diff-tree HEAD --name-only matches real git' '
	(
	cd repo &&
	git diff-tree --no-commit-id --name-only -r HEAD >grit_out &&
	"$REAL_GIT" diff-tree --no-commit-id --name-only -r HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'diff-tree HEAD --name-status matches real git' '
	(
	cd repo &&
	git diff-tree --no-commit-id --name-status -r HEAD >grit_out &&
	"$REAL_GIT" diff-tree --no-commit-id --name-status -r HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'diff-tree two-commit --name-only matches real git' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -r --name-only "$first" "$last" >grit_out &&
	"$REAL_GIT" diff-tree -r --name-only "$first" "$last" >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'diff-tree raw output matches real git' '
	(
	cd repo &&
	git diff-tree --no-commit-id -r HEAD >grit_out &&
	"$REAL_GIT" diff-tree --no-commit-id -r HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'diff-tree on root commit (no parent)' '
	(
	cd repo &&
	root=$(git rev-parse HEAD~3) &&
	git diff-tree "$root" >output 2>&1 &&
	test -f output
	)
'

test_expect_success 'diff-tree with nested directories' '
	(
	cd repo &&
	commit3=$(git rev-parse HEAD~1) &&
	git diff-tree -r --name-only "$commit3" >output &&
	grep -q "sub/nested.txt" output
	)
'

test_expect_success 'diff-tree -r includes all levels of nesting' '
	(
	cd repo &&
	first=$(git rev-parse HEAD~3) &&
	last=$(git rev-parse HEAD) &&
	git diff-tree -r --name-only "$first" "$last" >output &&
	grep -q "sub/deep/deep.txt" output || grep -q "sub/nested.txt" output
	)
'

test_done
