#!/bin/sh
# Tests for repack, prune-packed, gc, and count-objects verification.

test_description='repack, prune-packed, gc, count-objects'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# Helper: count loose objects
count_loose () {
	git count-objects | sed 's/ .*//'
}

# Helper: count in-pack objects from verbose output
count_packed () {
	git count-objects -v | grep "^in-pack:" | sed 's/in-pack: //'
}

# Helper: count packs
count_packs () {
	git count-objects -v | grep "^packs:" | sed 's/packs: //'
}

# Helper: count prune-packable
count_prunable () {
	git count-objects -v | grep "^prune-packable:" | sed 's/prune-packable: //'
}

# -- count-objects on fresh repo -----------------------------------------------

test_expect_success 'count-objects on empty repo shows 0' '
	(
	git init count-empty &&
	cd count-empty &&
	result=$(count_loose) &&
	test "$result" -eq 0
	)
'

test_expect_success 'count-objects -v shows all fields on empty repo' '
	(
	cd count-empty &&
	git count-objects -v >out.txt &&
	grep "^count:" out.txt &&
	grep "^size:" out.txt &&
	grep "^in-pack:" out.txt &&
	grep "^packs:" out.txt &&
	grep "^size-pack:" out.txt &&
	grep "^prune-packable:" out.txt &&
	grep "^garbage:" out.txt &&
	grep "^size-garbage:" out.txt
	)
'

test_expect_success 'count-objects after one commit shows loose objects' '
	(
	cd count-empty &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "content1" >file1.txt &&
	git add file1.txt &&
	git commit -m "first" &&
	result=$(count_loose) &&
	test "$result" -gt 0
	)
'

test_expect_success 'count-objects verbose shows 0 in-pack before repack' '
	(
	cd count-empty &&
	result=$(count_packed) &&
	test "$result" -eq 0
	)
'

test_expect_success 'count-objects verbose shows 0 packs before repack' '
	(
	cd count-empty &&
	result=$(count_packs) &&
	test "$result" -eq 0
	)
'

test_expect_success 'count-objects without -v shows summary line' '
	(
	cd count-empty &&
	git count-objects >out.txt &&
	grep "objects" out.txt
	)
'

# -- repack basic (separate repo to keep clean) --------------------------------

test_expect_success 'setup repack repo with multiple commits' '
	(
	git init repack-repo &&
	cd repack-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "a" >a.txt && git add a.txt && git commit -m "c1" &&
	echo "b" >b.txt && git add b.txt && git commit -m "c2" &&
	echo "c" >c.txt && git add c.txt && git commit -m "c3"
	)
'

test_expect_success 'repack creates a pack file' '
	(
	cd repack-repo &&
	git repack &&
	result=$(count_packs) &&
	test "$result" -ge 1
	)
'

test_expect_success 'repack puts objects in pack' '
	(
	cd repack-repo &&
	packed=$(count_packed) &&
	test "$packed" -gt 0
	)
'

test_expect_success 'repack leaves loose objects by default' '
	(
	cd repack-repo &&
	result=$(count_loose) &&
	test "$result" -gt 0
	)
'

test_expect_success 'prune-packable count is positive after repack' '
	(
	cd repack-repo &&
	result=$(count_prunable) &&
	test "$result" -gt 0
	)
'

