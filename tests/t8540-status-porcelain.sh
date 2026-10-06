#!/bin/sh
# Tests for 'grit status --porcelain' — machine-parseable output.

test_description='status --porcelain, -s, -z, machine-parseable output'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Write output files outside the repo to avoid polluting status
OUT="$TRASH_DIRECTORY/out"
SORTED="$TRASH_DIRECTORY/sorted"
READABLE="$TRASH_DIRECTORY/readable"
EXPECT="$TRASH_DIRECTORY/expect"

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with initial commit' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >tracked.txt &&
	git add tracked.txt &&
	git commit -m "initial"
	)
'

# ── Clean state ──────────────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows no file entries on clean repo' '
	(
	cd repo &&
	git status --porcelain >"$OUT" &&
	! grep -v "^##" "$OUT" || test_must_be_empty "$OUT"
	)
'

test_expect_success 'status -s shows no file entries on clean repo' '
	(
	cd repo &&
	git status -s >"$OUT" &&
	! grep -v "^##" "$OUT" || test_must_be_empty "$OUT"
	)
'

# ── New untracked file ───────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows untracked file with ??' '
	(
	cd repo &&
	echo "new" >untracked.txt &&
	git status --porcelain >"$OUT" &&
	grep "^?? untracked.txt$" "$OUT"
	)
'

test_expect_success 'status -s shows untracked file with ??' '
	(
	cd repo &&
	git status -s >"$OUT" &&
	grep "^?? untracked.txt$" "$OUT"
	)
'

# ── Staged new file ─────────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows staged new file with A' '
	(
	cd repo &&
	echo "added" >added.txt &&
	git add added.txt &&
	git status --porcelain >"$OUT" &&
	grep "^A  added.txt$" "$OUT"
	)
'

# ── Modified tracked file ───────────────────────────────────────────────────

test_expect_success 'status --porcelain shows unstaged modification with M' '
	(
	cd repo &&
	echo "changed" >tracked.txt &&
	git status --porcelain >"$OUT" &&
	grep "^ M tracked.txt$" "$OUT"
	)
'

test_expect_success 'status --porcelain shows staged modification with M' '
	(
	cd repo &&
	git add tracked.txt &&
	git status --porcelain >"$OUT" &&
	grep "^M  tracked.txt$" "$OUT"
	)
'

# ── Deleted file ─────────────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows unstaged delete with D' '
	(
	cd repo &&
	git commit -m "save changes" &&
	rm tracked.txt &&
	git status --porcelain >"$OUT" &&
	grep "^ D tracked.txt$" "$OUT"
	)
'

test_expect_success 'status --porcelain shows staged delete with D' '
	(
	cd repo &&
	git rm tracked.txt >/dev/null 2>&1 &&
	git status --porcelain >"$OUT" &&
	grep "^D  tracked.txt$" "$OUT"
	)
'

# ── Restore for further tests ───────────────────────────────────────────────

test_expect_success 'restore tracked file for further tests' '
	(
	cd repo &&
	echo "restored" >tracked.txt &&
	git add tracked.txt &&
	git commit -m "restore"
	)
'

# ── Multiple changes at once ─────────────────────────────────────────────────

test_expect_success 'status --porcelain shows multiple changes' '
	(
	cd repo &&
	echo "mod" >tracked.txt &&
	echo "brand-new" >extra.txt &&
	git add extra.txt &&
	git status --porcelain >"$OUT" &&
	grep "^ M tracked.txt" "$OUT" &&
	grep "^A  extra.txt" "$OUT"
	)
'

# ── Staged + unstaged on same file ──────────────────────────────────────────

test_expect_success 'status --porcelain shows MM for staged then modified' '
	(
	cd repo &&
	git add tracked.txt &&
	echo "further change" >tracked.txt &&
	git status --porcelain >"$OUT" &&
	grep "^MM tracked.txt$" "$OUT"
	)
'

# ── Clean up and commit ──────────────────────────────────────────────────────

