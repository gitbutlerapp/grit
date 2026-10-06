#!/bin/sh
# Tests for grit rev-list with --reverse and --count options.

test_description='grit rev-list --reverse and --count'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with linear history' '
	(
	"$REAL_GIT" init --initial-branch=master repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	for i in $(seq 1 15); do
		echo "content $i" >file.txt &&
		"$REAL_GIT" add file.txt &&
		GIT_AUTHOR_DATE="2024-01-$(printf "%02d" $i)T12:00:00+00:00" \
		GIT_COMMITTER_DATE="2024-01-$(printf "%02d" $i)T12:00:00+00:00" \
		"$REAL_GIT" commit -m "commit $i" || return 1
	done
	)
'

###########################################################################
# Section 2: Basic rev-list
###########################################################################

test_expect_success 'rev-list HEAD lists all commits' '
	(
	cd repo &&
	"$REAL_GIT" rev-list HEAD >expected &&
	"$GUST_BIN" rev-list HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list HEAD shows 15 commits' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD >actual &&
	test $(wc -l <actual) -eq 15
	)
'

test_expect_success 'rev-list HEAD output is SHA-only' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD >actual &&
	! grep " " actual
	)
'

test_expect_success 'rev-list HEAD contains HEAD commit' '
	(
	cd repo &&
	local head_sha=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" rev-list HEAD >actual &&
	grep "$head_sha" actual
	)
'

###########################################################################
# Section 3: --count
###########################################################################

test_expect_success 'rev-list --count HEAD shows 15' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD >actual &&
	echo "15" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --count HEAD >expected &&
	"$GUST_BIN" rev-list --count HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count HEAD~5 shows 10' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD~5 >actual &&
	echo "10" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count HEAD~5 matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --count HEAD~5 >expected &&
	"$GUST_BIN" rev-list --count HEAD~5 >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count HEAD~14 shows 1' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD~14 >actual &&
	echo "1" >expected &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 4: --reverse
###########################################################################

test_expect_success 'rev-list --reverse HEAD matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --reverse HEAD >expected &&
	"$GUST_BIN" rev-list --reverse HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --reverse shows oldest first' '
	(
	cd repo &&
	local first=$("$REAL_GIT" rev-list HEAD | tail -1) &&
	"$GUST_BIN" rev-list --reverse HEAD >actual &&
	head -1 actual | grep "$first"
	)
'

test_expect_success 'rev-list --reverse contains HEAD commit' '
	(
	cd repo &&
	local head_sha=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" rev-list --reverse HEAD >actual &&
	grep "$head_sha" actual
	)
'

test_expect_success 'rev-list --reverse has same count as normal' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD >normal &&
	"$GUST_BIN" rev-list --reverse HEAD >reversed &&
	test $(wc -l <normal) -eq $(wc -l <reversed)
	)
'

test_expect_success 'rev-list --reverse is exact reverse of normal' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD >normal &&
	"$GUST_BIN" rev-list --reverse HEAD >reversed &&
	tac normal >normal_reversed &&
	test_cmp normal_reversed reversed
	)
'

###########################################################################
# Section 5: --reverse with -n (max-count)
###########################################################################

test_expect_success 'rev-list --reverse -n 5 matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --reverse -n 5 HEAD >expected &&
	"$GUST_BIN" rev-list --reverse -n 5 HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --reverse -n 5 shows 5 commits' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --reverse -n 5 HEAD >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'rev-list --reverse -n 1 matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --reverse -n 1 HEAD >expected &&
	"$GUST_BIN" rev-list --reverse -n 1 HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 6: rev-list with range
###########################################################################

test_expect_success 'rev-list HEAD~5..HEAD matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list HEAD~5..HEAD >expected &&
	"$GUST_BIN" rev-list HEAD~5..HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list HEAD~5..HEAD shows 5 commits' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD~5..HEAD >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'rev-list --count HEAD~5..HEAD shows 5' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD~5..HEAD >actual &&
	echo "5" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --reverse HEAD~5..HEAD matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --reverse HEAD~5..HEAD >expected &&
	"$GUST_BIN" rev-list --reverse HEAD~5..HEAD >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count HEAD~5..HEAD matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --count HEAD~5..HEAD >expected &&
	"$GUST_BIN" rev-list --count HEAD~5..HEAD >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 7: Branches
###########################################################################

test_expect_success 'setup: create branch' '
	(
	cd repo &&
	"$REAL_GIT" checkout -b feature HEAD~5 &&
	echo "feature A" >feat.txt &&
	"$REAL_GIT" add feat.txt &&
	"$REAL_GIT" commit -m "feature A" &&
	echo "feature B" >feat.txt &&
	"$REAL_GIT" add feat.txt &&
	"$REAL_GIT" commit -m "feature B"
	)
'

test_expect_success 'rev-list --count feature matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --count feature >expected &&
	"$GUST_BIN" rev-list --count feature >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --reverse feature matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --reverse feature >expected &&
	"$GUST_BIN" rev-list --reverse feature >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list master..feature matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list master..feature >expected &&
	"$GUST_BIN" rev-list master..feature >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --count master..feature matches git' '
	(
	cd repo &&
	"$REAL_GIT" rev-list --count master..feature >expected &&
	"$GUST_BIN" rev-list --count master..feature >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 8: Edge cases
###########################################################################

test_expect_success 'rev-list HEAD~1..HEAD shows 1 commit' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD~1..HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'rev-list --count HEAD~1..HEAD shows 1' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD~1..HEAD >actual &&
	echo "1" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list HEAD..HEAD shows nothing' '
	(
	cd repo &&
	"$GUST_BIN" rev-list HEAD..HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'rev-list --count HEAD..HEAD shows 0' '
	(
	cd repo &&
	"$GUST_BIN" rev-list --count HEAD..HEAD >actual &&
	echo "0" >expected &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list with specific SHA matches git' '
	(
	cd repo &&
	local sha=$("$REAL_GIT" rev-parse HEAD~3) &&
	"$REAL_GIT" rev-list "$sha" >expected &&
	"$GUST_BIN" rev-list "$sha" >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'rev-list --reverse with specific SHA matches git' '
	(
	cd repo &&
	local sha=$("$REAL_GIT" rev-parse HEAD~3) &&
	"$REAL_GIT" rev-list --reverse "$sha" >expected &&
	"$GUST_BIN" rev-list --reverse "$sha" >actual &&
	test_cmp expected actual
	)
'

test_done
