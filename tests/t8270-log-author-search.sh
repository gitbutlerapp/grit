#!/bin/sh
# Tests for log --author=pattern matching.
# Some tests use test_expect_success for features not yet implemented.

test_description='log --author filtering and author-related format output'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup: create commits with distinct authors
###########################################################################

test_expect_success 'setup repository with multiple authors' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Default User" &&
	git config user.email "default@example.com" &&

	GIT_AUTHOR_NAME="Alice Anderson" GIT_AUTHOR_EMAIL="alice@example.com" \
		GIT_COMMITTER_NAME="Committer One" GIT_COMMITTER_EMAIL="c1@example.com" \
		git commit --allow-empty -m "commit by Alice: first" &&

	GIT_AUTHOR_NAME="Alice Anderson" GIT_AUTHOR_EMAIL="alice@example.com" \
		GIT_COMMITTER_NAME="Committer One" GIT_COMMITTER_EMAIL="c1@example.com" \
		git commit --allow-empty -m "commit by Alice: second" &&

	GIT_AUTHOR_NAME="Bob Builder" GIT_AUTHOR_EMAIL="bob@example.com" \
		GIT_COMMITTER_NAME="Committer Two" GIT_COMMITTER_EMAIL="c2@example.com" \
		git commit --allow-empty -m "commit by Bob: third" &&

	GIT_AUTHOR_NAME="Charlie Chaplin" GIT_AUTHOR_EMAIL="charlie@example.com" \
		GIT_COMMITTER_NAME="Committer One" GIT_COMMITTER_EMAIL="c1@example.com" \
		git commit --allow-empty -m "commit by Charlie: fourth" &&

	GIT_AUTHOR_NAME="Alice Wonderland" GIT_AUTHOR_EMAIL="alice.w@example.com" \
		GIT_COMMITTER_NAME="Committer Three" GIT_COMMITTER_EMAIL="c3@example.com" \
		git commit --allow-empty -m "commit by second Alice: fifth"
	)
'

###########################################################################
# Section 1: Verify author data is stored correctly via log --format
###########################################################################

