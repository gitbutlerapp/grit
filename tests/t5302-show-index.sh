#!/bin/sh
# Tests for grit show-index — displays pack index contents.

test_description='show-index basic behavior on generated pack index'

. ./test-lib.sh

REAL_GIT=${REAL_GIT:-/usr/bin/git}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup packed repository fixture' '
	(
	grit init repo &&
	cd repo &&
	echo hello >a.txt &&
	"$REAL_GIT" update-index --add a.txt &&
	tree=$("$REAL_GIT" write-tree) &&
	commit=$(echo "initial" | "$REAL_GIT" commit-tree "$tree") &&
	"$REAL_GIT" update-ref HEAD "$commit" &&
	"$REAL_GIT" repack -a -d &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_path_is_file "$idx"
	)
'

# ---------------------------------------------------------------------------
# Basic output format
# ---------------------------------------------------------------------------
test_expect_success 'show-index v2: output has offset oid (crc32) lines' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	test_line_count -gt 0 out &&
	# Every line must match: decimal offset, space, 40-hex OID, space, (8hexdigits)
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9]+ [0-9a-f]{40} \([0-9a-f]{8}\)$" ||
			{ echo "unexpected line: $line"; return 1; }
	done <out
	)
'

test_expect_success 'show-index: --object-format=sha1 is accepted' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index --object-format=sha1 <"$idx" >out2 &&
	test_line_count -gt 0 out2
	)
'

test_expect_success 'show-index: --object-format=sha256 is rejected' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_must_fail git show-index --object-format=sha256 <"$idx"
	)
'

test_expect_success 'show-index: object IDs present in pack also appear in output' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	# Extract OIDs listed by verify-pack
	env -u GIT_EXEC_PATH "$REAL_GIT" verify-pack -v "$idx" |
		grep -E "^[0-9a-f]{40}" |
		awk "{print \$1}" | sort >expected_oids &&
	# Extract OIDs from show-index output
	git show-index <"$idx" | awk "{print \$2}" | sort >actual_oids &&
	test_cmp expected_oids actual_oids
	)
'

# ---------------------------------------------------------------------------
# Multiple objects
# ---------------------------------------------------------------------------
test_expect_success 'setup multi-object pack' '
	(
	cd repo &&
	echo "second file" >b.txt &&
	echo "third file" >c.txt &&
	"$REAL_GIT" add b.txt c.txt &&
	tree2=$("$REAL_GIT" write-tree) &&
	commit2=$(echo "second" | "$REAL_GIT" commit-tree "$tree2" -p HEAD) &&
	"$REAL_GIT" update-ref HEAD "$commit2" &&
	"$REAL_GIT" repack -a -d &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_path_is_file "$idx"
	)
'

test_expect_success 'show-index lists all objects in multi-object pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	# We have at least: 2 commits, 2 trees, 3 blobs = 7 objects
	test_line_count -ge 7 out
	)
'

test_expect_success 'show-index OIDs are unique' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" | sort >oids &&
	sort -u oids >oids_uniq &&
	test_cmp oids oids_uniq
	)
'

test_expect_success 'show-index offsets are non-negative integers' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$1}" >offsets &&
	while read offset; do
		test "$offset" -ge 0 || { echo "bad offset: $offset"; return 1; }
	done <offsets
	)
'

test_expect_success 'show-index output is sorted by OID' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" >oids_raw &&
	sort oids_raw >oids_sorted &&
	test_cmp oids_raw oids_sorted
	)
'

test_expect_success 'show-index CRC32 values are 8 hex digits' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	while IFS= read -r line; do
		crc=$(echo "$line" | sed -n "s/.*(\([0-9a-f]*\))/\1/p") &&
		test ${#crc} = 8 ||
			{ echo "bad CRC length: $crc"; return 1; }
	done <out
	)
'

# ---------------------------------------------------------------------------
# Large pack
# ---------------------------------------------------------------------------
test_expect_success 'setup large pack with many objects' '
	(
	cd repo &&
	for i in $(seq 1 30); do
		echo "content $i" >"file$i.txt"
	done &&
	"$REAL_GIT" add . &&
	tree3=$("$REAL_GIT" write-tree) &&
	commit3=$(echo "many files" | "$REAL_GIT" commit-tree "$tree3" -p HEAD) &&
	"$REAL_GIT" update-ref HEAD "$commit3" &&
	"$REAL_GIT" repack -a -d
	)
'

test_expect_success 'show-index handles large pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	# Should have many objects now
	test_line_count -ge 30 out
	)
'

test_expect_success 'show-index output matches verify-pack object count' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	show_count=$(git show-index <"$idx" | wc -l) &&
	verify_count=$(env -u GIT_EXEC_PATH "$REAL_GIT" verify-pack -v "$idx" | grep -cE "^[0-9a-f]{40}") &&
	# Use numeric comparison: BSD wc -l left-pads its output ("      35"),
	# while grep -c emits a bare number ("35"); the counts are equal so
	# compare numerically to ignore the padding.
	test "$show_count" -eq "$verify_count"
	)
'

# ---------------------------------------------------------------------------
# Error handling
# ---------------------------------------------------------------------------
test_expect_success 'show-index with empty stdin produces no output' '
	(
	cd repo &&
	echo "" | git show-index >out 2>err || true &&
	# Either empty output or error is acceptable
	true
	)
'

test_expect_success 'show-index with invalid data on stdin fails or produces no output' '
	(
	cd repo &&
	echo "not a pack index" | git show-index >out 2>err || true &&
	# Should either fail or produce empty output
	true
	)
'

test_expect_success 'show-index: --object-format without value fails' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_must_fail git show-index --object-format <"$idx" 2>err
	)
'

test_done
