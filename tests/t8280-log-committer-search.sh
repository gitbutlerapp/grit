#!/bin/sh
# Tests for log --committer=pattern matching.
# Some tests use test_expect_success for features not yet implemented.

test_description='log --committer filtering and committer-related format output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup: create commits with distinct committers
###########################################################################

test_expect_success 'setup repository with multiple committers' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Default User" &&
	git config user.email "default@example.com" &&

	GIT_AUTHOR_NAME="Author One" GIT_AUTHOR_EMAIL="a1@example.com" \
		GIT_COMMITTER_NAME="Dana Developer" GIT_COMMITTER_EMAIL="dana@example.com" \
		git commit --allow-empty -m "first commit" &&

	GIT_AUTHOR_NAME="Author Two" GIT_AUTHOR_EMAIL="a2@example.com" \
		GIT_COMMITTER_NAME="Dana Developer" GIT_COMMITTER_EMAIL="dana@example.com" \
		git commit --allow-empty -m "second commit" &&

	GIT_AUTHOR_NAME="Author Three" GIT_AUTHOR_EMAIL="a3@example.com" \
		GIT_COMMITTER_NAME="Eve Engineer" GIT_COMMITTER_EMAIL="eve@example.com" \
		git commit --allow-empty -m "third commit" &&

	GIT_AUTHOR_NAME="Author Four" GIT_AUTHOR_EMAIL="a4@example.com" \
		GIT_COMMITTER_NAME="Frank Fixer" GIT_COMMITTER_EMAIL="frank@corp.example.com" \
		git commit --allow-empty -m "fourth commit" &&

	GIT_AUTHOR_NAME="Author Five" GIT_AUTHOR_EMAIL="a5@example.com" \
		GIT_COMMITTER_NAME="Dana Deploy" GIT_COMMITTER_EMAIL="dana.deploy@example.com" \
		git commit --allow-empty -m "fifth commit" &&

	GIT_AUTHOR_NAME="Author Six" GIT_AUTHOR_EMAIL="a6@example.com" \
		GIT_COMMITTER_NAME="Grace Garcia" GIT_COMMITTER_EMAIL="grace@example.com" \
		git commit --allow-empty -m "sixth commit"
	)
'

###########################################################################
# Section 1: Verify committer data is stored correctly via log --format
###########################################################################

test_expect_success 'log --format=%cn shows committer names' '
	(
	cd repo &&
	git log --format="%cn" >actual &&
	test $(wc -l <actual) -eq 6
	)
'

test_expect_success 'log --format=%ce shows committer emails' '
	(
	cd repo &&
	git log --format="%ce" >actual &&
	grep "dana@example.com" actual &&
	grep "eve@example.com" actual &&
	grep "frank@corp.example.com" actual &&
	grep "grace@example.com" actual
	)
'