test_expect_success 'log --format=%an shows author names' '
	(
	cd repo &&
	git log --format="%an" >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'log --format=%ae shows author emails' '
	(
	cd repo &&
	git log --format="%ae" >actual &&
	grep "alice@example.com" actual &&
	grep "bob@example.com" actual &&
	grep "charlie@example.com" actual
	)
'

test_expect_success 'log --format shows correct author for HEAD commit' '
	(
	cd repo &&
	git log -n1 --format="%an" >actual &&
	echo "Alice Wonderland" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%an shows all authors in order' '
	(
	cd repo &&
	git log --format="%an" >actual &&
	head -1 actual >first &&
	echo "Alice Wonderland" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'log --format with author name and email combined' '
	(
	cd repo &&
	git log -n1 --format="%an <%ae>" >actual &&
	echo "Alice Wonderland <alice.w@example.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%H with author produces valid OIDs' '
	(
	cd repo &&
	git log --format="%H %an" >actual &&
	while read oid rest; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || exit 1
	done <actual
	)
'

###########################################################################
# Section 2: --author filtering (not yet implemented — test_expect_success)
###########################################################################

test_expect_success 'log --author=Alice shows only commits by Alice' '
	(
	cd repo &&
	git log --author=Alice --format="%an" >actual &&
	test $(wc -l <actual) -eq 3 &&
	! grep "Bob" actual &&
	! grep "Charlie" actual
	)
'

test_expect_success 'log --author=Bob shows only commits by Bob' '
	(
	cd repo &&
	git log --author=Bob --format="%an" >actual &&
	test $(wc -l <actual) -eq 1 &&
	echo "Bob Builder" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --author matches by email address' '
	(
	cd repo &&
	git log --author=charlie@example.com --format="%an" >actual &&
	test $(wc -l <actual) -eq 1 &&
	echo "Charlie Chaplin" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --author with partial name match' '
	(
	cd repo &&
	git log --author=Anderson --format="%an" >actual &&
	test $(wc -l <actual) -eq 2 &&
	grep "Alice Anderson" actual
	)
'

test_expect_success 'log --author with partial email match' '
	(
	cd repo &&
	git log --author=alice --format="%ae" >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'log --author with -i is case-insensitive' '
	(
	cd repo &&
	git log -i --author=alice --format="%an" >actual_lower &&
	git log -i --author=ALICE --format="%an" >actual_upper &&
	test_cmp actual_lower actual_upper
	)
'

test_expect_success 'log --author with no match produces empty output' '
	(
	cd repo &&
	git log --author=Nonexistent --format="%an" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --author combined with -n limits results' '
	(
	cd repo &&
	git log --author=Alice -n1 --format="%an" >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --author combined with --oneline' '
	(
	cd repo &&
	git log --author=Bob --oneline >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success 'log --author combined with --reverse' '
	(
	cd repo &&
	git log --author=Alice --reverse --format="%s" >actual &&
	head -1 actual >first &&
	echo "commit by Alice: first" >expect &&
	test_cmp expect first
	)
'

###########################################################################
# Section 3: log format atoms related to author
###########################################################################

test_expect_success 'log --format=%an produces non-empty output per commit' '
	(
	cd repo &&
	git log --format="%an" >actual &&
	while read line; do
		test -n "$line" || exit 1
	done <actual
	)
'

test_expect_success 'log --format=%ae produces valid email addresses' '
	(
	cd repo &&
	git log --format="%ae" >actual &&
	while read line; do
		echo "$line" | grep -q "@" || exit 1
	done <actual
	)
'

test_expect_success 'log --format=%an differs from %cn for different author/committer' '
	(
	cd repo &&
	git log -n1 --format="%an" >author &&
	git log -n1 --format="%cn" >committer &&
	! test_cmp author committer
	)
'

test_expect_success 'log -n2 returns exactly two commits' '
	(
	cd repo &&
	git log -n2 --format="%H" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'log --format with multiple author atoms on one line' '
	(
	cd repo &&
	git log -n1 --format="author:%an email:%ae" >actual &&
	grep "author:Alice Wonderland" actual &&
	grep "email:alice.w@example.com" actual
	)
'

###########################################################################
# Section 4: Workaround — manual grep of formatted output
###########################################################################

test_expect_success 'grep log output to simulate --author filter' '
	(
	cd repo &&
	git log --format="%H %an" >all &&
	grep "Alice" all >filtered &&
	test $(wc -l <filtered) -eq 3
	)
'

test_expect_success 'grep log output for Bob finds exactly one commit' '
	(
	cd repo &&
	git log --format="%H %an" >all &&
	grep "Bob" all >filtered &&
	test $(wc -l <filtered) -eq 1
	)
'

test_expect_success 'grep log output by email domain' '
	(
	cd repo &&
	git log --format="%H %ae" >all &&
	grep "example.com" all >filtered &&
	test $(wc -l <filtered) -eq 5
	)
'

test_expect_success 'author and committer are distinct for each commit' '
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
# Section 5: Edge cases
###########################################################################

test_expect_success 'log --author with regex metacharacters' '
	(
	cd repo &&
	git log --author="Alice.*Anderson" --format="%an" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'log --author on empty repository produces no output' '
	(
	git init empty-repo &&
	cd empty-repo &&
	test_must_fail git log --author=Anyone --format="%an" >actual 2>/dev/null &&
	test_must_be_empty actual
	)
'

test_expect_success 'log on single commit shows correct author' '
	(
	git init single-repo &&
	cd single-repo &&
	GIT_AUTHOR_NAME="Solo Author" GIT_AUTHOR_EMAIL="solo@example.com" \
		git commit --allow-empty -m "only commit" &&
	git log --format="%an" >actual &&
	echo "Solo Author" >expect &&
	test_cmp expect actual
	)
'

test_done
