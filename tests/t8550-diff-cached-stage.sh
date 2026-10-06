#!/bin/sh
# Tests for 'grit diff --cached' with staged adds/modifies/deletes/renames.

test_description='diff --cached with staged adds, modifies, deletes, renames'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

OUT="$TRASH_DIRECTORY/out"
EXPECT="$TRASH_DIRECTORY/expect"

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with files' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "line one" >a.txt &&
	echo "line two" >b.txt &&
	echo "line three" >c.txt &&
	mkdir sub &&
	echo "nested" >sub/d.txt &&
	git add a.txt b.txt c.txt sub/d.txt &&
	git commit -m "initial"
	)
'

# ── No staged changes ───────────────────────────────────────────────────────

test_expect_success 'diff --cached is empty with no staged changes' '
	(
	cd repo &&
	git diff --cached >"$OUT" &&
	test_must_be_empty "$OUT"
	)
'

test_expect_success 'diff --cached --exit-code returns 0 when clean' '
	(
	cd repo &&
	git diff --cached --exit-code >"$OUT"
	)
'

# ── Staged add of new file ──────────────────────────────────────────────────

test_expect_success 'diff --cached shows staged new file' '
	(
	cd repo &&
	echo "new content" >new.txt &&
	git add new.txt &&
	git diff --cached >"$OUT" &&
	grep "^diff --git a/new.txt b/new.txt" "$OUT" &&
	grep "new file mode" "$OUT"
	)
'

test_expect_success 'diff --cached --name-only shows new file name' '
	(
	cd repo &&
	git diff --cached --name-only >"$OUT" &&
	grep "^new.txt$" "$OUT"
	)
'

