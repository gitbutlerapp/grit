#!/bin/sh
# Tests for GIT_DIR, GIT_WORK_TREE, GIT_AUTHOR_*, GIT_COMMITTER_* env vars.

test_description='grit environment variable handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ===========================================================================
# Setup
# ===========================================================================

test_expect_success 'setup: init repo for env tests' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo hello >file.txt &&
	git add file.txt &&
	git commit -m "initial"
	)
'

# ===========================================================================
# GIT_DIR — running commands from outside the repo
# ===========================================================================

test_expect_success 'GIT_DIR allows rev-parse HEAD from outside the repo' '
	(
	cd repo &&
	git rev-parse HEAD >../expect &&
	cd .. &&
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git rev-parse HEAD >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_DIR with log --oneline works from outside repo' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git log --oneline -n 1 >actual &&
	grep "initial" actual
'

test_expect_success 'GIT_DIR with cat-file works from outside repo' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	cd .. &&
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git cat-file -t "$oid" >actual &&
	echo commit >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_DIR with cat-file -p works from outside repo' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	cd .. &&
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git cat-file -p "$oid" >actual &&
	grep "initial" actual
	)
'

test_expect_success 'GIT_DIR with symbolic-ref works from outside repo' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git symbolic-ref HEAD >actual &&
	echo "refs/heads/${GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME:-master}" >expect &&
	test_cmp expect actual
'

test_expect_success 'GIT_DIR with branch works from outside repo' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git branch >actual &&
	grep "${GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME:-master}" actual
'

test_expect_success 'GIT_DIR with show-ref works from outside repo' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" git show-ref >actual &&
	grep "refs/heads/${GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME:-master}" actual
'

test_expect_success 'GIT_DIR pointing to nonexistent dir fails' '
	test_must_fail env GIT_DIR=/nonexistent/path git rev-parse HEAD 2>err &&
	test -s err
'

test_expect_success 'GIT_DIR with for-each-ref works from outside repo' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" \
	git for-each-ref --format="%(refname)" refs/heads/ >actual &&
	echo "refs/heads/${GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME:-master}" >expect &&
	test_cmp expect actual
'

# ===========================================================================
# GIT_DIR + GIT_WORK_TREE
# ===========================================================================

