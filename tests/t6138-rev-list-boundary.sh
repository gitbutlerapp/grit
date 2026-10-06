#!/bin/sh
# Tests for rev-list: exclusion with ^, --count, --reverse, --max-count,
# --skip, --first-parent, --topo-order, --date-order, --ancestry-path,
# --all, --stdin, --parents, and --format options.

test_description='rev-list advanced traversal and formatting'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=master
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with branching history' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo A >file.txt &&
	git add file.txt &&
	grit commit -m "commit A" &&
	echo B >file.txt &&
	git add file.txt &&
	grit commit -m "commit B" &&
	echo C >file.txt &&
	git add file.txt &&
	grit commit -m "commit C" &&
	grit tag v1.0 &&
	git checkout -b topic HEAD~2 &&
	echo D >file.txt &&
	git add file.txt &&
	grit commit -m "commit D" &&
	echo E >file.txt &&
	git add file.txt &&
	grit commit -m "commit E" &&
	git checkout master
	)
'

# ── basic rev-list ────────────────────────────────────────────────────────

test_expect_success 'rev-list HEAD lists all commits on master' '
	(
	cd repo &&
	grit rev-list HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

test_expect_success 'rev-list --all lists all reachable commits' '
	(
	cd repo &&
	grit rev-list --all >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success 'rev-list --count HEAD counts commits' '
	(
	cd repo &&
	grit rev-list --count HEAD >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'rev-list --count --all counts all commits' '
	(
	cd repo &&
	grit rev-list --count --all >actual &&
	echo "5" >expect &&
	test_cmp expect actual
	)
'

# ── exclusion with ^ ──────────────────────────────────────────────────────

test_expect_success 'rev-list topic ^master shows topic-only commits' '
	(
	cd repo &&
	grit rev-list topic ^master >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'rev-list master ^topic shows master-only commits' '
	(
	cd repo &&
	grit rev-list master ^topic >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'rev-list with common ancestor excluded' '
	(
	cd repo &&
	base=$(grit rev-parse HEAD~2) &&
	grit rev-list HEAD "^$base" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success 'excluding HEAD from itself yields empty list' '
	(
	cd repo &&
	grit rev-list HEAD ^HEAD >actual &&
	test_must_be_empty actual
	)
'

# ── --reverse ─────────────────────────────────────────────────────────────

test_expect_success '--reverse outputs oldest first' '
	(
	cd repo &&
	grit rev-list --reverse HEAD >actual &&
	first=$(head -1 actual) &&
	root=$(grit rev-list HEAD | tail -1) &&
	test "$first" = "$root"
	)
'

test_expect_success '--reverse with --count still gives count' '
	(
	cd repo &&
	grit rev-list --reverse --count HEAD >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

# ── --max-count ───────────────────────────────────────────────────────────

test_expect_success '--max-count=1 returns only one commit' '
	(
	cd repo &&
	grit rev-list --max-count=1 HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success '--max-count=2 returns two commits' '
	(
	cd repo &&
	grit rev-list --max-count=2 HEAD >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success '--max-count=0 returns nothing' '
	(
	cd repo &&
	grit rev-list --max-count=0 HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--max-count larger than total still works' '
	(
	cd repo &&
	grit rev-list --max-count=100 HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

# ── --skip ────────────────────────────────────────────────────────────────

test_expect_success '--skip=1 skips first commit' '
	(
	cd repo &&
	grit rev-list --skip=1 HEAD >actual &&
	test $(wc -l <actual) -eq 2
	)
'

test_expect_success '--skip=2 skips first two commits' '
	(
	cd repo &&
	grit rev-list --skip=2 HEAD >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_expect_success '--skip and --max-count combine correctly' '
	(
	cd repo &&
	grit rev-list --skip=1 --max-count=1 HEAD >actual &&
	test $(wc -l <actual) -eq 1 &&
	full=$(grit rev-list HEAD) &&
	second=$(echo "$full" | sed -n 2p) &&
	got=$(cat actual) &&
	test "$got" = "$second"
	)
'

# ── --first-parent ────────────────────────────────────────────────────────

test_expect_success '--first-parent follows only first parent' '
	(
	cd repo &&
	grit rev-list --first-parent HEAD >actual &&
	test $(wc -l <actual) -eq 3
	)
'

# ── --topo-order / --date-order ───────────────────────────────────────────

test_expect_success '--topo-order lists commits' '
	(
	cd repo &&
	grit rev-list --topo-order --all >actual &&
	test $(wc -l <actual) -eq 5
	)
'

test_expect_success '--date-order lists commits' '
	(
	cd repo &&
	grit rev-list --date-order --all >actual &&
	test $(wc -l <actual) -eq 5
	)
'

# ── --ancestry-path ──────────────────────────────────────────────────────

test_expect_success '--ancestry-path limits to path between endpoints' '
	(
	cd repo &&
	base=$(grit rev-parse master~2) &&
	grit rev-list --ancestry-path topic "^$base" >actual &&
	test $(wc -l <actual) -eq 2
	)
'

# ── --parents ─────────────────────────────────────────────────────────────

test_expect_success '--parents shows parent hashes' '
	(
	cd repo &&
	grit rev-list --parents HEAD >actual &&
	head -1 actual >first_line &&
	# first line should have: commit_hash parent_hash
	test $(wc -w <first_line) -eq 2
	)
'

test_expect_success '--parents root commit has no parent' '
	(
	cd repo &&
	root=$(grit rev-list HEAD | tail -1) &&
	grit rev-list --parents HEAD >actual &&
	grep "^$root" actual >root_line &&
	# root commit line should have just its own hash (1 word)
	test $(wc -w <root_line) -eq 1
	)
'

# ── --format ──────────────────────────────────────────────────────────────

test_expect_success '--format=%H shows full hashes' '
	(
	cd repo &&
	grit rev-list --format=%H HEAD >actual &&
	grep -c "^[0-9a-f]\{40\}$" actual >count &&
	# 3 commits × 2 lines each (commit header + format line) = 6, but 3 hash lines
	test $(cat count) -eq 3
	)
'

test_expect_success '--format=%h shows short hashes' '
	(
	cd repo &&
	grit rev-list --format=%h HEAD >actual &&
	# should have abbreviated hashes (not 40 chars)
	grep -v "^commit " actual >hashes &&
	while read h; do
		len=${#h} &&
		test "$len" -lt 40 || return 1
	done <hashes
	)
'

test_expect_success '--format=%s shows subjects' '
	(
	cd repo &&
	grit rev-list --format=%s HEAD >actual &&
	grep "commit A" actual &&
	grep "commit B" actual &&
	grep "commit C" actual
	)
'

# ── --stdin ───────────────────────────────────────────────────────────────

test_expect_success '--stdin reads refs from stdin' '
	(
	cd repo &&
	echo HEAD | grit rev-list --stdin >actual &&
	grit rev-list HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin with exclusion' '
	(
	cd repo &&
	head_sha=$(grit rev-parse HEAD) &&
	parent_sha=$(grit rev-parse HEAD~1) &&
	printf "%s\n^%s\n" "$head_sha" "$parent_sha" | grit rev-list --stdin >actual &&
	test $(wc -l <actual) -eq 1
	)
'

test_done