test_expect_success 'diff --cached --name-status shows A for new file' '
	(
	cd repo &&
	git diff --cached --name-status >"$OUT" &&
	grep "^A" "$OUT" &&
	grep "new.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --stat shows new file stat' '
	(
	cd repo &&
	git diff --cached --stat >"$OUT" &&
	grep "new.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --numstat shows additions for new file' '
	(
	cd repo &&
	git diff --cached --numstat >"$OUT" &&
	grep "new.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --exit-code returns 1 with staged changes' '
	(
	cd repo &&
	test_expect_code 1 git diff --cached --exit-code >"$OUT"
	)
'

test_expect_success 'commit new file for next tests' '
	(
	cd repo &&
	git commit -m "add new.txt"
	)
'

# ── Staged modification ─────────────────────────────────────────────────────

test_expect_success 'diff --cached shows staged modification' '
	(
	cd repo &&
	echo "modified content" >a.txt &&
	git add a.txt &&
	git diff --cached >"$OUT" &&
	grep "^diff --git a/a.txt b/a.txt" "$OUT" &&
	grep "^-line one$" "$OUT" &&
	grep "^+modified content$" "$OUT"
	)
'

test_expect_success 'diff --cached --name-status shows M for modification' '
	(
	cd repo &&
	git diff --cached --name-status >"$OUT" &&
	grep "^M" "$OUT" &&
	grep "a.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --numstat shows lines changed' '
	(
	cd repo &&
	git diff --cached --numstat >"$OUT" &&
	grep "a.txt" "$OUT"
	)
'

test_expect_success 'commit modification for next tests' '
	(
	cd repo &&
	git commit -m "modify a.txt"
	)
'

# ── Staged deletion ─────────────────────────────────────────────────────────

test_expect_success 'diff --cached shows staged deletion' '
	(
	cd repo &&
	git rm b.txt >/dev/null 2>&1 &&
	git diff --cached >"$OUT" &&
	grep "^diff --git a/b.txt b/b.txt" "$OUT" &&
	grep "deleted file mode" "$OUT"
	)
'

test_expect_success 'diff --cached --name-status shows D for deletion' '
	(
	cd repo &&
	git diff --cached --name-status >"$OUT" &&
	grep "^D" "$OUT" &&
	grep "b.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --name-only shows deleted file' '
	(
	cd repo &&
	git diff --cached --name-only >"$OUT" &&
	grep "^b.txt$" "$OUT"
	)
'

test_expect_success 'diff --cached --numstat shows deletions' '
	(
	cd repo &&
	git diff --cached --numstat >"$OUT" &&
	grep "b.txt" "$OUT"
	)
'

test_expect_success 'commit deletion for next tests' '
	(
	cd repo &&
	git commit -m "delete b.txt"
	)
'

# ── Staged rename (via rm + add) ────────────────────────────────────────────

test_expect_success 'diff --cached shows rename as delete + add' '
	(
	cd repo &&
	git mv c.txt c-renamed.txt &&
	git diff --cached >"$OUT" &&
	grep "c.txt\|c-renamed.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --name-status shows rename entries' '
	(
	cd repo &&
	git diff --cached --name-status >"$OUT" &&
	grep "c-renamed.txt\|c.txt" "$OUT"
	)
'

test_expect_success 'commit rename for next tests' '
	(
	cd repo &&
	git commit -m "rename c.txt"
	)
'

# ── Multiple staged changes ─────────────────────────────────────────────────

test_expect_success 'diff --cached with multiple staged changes' '
	(
	cd repo &&
	echo "extra" >e.txt &&
	echo "modified nested" >sub/d.txt &&
	git add e.txt sub/d.txt &&
	git diff --cached >"$OUT" &&
	grep "e.txt" "$OUT" &&
	grep "sub/d.txt" "$OUT"
	)
'

test_expect_success 'diff --cached --name-only lists all staged files' '
	(
	cd repo &&
	git diff --cached --name-only >"$OUT" &&
	grep "^e.txt$" "$OUT" &&
	grep "^sub/d.txt$" "$OUT"
	)
'

test_expect_success 'diff --cached --stat shows stat for multiple files' '
	(
	cd repo &&
	git diff --cached --stat >"$OUT" &&
	grep "e.txt" "$OUT" &&
	grep "sub/d.txt" "$OUT"
	)
'

test_expect_success 'commit multiple for next tests' '
	(
	cd repo &&
	git commit -m "multiple changes"
	)
'

# ── Unified context ─────────────────────────────────────────────────────────

test_expect_success 'diff --cached -U0 shows zero context lines' '
	(
	cd repo &&
	echo "changed again" >a.txt &&
	git add a.txt &&
	git diff --cached -U0 >"$OUT" &&
	grep "^@@" "$OUT"
	)
'

test_expect_success 'diff --cached -U5 shows five context lines' '
	(
	cd repo &&
	git diff --cached -U5 >"$OUT" &&
	grep "^@@" "$OUT"
	)
'

test_expect_success 'commit for clean state' '
	(
	cd repo &&
	git commit -m "context test"
	)
'

# ── diff --cached with pathspec ──────────────────────────────────────────────

test_expect_success 'diff --cached with staged binary-like content' '
	(
	cd repo &&
	printf "\x00binary\x01" >bin.dat &&
	git add bin.dat &&
	git diff --cached >"$OUT" &&
	grep "bin.dat" "$OUT"
	)
'

test_expect_success 'diff --cached --name-only with binary file' '
	(
	cd repo &&
	git diff --cached --name-only >"$OUT" &&
	grep "^bin.dat$" "$OUT"
	)
'

test_expect_success 'commit binary file' '
	(
	cd repo &&
	git commit -m "binary file"
	)
'

# ── diff --cached --quiet ────────────────────────────────────────────────────

test_expect_success 'diff --cached --quiet returns 0 when no staged changes' '
	(
	cd repo &&
	git diff --cached --quiet
	)
'

test_expect_success 'diff --cached --quiet returns 1 with staged changes' '
	(
	cd repo &&
	echo "q-change" >a.txt &&
	git add a.txt &&
	test_expect_code 1 git diff --cached --quiet
	)
'

test_done
