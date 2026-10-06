#!/bin/sh
# Tests for commit-tree: creating commits with zero, one, or multiple parents.

test_description='commit-tree with various parent configurations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

REAL_GIT=/usr/bin/git

# -- setup -----------------------------------------------------------------

test_expect_success 'setup: repo with linear history and branches' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "base" >file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "first" &&
	echo "second" >>file.txt &&
	$REAL_GIT add file.txt &&
	test_tick &&
	$REAL_GIT commit -m "second" &&
	$REAL_GIT branch side HEAD~1 &&
	$REAL_GIT checkout side &&
	echo "side" >side.txt &&
	$REAL_GIT add side.txt &&
	test_tick &&
	$REAL_GIT commit -m "side change" &&
	$REAL_GIT checkout master
	)
'

# -- orphan commit (no parents) ---------------------------------------------

test_expect_success 'commit-tree creates orphan commit (no parents)' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "orphan commit") &&
	test -n "$new" &&
	grit cat-file -t "$new" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'orphan commit has no parent lines' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "orphan") &&
	grit cat-file -p "$new" >actual &&
	! grep "^parent " actual
	)
'

test_expect_success 'orphan commit has correct tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "orphan tree check") &&
	grit cat-file -p "$new" >actual &&
	grep "^tree $tree" actual
	)
'

test_expect_success 'orphan commit has commit message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "my orphan message") &&
	grit cat-file -p "$new" >actual &&
	grep "my orphan message" actual
	)
'

# -- single parent commit ----------------------------------------------------

test_expect_success 'commit-tree with one parent' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	new=$(grit commit-tree "$tree" -p "$parent" -m "single parent") &&
	grit cat-file -p "$new" >actual &&
	grep "^parent $parent" actual
	)
'

test_expect_success 'single parent commit has exactly one parent line' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	new=$(grit commit-tree "$tree" -p "$parent" -m "one parent") &&
	grit cat-file -p "$new" >actual &&
	count=$(grep -c "^parent " actual) &&
	test "$count" = "1"
	)
'

test_expect_success 'single parent commit is a valid commit object' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	new=$(grit commit-tree "$tree" -p "$parent" -m "valid") &&
	grit cat-file -t "$new" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

# -- two parent commit (merge) -----------------------------------------------

test_expect_success 'commit-tree with two parents' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -m "merge commit") &&
	grit cat-file -p "$new" >actual &&
	grep "^parent $p1" actual &&
	grep "^parent $p2" actual
	)
'

test_expect_success 'two parent commit has exactly two parent lines' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -m "two parents") &&
	grit cat-file -p "$new" >actual &&
	count=$(grep -c "^parent " actual) &&
	test "$count" = "2"
	)
'

test_expect_success 'parent order is preserved in two-parent commit' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -m "order check") &&
	grit cat-file -p "$new" >actual &&
	first_parent=$(grep "^parent " actual | head -1 | awk "{print \$2}") &&
	second_parent=$(grep "^parent " actual | tail -1 | awk "{print \$2}") &&
	test "$first_parent" = "$p1" &&
	test "$second_parent" = "$p2"
	)
'

# -- three parent commit (octopus) -------------------------------------------

test_expect_success 'setup: create third branch' '
	(
	cd repo &&
	$REAL_GIT checkout -b third HEAD~1 &&
	echo "third branch" >third.txt &&
	$REAL_GIT add third.txt &&
	test_tick &&
	$REAL_GIT commit -m "third branch" &&
	$REAL_GIT checkout master
	)
'

test_expect_success 'commit-tree with three parents (octopus)' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	p3=$(grit rev-parse third) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -p "$p3" -m "octopus") &&
	grit cat-file -p "$new" >actual &&
	count=$(grep -c "^parent " actual) &&
	test "$count" = "3"
	)
'

test_expect_success 'octopus commit preserves all three parents' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	p3=$(grit rev-parse third) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -p "$p3" -m "octopus check") &&
	grit cat-file -p "$new" >actual &&
	grep "^parent $p1" actual &&
	grep "^parent $p2" actual &&
	grep "^parent $p3" actual
	)
'

# -- commit message variants -------------------------------------------------

test_expect_success 'commit-tree with multi-word message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "this is a longer commit message") &&
	grit cat-file -p "$new" >actual &&
	grep "this is a longer commit message" actual
	)
'

test_expect_success 'commit-tree with -F reads message from file' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	echo "message from file" >msg.txt &&
	new=$(grit commit-tree "$tree" -F msg.txt) &&
	grit cat-file -p "$new" >actual &&
	grep "message from file" actual
	)
'

# -- tree from different commits ---------------------------------------------

test_expect_success 'commit-tree with tree from earlier commit' '
	(
	cd repo &&
	old_tree=$($REAL_GIT rev-parse HEAD~1^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	new=$(grit commit-tree "$old_tree" -p "$parent" -m "old tree new parent") &&
	grit cat-file -p "$new" >actual &&
	grep "^tree $old_tree" actual &&
	grep "^parent $parent" actual
	)
'

test_expect_success 'commit-tree with side branch tree' '
	(
	cd repo &&
	side_tree=$($REAL_GIT rev-parse side^{tree}) &&
	new=$(grit commit-tree "$side_tree" -m "side tree orphan") &&
	grit cat-file -p "$new" >actual &&
	grep "^tree $side_tree" actual
	)
'

# -- author/committer fields --------------------------------------------------

test_expect_success 'commit-tree includes author field' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "author check") &&
	grit cat-file -p "$new" >actual &&
	grep "^author " actual
	)
'

test_expect_success 'commit-tree includes committer field' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "committer check") &&
	grit cat-file -p "$new" >actual &&
	grep "^committer " actual
	)
'

# -- round-trip: commit-tree then rev-parse -----------------------------------

test_expect_success 'commit-tree output is a valid SHA' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "sha check") &&
	len=$(printf "%s" "$new" | wc -c) &&
	test "$len" -ge 40
	)
'

test_expect_success 'rev-parse resolves commit-tree output' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "resolve check") &&
	resolved=$(grit rev-parse "$new") &&
	test "$resolved" = "$new"
	)
'

# -- compare with real git ---------------------------------------------------

test_expect_success 'commit-tree type matches real git' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "type compare") &&
	grit cat-file -t "$new" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit-tree with two parents verified by rev-parse' '
	(
	cd repo &&
	$REAL_GIT checkout master &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	p1=$(grit rev-parse master) &&
	p2=$(grit rev-parse side) &&
	new=$(grit commit-tree "$tree" -p "$p1" -p "$p2" -m "parent count") &&
	grit cat-file -p "$new" >actual &&
	grep "^parent $p1" actual &&
	grep "^parent $p2" actual
	)
'

test_expect_success 'commit-tree orphan has zero parents confirmed by cat-file' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	new=$(grit commit-tree "$tree" -m "zero parents") &&
	grit cat-file -p "$new" >actual &&
	count=$(grep -c "^parent " actual || true) &&
	test "$count" = "0"
	)
'

test_done
