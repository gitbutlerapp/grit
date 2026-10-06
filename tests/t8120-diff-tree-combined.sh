#!/bin/sh
# Tests for diff-tree: comparing trees, raw/name-only/name-status/patch/stat formats.

test_description='diff-tree formats and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Use system git for operations grit does not support (merge),
# and grit for the commands under test.
SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with linear history' '
	(
	"$SYS_GIT" init repo &&
	cd repo &&
	"$SYS_GIT" config user.name "Test User" &&
	"$SYS_GIT" config user.email "test@example.com" &&
	echo "base content" >base.txt &&
	echo "shared" >shared.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "initial commit" &&
	echo "added file" >added.txt &&
	"$SYS_GIT" add added.txt &&
	"$SYS_GIT" commit -m "add file" &&
	echo "modified base" >base.txt &&
	"$SYS_GIT" add base.txt &&
	"$SYS_GIT" commit -m "modify base"
	)
'

# ── Basic two-commit diff-tree ───────────────────────────────────────────

test_expect_success 'diff-tree: two commits shows raw diff' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "base.txt" out
	)
'

test_expect_success 'diff-tree: same commit produces no output' '
	(
	cd repo &&
	git diff-tree HEAD HEAD >out &&
	test_must_be_empty out
	)
'

test_expect_success 'diff-tree: shows added file between commits' '
	(
	cd repo &&
	git diff-tree HEAD~2 HEAD~1 >out &&
	grep "added.txt" out
	)
'

test_expect_success 'diff-tree: single commit arg diffs against parent' '
	(
	cd repo &&
	git diff-tree HEAD >out &&
	grep "base.txt" out
	)
'

# ── Raw format details ──────────────────────────────────────────────────

test_expect_success 'diff-tree: raw format has colon prefix' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "^:" out
	)
'

test_expect_success 'diff-tree: raw format shows old and new modes' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep -E "^:[0-9]{6} [0-9]{6}" out
	)
'

test_expect_success 'diff-tree: raw format contains SHA-1 hashes' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep -E "[0-9a-f]{40}" out
	)
'

test_expect_success 'diff-tree: modification shows M status' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "M" out | grep "base.txt"
	)
'

test_expect_success 'diff-tree: addition shows A status' '
	(
	cd repo &&
	git diff-tree HEAD~2 HEAD~1 >out &&
	grep "A" out | grep "added.txt"
	)
'

# ── --name-only ──────────────────────────────────────────────────────────

test_expect_success 'diff-tree --name-only: shows only filenames' '
	(
	cd repo &&
	git diff-tree --name-only HEAD~1 HEAD >out &&
	grep "base.txt" out &&
	! grep "^:" out
	)
'

test_expect_success 'diff-tree --name-only: addition shows filename' '
	(
	cd repo &&
	git diff-tree --name-only HEAD~2 HEAD~1 >out &&
	grep "added.txt" out
	)
'

# ── --name-status ────────────────────────────────────────────────────────

test_expect_success 'diff-tree --name-status: shows status and filename' '
	(
	cd repo &&
	git diff-tree --name-status HEAD~1 HEAD >out &&
	grep "^M" out | grep "base.txt"
	)
'

test_expect_success 'diff-tree --name-status: addition shows A' '
	(
	cd repo &&
	git diff-tree --name-status HEAD~2 HEAD~1 >out &&
	grep "^A" out | grep "added.txt"
	)
'

# ── --no-commit-id ───────────────────────────────────────────────────────

test_expect_success 'diff-tree --no-commit-id: no commit hash line' '
	(
	cd repo &&
	git diff-tree --no-commit-id HEAD >out &&
	! grep "^[0-9a-f]\{40\}$" out
	)
'

# ── -p (patch) ───────────────────────────────────────────────────────────

test_expect_success 'diff-tree -p: shows diff header' '
	(
	cd repo &&
	git diff-tree -p HEAD~1 HEAD >out &&
	grep "^diff --git" out
	)
'

test_expect_success 'diff-tree -p: shows old and new content' '
	(
	cd repo &&
	git diff-tree -p HEAD~1 HEAD >out &&
	grep "^-base content" out &&
	grep "^+modified base" out
	)
'

test_expect_success 'diff-tree -p: shows hunk header' '
	(
	cd repo &&
	git diff-tree -p HEAD~1 HEAD >out &&
	grep "^@@" out
	)
'

# ── --stat ───────────────────────────────────────────────────────────────

test_expect_success 'diff-tree --stat: shows diffstat' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >out &&
	grep "base.txt" out
	)
'

test_expect_success 'diff-tree --stat: shows insertions/deletions' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >out &&
	grep "1" out
	)
'

# ── -r (recursive) ──────────────────────────────────────────────────────

test_expect_success 'diff-tree -r: recursive mode shows files' '
	(
	cd repo &&
	git diff-tree -r HEAD~1 HEAD >out &&
	grep "base.txt" out
	)
'

# ── Deletion ─────────────────────────────────────────────────────────────

test_expect_success 'setup: commit with file deletion' '
	(
	cd repo &&
	"$SYS_GIT" rm shared.txt &&
	"$SYS_GIT" commit -m "remove shared"
	)
'

test_expect_success 'diff-tree: deletion shows D status' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "D" out | grep "shared.txt"
	)
'

test_expect_success 'diff-tree --name-status: deletion shows D' '
	(
	cd repo &&
	git diff-tree --name-status HEAD~1 HEAD >out &&
	grep "^D" out | grep "shared.txt"
	)
'

test_expect_success 'diff-tree -p: deletion shows removed content' '
	(
	cd repo &&
	git diff-tree -p HEAD~1 HEAD >out &&
	grep "^-shared" out
	)
'

# ── Multiple files changed ──────────────────────────────────────────────

test_expect_success 'setup: commit with multiple new files' '
	(
	cd repo &&
	echo "new1" >new1.txt &&
	echo "new2" >new2.txt &&
	echo "new3" >new3.txt &&
	"$SYS_GIT" add new1.txt new2.txt new3.txt &&
	"$SYS_GIT" commit -m "add three files"
	)
'

test_expect_success 'diff-tree: shows all added files' '
	(
	cd repo &&
	git diff-tree HEAD~1 HEAD >out &&
	grep "new1.txt" out &&
	grep "new2.txt" out &&
	grep "new3.txt" out
	)
'

test_expect_success 'diff-tree --name-only: lists all new files' '
	(
	cd repo &&
	git diff-tree --name-only HEAD~1 HEAD >out &&
	grep "new1.txt" out &&
	grep "new2.txt" out &&
	grep "new3.txt" out
	)
'

test_expect_success 'diff-tree --stat: shows stat for all new files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >out &&
	grep "new1.txt" out &&
	grep "new2.txt" out &&
	grep "new3.txt" out
	)
'

# ── Tree objects directly ────────────────────────────────────────────────

test_expect_success 'diff-tree: compare tree objects' '
	(
	cd repo &&
	tree1=$("$SYS_GIT" rev-parse HEAD~1^{tree}) &&
	tree2=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	git diff-tree "$tree1" "$tree2" >out &&
	grep "new1.txt" out
	)
'

test_done