test_expect_success 'pack files exist in objects/pack' '
	(
	cd repack-repo &&
	ls .git/objects/pack/*.pack >/dev/null 2>&1
	)
'

test_expect_success 'pack index files exist alongside packs' '
	(
	cd repack-repo &&
	ls .git/objects/pack/*.idx >/dev/null 2>&1
	)
'

test_expect_success 'repository works after repack (before prune)' '
	(
	cd repack-repo &&
	git log --oneline >out.txt &&
	grep "c1" out.txt &&
	grep "c3" out.txt
	)
'

test_expect_success 'count-objects -v size-pack is non-zero after packing' '
	(
	cd repack-repo &&
	size=$(git count-objects -v | grep "^size-pack:" | sed "s/size-pack: //") &&
	test "$size" -gt 0
	)
'

test_expect_success 'second repack is idempotent' '
	(
	cd repack-repo &&
	packed_before=$(count_packed) &&
	git repack &&
	packed_after=$(count_packed) &&
	test "$packed_before" -eq "$packed_after"
	)
'

test_expect_success 'new objects after repack are loose' '
	(
	cd repack-repo &&
	echo "new" >new.txt &&
	git add new.txt &&
	git commit -m "new after repack" &&
	loose=$(count_loose) &&
	test "$loose" -gt 0
	)
'

test_expect_success 'repack collects new loose objects into pack' '
	(
	cd repack-repo &&
	git repack &&
	packed=$(count_packed) &&
	test "$packed" -gt 0
	)
'

# -- prune-packed in isolated repo ---------------------------------------------

test_expect_success 'prune-packed removes loose objects that are packed' '
	(
	git init prune-repo &&
	cd prune-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "x" >x.txt && git add x.txt && git commit -m "px" &&
	git repack &&
	before=$(count_loose) &&
	test "$before" -gt 0 &&
	git prune-packed &&
	after=$(count_loose) &&
	test "$after" -eq 0
	)
'

test_expect_success 'prune-packed sets prune-packable to 0' '
	(
	cd prune-repo &&
	result=$(count_prunable) &&
	test "$result" -eq 0
	)
'

test_expect_success 'prune-packed -n dry-run does not remove objects' '
	(
	git init prune-dry &&
	cd prune-dry &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "y" >y.txt && git add y.txt && git commit -m "py" &&
	git repack &&
	before=$(count_loose) &&
	git prune-packed -n &&
	after=$(count_loose) &&
	test "$before" -eq "$after"
	)
'

# -- gc in isolated repo -------------------------------------------------------

test_expect_success 'gc runs without error' '
	(
	git init gc-repo &&
	cd gc-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "g1" >g1.txt && git add g1.txt && git commit -m "gc1" &&
	echo "g2" >g2.txt && git add g2.txt && git commit -m "gc2" &&
	git gc
	)
'

test_expect_success 'gc packs objects' '
	(
	cd gc-repo &&
	packed=$(count_packed) &&
	test "$packed" -gt 0
	)
'

test_expect_success 'gc creates pack files' '
	(
	cd gc-repo &&
	packs=$(count_packs) &&
	test "$packs" -ge 1
	)
'

# -- count-objects edge cases --------------------------------------------------

test_expect_success 'count-objects on bare repo works' '
	(
	git init --bare bare-count.git &&
	cd bare-count.git &&
	result=$(count_loose) &&
	test "$result" -eq 0
	)
'

test_expect_success 'count-objects -v on bare repo shows all fields' '
	(
	cd bare-count.git &&
	git count-objects -v >out.txt &&
	grep "^count:" out.txt &&
	grep "^in-pack:" out.txt
	)
'

# -- multiple packs ------------------------------------------------------------

test_expect_success 'multiple repacks with new objects create correct counts' '
	(
	git init multi-pack &&
	cd multi-pack &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "m1" >m1.txt && git add m1.txt && git commit -m "mp1" &&
	git repack &&
	first_packed=$(count_packed) &&
	echo "m2" >m2.txt && git add m2.txt && git commit -m "mp2" &&
	git repack &&
	second_packed=$(count_packed) &&
	test "$second_packed" -ge "$first_packed"
	)
'

# -- garbage count -------------------------------------------------------------

test_expect_success 'count-objects -v garbage is 0 in clean repo' '
	(
	git init clean-repo &&
	cd clean-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "clean" >c.txt && git add c.txt && git commit -m "clean" &&
	garbage=$(git count-objects -v | grep "^garbage:" | sed "s/garbage: //") &&
	test "$garbage" -eq 0
	)
'

test_expect_success 'count-objects -v size-garbage is 0 in clean repo' '
	(
	cd clean-repo &&
	sg=$(git count-objects -v | grep "^size-garbage:" | sed "s/size-garbage: //") &&
	test "$sg" -eq 0
	)
'

test_done
