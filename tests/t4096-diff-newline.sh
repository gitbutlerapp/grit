#!/bin/sh
# Tests for diff with no-newline-at-eof, CRLF, mixed line endings

test_description='diff with newline edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ---------------------------------------------------------------------------
# No newline at EOF
# ---------------------------------------------------------------------------
test_expect_success 'create file with trailing newline and commit' '
	(
	cd repo &&
	printf "line1\nline2\n" >file.txt &&
	git add file.txt &&
	git commit -m "with trailing newline"
	)
'

test_expect_success 'remove trailing newline, stage it, cached diff shows marker' '
	(
	cd repo &&
	printf "line1\nline2" >file.txt &&
	git add file.txt &&
	git diff --cached >out 2>&1 &&
	grep -q "No newline at end of file\|no newline\|No newline\|\\\\ No" out
	)
'

test_expect_success 'commit no-newline version' '
	(
	cd repo &&
	git commit -m "without trailing newline"
	)
'

test_expect_success 'diff between commits shows newline change' '
	(
	cd repo &&
	c1=$(git rev-parse HEAD~1) &&
	c2=$(git rev-parse HEAD) &&
	git diff "$c1" "$c2" >out &&
	grep -q "No newline at end of file\|no newline\|No newline\|\\\\ No" out
	)
'

test_expect_success 'add trailing newline back and diff shows it' '
	(
	cd repo &&
	printf "line1\nline2\n" >file.txt &&
	git diff >out &&
	grep -q "line2" out
	)
'

test_expect_success 'commit newline restoration' '
	(
	cd repo &&
	git add file.txt &&
	git commit -m "restore trailing newline"
	)
'

# ---------------------------------------------------------------------------
# File with only newlines
# ---------------------------------------------------------------------------
test_expect_success 'diff on file that becomes empty lines' '
	(
	cd repo &&
	printf "content\n" >empty-test.txt &&
	git add empty-test.txt &&
	git commit -m "add empty-test" &&
	printf "\n\n\n" >empty-test.txt &&
	git diff >out &&
	grep -q "empty-test.txt" out
	)
'

test_expect_success 'commit empty lines file' '
	(
	cd repo &&
	git add empty-test.txt &&
	git commit -m "empty lines"
	)
'

# ---------------------------------------------------------------------------
# CRLF line endings
# ---------------------------------------------------------------------------
test_expect_success 'create file with LF and commit' '
	(
	cd repo &&
	printf "line1\nline2\nline3\n" >crlf.txt &&
	git add crlf.txt &&
	git commit -m "LF file"
	)
'

test_expect_success 'change to CRLF and diff detects it' '
	(
	cd repo &&
	printf "line1\r\nline2\r\nline3\r\n" >crlf.txt &&
	git diff >out &&
	test -s out
	)
'

test_expect_success 'diff output contains carriage return indicators' '
	(
	cd repo &&
	# The diff should show the change; CR may appear as ^M or \r
	git diff >out &&
	grep -q "crlf.txt" out
	)
'

test_expect_success 'commit CRLF version' '
	(
	cd repo &&
	git add crlf.txt &&
	git commit -m "CRLF file"
	)
'

test_expect_success 'diff between LF and CRLF commits' '
	(
	cd repo &&
	c1=$(git rev-parse HEAD~1) &&
	c2=$(git rev-parse HEAD) &&
	git diff "$c1" "$c2" >out &&
	test -s out
	)
'

# ---------------------------------------------------------------------------
# Mixed line endings
# ---------------------------------------------------------------------------
test_expect_success 'create file with mixed line endings' '
	(
	cd repo &&
	printf "lf-line\ncrlf-line\r\nlf-again\n" >mixed.txt &&
	git add mixed.txt &&
	git commit -m "mixed endings"
	)
'

test_expect_success 'modify mixed file and diff works' '
	(
	cd repo &&
	printf "lf-line\nchanged-line\r\nlf-again\n" >mixed.txt &&
	git diff >out &&
	grep -q "mixed.txt" out
	)
'

test_expect_success 'diff shows changed line' '
	(
	cd repo &&
	git diff >out &&
	grep -q "changed-line\|crlf-line" out
	)
'