test_expect_success 'log --format=%cn for HEAD shows correct committer' '
	(
	cd repo &&
	git log -n1 --format="%cn" >actual &&
	echo "Grace Garcia" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%ce for HEAD shows correct email' '
	(
	cd repo &&
	git log -n1 --format="%ce" >actual &&
	echo "grace@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format with committer name and email combined' '
	(
	cd repo &&
	git log -n1 --format="%cn <%ce>" >actual &&
	echo "Grace Garcia <grace@example.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'committer differs from author when explicitly set' '
	(
	cd repo &&
	git log -n1 --format="%an" >author &&
	git log -n1 --format="%cn" >committer &&
	! test_cmp author committer
	)
'

###########################################################################
# Section 2: --committer filtering (not yet implemented)
###########################################################################

test_expect_success 'log --committer=Dana shows commits by Dana' '
	(
	cd repo &&
	git log --committer=Dana --format="%cn" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --committer="Eve Engineer" shows one commit' '
	(
	cd repo &&
	git log --committer="Eve Engineer" --format="%cn" >actual &&
	test $(wc -l <actual) -eq 1 &&
	echo "Eve Engineer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --committer matches email address' '
	(
	cd repo &&
	git log --committer=frank@corp.example.com --format="%cn" >actual &&
	test $(wc -l <actual) -eq 1 &&
	echo "Frank Fixer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --committer with partial name match' '
	(
	cd repo &&
	git log --committer=Developer --format="%cn" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'log --committer matches email domain' '
	(
	cd repo &&
	git log --committer=corp.example.com --format="%cn" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log -i --committer is case-insensitive' '
	(
	cd repo &&
	git log -i --committer=dana --format="%cn" >lower &&
	git log -i --committer=DANA --format="%cn" >upper &&
	test_cmp lower upper
	)
'

test_expect_success 'log --committer with no match produces empty output' '
	(
	cd repo &&
	git log --committer=Nobody --format="%cn" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --committer combined with -n limits results' '
	(
	cd repo &&
	git log --committer=Dana -n1 --format="%cn" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --committer combined with --oneline' '
	(
	cd repo &&
	git log --committer="Eve Engineer" --oneline >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --committer combined with --reverse' '
	(
	cd repo &&
	git log --committer=Dana --reverse --format="%s" >actual &&
	head -1 actual >first &&
	echo "first commit" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'log --committer combined with --skip' '
	(
	cd repo &&
	git log --committer=Dana --skip=1 --format="%cn" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

###########################################################################
# Section 3: Committer format atoms
###########################################################################

test_expect_success 'log --format=%cn produces non-empty names' '
	(
	cd repo &&
	git log --format="%cn" >actual &&
	while read line; do
		test -n "$line" || exit 1
	done <actual
	)
'

test_expect_success 'log --format=%ce produces valid emails' '
	(
	cd repo &&
	git log --format="%ce" >actual &&
	while read line; do
		echo "$line" | grep -q "@" || exit 1
	done <actual
	)
'

test_expect_success 'log --format with multiple committer fields' '
	(
	cd repo &&
	git log -n1 --format="name:%cn email:%ce" >actual &&
	grep "name:Grace Garcia" actual &&
	grep "email:grace@example.com" actual
	)
'

test_expect_success 'log --format=%cn and %an are independent' '
	(
	cd repo &&
	git log --format="%an|%cn" >actual &&
	while IFS="|" read author committer; do
		test -n "$author" || exit 1
		test -n "$committer" || exit 1
	done <actual
	)
'

###########################################################################
# Section 4: Workaround — grep formatted output for committer filtering
###########################################################################

test_expect_success 'grep log format output to simulate --committer filter' '
	(
	cd repo &&
	git log --format="%H %cn" >all &&
	grep "Dana" all >filtered &&
	test $(wc -l <filtered) -eq 3
	)
'

test_expect_success 'grep committer output for exact name' '
	(
	cd repo &&
	git log --format="%H %cn" >all &&
	grep "Eve Engineer" all >filtered &&
	test $(wc -l <filtered) -eq 1
	)
'

test_expect_success 'grep committer email for domain-specific match' '
	(
	cd repo &&
	git log --format="%H %ce" >all &&
	grep "corp.example.com" all >filtered &&
	test $(wc -l <filtered) -eq 1
	)
'

test_expect_success 'committer count matches total commits' '
	(
	cd repo &&
	git log --format="%cn" >actual &&
	git rev-list --count HEAD >count &&
	test $(wc -l <actual) -eq $(cat count)
	)
'

###########################################################################
# Section 5: Edge cases
###########################################################################

test_expect_success 'log --committer with regex metacharacters' '
	(
	cd repo &&
	git log --committer="Dana.*" --format="%cn" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log on repo with single commit shows committer correctly' '
	(
	git init single-repo &&
	cd single-repo &&
	git config user.name "Temp" &&
	git config user.email "temp@example.com" &&
	GIT_COMMITTER_NAME="Solo Committer" GIT_COMMITTER_EMAIL="solo@example.com" \
		git commit --allow-empty -m "only" &&
	git log --format="%cn" >actual &&
	echo "Solo Committer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'committer and author can be the same person' '
	(
	git init same-person &&
	cd same-person &&
	GIT_AUTHOR_NAME="Same Person" GIT_AUTHOR_EMAIL="same@example.com" \
		GIT_COMMITTER_NAME="Same Person" GIT_COMMITTER_EMAIL="same@example.com" \
		git commit --allow-empty -m "same author and committer" &&
	git log --format="%an" >author &&
	git log --format="%cn" >committer &&
	test_cmp author committer
	)
'

test_done