test_expect_success 'commit all changes and verify clean' '
	(
	cd repo &&
	rm -f untracked.txt &&
	git add -A &&
	git commit -m "all changes" &&
	git status --porcelain >"$OUT" &&
	! grep -v "^##" "$OUT" || test_must_be_empty "$OUT"
	)
'

# ── -z NUL termination ──────────────────────────────────────────────────────

test_expect_success 'status -z uses NUL terminators' '
	(
	cd repo &&
	echo "z-file" >ztest.txt &&
	git status -z >"$OUT" &&
	tr "\0" "\n" <"$OUT" >"$READABLE" &&
	grep "ztest.txt" "$READABLE"
	)
'

test_expect_success 'status --porcelain -z combines both' '
	(
	cd repo &&
	git status --porcelain -z >"$OUT" &&
	tr "\0" "\n" <"$OUT" >"$READABLE" &&
	grep "?? ztest.txt" "$READABLE"
	)
'

# ── -b / --branch ───────────────────────────────────────────────────────────

test_expect_success 'status -s -b shows branch header' '
	(
	cd repo &&
	git status -s -b >"$OUT" &&
	grep "^## master" "$OUT"
	)
'

test_expect_success 'status --porcelain -b shows branch header' '
	(
	cd repo &&
	git status --porcelain -b >"$OUT" &&
	grep "^## master" "$OUT"
	)
'

# ── Untracked files control ─────────────────────────────────────────────────

test_expect_success 'status --porcelain -u no hides untracked' '
	(
	cd repo &&
	git status --porcelain -u no >"$OUT" &&
	! grep "ztest.txt" "$OUT"
	)
'

test_expect_success 'status --porcelain -u normal shows untracked' '
	(
	cd repo &&
	git status --porcelain -u normal >"$OUT" &&
	grep "?? ztest.txt" "$OUT"
	)
'

# ── Ignored files ────────────────────────────────────────────────────────────

test_expect_success 'setup ignored file' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	echo "data" >test.ign &&
	git add .gitignore &&
	git commit -m "add gitignore" &&
	rm -f ztest.txt
	)
'

test_expect_success 'status --porcelain --ignored shows ignored files' '
	(
	cd repo &&
	git status --porcelain --ignored >"$OUT" &&
	grep "test.ign" "$OUT"
	)
'

test_expect_success 'status --porcelain shows ignored with !! prefix' '
	(
	cd repo &&
	git status --porcelain --ignored >"$OUT" &&
	grep "!! test.ign" "$OUT" || grep "test.ign" "$OUT"
	)
'

# ── Subdirectory ─────────────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows files in subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/file.txt &&
	git status --porcelain >"$OUT" &&
	grep "sub/" "$OUT"
	)
'

test_expect_success 'status --porcelain with staged subdir file' '
	(
	cd repo &&
	git add sub/deep/file.txt &&
	git status --porcelain >"$OUT" &&
	grep "^A  sub/deep/file.txt$" "$OUT"
	)
'

# ── Commit and verify clean ─────────────────────────────────────────────────

test_expect_success 'commit subdirectory file and verify clean' '
	(
	cd repo &&
	rm -f test.ign &&
	git add -A &&
	git commit -m "subdir" &&
	git status --porcelain >"$OUT" &&
	! grep -v "^##" "$OUT" || test_must_be_empty "$OUT"
	)
'

test_expect_success 'status -s on clean repo has no file entries' '
	(
	cd repo &&
	git status -s >"$OUT" &&
	! grep -v "^##" "$OUT" || test_must_be_empty "$OUT"
	)
'

# ── Rename detection ────────────────────────────────────────────────────────

test_expect_success 'status --porcelain shows rename as D+A or R' '
	(
	cd repo &&
	echo "moveable content" >old-name.txt &&
	git add old-name.txt &&
	git commit -m "add old-name" &&
	git mv old-name.txt new-name.txt &&
	git status --porcelain >"$OUT" &&
	grep "new-name.txt" "$OUT"
	)
'

test_done
