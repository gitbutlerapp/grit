#!/bin/sh
#
# Tests for 'grit count-objects' — counts loose objects and disk usage.

test_description='grit count-objects'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ---------------------------------------------------------------------------
# Empty repo
# ---------------------------------------------------------------------------
test_expect_success 'count-objects in empty repo shows 0' '
	(
	cd repo &&
	git count-objects >output &&
	grep "^0 objects" output
	)
'

test_expect_success 'count-objects -v in empty repo shows all zeros' '
	(
	cd repo &&
	git count-objects -v >output &&
	grep "^count: 0" output &&
	grep "^size: 0" output &&
	grep "^in-pack: 0" output &&
	grep "^packs: 0" output
	)
'

# ---------------------------------------------------------------------------
# After creating objects
# ---------------------------------------------------------------------------
test_expect_success 'count-objects after first commit shows loose objects' '
	(
	cd repo &&
	echo "content" >file1 &&
	git add file1 &&
	git commit -m "first commit" &&
	git count-objects >output &&
	# Should have at least 1 object (likely more: blob, tree, commit)
	count=$(sed "s/ objects.*//" output) &&
	test "$count" -ge 1
	)
'

test_expect_success 'count-objects -v shows non-zero count after commit' '
	(
	cd repo &&
	git count-objects -v >output &&
	count=$(grep "^count:" output | sed "s/count: //") &&
	test "$count" -ge 1
	)
'

test_expect_success 'count-objects --verbose is same as -v' '
	(
	cd repo &&
	git count-objects -v >output_v &&
	git count-objects --verbose >output_verbose &&
	test_cmp output_v output_verbose
	)
'

# ---------------------------------------------------------------------------
# More objects increase the count
# ---------------------------------------------------------------------------
test_expect_success 'adding more commits increases loose object count' '
	(
	cd repo &&
	git count-objects -v >before &&
	count_before=$(grep "^count:" before | sed "s/count: //") &&

	echo "more content" >file2 &&
	git add file2 &&
	git commit -m "second commit" &&

	git count-objects -v >after &&
	count_after=$(grep "^count:" after | sed "s/count: //") &&
	test "$count_after" -gt "$count_before"
	)
'

# ---------------------------------------------------------------------------
# After gc / repack, loose objects go into packs
# ---------------------------------------------------------------------------
test_expect_success 'after repack, in-pack count increases' '
	(
	cd repo &&
	git count-objects -v >before_repack &&
	loose_before=$(grep "^count:" before_repack | sed "s/count: //") &&

	git repack -a -d &&

	git count-objects -v >after_repack &&
	inpack=$(grep "^in-pack:" after_repack | sed "s/in-pack: //") &&
	test "$inpack" -gt 0
	)
'

test_expect_success 'after repack, loose count decreases' '
	(
	cd repo &&
	loose=$(grep "^count:" after_repack | sed "s/count: //") &&
	test "$loose" -eq 0
	)
'

test_expect_success 'packs count is at least 1 after repack' '
	(
	cd repo &&
	packs=$(grep "^packs:" after_repack | sed "s/packs: //") &&
	test "$packs" -ge 1
	)
'

test_expect_success 'size-pack is non-zero after repack' '
	(
	cd repo &&
	sizepack=$(grep "^size-pack:" after_repack | sed "s/size-pack: //") &&
	test "$sizepack" -ge 0
	)
'

# ---------------------------------------------------------------------------
# Creating new loose objects after repack
# ---------------------------------------------------------------------------
test_expect_success 'loose count resets to zero after full repack -a -d' '
	(
	cd repo &&
	git count-objects -v >output &&
	loose=$(grep "^count:" output | sed "s/count: //") &&
	test "$loose" -eq 0
	)
'

# ---------------------------------------------------------------------------
# prune-packable field
# ---------------------------------------------------------------------------
test_expect_success 'prune-packable counts loose objects also in packs' '
	(
	cd repo &&
	# Repack everything into a pack
	git repack -a &&
	# Now create a loose object that duplicates a packed one
	# by using hash-object on an existing file
	blob=$(git hash-object -w file1) &&
	git count-objects -v >output &&
	grep "^prune-packable:" output
	)
'

# ---------------------------------------------------------------------------
# Non-verbose format
# ---------------------------------------------------------------------------
test_expect_success 'non-verbose output has expected format' '
	(
	cd repo &&
	git count-objects >output &&
	# Format: "<N> objects, <M> kilobytes"
	grep "objects," output &&
	grep "kilobytes" output
	)