test_expect_success 'GIT_DIR + GIT_WORK_TREE allows ls-files from work tree' '
	(
	cd repo &&
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" \
	GIT_WORK_TREE="$TRASH_DIRECTORY/repo" \
	git ls-files >actual &&
	echo file.txt >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_DIR + GIT_WORK_TREE with status from outside' '
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" \
	GIT_WORK_TREE="$TRASH_DIRECTORY/repo" \
	git status >actual 2>&1 &&
	grep -q "On branch ${GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME:-master}" actual
'

test_expect_success 'GIT_DIR + GIT_WORK_TREE diff detects changes' '
	echo modified >repo/file.txt &&
	GIT_DIR="$TRASH_DIRECTORY/repo/.git" \
	GIT_WORK_TREE="$TRASH_DIRECTORY/repo" \
	git diff --name-only >actual &&
	echo file.txt >expect &&
	test_cmp expect actual &&
	echo hello >repo/file.txt
'

# ===========================================================================
# --git-dir flag
# ===========================================================================

test_expect_success '--git-dir flag works like GIT_DIR env' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	cd .. &&
	git --git-dir="$TRASH_DIRECTORY/repo/.git" cat-file -t "$oid" >actual &&
	echo commit >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--git-dir flag overrides env for rev-parse' '
	(
	git --git-dir="$TRASH_DIRECTORY/repo/.git" rev-parse HEAD >actual &&
	cd repo &&
	git rev-parse HEAD >../expect &&
	cd .. &&
	test_cmp expect actual
	)
'

# ===========================================================================
# GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL
# ===========================================================================

test_expect_success 'GIT_AUTHOR_NAME overrides author name in commit' '
	(
	cd repo &&
	echo "author-test" >author-test.txt &&
	git add author-test.txt &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	GIT_AUTHOR_NAME="Custom Author" \
	GIT_AUTHOR_EMAIL="custom@example.com" \
	git commit -m "custom author commit" &&
	git log -n 1 --format="%an" >actual &&
	echo "Custom Author" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_AUTHOR_EMAIL overrides author email in commit' '
	(
	cd repo &&
	git log -n 1 --format="%ae" >actual &&
	echo "custom@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_AUTHOR_NAME does not affect committer' '
	(
	cd repo &&
	git log -n 1 --format="%cn" >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_AUTHOR_DATE overrides author date' '
	(
	cd repo &&
	echo "date-test" >date-test.txt &&
	git add date-test.txt &&
	GIT_AUTHOR_DATE="2005-04-07T22:13:13" \
	git commit -m "custom date commit" &&
	git log -n 1 --format="%ai" >actual &&
	grep "2005-04-07" actual
	)
'

# ===========================================================================
# GIT_COMMITTER_NAME / GIT_COMMITTER_EMAIL
# ===========================================================================

test_expect_success 'GIT_COMMITTER_NAME overrides committer name' '
	(
	cd repo &&
	echo "committer-test" >committer-test.txt &&
	git add committer-test.txt &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	GIT_COMMITTER_NAME="Custom Committer" \
	GIT_COMMITTER_EMAIL="committer@example.com" \
	git commit -m "custom committer commit" &&
	git log -n 1 --format="%cn" >actual &&
	echo "Custom Committer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_COMMITTER_EMAIL overrides committer email' '
	(
	cd repo &&
	git log -n 1 --format="%ce" >actual &&
	echo "committer@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_COMMITTER_NAME does not affect author' '
	(
	cd repo &&
	git log -n 1 --format="%an" >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'GIT_COMMITTER_DATE overrides committer date' '
	(
	cd repo &&
	echo "cdate-test" >cdate-test.txt &&
	git add cdate-test.txt &&
	GIT_COMMITTER_DATE="2010-01-01T00:00:00" \
	git commit -m "custom committer date" &&
	git log -n 1 --format="%ci" >actual &&
	grep "2010-01-01" actual
	)
'

# ===========================================================================
# Both author and committer overrides simultaneously
# ===========================================================================

test_expect_success 'both GIT_AUTHOR_* and GIT_COMMITTER_* can be set at once' '
	(
	cd repo &&
	echo "both-test" >both-test.txt &&
	git add both-test.txt &&
	GIT_AUTHOR_NAME="Author Name" \
	GIT_AUTHOR_EMAIL="author@test.org" \
	GIT_COMMITTER_NAME="Committer Name" \
	GIT_COMMITTER_EMAIL="committer@test.org" \
	git commit -m "both overridden" &&
	git log -n 1 --format="%an <%ae>" >actual &&
	echo "Author Name <author@test.org>" >expect &&
	test_cmp expect actual &&
	git log -n 1 --format="%cn <%ce>" >actual &&
	echo "Committer Name <committer@test.org>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'author and committer can be different people' '
	(
	cd repo &&
	echo "diff-people" >diff-people.txt &&
	git add diff-people.txt &&
	GIT_AUTHOR_NAME="Alice" \
	GIT_AUTHOR_EMAIL="alice@example.com" \
	GIT_COMMITTER_NAME="Bob" \
	GIT_COMMITTER_EMAIL="bob@example.com" \
	git commit -m "alice authored, bob committed" &&
	git log -n 1 --format="%an" >author_actual &&
	echo "Alice" >author_expect &&
	test_cmp author_expect author_actual &&
	git log -n 1 --format="%cn" >committer_actual &&
	echo "Bob" >committer_expect &&
	test_cmp committer_expect committer_actual
	)
'

# ===========================================================================
# Verify via cat-file (lower level than log)
# ===========================================================================

test_expect_success 'cat-file -p shows correct author from GIT_AUTHOR_* override' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	git cat-file -p "$oid" >actual &&
	grep "Alice <alice@example.com>" actual
	)
'

test_expect_success 'cat-file -p shows correct committer from GIT_COMMITTER_* override' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	git cat-file -p "$oid" >actual &&
	grep "Bob <bob@example.com>" actual
	)
'

# ===========================================================================
# Special characters in author/committer
# ===========================================================================

test_expect_success 'GIT_AUTHOR_NAME with special characters' '
	(
	cd repo &&
	echo "special-author" >special-author.txt &&
	git add special-author.txt &&
	GIT_AUTHOR_NAME="Ørjan Müller-Straße" \
	GIT_AUTHOR_EMAIL="special@example.com" \
	git commit -m "special chars in author" &&
	git log -n 1 --format="%an" >actual &&
	echo "Ørjan Müller-Straße" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'cat-file confirms special character author' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	git cat-file -p "$oid" >actual &&
	grep "Ørjan Müller-Straße" actual
	)
'

test_expect_success 'GIT_COMMITTER_NAME with special characters' '
	(
	cd repo &&
	echo "special-committer" >special-committer.txt &&
	git add special-committer.txt &&
	GIT_COMMITTER_NAME="José García" \
	GIT_COMMITTER_EMAIL="jose@example.com" \
	git commit -m "special chars in committer" &&
	git log -n 1 --format="%cn" >actual &&
	echo "José García" >expect &&
	test_cmp expect actual
	)
'

# ===========================================================================
# Edge: empty author name
# ===========================================================================

test_expect_success 'empty GIT_AUTHOR_NAME is rejected (matches git ident.c)' '
	(
	cd repo &&
	echo "empty-author" >empty-author.txt &&
	git add empty-author.txt &&
	test_must_fail env GIT_AUTHOR_NAME="" \
		GIT_AUTHOR_EMAIL="empty@example.com" \
		git commit -m "empty author name" 2>err &&
	test_grep "empty ident name" err &&
	test_grep "Author identity unknown" err
	)
'

test_done
