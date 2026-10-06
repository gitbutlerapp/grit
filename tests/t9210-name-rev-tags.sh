#!/bin/sh
#
# Tests for 'grit name-rev' — naming commits relative to refs/tags,
# with --name-only, --tags, --all, --annotate-stdin, --exclude.

test_description='grit name-rev with tags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup: linear history with tags
#
#   A (v1.0) --- B (v2.0) --- C --- D (v3.0) --- E  (master)
#
# ---------------------------------------------------------------------------
test_expect_success 'setup: linear history with tags' '
	(
	git init --initial-branch=master repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo a >file &&
	git add file &&
	git commit -m "release 1.0" &&
	git tag v1.0 &&
	echo b >file &&
	git add file &&
	git commit -m "release 2.0" &&
	git tag v2.0 &&
	echo c >file &&
	git add file &&
	git commit -m "wip" &&
	echo d >file &&
	git add file &&
	git commit -m "release 3.0" &&
	git tag v3.0 &&
	echo e >file &&
	git add file &&
	git commit -m "post release"
	)
'

# ---------------------------------------------------------------------------
# Basic name-rev
# ---------------------------------------------------------------------------
test_expect_success 'name-rev HEAD shows master' '
	(
	cd repo &&
	grit name-rev HEAD >actual &&
	grep "master" actual
	)
'

test_expect_success 'name-rev of tagged commit shows tag name' '
	(
	cd repo &&
	oid=$(git rev-parse v1.0) &&
	grit name-rev "$oid" >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'name-rev of v2.0 commit shows v2.0 or equivalent' '
	(
	cd repo &&
	oid=$(git rev-parse v2.0) &&
	grit name-rev "$oid" >actual &&
	grep "v2.0\|v3.0~2" actual
	)
'

test_expect_success 'name-rev with tilde notation for ancestor' '
	(
	cd repo &&
	oid=$(git rev-parse v3.0~1) &&
	grit name-rev "$oid" >actual &&
	grep "~" actual
	)
'

# ---------------------------------------------------------------------------
# --name-only
# ---------------------------------------------------------------------------
test_expect_success '--name-only shows just the name' '
	(
	cd repo &&
	grit name-rev --name-only HEAD >actual &&
	echo "master" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--name-only for tagged commit' '
	(
	cd repo &&
	grit name-rev --name-only $(git rev-parse v1.0) >actual &&
	# Should contain tag reference
	grep "v1.0\|tags/v1.0" actual
	)
'

test_expect_success '--name-only strips OID prefix' '
	(
	cd repo &&
	grit name-rev --name-only HEAD >actual &&
	! grep "^[0-9a-f]\{40\}" actual
	)
'

# ---------------------------------------------------------------------------
# --tags
# ---------------------------------------------------------------------------
test_expect_success '--tags names relative to tags only' '
	(
	cd repo &&
	grit name-rev --tags $(git rev-parse v1.0) >actual &&
	grep "v1.0" actual
	)
'

test_expect_success '--tags for commit between tags uses tilde' '
	(
	cd repo &&
	oid=$(git rev-parse v3.0~1) &&
	grit name-rev --tags "$oid" >actual &&
	grep "v3.0~1\|v2.0" actual
	)
'

test_expect_success '--tags for unreachable-from-tags commit shows undefined' '
	(
	cd repo &&
	grit name-rev --tags HEAD >actual &&
	grep "undefined" actual
	)
'

test_expect_success '--tags --name-only for tagged commit' '
	(
	cd repo &&
	grit name-rev --tags --name-only $(git rev-parse v2.0) >actual &&
	grep "v2.0\|v3.0~2" actual
	)
'

# ---------------------------------------------------------------------------
# --all
# ---------------------------------------------------------------------------
test_expect_success '--all lists all refs with names' '
	(
	cd repo &&
	grit name-rev --all >actual &&
	grep "master" actual &&
	grep "v1.0" actual
	)
'

test_expect_success '--all output contains OIDs' '
	(
	cd repo &&
	grit name-rev --all >actual &&
	grep "[0-9a-f]\{40\}" actual
	)
'

test_expect_success '--all lists every tag' '
	(
	cd repo &&
	grit name-rev --all >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v3.0" actual
	)
'

# ---------------------------------------------------------------------------
# Multiple args
# ---------------------------------------------------------------------------
test_expect_success 'name-rev with multiple OIDs' '
	(
	cd repo &&
	grit name-rev HEAD HEAD~1 >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'name-rev HEAD HEAD~2 gives distinct names' '
	(
	cd repo &&
	grit name-rev --name-only HEAD HEAD~2 >actual &&
	line1=$(sed -n 1p actual) &&
	line2=$(sed -n 2p actual) &&
	test "$line1" != "$line2"
	)
'

# ---------------------------------------------------------------------------
# --annotate-stdin
# ---------------------------------------------------------------------------
test_expect_success '--annotate-stdin annotates OIDs in input' '
	(
	cd repo &&
	oid=$(git rev-parse HEAD) &&
	echo "$oid" | grit name-rev --annotate-stdin >actual &&
	grep "(master)" actual
	)
'

test_expect_success '--annotate-stdin preserves surrounding text' '
	(
	cd repo &&
	oid=$(git rev-parse v1.0) &&
	echo "commit $oid is tagged" | grit name-rev --annotate-stdin >actual &&
	grep "is tagged" actual &&
	grep "(.*v1.0.*)" actual
	)
'

test_expect_success '--annotate-stdin with multiple lines' '
	(
	cd repo &&
	oid1=$(git rev-parse HEAD) &&
	oid2=$(git rev-parse v1.0) &&
	printf "%s\n%s\n" "$oid1" "$oid2" | grit name-rev --annotate-stdin >actual &&
	test_line_count = 2 actual
	)
'

# ---------------------------------------------------------------------------
# --exclude
# ---------------------------------------------------------------------------
test_expect_success '--exclude tags/* uses only branch names' '
	(
	cd repo &&
	grit name-rev --exclude="tags/*" HEAD >actual &&
	grep "master" actual
	)
'

# ---------------------------------------------------------------------------
# Branches
# ---------------------------------------------------------------------------
test_expect_success 'setup: add a feature branch' '
	(
	cd repo &&
	git checkout -b feature v2.0 &&
	echo feat >feat &&
	git add feat &&
	git commit -m "feature work"
	)
'

test_expect_success 'name-rev on feature tip shows feature branch' '
	(
	cd repo &&
	grit name-rev --name-only HEAD >actual &&
	grep "feature" actual
	)
'

test_expect_success 'name-rev on master tip after branch creation' '
	(
	cd repo &&
	grit name-rev --name-only $(git rev-parse master) >actual &&
	grep "master" actual
	)
'

# ---------------------------------------------------------------------------
# Edge cases
# ---------------------------------------------------------------------------
test_expect_success 'name-rev with invalid OID prints skip message' '
	(
	cd repo &&
	grit name-rev 0000000000000000000000000000000000000bad 2>err;
	cat err >combined &&
	grep -i "skip\|could not\|undefined" combined
	)
'

test_expect_success 'name-rev root commit is reachable' '
	(
	cd repo &&
	oid=$(git rev-parse v1.0) &&
	grit name-rev "$oid" >actual &&
	! grep "undefined" actual
	)
'

test_expect_success '--name-only --all lists names for all refs' '
	(
	cd repo &&
	grit name-rev --all >actual &&
	lines=$(wc -l <actual) &&
	test "$lines" -ge 4
	)
'

test_done