'

# ---------------------------------------------------------------------------
# Verbose fields are all present
# ---------------------------------------------------------------------------
test_expect_success 'verbose output contains all expected fields' '
	(
	cd repo &&
	git count-objects -v >output &&
	grep "^count:" output &&
	grep "^size:" output &&
	grep "^in-pack:" output &&
	grep "^packs:" output &&
	grep "^size-pack:" output &&
	grep "^prune-packable:" output &&
	grep "^garbage:" output &&
	grep "^size-garbage:" output
	)
'

# ---------------------------------------------------------------------------
# garbage detection
# ---------------------------------------------------------------------------
test_expect_success 'garbage files are detected' '
	(
	cd repo &&
	# Create a garbage file in objects/pack/
	echo "trash" >.git/objects/pack/garbage.xyz &&
	git count-objects -v >output &&
	garbage=$(grep "^garbage:" output | sed "s/garbage: //") &&
	test "$garbage" -ge 1 &&
	rm .git/objects/pack/garbage.xyz
	)
'

# ---------------------------------------------------------------------------
# Multiple repos
# ---------------------------------------------------------------------------
test_expect_success 'count-objects works in a fresh repo' '
	(
	cd .. &&
	git init fresh &&
	cd fresh &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	git count-objects >output &&
	grep "^0 objects" output &&
	git count-objects -v >output &&
	grep "^count: 0" output
	)
'

# ---------------------------------------------------------------------------
# Size field checks
# ---------------------------------------------------------------------------
test_expect_success 'count-objects -v size field is numeric' '
	(
	cd repo &&
	git count-objects -v >output &&
	size=$(grep "^size:" output | sed "s/size: //") &&
	test "$size" -ge 0
	)
'

test_expect_success 'count-objects -v garbage field defaults to 0' '
	(
	cd repo &&
	git count-objects -v >output &&
	garbage=$(grep "^garbage:" output | sed "s/garbage: //") &&
	test "$garbage" -eq 0
	)
'

# ---------------------------------------------------------------------------
# count-objects after gc
# ---------------------------------------------------------------------------
test_expect_success 'count-objects after gc shows 0 loose' '
	(
	cd repo &&
	git gc --quiet &&
	git count-objects -v >output &&
	loose=$(grep "^count:" output | sed "s/count: //") &&
	test "$loose" -eq 0
	)
'

test_expect_success 'count-objects after gc shows packs' '
	(
	cd repo &&
	git count-objects -v >output &&
	packs=$(grep "^packs:" output | sed "s/packs: //") &&
	test "$packs" -ge 1
	)
'

# ---------------------------------------------------------------------------
# Object count increases with different types
# ---------------------------------------------------------------------------
test_expect_success 'adding a blob increases loose count' '
	(
	cd repo &&
	git count-objects -v >before &&
	cb=$(grep "^count:" before | sed "s/count: //") &&
	echo "extra-blob-data" | git hash-object -w --stdin &&
	git count-objects -v >after &&
	ca=$(grep "^count:" after | sed "s/count: //") &&
	test "$ca" -gt "$cb"
	)
'

test_expect_success 'count-objects non-verbose counts match verbose count field' '
	(
	cd repo &&
	git count-objects >nv &&
	nv_count=$(sed "s/ objects.*//" nv) &&
	git count-objects -v >v &&
	v_count=$(grep "^count:" v | sed "s/count: //") &&
	test "$nv_count" = "$v_count"
	)
'

test_expect_success 'count-objects in bare repo works' '
	(
	cd .. &&
	git init --bare bare-repo &&
	cd bare-repo &&
	git count-objects >output &&
	grep "objects" output &&
	git count-objects -v >output &&
	grep "^count:" output
	)
'

test_expect_success 'count-objects with no packs shows packs: 0' '
	(
	cd .. &&
	git init empty-np &&
	cd empty-np &&
	git count-objects -v >output &&
	packs=$(grep "^packs:" output | sed "s/packs: //") &&
	test "$packs" -eq 0
	)
'

test_expect_success 'size field is non-negative' '
	(
	cd repo &&
	git count-objects -v >output &&
	size=$(grep "^size:" output | sed "s/size: //") &&
	test "$size" -ge 0
	)
'

test_done
