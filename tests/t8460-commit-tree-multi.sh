#!/bin/sh
# Tests for commit-tree with multiple -p parents, complex DAGs.

test_description='commit-tree with multiple parents and complex DAGs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@test.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	TREE=$(git write-tree) &&
	git update-ref refs/tags/BASE_TREE "$TREE"
	)
'

# Helper to get tree OID
get_tree () {
	git rev-parse BASE_TREE
}

test_expect_success 'commit-tree creates root commit (no parents)' '
	(
	cd repo &&
	oid=$(echo "root" | git commit-tree $(get_tree)) &&
	test -n "$oid" &&
	git cat-file -t "$oid" >actual &&
	echo commit >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'root commit has no parent lines' '
	(
	cd repo &&
	oid=$(echo "root" | git commit-tree $(get_tree)) &&
	git cat-file -p "$oid" >out &&
	! grep "^parent " out
	)
'

test_expect_success 'commit-tree with single -p parent' '
	(
	cd repo &&
	root=$(echo "root" | git commit-tree $(get_tree)) &&
	child=$(echo "child" | git commit-tree $(get_tree) -p "$root") &&
	git cat-file -p "$child" >out &&
	grep "^parent $root" out
	)
'

test_expect_success 'commit-tree with two -p parents (merge commit)' '
	(
	cd repo &&
	p1=$(echo "p1" | git commit-tree $(get_tree)) &&
	p2=$(echo "p2" | git commit-tree $(get_tree)) &&
	merge=$(echo "merge" | git commit-tree $(get_tree) -p "$p1" -p "$p2") &&
	git cat-file -p "$merge" >out &&
	test $(grep -c "^parent" out) = 2 &&
	grep "^parent $p1" out &&
	grep "^parent $p2" out
	)
'

test_expect_success 'commit-tree with three -p parents (octopus)' '
	(
	cd repo &&
	p1=$(echo "p1" | git commit-tree $(get_tree)) &&
	p2=$(echo "p2" | git commit-tree $(get_tree)) &&
	p3=$(echo "p3" | git commit-tree $(get_tree)) &&
	oct=$(echo "octopus" | git commit-tree $(get_tree) -p "$p1" -p "$p2" -p "$p3") &&
	git cat-file -p "$oct" >out &&
	test $(grep -c "^parent" out) = 3
	)
'

test_expect_success 'commit-tree with four -p parents' '
	(
	cd repo &&
	p1=$(echo "p1" | git commit-tree $(get_tree)) &&
	p2=$(echo "p2" | git commit-tree $(get_tree)) &&
	p3=$(echo "p3" | git commit-tree $(get_tree)) &&
	p4=$(echo "p4" | git commit-tree $(get_tree)) &&
	m=$(echo "quad" | git commit-tree $(get_tree) -p "$p1" -p "$p2" -p "$p3" -p "$p4") &&
	git cat-file -p "$m" >out &&
	test $(grep -c "^parent" out) = 4
	)
'

test_expect_success 'parent order is preserved in commit object' '
	(
	cd repo &&
	p1=$(echo "first" | git commit-tree $(get_tree)) &&
	p2=$(echo "second" | git commit-tree $(get_tree)) &&
	p3=$(echo "third" | git commit-tree $(get_tree)) &&
	m=$(echo "ordered" | git commit-tree $(get_tree) -p "$p1" -p "$p2" -p "$p3") &&
	git cat-file -p "$m" | grep "^parent" >parents &&
	sed -n 1p parents >first_parent &&
	sed -n 2p parents >second_parent &&
	sed -n 3p parents >third_parent &&
	grep "$p1" first_parent &&
	grep "$p2" second_parent &&
	grep "$p3" third_parent
	)
'

test_expect_success 'build a diamond DAG with commit-tree' '
	(
	cd repo &&
	root=$(echo "root" | git commit-tree $(get_tree)) &&
	left=$(echo "left" | git commit-tree $(get_tree) -p "$root") &&
	right=$(echo "right" | git commit-tree $(get_tree) -p "$root") &&
	merge=$(echo "merge" | git commit-tree $(get_tree) -p "$left" -p "$right") &&
	git cat-file -p "$merge" >out &&
	test $(grep -c "^parent" out) = 2 &&
	# Verify the merge base
	mb=$(git merge-base "$left" "$right") &&
	test "$mb" = "$root"
	)
'

test_expect_success 'build a chain with commit-tree' '
	(
	cd repo &&
	c0=$(echo "c0" | git commit-tree $(get_tree)) &&
	c1=$(echo "c1" | git commit-tree $(get_tree) -p "$c0") &&
	c2=$(echo "c2" | git commit-tree $(get_tree) -p "$c1") &&
	c3=$(echo "c3" | git commit-tree $(get_tree) -p "$c2") &&
	c4=$(echo "c4" | git commit-tree $(get_tree) -p "$c3") &&
	git cat-file -p "$c4" >out &&
	grep "^parent $c3" out &&
	git merge-base --is-ancestor "$c0" "$c4"
	)
'

test_expect_success 'commit-tree with -m flag' '
	(
	cd repo &&
	oid=$(git commit-tree $(get_tree) -m "hello world") &&
	git cat-file -p "$oid" >out &&
	grep "hello world" out
	)
'

test_expect_success 'commit-tree with -F reads message from file' '
	(
	cd repo &&
	echo "message from file" >msgfile &&
	oid=$(git commit-tree $(get_tree) -F msgfile) &&
	git cat-file -p "$oid" >out &&
	grep "message from file" out
	)
'

test_expect_success 'commit-tree with empty -m message' '
	(
	cd repo &&
	oid=$(git commit-tree $(get_tree) -m "") &&
	git cat-file -t "$oid" >type &&
	echo commit >expect &&
	test_cmp expect type
	)
'

test_expect_success 'commit-tree with multi-line message via -F' '
	(
	cd repo &&
	printf "line one\nline two\nline three\n" >multi &&
	oid=$(git commit-tree $(get_tree) -F multi) &&
	git cat-file -p "$oid" >out &&
	grep "line one" out &&
	grep "line two" out &&
	grep "line three" out
	)
'

test_expect_success 'commit-tree respects GIT_AUTHOR_NAME' '
	(
	cd repo &&
	oid=$(echo "author" | GIT_AUTHOR_NAME="Custom Author" git commit-tree $(get_tree)) &&
	git cat-file -p "$oid" >out &&
	grep "author Custom Author" out
	)
'

test_expect_success 'commit-tree respects GIT_COMMITTER_NAME' '
	(
	cd repo &&
	oid=$(echo "committer" | GIT_COMMITTER_NAME="Custom Committer" git commit-tree $(get_tree)) &&
	git cat-file -p "$oid" >out &&
	grep "committer Custom Committer" out
	)
'

test_expect_success 'commit-tree respects GIT_AUTHOR_DATE' '
	(
	cd repo &&
	oid=$(echo "dated" | GIT_AUTHOR_DATE="1234567890 +0000" git commit-tree $(get_tree)) &&
	git cat-file -p "$oid" >out &&
	grep "author.*1234567890 +0000" out
	)
'

test_expect_success 'commit-tree respects GIT_COMMITTER_DATE' '
	(
	cd repo &&
	oid=$(echo "cdated" | GIT_COMMITTER_DATE="1234567890 +0000" git commit-tree $(get_tree)) &&
	git cat-file -p "$oid" >out &&
	grep "committer.*1234567890 +0000" out
	)
'

test_expect_success 'commit-tree: tree field in output matches input tree' '
	(
	cd repo &&
	tree=$(get_tree) &&
	oid=$(echo "tree check" | git commit-tree "$tree") &&
	git cat-file -p "$oid" >out &&
	grep "^tree $tree" out
	)
'

test_expect_success 'commit-tree output is 40-hex OID' '
	(
	cd repo &&
	oid=$(echo "oid" | git commit-tree $(get_tree)) &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'build complex DAG: two merges sharing a base' '
	(
	cd repo &&
	base=$(echo "base" | git commit-tree $(get_tree)) &&
	a=$(echo "a" | git commit-tree $(get_tree) -p "$base") &&
	b=$(echo "b" | git commit-tree $(get_tree) -p "$base") &&
	c=$(echo "c" | git commit-tree $(get_tree) -p "$base") &&
	m1=$(echo "m1" | git commit-tree $(get_tree) -p "$a" -p "$b") &&
	m2=$(echo "m2" | git commit-tree $(get_tree) -p "$b" -p "$c") &&
	git cat-file -p "$m1" >out1 &&
	git cat-file -p "$m2" >out2 &&
	test $(grep -c "^parent" out1) = 2 &&
	test $(grep -c "^parent" out2) = 2 &&
	# merge-base of m1 and m2 should be b
	mb=$(git merge-base "$m1" "$m2") &&
	test "$mb" = "$b"
	)
'

test_expect_success 'duplicate parents are deduplicated' '
	(
	cd repo &&
	p=$(echo "dup-parent" | git commit-tree $(get_tree)) &&
	m=$(echo "dup" | git commit-tree $(get_tree) -p "$p" -p "$p") &&
	git cat-file -p "$m" >out &&
	test $(grep -c "^parent" out) = 1
	)
'

test_expect_success 'commit-tree with same parent used by different children' '
	(
	cd repo &&
	root=$(echo "shared-root" | git commit-tree $(get_tree)) &&
	c1=$(echo "child1" | git commit-tree $(get_tree) -p "$root") &&
	c2=$(echo "child2" | git commit-tree $(get_tree) -p "$root") &&
	c3=$(echo "child3" | git commit-tree $(get_tree) -p "$root") &&
	git cat-file -p "$c1" >o1 && grep "^parent $root" o1 &&
	git cat-file -p "$c2" >o2 && grep "^parent $root" o2 &&
	git cat-file -p "$c3" >o3 && grep "^parent $root" o3
	)
'

test_expect_success 'build deep octopus: 5 parents' '
	(
	cd repo &&
	p1=$(echo "o1" | git commit-tree $(get_tree)) &&
	p2=$(echo "o2" | git commit-tree $(get_tree)) &&
	p3=$(echo "o3" | git commit-tree $(get_tree)) &&
	p4=$(echo "o4" | git commit-tree $(get_tree)) &&
	p5=$(echo "o5" | git commit-tree $(get_tree)) &&
	oct=$(echo "big-oct" | git commit-tree $(get_tree) \
		-p "$p1" -p "$p2" -p "$p3" -p "$p4" -p "$p5") &&
	git cat-file -p "$oct" >out &&
	test $(grep -c "^parent" out) = 5
	)
'

test_expect_success 'cascading merges: merge of merges' '
	(
	cd repo &&
	a=$(echo "ca" | git commit-tree $(get_tree)) &&
	b=$(echo "cb" | git commit-tree $(get_tree)) &&
	c=$(echo "cc" | git commit-tree $(get_tree)) &&
	d=$(echo "cd" | git commit-tree $(get_tree)) &&
	m1=$(echo "cm1" | git commit-tree $(get_tree) -p "$a" -p "$b") &&
	m2=$(echo "cm2" | git commit-tree $(get_tree) -p "$c" -p "$d") &&
	top=$(echo "top" | git commit-tree $(get_tree) -p "$m1" -p "$m2") &&
	git cat-file -p "$top" >out &&
	test $(grep -c "^parent" out) = 2
	)
'

test_expect_success 'commit-tree stdin vs -m produce same message content' '
	(
	cd repo &&
	oid1=$(echo "same msg" | git commit-tree $(get_tree)) &&
	oid2=$(git commit-tree $(get_tree) -m "same msg") &&
	git cat-file -p "$oid1" | tail -1 >msg1 &&
	git cat-file -p "$oid2" | tail -1 >msg2 &&
	test_cmp msg1 msg2
	)
'

test_expect_success 'each commit-tree call produces unique OID' '
	(
	cd repo &&
	test_tick &&
	oid1=$(echo "u1" | git commit-tree $(get_tree)) &&
	test_tick &&
	oid2=$(echo "u2" | git commit-tree $(get_tree)) &&
	test "$oid1" != "$oid2"
	)
'

test_done
