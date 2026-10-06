#!/bin/sh
# Tests for grit diff with empty files, empty diffs, no-change scenarios

test_description='grit diff empty files and no-change scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repo' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "content" >file.txt &&
	echo "other" >other.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# === no changes ===

test_expect_success 'diff is empty when no changes' '
	(
	cd repo &&
	git diff >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --cached is empty when nothing staged' '
	(
	cd repo &&
	git diff --cached >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --exit-code returns 0 when clean' '
	(
	cd repo &&
	git diff --exit-code
	)
'

test_expect_success 'diff --quiet returns 0 when clean' '
	(
	cd repo &&
	git diff --quiet
	)
'

test_expect_success 'diff --stat is empty when clean' '
	(
	cd repo &&
	git diff --stat >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --numstat is empty when clean' '
	(
	cd repo &&
	git diff --numstat >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --name-only is empty when clean' '
	(
	cd repo &&
	git diff --name-only >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --name-status is empty when clean' '
	(
	cd repo &&
	git diff --name-status >../actual &&
	test_must_be_empty ../actual
	)
'

# === empty file operations ===

test_expect_success 'add empty file and diff --cached shows it' '
	(
	cd repo &&
	: >empty.txt &&
	git add empty.txt &&
	git diff --cached >../actual &&
	grep "empty.txt" ../actual
	)
'

test_expect_success 'diff --cached --name-status shows A for empty file' '
	(
	cd repo &&
	git diff --cached --name-status >../actual &&
	grep "^A.*empty.txt" ../actual
	)
'

test_expect_success 'commit empty file' '
	(
	cd repo &&
	git commit -m "add empty file"
	)
'

test_expect_success 'diff is empty after committing empty file' '
	(
	cd repo &&
	git diff >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'add content to previously empty file detected by diff' '
	(
	cd repo &&
	echo "now has content" >empty.txt &&
	git diff --name-only >../actual &&
	grep "empty.txt" ../actual &&
	git diff --exit-code && exit 1 || true
	)
'

test_expect_success 'cached diff shows + lines for content added to empty file' '
	(
	cd repo &&
	git add empty.txt &&
	git diff --cached >../actual &&
	grep "+now has content" ../actual &&
	git reset HEAD empty.txt &&
	git checkout -- empty.txt
	)
'

test_expect_success 'truncate file to empty detected by diff' '
	(
	cd repo &&
	: >file.txt &&
	git diff --name-only >../actual &&
	grep "file.txt" ../actual
	)
'

test_expect_success 'diff --stat for truncated file' '
	(
	cd repo &&
	git diff --stat >../actual &&
	grep "file.txt" ../actual
	)
'

test_expect_success 'cached diff shows removal when truncating to empty' '
	(
	cd repo &&
	git add file.txt &&
	git diff --cached >../actual &&
	grep "file.txt" ../actual &&
	grep "^-content" ../actual &&
	git reset HEAD file.txt &&
	git checkout -- file.txt
	)
'

# === same commit diff ===

test_expect_success 'diff HEAD HEAD is empty' '
	(
	cd repo &&
	git diff HEAD HEAD >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff HEAD HEAD --exit-code returns 0' '
	(
	cd repo &&
	git diff --exit-code HEAD HEAD
	)
'

test_expect_success 'diff --stat HEAD HEAD is empty' '
	(
	cd repo &&
	git diff --stat HEAD HEAD >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff --name-only HEAD HEAD is empty' '
	(
	cd repo &&
	git diff --name-only HEAD HEAD >../actual &&
	test_must_be_empty ../actual
	)
'

# === re-stage same content (no actual change) ===

test_expect_success 'staging unchanged file produces empty cached diff' '
	(
	cd repo &&
	git add file.txt &&
	git diff --cached >../actual &&
	test_must_be_empty ../actual
	)
'

# === write same content produces no diff ===

test_expect_success 'overwriting with same content produces no diff' '
	(
	cd repo &&
	echo "content" >file.txt &&
	git diff >../actual &&
	test_must_be_empty ../actual
	)
'

# === empty file to empty file (no change) ===

test_expect_success 'touching empty file produces no diff' '
	(
	cd repo &&
	touch empty.txt &&
	git diff >../actual &&
	test_must_be_empty ../actual
	)
'

# === delete and re-create with same content ===

test_expect_success 'delete and recreate with same content shows no diff' '
	(
	cd repo &&
	rm file.txt &&
	echo "content" >file.txt &&
	git diff >../actual &&
	test_must_be_empty ../actual
	)
'

# === diff between commits where file is unchanged ===

test_expect_success 'setup: change only one file in new commit' '
	(
	cd repo &&
	echo "changed other" >other.txt &&
	git add other.txt &&
	git commit -m "change other only"
	)
'

test_expect_success 'diff between commits -- file.txt is empty (file unchanged)' '
	(
	cd repo &&
	git diff HEAD~1 HEAD -- file.txt >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'diff between commits -- other.txt shows changes' '
	(
	cd repo &&
	git diff HEAD~1 HEAD -- other.txt >../actual &&
	grep "other.txt" ../actual
	)
'

# === diff --cached for empty to non-empty ===

test_expect_success 'diff --cached detects change from empty to content' '
	(
	cd repo &&
	echo "filled" >empty.txt &&
	git add empty.txt &&
	git diff --cached >../actual &&
	grep "+filled" ../actual &&
	git reset HEAD empty.txt &&
	git checkout -- empty.txt
	)
'

# === diff --exit-code for various no-change scenarios ===

test_expect_success 'diff --exit-code 0 after touching unmodified file' '
	(
	cd repo &&
	touch file.txt &&
	git diff --exit-code
	)
'

test_expect_success 'diff --cached --exit-code 0 when nothing staged' '
	(
	cd repo &&
	git diff --cached --exit-code
	)
'

test_done
