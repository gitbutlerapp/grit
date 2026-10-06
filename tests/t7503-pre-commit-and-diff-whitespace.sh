#!/bin/sh
# Tests for pre-commit hook behavior and diff whitespace handling.
# Tests exercise hook behavior and diff whitespace detection.

test_description='grit pre-commit hooks and diff whitespace'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test User"
	)
'

# --- Hook behavior ---

test_expect_success 'pre-commit hook exit 1 blocks commit' '
	(
	cd repo &&
	echo "first" >file &&
	git add file &&
	git commit -m "initial" &&
	mkdir -p .git/hooks &&
	printf "#!/bin/sh\nexit 1\n" >.git/hooks/pre-commit &&
	chmod +x .git/hooks/pre-commit &&
	echo "second" >>file &&
	git add file &&
	test_must_fail git commit -m "should be blocked by pre-commit"
	)
'

test_expect_success 'commit-msg hook exit 1 blocks commit' '
	(
	cd repo &&
	rm -f .git/hooks/pre-commit &&
	printf "#!/bin/sh\nexit 1\n" >.git/hooks/commit-msg &&
	chmod +x .git/hooks/commit-msg &&
	test_must_fail git commit -m "should be blocked by commit-msg"
	)
'

test_expect_success 'post-commit hook is executed' '
	(
	cd repo &&
	rm -f .git/hooks/commit-msg &&
	printf "#!/bin/sh\ntouch ../post-commit-ran\n" >.git/hooks/post-commit &&
	chmod +x .git/hooks/post-commit &&
	rm -f ../post-commit-ran &&
	git commit -m "should trigger post-commit" &&
	test -f ../post-commit-ran
	)
'

# --- Diff whitespace tests ---

test_expect_success 'diff detects trailing whitespace changes' '
	(
	cd repo &&
	rm -f .git/hooks/* &&
	printf "hello  \nworld\n" >ws.txt &&
	git add ws.txt &&
	git commit -m "with trailing spaces" &&
	printf "hello\nworld\n" >ws.txt &&
	git diff >actual &&
	grep "hello" actual
	)
'

test_expect_success 'diff shows file with whitespace addition' '
	(
	cd repo &&
	git add ws.txt && git commit -m "clean" &&
	printf "hello   \nworld\n" >ws.txt &&
	git diff >actual &&
	grep "hello" actual
	)
'

test_expect_success 'diff detects tab vs space changes' '
	(
	cd repo &&
	git add ws.txt && git commit -m "ws2" &&
	printf "\tindented\n" >tab.txt &&
	git add tab.txt &&
	git commit -m "tab indent" &&
	printf "    indented\n" >tab.txt &&
	git diff >actual &&
	grep "indented" actual
	)
'

test_expect_success 'diff --stat shows changed files' '
	(
	cd repo &&
	git diff --stat >actual &&
	grep "tab.txt" actual
	)
'

test_expect_success 'diff --numstat counts whitespace changes' '
	(
	cd repo &&
	git diff --numstat >actual &&
	grep "tab.txt" actual
	)
'

test_expect_success 'diff --name-only shows files with whitespace changes' '
	(
	cd repo &&
	git diff --name-only >actual &&
	grep "tab.txt" actual
	)
'

test_expect_success 'diff --name-status shows M for whitespace change' '
	(
	cd repo &&
	git diff --name-status >actual &&
	grep "^M" actual &&
	grep "tab.txt" actual
	)
'

test_expect_success 'diff with blank line removal' '
	(
	cd repo &&
	git add tab.txt && git commit -m "spaces" &&
	printf "line1\n\nline3\n" >blank.txt &&
	git add blank.txt &&
	git commit -m "with blank" &&
	printf "line1\nline3\n" >blank.txt &&
	git diff >actual &&
	test -s actual
	)
'

test_expect_success 'diff with only whitespace in new file' '
	(
	cd repo &&
	printf "   \n  \n \n" >spaces-only.txt &&
	git add spaces-only.txt &&
	git diff --cached >actual &&
	grep "spaces-only.txt" actual
	)
'

test_expect_success 'diff --cached shows staged whitespace changes' '
	(
	cd repo &&
	git diff --cached --stat >actual &&
	grep "spaces-only.txt" actual
	)
'

test_expect_success 'diff -U0 with whitespace change' '
	(
	cd repo &&
	git commit -m "staged" &&
	printf "line1\nline2\nline3\n" >ctx.txt &&
	git add ctx.txt && git commit -m "ctx base" &&
	printf "line1\nline2  \nline3\n" >ctx.txt &&
	git diff -U0 >actual &&
	grep "line2" actual
	)
'

test_expect_success 'diff -U1 shows context lines' '
	(
	cd repo &&
	git diff -U1 >actual &&
	test -s actual
	)
'

test_expect_success 'diff --exit-code returns 1 for whitespace differences' '
	(
	cd repo &&
	test_expect_code 1 git diff --exit-code
	)
'

test_expect_success 'diff -q suppresses diff output' '
	(
	cd repo &&
	git diff -q >actual 2>&1 || true &&
	! test -s actual
	)
'

test_expect_success 'diff between commits with whitespace changes' '
	(
	cd repo &&
	git add ctx.txt && git commit -m "trailing space" &&
	git diff HEAD~1 HEAD -- ctx.txt >actual &&
	grep "line2" actual
	)
'

test_expect_success 'diff CRLF vs LF' '
	(
	cd repo &&
	printf "line1\r\nline2\r\n" >crlf.txt &&
	git add crlf.txt && git commit -m "crlf" &&
	printf "line1\nline2\n" >crlf.txt &&
	git diff >actual &&
	test -s actual
	)
'

test_expect_success 'diff empty file vs whitespace-only' '
	(
	cd repo &&
	git add crlf.txt && git commit -m "lf" &&
	>empty.txt &&
	git add empty.txt && git commit -m "empty" &&
	printf " \n" >empty.txt &&
	git diff >actual &&
	test -s actual
	)
'

test_expect_success 'diff --stat with multiple whitespace-changed files' '
	(
	cd repo &&
	printf "a  \n" >ws1.txt &&
	printf "b  \n" >ws2.txt &&
	git add ws1.txt ws2.txt && git commit -m "two ws files" &&
	printf "a\n" >ws1.txt &&
	printf "b\n" >ws2.txt &&
	git diff --stat >actual &&
	grep "ws1.txt" actual &&
	grep "ws2.txt" actual
	)
'

test_done
