#!/bin/sh
#
# Test pack-objects --revs with ^ref exclusion and --stdin-packs

test_description='pack-objects --revs exclusion and --stdin-packs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: create repo with branches' '
	rm -rf .git &&
	git init -q -b master &&
	git config user.name "Test" &&
	git config user.email "t@t" &&
	echo "base" >base.txt &&
	git add base.txt &&
	test_tick &&
	git commit -m "base" &&
	git checkout -b feature &&
	echo "feature" >feature.txt &&
	git add feature.txt &&
	test_tick &&
	git commit -m "feature" &&
	git checkout master
'

test_expect_success 'pack-objects --revs packs reachable objects' '
	echo HEAD | git pack-objects --revs testpack &&
	git verify-pack testpack-*.pack
'

test_expect_success 'pack-objects --revs with ^ref excludes objects' '
	rm -f exclpack-* &&
	printf "feature\n^master\n" | git pack-objects --revs exclpack &&
	# The pack should only contain objects unique to feature branch
	git verify-pack -v exclpack-*.pack >objects &&
	# Should not contain base.txt blob (reachable from master)
	base_blob=$(git rev-parse master:base.txt) &&
	! grep "$base_blob" objects
'

test_expect_success 'pack-objects --revs ^ref pack is smaller' '
	rm -f fullpack-* exclpack2-* &&
	echo feature | git pack-objects --revs fullpack &&
	printf "feature\n^master\n" | git pack-objects --revs exclpack2 &&
	full_size=$(wc -c <fullpack-*.pack) &&
	excl_size=$(wc -c <exclpack2-*.pack) &&
	test "$excl_size" -lt "$full_size"
'

test_expect_success 'pack-objects --delta-base-offset accepted' '
	rm -f dbopack-* &&
	echo HEAD | git pack-objects --revs --delta-base-offset dbopack &&
	git verify-pack dbopack-*.pack
'

test_expect_success 'setup: repack for stdin-packs test' '
	git repack -a -d
'

test_expect_success 'pack-objects --stdin-packs reads pack names' '
	rm -f sppack-* &&
	pack_name=$(ls .git/objects/pack/pack-*.pack | head -1 | sed "s/.*\///;s/\.pack$//") &&
	echo "$pack_name" | git pack-objects --stdin-packs sppack &&
	git verify-pack sppack-*.pack
'

test_expect_success 'index-pack --verify succeeds on valid pack' '
	pack_file=$(ls .git/objects/pack/pack-*.pack | head -1) &&
	git index-pack --verify "$pack_file"
'

test_expect_success 'index-pack --verify reports pack path' '
	pack_file=$(ls .git/objects/pack/pack-*.pack | head -1) &&
	git index-pack --verify "$pack_file" 2>err &&
	grep "ok" err
'

test_done
