#!/bin/sh
# Extended tests for write-tree: basic, --missing-ok, cache-tree, subdirs.

test_description='write-tree extra'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	git add file.txt &&
	git commit -m "initial"
	)
'

# ── Basic write-tree ─────────────────────────────────────────────────────

test_expect_success 'write-tree produces valid OID' '
	(
	cd repo &&
	oid=$(git write-tree) &&
	len=$(printf "%s" "$oid" | wc -c | tr -d " ") &&
	test "$len" = "40" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'write-tree output matches HEAD tree' '
	(
	cd repo &&
	expected=$(git rev-parse HEAD^{tree}) &&
	actual=$(git write-tree) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'write-tree creates a tree object' '
	(
	cd repo &&
	oid=$(git write-tree) &&
	type=$(git cat-file -t "$oid") &&
	test "$type" = "tree"
	)
'

# ── write-tree after index changes ──────────────────────────────────────

test_expect_success 'write-tree reflects staged additions' '
	(
	cd repo &&
	echo "new" >new.txt &&
	git add new.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "new.txt" out
	)
'

test_expect_success 'write-tree reflects staged deletions' '
	(
	cd repo &&
	git rm --cached new.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	! grep "new.txt" out
	)
'

test_expect_success 'write-tree with modified file' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	git add file.txt &&
	oid=$(git write-tree) &&
	prev=$(git rev-parse HEAD^{tree}) &&
	test "$oid" != "$prev"
	)
'

test_expect_success 'write-tree after reset matches original' '
	(
	cd repo &&
	echo "initial" >file.txt &&
	git add file.txt &&
	oid=$(git write-tree) &&
	orig=$(git rev-parse HEAD^{tree}) &&
	test "$oid" = "$orig"
	)
'

# ── Subdirectories ───────────────────────────────────────────────────────

test_expect_success 'write-tree with subdirectory' '
	(
	cd repo &&
	mkdir -p sub &&
	echo "sub content" >sub/s.txt &&
	git add sub/s.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "sub" out
	)
'

test_expect_success 'write-tree with nested directories' '
	(
	cd repo &&
	mkdir -p a/b/c &&
	echo "deep" >a/b/c/deep.txt &&
	git add a/b/c/deep.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "^040000 tree" out | grep "a"
	)
'

test_expect_success 'write-tree subtree contains correct entries' '
	(
	cd repo &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	a_tree=$(grep "	a$" out | awk "{print \$3}") &&
	git cat-file -p "$a_tree" >a_out &&
	grep "b" a_out
	)
'

test_expect_success 'write-tree with multiple files in subdir' '
	(
	cd repo &&
	echo "x" >sub/x.txt &&
	echo "y" >sub/y.txt &&
	git add sub/x.txt sub/y.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	sub_tree=$(grep "	sub$" out | awk "{print \$3}") &&
	git cat-file -p "$sub_tree" >sub_out &&
	grep "s.txt" sub_out &&
	grep "x.txt" sub_out &&
	grep "y.txt" sub_out
	)
'

# ── --missing-ok ─────────────────────────────────────────────────────────

test_expect_success 'write-tree --missing-ok succeeds normally' '
	(
	cd repo &&
	oid=$(git write-tree --missing-ok) &&
	test -n "$oid" &&
	echo "$oid" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'write-tree --missing-ok matches write-tree for clean index' '
	(
	cd repo &&
	oid1=$(git write-tree) &&
	oid2=$(git write-tree --missing-ok) &&
	test "$oid1" = "$oid2"
	)
'

# ── Determinism ──────────────────────────────────────────────────────────

test_expect_success 'write-tree is deterministic' '
	(
	cd repo &&
	oid1=$(git write-tree) &&
	oid2=$(git write-tree) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'write-tree same content different repo gives same OID' '
	(
	cd repo &&
	oid1=$(git write-tree) &&
	cd .. &&
	git init repo2 &&
	cd repo2 &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "initial" >file.txt &&
	mkdir -p sub a/b/c &&
	echo "sub content" >sub/s.txt &&
	echo "x" >sub/x.txt &&
	echo "y" >sub/y.txt &&
	echo "deep" >a/b/c/deep.txt &&
	git add . &&
	oid2=$(git write-tree) &&
	test "$oid1" = "$oid2"
	)
'

# ── Empty index ──────────────────────────────────────────────────────────

test_expect_success 'write-tree on empty index gives empty tree' '
	(
	git init empty-repo &&
	cd empty-repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	oid=$(git write-tree) &&
	test "$oid" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

# ── Cache-tree invalidation ──────────────────────────────────────────────

test_expect_success 'write-tree after add updates correctly' '
	(
	cd repo &&
	echo "cache1" >cache1.txt &&
	git add cache1.txt &&
	oid1=$(git write-tree) &&
	echo "cache2" >cache2.txt &&
	git add cache2.txt &&
	oid2=$(git write-tree) &&
	test "$oid1" != "$oid2" &&
	git cat-file -p "$oid2" >out &&
	grep "cache1.txt" out &&
	grep "cache2.txt" out
	)
'

test_expect_success 'write-tree after rm updates correctly' '
	(
	cd repo &&
	git rm --cached cache2.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "cache1.txt" out &&
	! grep "cache2.txt" out
	)
'

test_expect_success 'write-tree after update-index changes' '
	(
	cd repo &&
	echo "v2" >cache1.txt &&
	git add cache1.txt &&
	oid_new=$(git write-tree) &&
	echo "initial" >cache1.txt &&
	git add cache1.txt &&
	oid_orig=$(git write-tree) &&
	test "$oid_new" != "$oid_orig"
	)
'

# ── write-tree + commit-tree roundtrip ──────────────────────────────────

test_expect_success 'write-tree then commit-tree creates valid commit' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	oid=$(echo "roundtrip" | git commit-tree "$tree") &&
	git cat-file -t "$oid" >type &&
	test "$(cat type)" = "commit" &&
	git cat-file -p "$oid" >out &&
	grep "^tree $tree" out
	)
'

test_expect_success 'write-tree + commit-tree + update-ref roundtrip' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	parent=$(git rev-parse HEAD) &&
	oid=$(echo "full roundtrip" | git commit-tree "$tree" -p "$parent") &&
	git update-ref refs/heads/roundtrip "$oid" &&
	resolved=$(git rev-parse refs/heads/roundtrip) &&
	test "$resolved" = "$oid"
	)
'

# ── Executable bit ───────────────────────────────────────────────────────

test_expect_success 'write-tree records executable permission' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	git add script.sh &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "100755.*script.sh" out
	)
'

test_expect_success 'write-tree records regular permission' '
	(
	cd repo &&
	git cat-file -p "$(git write-tree)" >out &&
	grep "100644.*file.txt" out
	)
'

# ── Symlinks ─────────────────────────────────────────────────────────────

test_expect_success 'write-tree records symlink' '
	(
	cd repo &&
	ln -s file.txt link.txt &&
	git add link.txt &&
	oid=$(git write-tree) &&
	git cat-file -p "$oid" >out &&
	grep "120000.*link.txt" out
	)
'

test_done
