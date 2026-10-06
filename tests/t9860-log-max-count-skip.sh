#!/bin/sh
# Tests for grit log with --max-count (-n) and --skip options.

test_description='grit log --max-count and --skip'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup - create 20 commits for pagination tests
###########################################################################

test_expect_success 'setup: create repository with 20 commits' '
	(
	"$REAL_GIT" init --initial-branch=master repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	for i in $(seq 1 20); do
		echo "commit $i" >file.txt &&
		"$REAL_GIT" add file.txt &&
		GIT_AUTHOR_DATE="2024-01-$(printf "%02d" $i)T12:00:00+00:00" \
		GIT_COMMITTER_DATE="2024-01-$(printf "%02d" $i)T12:00:00+00:00" \
		"$REAL_GIT" commit -m "commit number $i" || return 1
	done
	)
'

###########################################################################
# Section 2: --max-count / -n basics
###########################################################################

test_expect_success 'log -n 1 shows one commit' '
	(
	cd repo &&
	"$GUST_BIN" log -n 1 --oneline >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log -n 1 matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 1 --oneline >expected &&
	"$GUST_BIN" log -n 1 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 5 shows five commits' '
	(
	cd repo &&
	"$GUST_BIN" log -n 5 --oneline >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'log -n 5 matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 5 --oneline >expected &&
	"$GUST_BIN" log -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 10 shows ten commits' '
	(
	cd repo &&
	"$GUST_BIN" log -n 10 --oneline >actual &&
	test $(wc -l <actual) -eq 10
	)
'

test_expect_success 'log -n 10 matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 10 --oneline >expected &&
	"$GUST_BIN" log -n 10 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 20 shows all 20 commits' '
	(
	cd repo &&
	"$GUST_BIN" log -n 20 --oneline >actual &&
	test $(wc -l <actual) -eq 20
	)
'

test_expect_success 'log -n 100 shows all 20 (no more than exist)' '
	(
	cd repo &&
	"$GUST_BIN" log -n 100 --oneline >actual &&
	test $(wc -l <actual) -eq 20
	)
'

test_expect_success 'log --max-count=3 matches git' '
	(
	cd repo &&
	"$REAL_GIT" log --max-count=3 --oneline >expected &&
	"$GUST_BIN" log --max-count=3 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 1 shows latest commit message' '
	(
	cd repo &&
	"$GUST_BIN" log -n 1 --oneline >actual &&
	grep "commit number 20" actual
	)
'

###########################################################################
# Section 3: --skip basics
###########################################################################

test_expect_success 'log --skip=1 skips first commit' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=1 --oneline >expected &&
	"$GUST_BIN" log --skip=1 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log --skip=1 first line is second commit' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=1 --oneline >actual &&
	head -1 actual | grep "commit number 19"
	)
'

test_expect_success 'log --skip=5 skips first five' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=5 --oneline >expected &&
	"$GUST_BIN" log --skip=5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log --skip=5 shows 15 commits' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=5 --oneline >actual &&
	test $(wc -l <actual) -eq 15
	)
'

test_expect_success 'log --skip=19 shows one commit' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=19 --oneline >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --skip=19 shows first commit' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=19 --oneline >actual &&
	grep "commit number 1" actual
	)
'

test_expect_success 'log --skip=20 shows nothing' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=20 --oneline >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --skip=100 shows nothing' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=100 --oneline >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: --skip + --max-count combined (pagination)
###########################################################################

test_expect_success 'log --skip=0 -n 5 is first page' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=0 -n 5 --oneline >expected &&
	"$GUST_BIN" log --skip=0 -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log --skip=5 -n 5 is second page' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=5 -n 5 --oneline >expected &&
	"$GUST_BIN" log --skip=5 -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log --skip=10 -n 5 is third page' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=10 -n 5 --oneline >expected &&
	"$GUST_BIN" log --skip=10 -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log --skip=15 -n 5 is fourth page' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=15 -n 5 --oneline >expected &&
	"$GUST_BIN" log --skip=15 -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'pages cover all commits without overlap' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=0 -n 5 --oneline >page1 &&
	"$GUST_BIN" log --skip=5 -n 5 --oneline >page2 &&
	"$GUST_BIN" log --skip=10 -n 5 --oneline >page3 &&
	"$GUST_BIN" log --skip=15 -n 5 --oneline >page4 &&
	cat page1 page2 page3 page4 >all_pages &&
	"$GUST_BIN" log --oneline >all_commits &&
	test_cmp all_commits all_pages
	)
'

test_expect_success 'log --skip=18 -n 5 returns only last 2' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=18 -n 5 --oneline >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'log --skip=18 -n 5 matches git' '
	(
	cd repo &&
	"$REAL_GIT" log --skip=18 -n 5 --oneline >expected &&
	"$GUST_BIN" log --skip=18 -n 5 --oneline >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 5: -n with format options
###########################################################################

test_expect_success 'log -n 3 --format=short matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 3 --format=short >expected &&
	"$GUST_BIN" log -n 3 --format=short >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 2 --format=medium matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 2 --format=medium >expected &&
	"$GUST_BIN" log -n 2 --format=medium >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 2 --format=full matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 2 --format=full >expected &&
	"$GUST_BIN" log -n 2 --format=full >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 6: -n 0 edge case
###########################################################################

test_expect_success 'log -n 1 --skip=20 shows nothing' '
	(
	cd repo &&
	"$GUST_BIN" log -n 1 --skip=20 --oneline >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 7: -n with branches
###########################################################################

test_expect_success 'setup: create branch with extra commits' '
	(
	cd repo &&
	"$REAL_GIT" checkout -b feature &&
	echo "feature 1" >feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "feature commit 1" &&
	echo "feature 2" >feature.txt &&
	"$REAL_GIT" add feature.txt &&
	"$REAL_GIT" commit -m "feature commit 2"
	)
'

test_expect_success 'log -n 3 on feature branch matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 3 --oneline >expected &&
	"$GUST_BIN" log -n 3 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 1 on feature shows latest feature commit' '
	(
	cd repo &&
	"$GUST_BIN" log -n 1 --oneline >actual &&
	grep "feature commit 2" actual
	)
'

test_expect_success 'log --skip=2 -n 1 on feature shows commit 20' '
	(
	cd repo &&
	"$GUST_BIN" log --skip=2 -n 1 --oneline >actual &&
	grep "commit number 20" actual
	)
'

test_expect_success 'switch back to master' '
	(
	cd repo &&
	"$REAL_GIT" checkout master
	)
'

###########################################################################
# Section 8: -n with pathspec
###########################################################################

test_expect_success 'setup: multiple files with different histories' '
	(
	cd repo &&
	echo "alpha" >alpha.txt &&
	"$REAL_GIT" add alpha.txt &&
	"$REAL_GIT" commit -m "add alpha" &&
	echo "beta" >beta.txt &&
	"$REAL_GIT" add beta.txt &&
	"$REAL_GIT" commit -m "add beta" &&
	echo "alpha v2" >alpha.txt &&
	"$REAL_GIT" add alpha.txt &&
	"$REAL_GIT" commit -m "update alpha"
	)
'

test_expect_success 'log -n 1 after extra commits matches git' '
	(
	cd repo &&
	"$REAL_GIT" log -n 1 --oneline >expected &&
	"$GUST_BIN" log -n 1 --oneline >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'log -n 3 shows latest 3 after extra commits' '
	(
	cd repo &&
	"$REAL_GIT" log -n 3 --oneline >expected &&
	"$GUST_BIN" log -n 3 --oneline >actual &&
	test_cmp expected actual
	)
'

test_done
