#!/bin/sh
# Tests for `grit diff-files` (index vs working tree).

test_description='diff-files compares working tree against the index'

. ./test-lib.sh

# ---------------------------------------------------------------------------
# Helper: create a commit from current index state.
# ---------------------------------------------------------------------------
make_commit () {
	msg=$1
	parent=${2-}
	tree=$(git write-tree) || return 1
	if test -n "$parent"
	then
		commit=$(printf '%s\n' "$msg" | git commit-tree "$tree" -p "$parent") || return 1
	else
		commit=$(printf '%s\n' "$msg" | git commit-tree "$tree") || return 1
	fi
	git update-ref HEAD "$commit" || return 1
	printf '%s\n' "$commit"
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	printf "hello\n" >file.txt &&
	git update-index --add file.txt &&
	c1=$(make_commit initial) &&
	test -n "$c1"
	)
'

# ---------------------------------------------------------------------------
# Basic: no changes
# ---------------------------------------------------------------------------
test_expect_success 'diff-files is silent when working tree matches index' '
	(
	cd repo &&
	git diff-files >out &&
	test ! -s out
	)
'

# ---------------------------------------------------------------------------
# Modified file
# ---------------------------------------------------------------------------
test_expect_success 'diff-files reports modified file' '
	(
	cd repo &&
	printf "world\n" >file.txt &&
	git diff-files >out &&
	grep " M	file.txt$" out
	)
'

test_expect_success 'diff-files raw output has correct format' '
	(
	cd repo &&
	git diff-files >out &&
	# Line starts with colon, two octal modes, two OIDs, status, path
	grep "^:100644 100644 [0-9a-f]\{40\} 0\{40\} M	file.txt$" out
	)
'

# ---------------------------------------------------------------------------
# Deleted file
# ---------------------------------------------------------------------------
test_expect_success 'diff-files reports deleted file' '
	(
	cd repo &&
	# restore worktree first, then delete
	printf "hello\n" >file.txt &&
	git update-index file.txt &&
	rm file.txt &&
	git diff-files >out &&
	grep " D	file.txt$" out
	)
'

# ---------------------------------------------------------------------------
# --exit-code
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --exit-code exits 1 when differences exist' '
	(
	cd repo &&
	# file.txt is currently deleted in worktree, stage still has it
	test_must_fail git diff-files --exit-code
	)
'

test_expect_success 'diff-files --exit-code exits 0 when no differences' '
	(
	cd repo &&
	printf "hello\n" >file.txt &&
	git update-index file.txt &&
	git diff-files --exit-code
	)
'

# ---------------------------------------------------------------------------
# --quiet
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --quiet suppresses output and exits 1' '
	(
	cd repo &&
	printf "changed\n" >file.txt &&
	test_must_fail git diff-files --quiet >quiet_out 2>/dev/null &&
	test ! -s quiet_out
	)
'

# ---------------------------------------------------------------------------
# --name-only
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --name-only prints only paths' '
	(
	cd repo &&
	git diff-files --name-only >out &&
	printf "file.txt\n" >expect &&
	test_cmp expect out
	)
'

# ---------------------------------------------------------------------------
# --name-status
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --name-status prints status and path' '
	(
	cd repo &&
	git diff-files --name-status >out &&
	printf "M\tfile.txt\n" >expect &&
	test_cmp expect out
	)
'

# ---------------------------------------------------------------------------
# Pathspec filtering
# ---------------------------------------------------------------------------
test_expect_success 'pathspec limits output' '
	(
	cd repo &&
	printf "other\n" >other.txt &&
	git update-index --add other.txt &&
	printf "other_mod\n" >other.txt &&
	# both file.txt and other.txt are modified; ask for only other.txt
	git diff-files -- other.txt >out &&
	grep "other.txt" out &&
	! grep "file.txt" out
	)
'

# ---------------------------------------------------------------------------
# --raw (explicit)
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --raw is equivalent to default' '
	(
	cd repo &&
	git diff-files >default_out &&
	git diff-files --raw >raw_out &&
	test_cmp default_out raw_out
	)
'

# ---------------------------------------------------------------------------
# Multiple files
# ---------------------------------------------------------------------------
test_expect_success 'diff-files reports multiple changed files' '
	(
	cd repo &&
	git diff-files >out &&
	lines=$(wc -l <out) &&
	test "$lines" -ge 2
	)
'

# ---------------------------------------------------------------------------
# --numstat
# ---------------------------------------------------------------------------
test_expect_success 'diff-files --numstat reports insertion/deletion counts' '
	(
	cd repo &&
	# restore both files and make known changes
	printf "line1\nline2\n" >file.txt &&
	git update-index file.txt &&
	printf "line1_mod\nline2\n" >file.txt &&
	git diff-files --numstat >out &&
	grep "file.txt" out
	)
'

# ---------------------------------------------------------------------------
# New file added to working tree but not staged
# ---------------------------------------------------------------------------
test_expect_success 'diff-files ignores untracked files' '
	(
	cd repo &&
	printf "extra\n" >untracked.txt &&
	git diff-files >out &&
	! grep "untracked.txt" out
	)
'

# ---------------------------------------------------------------------------
# Additional diff-files tests
# ---------------------------------------------------------------------------

test_expect_success 'diff-files -p shows unified patch output' '
	(
	cd repo &&
	printf "line1_mod\nline2\n" >file.txt &&
	git update-index file.txt &&
	printf "line1_changed\nline2\n" >file.txt &&
	git diff-files -p >out &&
	grep "^diff --git" out &&
	grep "^---" out &&
	grep "^+++" out &&
	grep "^@@" out
	)
