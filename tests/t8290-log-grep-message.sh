#!/bin/sh
# Tests for log --grep=pattern (commit message search).
# Some tests use test_expect_success for features not yet implemented.

test_description='log --grep commit message filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup: create commits with varied messages
###########################################################################

test_expect_success 'setup repository with diverse commit messages' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	git commit --allow-empty -m "feat: add login system" &&
	git commit --allow-empty -m "fix: resolve null pointer in auth" &&
	git commit --allow-empty -m "docs: update README with examples" &&
	git commit --allow-empty -m "feat: add user profile page" &&
	git commit --allow-empty -m "fix: handle edge case in parser" &&
	git commit --allow-empty -m "refactor: clean up database layer" &&
	git commit --allow-empty -m "feat: implement search API" &&
	git commit --allow-empty -m "test: add integration tests" &&
	git commit --allow-empty -m "fix: correct off-by-one error" &&
	git commit --allow-empty -m "chore: update dependencies"
	)
'

###########################################################################
# Section 1: Verify commit messages via log --format
###########################################################################

test_expect_success 'log --format=%s shows subject lines' '
	(
	cd repo &&
	git log --format="%s" >actual &&
	test $(wc -l <actual) -eq 10
	)
'

test_expect_success 'log --format=%s for HEAD shows latest message' '
	(
	cd repo &&
	git log -n1 --format="%s" >actual &&
	echo "chore: update dependencies" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%s --reverse shows oldest first' '
	(
	cd repo &&
	git log --reverse --format="%s" >actual &&
	head -1 actual >first &&
	echo "feat: add login system" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'log --format=%H %s shows OID with message' '
	(
	cd repo &&
	git log --format="%H %s" >actual &&
	while read oid msg; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || exit 1
		test -n "$msg" || exit 1
	done <actual
	)
'

test_expect_success 'log -n3 --format=%s returns exactly 3 messages' '
	(
	cd repo &&
	git log -n3 --format="%s" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

###########################################################################
# Section 2: --grep filtering (not yet implemented)
###########################################################################

test_expect_success 'log --grep=feat shows feature commits' '
	(
	cd repo &&
	git log --grep=feat --format="%s" >actual &&
	test $(wc -l <actual) -eq 3 &&
	grep "feat:" actual
	)
'

test_expect_success 'log --grep=fix shows bug fix commits' '
	(
	cd repo &&
	git log --grep=fix --format="%s" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --grep=docs shows documentation commits' '
	(
	cd repo &&
	git log --grep=docs --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --grep with substring match' '
	(
	cd repo &&
	git log --grep="add" --format="%s" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --grep with exact phrase' '
	(
	cd repo &&
	git log --grep="off-by-one" --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --grep is case-sensitive by default' '
	(
	cd repo &&
	git log --grep=FEAT --format="%s" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --grep with no match produces empty output' '
	(
	cd repo &&
	git log --grep=nonexistent --format="%s" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --grep combined with -n' '
	(
	cd repo &&
	git log --grep=feat -n1 --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --grep combined with --oneline' '
	(
	cd repo &&
	git log --grep=fix --oneline >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --grep combined with --reverse' '
	(
	cd repo &&
	git log --grep=feat --reverse --format="%s" >actual &&
	head -1 actual >first &&
	echo "feat: add login system" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'log --grep with regex pattern' '
	(
	cd repo &&
	git log --grep="add.*system" --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --grep with alternation' '
	(
	cd repo &&
	git log --grep="feat\|docs" --format="%s" >actual &&
	test $(wc -l <actual) -eq 4
	)
'

###########################################################################
# Section 3: Workaround — grep formatted output
###########################################################################

test_expect_success 'grep log output for feat: prefix' '
	(
	cd repo &&
	git log --format="%s" >all &&
	grep "^feat:" all >filtered &&
	test $(wc -l <filtered) -eq 3
	)
'

test_expect_success 'grep log output for fix: prefix' '
	(
	cd repo &&
	git log --format="%s" >all &&
	grep "^fix:" all >filtered &&
	test $(wc -l <filtered) -eq 3
	)
'

test_expect_success 'grep log output for substring in message' '
	(
	cd repo &&
	git log --format="%s" >all &&
	grep "add" all >filtered &&
	test $(wc -l <filtered) -eq 3
	)
'

test_expect_success 'grep log output for exact phrase' '
	(
	cd repo &&
	git log --format="%s" >all &&
	grep "off-by-one" all >filtered &&
	test $(wc -l <filtered) -eq 1
	)
'

test_expect_success 'grep log output with OIDs for traceability' '
	(
	cd repo &&
	git log --format="%H %s" >all &&
	grep "fix:" all >filtered &&
	while read oid msg; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || exit 1
	done <filtered
	)
'

###########################################################################
# Section 4: Message body vs subject
###########################################################################

test_expect_success 'log --format=%s shows only subject line' '
	(
	cd repo &&
	git log -n1 --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --format=%s for empty message body shows subject only' '
	(
	cd repo &&
	git log --format="%s" >actual &&
	while read line; do
		test -n "$line" || exit 1
	done <actual
	)
'

test_expect_success 'count commits matching pattern via grep' '
	(
	cd repo &&
	git log --format="%s" >all &&
	count=$(grep -c ":" all) &&
	test "$count" -eq 10
	)
'

###########################################################################
# Section 5: Edge cases
###########################################################################

test_expect_success 'log --grep on empty repository produces no output' '
	(
	git init empty-repo &&
	cd empty-repo &&
	test_must_fail git log --grep=anything --format="%s" >actual 2>/dev/null &&
	test_must_be_empty actual
	)
'

test_expect_success 'log on repo with special characters in message' '
	(
	git init special-repo &&
	cd special-repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	git commit --allow-empty -m "fix: handle '\''quotes'\'' and \"doubles\"" &&
	git log --format="%s" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log shows multi-word messages correctly' '
	(
	cd repo &&
	git log --format="%s" >actual &&
	grep "clean up database layer" actual
	)
'

test_expect_success 'log --skip with --format=%s works' '
	(
	cd repo &&
	git log --skip=8 --format="%s" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_done