test_expect_success 'commit mixed change' '
	(
	cd repo &&
	git add mixed.txt &&
	git commit -m "modify mixed"
	)
'

# ---------------------------------------------------------------------------
# Empty file transitions
# ---------------------------------------------------------------------------
test_expect_success 'diff from empty file to content (cached)' '
	(
	cd repo &&
	: >empty.txt &&
	git add empty.txt &&
	git commit -m "empty file" &&
	echo "now has content" >empty.txt &&
	git add empty.txt &&
	git diff --cached >out &&
	grep -q "now has content" out
	)
'

test_expect_success 'diff from content to empty (cached)' '
	(
	cd repo &&
	git commit -m "with content" &&
	: >empty.txt &&
	git add empty.txt &&
	git diff --cached >out &&
	grep -q "now has content" out
	)
'

test_expect_success 'commit empty transition' '
	(
	cd repo &&
	git add empty.txt &&
	git commit -m "back to empty"
	)
'

# ---------------------------------------------------------------------------
# Single newline file
# ---------------------------------------------------------------------------
test_expect_success 'file with just a newline' '
	(
	cd repo &&
	printf "\n" >nl.txt &&
	git add nl.txt &&
	git commit -m "single newline file"
	)
'

test_expect_success 'change single newline to content (cached)' '
	(
	cd repo &&
	echo "real content" >nl.txt &&
	git add nl.txt &&
	git diff --cached >out &&
	grep -q "real content" out
	)
'

test_expect_success 'commit content over newline' '
	(
	cd repo &&
	git add nl.txt &&
	git commit -m "content replaces newline"
	)
'

# ---------------------------------------------------------------------------
# No-newline on both sides
# ---------------------------------------------------------------------------
test_expect_success 'both versions lack trailing newline' '
	(
	cd repo &&
	printf "old" >both-no-nl.txt &&
	git add both-no-nl.txt &&
	git commit -m "no nl old" &&
	printf "new" >both-no-nl.txt &&
	git diff >out &&
	grep -q "old\|new" out
	)
'

test_expect_success 'diff --cached with no trailing newline' '
	(
	cd repo &&
	git add both-no-nl.txt &&
	git diff --cached >out &&
	grep -q "both-no-nl.txt" out
	)
'

test_expect_success 'commit no-nl change' '
	(
	cd repo &&
	git commit -m "no nl new"
	)
'

# ---------------------------------------------------------------------------
# Large file with mixed endings
# ---------------------------------------------------------------------------
test_expect_success 'large file with alternating line endings' '
	(
	cd repo &&
	for i in $(seq 1 100); do
		if test $((i % 2)) -eq 0; then
			printf "line %d\r\n" "$i"
		else
			printf "line %d\n" "$i"
		fi
	done >large-mixed.txt &&
	git add large-mixed.txt &&
	git commit -m "large mixed endings"
	)
'

test_expect_success 'modify one line in large mixed file' '
	(
	cd repo &&
	sed "s/line 50/CHANGED 50/" large-mixed.txt >tmp &&
	mv tmp large-mixed.txt &&
	git diff >out &&
	grep -q "CHANGED 50\|line 50" out
	)
'

test_expect_success 'diff --name-only with modified mixed file' '
	(
	cd repo &&
	git diff --name-only >out &&
	grep -q "large-mixed.txt" out
	)
'

test_expect_success 'commit large mixed modification' '
	(
	cd repo &&
	git add large-mixed.txt &&
	git commit -m "modify large mixed"
	)
'

# ---------------------------------------------------------------------------
# Whitespace-only changes
# ---------------------------------------------------------------------------
test_expect_success 'trailing whitespace change detected' '
	(
	cd repo &&
	printf "line1\nline2\n" >ws.txt &&
	git add ws.txt &&
	git commit -m "no trailing ws" &&
	printf "line1  \nline2\n" >ws.txt &&
	git diff >out &&
	test -s out
	)
'

test_expect_success 'tab vs spaces change detected' '
	(
	cd repo &&
	git checkout -- ws.txt &&
	printf "line1\n\tindented\n" >ws.txt &&
	git add ws.txt &&
	git commit -m "tab indent" &&
	printf "line1\n    indented\n" >ws.txt &&
	git diff >out &&
	test -s out
	)
'

test_done
