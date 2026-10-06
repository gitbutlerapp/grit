#!/bin/sh
# Tests for commit with author/date overrides and various commit options.

test_description='commit author and date handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup: init repo' '
	(
	git init author-repo &&
	cd author-repo &&
	git config user.email "default@test.com" &&
	git config user.name "Default User"
	)
'

# -- basic commit --------------------------------------------------------------

test_expect_success 'commit records message' '
	(
	cd author-repo &&
	echo "first" >file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "first commit" &&
	git log --format=%s -n 1 >out &&
	echo "first commit" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit records author name from env' '
	(
	cd author-repo &&
	git log --format="%an" -n 1 >out &&
	echo "Test Author" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit records author email from env' '
	(
	cd author-repo &&
	git log --format="%ae" -n 1 >out &&
	echo "author@test.com" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit records committer name from env' '
	(
	cd author-repo &&
	git log --format="%cn" -n 1 >out &&
	echo "Test User" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit records committer email from env' '
	(
	cd author-repo &&
	git log --format="%ce" -n 1 >out &&
	echo "test@test.com" >expect &&
	test_cmp expect out
	)
'

# -- author override -----------------------------------------------------------

test_expect_success 'commit --author overrides author name' '
	(
	cd author-repo &&
	echo "second" >file2.txt &&
	git add file2.txt &&
	test_tick &&
	git commit --author="Custom Author <custom@example.com>" -m "custom author" &&
	git log --format="%an" -n 1 >out &&
	echo "Custom Author" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit --author overrides author email' '
	(
	cd author-repo &&
	git log --format="%ae" -n 1 >out &&
	echo "custom@example.com" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit --author does not change committer' '
	(
	cd author-repo &&
	git log --format="%cn" -n 1 >out &&
	echo "Test User" >expect &&
	test_cmp expect out
	)
'

# -- date via environment variables --------------------------------------------

test_expect_success 'GIT_AUTHOR_DATE overrides author date' '
	(
	cd author-repo &&
	echo "dated" >dated.txt &&
	git add dated.txt &&
	GIT_AUTHOR_DATE="1112911993 +0000" \
	GIT_COMMITTER_DATE="1112911993 +0000" \
	git commit -m "dated commit" &&
	git log --format="%ai" -n 1 >out &&
	grep "2005-04-07" out
	)
'

test_expect_success 'GIT_COMMITTER_DATE overrides committer date' '
	(
	cd author-repo &&
	git log --format="%ci" -n 1 >out &&
	grep "2005-04-07" out
	)
'

# -- commit --date flag --------------------------------------------------------

test_expect_success 'commit --date overrides author date' '
	(
	cd author-repo &&
	echo "date-flag" >date-flag.txt &&
	git add date-flag.txt &&
	git commit --date="1234567890 +0000" -m "date flag commit" &&
	git log --format="%ai" -n 1 >out &&
	grep "2009-02-13" out
	)
'

# -- multiple commits and ordering ---------------------------------------------

test_expect_success 'each commit gets its own timestamp' '
	(
	cd author-repo &&
	echo "a" >tick-a.txt &&
	git add tick-a.txt &&
	test_tick &&
	git commit -m "tick a" &&
	date_a=$(git log --format="%ai" -n 1) &&
	echo "b" >tick-b.txt &&
	git add tick-b.txt &&
	test_tick &&
	git commit -m "tick b" &&
	date_b=$(git log --format="%ai" -n 1) &&
	test "$date_a" != "$date_b"
	)
'

test_expect_success 'log shows commits in reverse chronological order' '
	(
	cd author-repo &&
	git log --format=%s -n 2 >out &&
	head -1 out >first-line &&
	echo "tick b" >expect &&
	test_cmp expect first-line
	)
'

# -- commit --allow-empty ------------------------------------------------------

test_expect_success 'commit --allow-empty creates empty commit' '
	(
	cd author-repo &&
	test_tick &&
	git commit --allow-empty -m "empty commit" &&
	git log --format=%s -n 1 >out &&
	echo "empty commit" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'empty commit has same tree as parent' '
	(
	cd author-repo &&
	tree_head=$(git log --format=%T -n 1) &&
	tree_parent=$(git log --format=%T --skip=1 -n 1) &&
	test "$tree_head" = "$tree_parent"
	)
'

# -- commit message formatting -------------------------------------------------

test_expect_success 'commit -m with special characters' '
	(
	cd author-repo &&
	echo "special" >special.txt &&
	git add special.txt &&
	test_tick &&
	git commit -m "fix: handle quotes and stuff" &&
	git log --format=%s -n 1 >out &&
	grep "quotes" out
	)
'

# -- amend ---------------------------------------------------------------------

test_expect_success 'commit --amend changes message' '
	(
	cd author-repo &&
	test_tick &&
	git commit --amend -m "amended message" &&
	git log --format=%s -n 1 >out &&
	echo "amended message" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit --amend does not add extra commit' '
	(
	cd author-repo &&
	count_before=$(git log --oneline | wc -l) &&
	test_tick &&
	git commit --amend -m "amended again" &&
	count_after=$(git log --oneline | wc -l) &&
	test "$count_before" -eq "$count_after"
	)
'

test_expect_success 'commit --amend preserves author when not overridden' '
	(
	cd author-repo &&
	git log --format="%an" -n 1 >out &&
	echo "Test Author" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit --amend with --author changes author' '
	(
	cd author-repo &&
	git commit --amend --author="Amended Author <amend@test.com>" -m "amend with author" &&
	git log --format="%an" -n 1 >out &&
	echo "Amended Author" >expect &&
	test_cmp expect out
	)
'

# -- commit with staged vs unstaged --------------------------------------------

test_expect_success 'commit only includes staged files' '
	(
	cd author-repo &&
	echo "staged" >staged-only.txt &&
	echo "not-staged" >not-staged.txt &&
	git add staged-only.txt &&
	test_tick &&
	git commit -m "only staged" &&
	git log --format=%s -n 1 >out &&
	echo "only staged" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'commit after modifying tracked file' '
	(
	cd author-repo &&
	echo "modified" >>staged-only.txt &&
	git add staged-only.txt &&
	test_tick &&
	git commit -m "modify tracked" &&
	git log --format=%s -n 1 >out &&
	echo "modify tracked" >expect &&
	test_cmp expect out
	)
'

# -- log format checks ---------------------------------------------------------

test_expect_success 'log --format=%H shows full 40-char hash' '
	(
	cd author-repo &&
	git log --format=%H -n 1 >out &&
	hash=$(cat out) &&
	test ${#hash} -eq 40
	)
'

test_expect_success 'log --format=%h shows abbreviated hash' '
	(
	cd author-repo &&
	git log --format=%h -n 1 >out &&
	hash=$(cat out) &&
	test ${#hash} -ge 7 &&
	test ${#hash} -le 40
	)
'

test_expect_success 'log --format=%T shows tree hash' '
	(
	cd author-repo &&
	git log --format=%T -n 1 >out &&
	tree=$(cat out) &&
	test ${#tree} -eq 40
	)
'

test_expect_success 'log --oneline shows one line per commit' '
	(
	cd author-repo &&
	git log --oneline -n 3 >out &&
	test "$(wc -l <out)" -eq 3
	)
'

test_expect_success 'log -n 1 limits to one commit' '
	(
	cd author-repo &&
	git log --oneline -n 1 >out &&
	test "$(wc -l <out)" -eq 1
	)
'

test_done
