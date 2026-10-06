#!/bin/sh
# t7901-stash-resolve-merge.sh - not an upstream test
# Tests stash operations verified with grit

test_description='stash resolve merge (stash verification with grit)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init stash-repo &&
	cd stash-repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	echo base >file &&
	git add file &&
	test_tick &&
	git commit -m initial
	)
'

test_expect_success 'stash saves changes' '
	(
	cd stash-repo &&
	echo modified >file &&
	git stash &&
	# verify stash restored the original content
	grep "base" file
	)
'

test_expect_success 'stash list shows entry' '
	(
	cd stash-repo &&
	git stash list >actual &&
	test -s actual
	)
'

test_expect_success 'stash pop restores changes' '
	(
	cd stash-repo &&
	git stash pop &&
	grep "modified" file &&
	git status --porcelain >actual &&
	test -s actual
	)
'

test_expect_success 'stash with untracked' '
	(
	cd stash-repo &&
	git add file &&
	test_tick &&
	git commit -m "commit modified" &&
	echo new_content >new_file &&
	git stash -u &&
	test ! -f new_file
	)
'

test_expect_success 'stash pop restores untracked' '
	(
	cd stash-repo &&
	git stash pop &&
	test -f new_file &&
	grep "new_content" new_file
	)
'

test_expect_success 'stash and status interaction' '
	(
	cd stash-repo &&
	rm new_file &&
	echo changes >file &&
	git stash &&
	# stash should restore previous content
	git stash list >actual &&
	test -s actual &&
	git stash pop &&
	grep "changes" file
	)
'

test_done
