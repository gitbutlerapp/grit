#!/bin/sh
# Tests for log formatting: --format variants, --oneline, --reverse, --graph,
# --skip, --max-count, and various format placeholders.

test_description='log formatting options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with multiple commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Log Author" &&
	git config user.email "log@test.com" &&
	echo "first" >file.txt &&
	git add file.txt &&
	git commit -m "first commit" &&
	echo "second" >>file.txt &&
	git add file.txt &&
	git commit -m "second commit" &&
	echo "third" >>file.txt &&
	git add file.txt &&
	git commit -m "third commit" &&
	echo "fourth" >>file.txt &&
	git add file.txt &&
	git commit -m "fourth commit" &&
	echo "fifth" >>file.txt &&
	git add file.txt &&
	git commit -m "fifth commit"
	)
'

# ── --format=%H (full hash) ─────────────────────────────────────────────────

test_expect_success 'format %H produces 40-char hex hash' '
	(
	cd repo &&
	git log -n1 --format="%H" >actual &&
	hash=$(cat actual) &&
	test $(echo "$hash" | wc -c) -eq 41 &&
	echo "$hash" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'format %H matches rev-parse HEAD' '
	(
	cd repo &&
	git log -n1 --format="%H" >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# ── --format=%h (abbreviated hash) ──────────────────────────────────────────

test_expect_success 'format %h produces abbreviated hash' '
	(
	cd repo &&
	git log -n1 --format="%h" >actual &&
	abbrev=$(cat actual) &&
	full=$(git rev-parse HEAD) &&
	# abbreviated hash should be a prefix of full hash
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

test_expect_success 'format %h is shorter than %H' '
	(
	cd repo &&
	h_len=$(git log -n1 --format="%h" | wc -c) &&
	H_len=$(git log -n1 --format="%H" | wc -c) &&
	test "$h_len" -lt "$H_len"
	)
'

# ── --format=%T (tree hash) ─────────────────────────────────────────────────

test_expect_success 'format %T produces tree hash' '
	(
	cd repo &&
	git log -n1 --format="%T" >actual &&
	tree_from_parse=$(git rev-parse HEAD^{tree}) &&
	echo "$tree_from_parse" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %t produces abbreviated tree hash' '
	(
	cd repo &&
	abbrev=$(git log -n1 --format="%t") &&
	full=$(git rev-parse HEAD^{tree}) &&
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

# ── --format=%P and %p (parent hashes) ─────────────────────────────────────

test_expect_success 'format %P produces parent full hash' '
	(
	cd repo &&
	parent_from_log=$(git log -n1 --format="%P") &&
	parent_from_parse=$(git rev-parse HEAD~1) &&
	test "$parent_from_log" = "$parent_from_parse"
	)
'

test_expect_success 'format %p produces abbreviated parent hash' '
	(
	cd repo &&
	abbrev=$(git log -n1 --format="%p") &&
	full=$(git rev-parse HEAD~1) &&
	case "$full" in
	"$abbrev"*) true ;;
	*) false ;;
	esac
	)
'

test_expect_success 'root commit has empty parent field' '
	(
	cd repo &&
	root=$(git log --reverse --format="%H" | head -1) &&
	git cat-file commit "$root" >raw &&
	! grep "^parent" raw
	)
'

# ── --format=%an, %ae, %cn, %ce (author/committer) ─────────────────────────

test_expect_success 'format %an shows author name' '
	(
	cd repo &&
	git log -n1 --format="%an" >actual &&
	echo "A U Thor" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %ae shows author email' '
	(
	cd repo &&
	git log -n1 --format="%ae" >actual &&
	echo "author@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %cn shows committer name' '
	(
	cd repo &&
	git log -n1 --format="%cn" >actual &&
	echo "C O Mitter" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %ce shows committer email' '
	(
	cd repo &&
	git log -n1 --format="%ce" >actual &&
	echo "committer@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format combined author fields' '
	(
	cd repo &&
	git log -n1 --format="%an <%ae>" >actual &&
	echo "A U Thor <author@example.com>" >expect &&
	test_cmp expect actual
	)
'

# ── --format=%s (subject) ──────────────────────────────────────────────────

test_expect_success 'format %s shows subject line' '
	(
	cd repo &&
	git log -n1 --format="%s" >actual &&
	echo "fifth commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %s across multiple commits' '
	(
	cd repo &&
	git log -n3 --format="%s" >actual &&
	head -1 actual >first &&
	echo "fifth commit" >expect &&
	test_cmp expect first &&
	tail -1 actual >last &&
	echo "third commit" >expect &&
	test_cmp expect last
	)
'

# ── --format=%b (body) ─────────────────────────────────────────────────────

test_expect_success 'format %b is empty for single-line commit messages' '
	(
	cd repo &&
	body=$(git log -n1 --format="%b") &&
	test -z "$body"
	)
'

# ── --format=%ad, %cd (dates) ──────────────────────────────────────────────

test_expect_success 'format %ad shows author date' '
	(
	cd repo &&
	date_str=$(git log -n1 --format="%ad") &&
	test -n "$date_str"
	)
'

test_expect_success 'format %cd shows committer date' '
	(
	cd repo &&
	date_str=$(git log -n1 --format="%cd") &&
	test -n "$date_str"
	)
'

# ── --format=%n (newline) ──────────────────────────────────────────────────

test_expect_success 'format %n inserts newline' '
	(
	cd repo &&
	git log -n1 --format="line1%nline2" >actual &&
	test_line_count = 2 actual &&
	head -1 actual >first &&
	echo "line1" >expect &&
	test_cmp expect first
	)
'

# ── --format=%% (literal percent) ──────────────────────────────────────────

test_expect_success 'format %% produces literal percent' '
	(
	cd repo &&
	git log -n1 --format="%%" >actual &&
	echo "%" >expect &&
	test_cmp expect actual
	)
'

# ── --format with static text ──────────────────────────────────────────────

test_expect_success 'format with prefix text' '
	(
	cd repo &&
	git log -n1 --format="commit: %h" >actual &&
	grep "^commit: " actual
	)
'

test_expect_success 'format with multiple placeholders and text' '
	(
	cd repo &&
	git log -n1 --format="[%h] %s (%an)" >actual &&
	grep "^\[" actual &&
	grep "fifth commit" actual &&
	grep "(A U Thor)" actual
	)
'

# ── --oneline ──────────────────────────────────────────────────────────────

test_expect_success 'oneline shows abbreviated hash and subject' '
	(
	cd repo &&
	git log --oneline -n1 >actual &&
	hash=$(git log -n1 --format="%h") &&
	grep "$hash" actual &&
	grep "fifth commit" actual
	)
'

test_expect_success 'oneline produces one line per commit' '
	(
	cd repo &&
	git log --oneline -n5 >actual &&
	test_line_count = 5 actual
	)
'

# ── --max-count / -n ───────────────────────────────────────────────────────

test_expect_success 'log -n1 shows exactly one commit' '
	(
	cd repo &&
	git log -n1 --format="%s" >actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'log -n3 shows exactly three commits' '
	(
	cd repo &&
	git log -n3 --format="%s" >actual &&
	test_line_count = 3 actual
	)
'

test_expect_success 'log -n1 with skip past end shows nothing' '
	(
	cd repo &&
	git log --skip=100 -n1 --format="%s" >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'log --max-count=2 shows two commits' '
	(
	cd repo &&
	git log --max-count=2 --format="%s" >actual &&
	test_line_count = 2 actual
	)
'

# ── --skip ─────────────────────────────────────────────────────────────────

test_expect_success 'log --skip=2 skips first two commits' '
	(
	cd repo &&
	git log --skip=2 -n1 --format="%s" >actual &&
	echo "third commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'log --skip=4 -n1 shows first commit' '
	(
	cd repo &&
	git log --skip=4 -n1 --format="%s" >actual &&
	echo "first commit" >expect &&
	test_cmp expect actual
	)
'

# ── --reverse ──────────────────────────────────────────────────────────────

test_expect_success 'log --reverse shows oldest first' '
	(
	cd repo &&
	git log --reverse --format="%s" >actual &&
	head -1 actual >first &&
	echo "first commit" >expect &&
	test_cmp expect first
	)
'

test_expect_success 'log --reverse last entry is newest' '
	(
	cd repo &&
	git log --reverse --format="%s" >actual &&
	tail -1 actual >last &&
	echo "fifth commit" >expect &&
	test_cmp expect last
	)
'

# ── Format on specific revision ────────────────────────────────────────────

test_expect_success 'log format on specific commit by hash' '
	(
	cd repo &&
	first_oid=$(git log --reverse --format="%H" | head -1) &&
	git cat-file commit "$first_oid" >raw &&
	grep "first commit" raw
	)
'

test_expect_success 'log --skip + -n picks correct commit' '
	(
	cd repo &&
	git log --skip=1 -n1 --format="%s" >actual &&
	echo "fourth commit" >expect &&
	test_cmp expect actual
	)
'

test_done