'

test_expect_success 'diff-files with no changes after update-index' '
	(
	cd repo &&
	printf "stable\n" >stable.txt &&
	git update-index --add stable.txt &&
	git diff-files -- stable.txt >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-files --exit-code with pathspec' '
	(
	cd repo &&
	git diff-files --exit-code -- stable.txt
	)
'

# ---------------------------------------------------------------------------
# Additional diff-files tests
# ---------------------------------------------------------------------------

test_expect_success 'diff-files --stat shows diffstat' '
	(
	cd repo &&
	printf "changed_content\n" >file.txt &&
	git diff-files --stat >out &&
	grep "file.txt" out &&
	grep "changed" out
	)
'

test_expect_success 'diff-files with multiple modified files' '
	(
	cd repo &&
	printf "mod2\n" >other.txt &&
	git diff-files --name-only >out &&
	grep "file.txt" out &&
	grep "other.txt" out
	)
'

test_expect_success 'diff-files --name-status shows M for multiple files' '
	(
	cd repo &&
	git diff-files --name-status >out &&
	count=$(grep -c "^M" out) &&
	test "$count" -ge 2
	)
'

test_expect_success 'diff-files -p shows --- and +++ headers' '
	(
	cd repo &&
	git diff-files -p >out &&
	grep "^--- a/" out &&
	grep "^+++ b/" out
	)
'

test_expect_success 'diff-files -p shows @@ hunk header' '
	(
	cd repo &&
	git diff-files -p >out &&
	grep "^@@" out
	)
'

test_expect_success 'diff-files --numstat shows numeric counts' '
	(
	cd repo &&
	git diff-files --numstat >out &&
	grep "file.txt" out &&
	grep "^[0-9]" out
	)
'

test_expect_success 'diff-files pathspec limits to specific file' '
	(
	cd repo &&
	git diff-files --name-only -- file.txt >out &&
	grep "file.txt" out &&
	! grep "other.txt" out
	)
'

test_expect_success 'diff-files pathspec with non-matching path is empty' '
	(
	cd repo &&
	git diff-files --name-only -- nonexistent >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-files --exit-code with pathspec on clean file' '
	(
	cd repo &&
	git diff-files --exit-code -- stable.txt
	)
'

test_expect_success 'diff-files --exit-code with pathspec on dirty file fails' '
	(
	cd repo &&
	test_must_fail git diff-files --exit-code -- file.txt
	)
'

test_expect_success 'diff-files --quiet with pathspec on clean file returns 0' '
	(
	cd repo &&
	git diff-files --quiet -- stable.txt
	)
'

test_expect_success 'diff-files --quiet with pathspec on dirty file returns 1' '
	(
	cd repo &&
	test_must_fail git diff-files --quiet -- file.txt
	)
'

# ---------------------------------------------------------------------------
# Additional diff-files tests: subdirs, multiple modifications
# ---------------------------------------------------------------------------

test_expect_success 'setup: clean state and add subdir files' '
	(
	cd repo &&
	git update-index file.txt other.txt &&
	mkdir -p sub/deep &&
	echo "sub content" >sub/a.txt &&
	echo "deep content" >sub/deep/b.txt &&
	git update-index --add sub/a.txt sub/deep/b.txt
	)
'

test_expect_success 'diff-files clean after adding subdir files' '
	(
	cd repo &&
	git diff-files --exit-code
	)
'

test_expect_success 'diff-files detects changes in subdirectory' '
	(
	cd repo &&
	echo modified >sub/a.txt &&
	git diff-files --name-only >out &&
	grep "sub/a.txt" out
	)
'

test_expect_success 'diff-files --name-status shows M for subdir file' '
	(
	cd repo &&
	git diff-files --name-status >out &&
	grep "^M.*sub/a.txt" out
	)
'

test_expect_success 'diff-files detects change in deeply nested file' '
	(
	cd repo &&
	echo modified-deep >sub/deep/b.txt &&
	git diff-files --name-only >out &&
	grep "sub/deep/b.txt" out
	)
'

test_expect_success 'diff-files pathspec with subdir prefix' '
	(
	cd repo &&
	git diff-files --name-only -- sub/ >out &&
	grep "sub/a.txt" out &&
	grep "sub/deep/b.txt" out &&
	! grep "file.txt" out
	)
'

test_expect_success 'diff-files --numstat with subdir changes' '
	(
	cd repo &&
	git diff-files --numstat -- sub/a.txt >out &&
	grep "sub/a.txt" out
	)
'

test_expect_success 'diff-files --exit-code fails with subdir changes' '
	(
	cd repo &&
	test_must_fail git diff-files --exit-code -- sub/a.txt
	)
'

test_expect_success 'diff-files --quiet with subdir pathspec' '
	(
	cd repo &&
	test_must_fail git diff-files --quiet -- sub/
	)
'

test_expect_success 'diff-files reports deleted file in working tree' '
	(
	cd repo &&
	rm sub/a.txt &&
	git diff-files --name-status >out &&
	grep "^D.*sub/a.txt" out
	)
'

test_expect_success 'restore and verify clean' '
	(
	cd repo &&
	echo "sub content" >sub/a.txt &&
	echo "deep content" >sub/deep/b.txt &&
	git update-index sub/a.txt sub/deep/b.txt &&
	git diff-files --exit-code
	)
'

test_done
