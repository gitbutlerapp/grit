#!/bin/sh
# Test log --format with author/committer placeholders, commit hash
# placeholders, tree hash, subject, body, and combined formats.
# Also tests with different author vs committer identities.

test_description='grit log --format author and committer'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- setup ----

test_expect_success 'setup: create repo with commits including dual identity' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "alice@example.com" &&
	grit config user.name "Alice Author" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first commit" &&
	echo "second" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "second commit" &&
	echo "third" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "third commit" &&
	echo "fourth" >>file.txt &&
	grit add file.txt &&
	test_tick &&
	GIT_AUTHOR_NAME="Different Author" \
	GIT_AUTHOR_EMAIL="diff@author.com" \
	GIT_COMMITTER_NAME="Other Committer" \
	GIT_COMMITTER_EMAIL="other@committer.com" \
	grit commit -m "dual identity commit"
	)
'

# ---- author name ----

test_expect_success 'log --format=%an shows author name for latest' '
	(
	cd repo &&
	grit log --format="%an" -n 1 >actual &&
	echo "Different Author" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%an shows all 4 commits' '
	(
	cd repo &&
	grit log --format="%an" >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" = "4"
	)
'

# ---- author email ----

test_expect_success 'log --format=%ae shows author email for latest' '
	(
	cd repo &&
	grit log --format="%ae" -n 1 >actual &&
	echo "diff@author.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%ae shows alice for second latest' '
	(
	cd repo &&
	grit log --format="%ae" -n 2 | tail -1 >actual &&
	echo "alice@example.com" >expect &&
	test_cmp expect actual
	)
'

# ---- committer name and email ----

test_expect_success 'log --format=%cn shows committer name for latest' '
	(
	cd repo &&
	grit log --format="%cn" -n 1 >actual &&
	echo "Other Committer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%ce shows committer email for latest' '
	(
	cd repo &&
	grit log --format="%ce" -n 1 >actual &&
	echo "other@committer.com" >expect &&
	test_cmp expect actual
	)
'

# ---- combined author format ----

test_expect_success 'log --format="%an <%ae>" shows combined author' '
	(
	cd repo &&
	grit log --format="%an <%ae>" -n 1 >actual &&
	echo "Different Author <diff@author.com>" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format="%cn <%ce>" shows combined committer' '
	(
	cd repo &&
	grit log --format="%cn <%ce>" -n 1 >actual &&
	echo "Other Committer <other@committer.com>" >expect &&
	test_cmp expect actual
	)
'

# ---- author vs committer differ ----

test_expect_success 'author and committer differ in dual identity commit' '
	(
	cd repo &&
	grit log --format="%an|%cn" -n 1 >actual &&
	echo "Different Author|Other Committer" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'author and committer emails differ in dual identity commit' '
	(
	cd repo &&
	grit log --format="%ae|%ce" -n 1 >actual &&
	echo "diff@author.com|other@committer.com" >expect &&
	test_cmp expect actual
	)
'

# ---- subject ----

test_expect_success 'log --format=%s shows subject for latest' '
	(
	cd repo &&
	grit log --format="%s" -n 1 >actual &&
	echo "dual identity commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format=%s shows different subjects for each commit' '
	(
	cd repo &&
	grit log --format="%s" >actual &&
	grep "first commit" actual &&
	grep "second commit" actual &&
	grep "third commit" actual &&
	grep "dual identity commit" actual
	)
'

test_expect_success 'log --format=%s last entry is first commit' '
	(
	cd repo &&
	grit log --format="%s" >actual &&
	tail -1 actual >last_line &&
	echo "first commit" >expect &&
	test_cmp expect last_line
	)
'

# ---- commit hash ----

test_expect_success 'log --format=%H shows full 40-char hash' '
	(
	cd repo &&
	grit log --format="%H" -n 1 >actual &&
	grep -E "^[0-9a-f]{40}$" actual
	)
'

test_expect_success 'log --format=%h shows abbreviated hash' '
	(
	cd repo &&
	grit log --format="%h" -n 1 >actual &&
	len=$(cat actual | tr -d "\n" | wc -c | tr -d " ") &&
	test "$len" -ge 4 &&
	test "$len" -le 40
	)
'

test_expect_success 'log --format=%h hash is prefix of %H' '
	(
	cd repo &&
	full_hash=$(grit log --format="%H" -n 1) &&
	short_hash=$(grit log --format="%h" -n 1) &&
	case "$full_hash" in
	"$short_hash"*) true ;;
	*) false ;;
	esac
	)
'

# ---- tree hash ----

test_expect_success 'log --format=%T shows full tree hash' '
	(
	cd repo &&
	grit log --format="%T" -n 1 >actual &&
	grep -E "^[0-9a-f]{40}$" actual
	)
'

test_expect_success 'log --format=%t shows abbreviated tree hash' '
	(
	cd repo &&
	grit log --format="%t" -n 1 >actual &&
	len=$(cat actual | tr -d "\n" | wc -c | tr -d " ") &&
	test "$len" -ge 4 &&
	test "$len" -le 40
	)
'

# ---- multi-field format strings ----

test_expect_success 'log --format with multiple placeholders' '
	(
	cd repo &&
	grit log --format="%h %an %s" -n 1 >actual &&
	grep "Different Author" actual &&
	grep "dual identity commit" actual
	)
'

test_expect_success 'log --format with pipe separators' '
	(
	cd repo &&
	grit log --format="%an|%ae|%s" -n 1 >actual &&
	echo "Different Author|diff@author.com|dual identity commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --format with committer pipe separators' '
	(
	cd repo &&
	grit log --format="%cn|%ce|%s" -n 1 >actual &&
	echo "Other Committer|other@committer.com|dual identity commit" >expect &&
	test_cmp expect actual
	)
'

# ---- format with -n limiting ----

test_expect_success 'log --format=%H -n 2 shows exactly 2 hashes' '
	(
	cd repo &&
	grit log --format="%H" -n 2 >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" = "2"
	)
'

test_expect_success 'log --format=%H -n 1 shows exactly 1 hash' '
	(
	cd repo &&
	grit log --format="%H" -n 1 >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" = "1"
	)
'

# ---- body placeholder ----

test_expect_success 'log --format=%b is empty for single-line messages' '
	(
	cd repo &&
	grit log --format="%b" -n 1 >actual &&
	# body should be empty or just whitespace for single-line commit messages
	cleaned=$(tr -d "[:space:]" <actual) &&
	test -z "$cleaned"
	)
'

# ---- date placeholders ----

test_expect_success 'log --format=%ad shows author date' '
	(
	cd repo &&
	grit log --format="%ad" -n 1 >actual &&
	test -s actual
	)
'

test_expect_success 'log --format=%cd shows committer date' '
	(
	cd repo &&
	grit log --format="%cd" -n 1 >actual &&
	test -s actual
	)
'

# ---- second-to-last commit has original identity ----

test_expect_success 'second commit has original author' '
	(
	cd repo &&
	grit log --format="%an" --skip 1 -n 1 >actual &&
	echo "Alice Author" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'second commit has original committer' '
	(
	cd repo &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	grit log --format="%cn" --skip 1 -n 1 >actual &&
	echo "Alice Author" >expect &&
	test_cmp expect actual
	)
'

# ---- unique hashes per commit ----

test_expect_success 'each commit has a unique hash' '
	(
	cd repo &&
	grit log --format="%H" >actual &&
	total=$(wc -l <actual | tr -d " ") &&
	unique=$(sort -u actual | wc -l | tr -d " ") &&
	test "$total" = "$unique"
	)
'

test_expect_success 'each commit has a unique tree hash' '
	(
	cd repo &&
	grit log --format="%T" >actual &&
	total=$(wc -l <actual | tr -d " ") &&
	unique=$(sort -u actual | wc -l | tr -d " ") &&
	test "$total" = "$unique"
	)
'

test_done
