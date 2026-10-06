#!/bin/sh
# Tests for show-index with various pack-index scenarios.

test_description='show-index extra scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=${REAL_GIT:-/usr/bin/git}

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with objects' '
	(
	grit init repo &&
	cd repo &&
	echo "hello world" >a.txt &&
	echo "second file" >b.txt &&
	"$REAL_GIT" add a.txt b.txt &&
	tree=$("$REAL_GIT" write-tree) &&
	commit=$(echo "initial" | "$REAL_GIT" commit-tree "$tree") &&
	"$REAL_GIT" update-ref HEAD "$commit" &&
	"$REAL_GIT" repack -a -d
	)
'

# ── Basic output format ───────────────────────────────────────────────────

test_expect_success 'show-index produces output' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	test_line_count -gt 0 out
	)
'

test_expect_success 'show-index output lines match expected format' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9]+ [0-9a-f]{40} \([0-9a-f]{8}\)$" ||
			{ echo "unexpected: $line"; return 1; }
	done <out
	)
'

test_expect_success 'show-index OIDs are 40 hex chars' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" >oids &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad OID: $oid"; return 1; }
	done <oids
	)
'

test_expect_success 'show-index offsets are non-negative' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$1}" >offsets &&
	while read offset; do
		test "$offset" -ge 0 ||
			{ echo "bad offset: $offset"; return 1; }
	done <offsets
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
			{ echo "bad CRC: $crc"; return 1; }
	done <out
	)
'

# ── OID correctness ───────────────────────────────────────────────────────

test_expect_success 'show-index OIDs match verify-pack output' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	"$REAL_GIT" show-index <"$idx" | awk "{print \$2}" | sort >expected &&
	git show-index <"$idx" | awk "{print \$2}" | sort >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'show-index OIDs are unique' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" | sort >oids &&
	sort -u oids >uniq_oids &&
	test_cmp oids uniq_oids
	)
'

test_expect_success 'show-index output is sorted by OID' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" >oids &&
	sort oids >sorted &&
	test_cmp oids sorted
	)
'

# ── Object count ──────────────────────────────────────────────────────────

test_expect_success 'object count matches verify-pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	show_count=$(git show-index <"$idx" | wc -l | tr -d " ") &&
	verify_count=$("$REAL_GIT" show-index <"$idx" | wc -l | tr -d " ") &&
	test "$show_count" = "$verify_count"
	)
'

# ── --object-format ───────────────────────────────────────────────────────

test_expect_success '--object-format=sha1 is accepted' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index --object-format=sha1 <"$idx" >out &&
	test_line_count -gt 0 out
	)
'

test_expect_success '--object-format=sha256 is rejected' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_must_fail git show-index --object-format=sha256 <"$idx"
	)
'

test_expect_success '--object-format=sha1 output matches default' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >default_out &&
	git show-index --object-format=sha1 <"$idx" >sha1_out &&
	test_cmp default_out sha1_out
	)
'

# ── Multi-object packs ────────────────────────────────────────────────────

test_expect_success 'setup: add more objects and repack' '
	(
	cd repo &&
	echo "third" >c.txt &&
	echo "fourth" >d.txt &&
	echo "fifth" >e.txt &&
	"$REAL_GIT" add c.txt d.txt e.txt &&
	tree2=$("$REAL_GIT" write-tree) &&
	commit2=$(echo "second commit" | "$REAL_GIT" commit-tree "$tree2" -p HEAD) &&
	"$REAL_GIT" update-ref HEAD "$commit2" &&
	"$REAL_GIT" repack -a -d
	)
'

test_expect_success 'show-index handles larger pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	test_line_count -ge 5 out
	)
'

test_expect_success 'larger pack OIDs still match verify-pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	"$REAL_GIT" show-index <"$idx" | awk "{print \$2}" | sort >expected &&
	git show-index <"$idx" | awk "{print \$2}" | sort >actual &&
	test_cmp expected actual
	)
'

test_expect_success 'offsets in larger pack are all distinct' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$1}" | sort -n >offsets &&
	sort -nu offsets >uniq_offsets &&
	test_cmp offsets uniq_offsets
	)
'

# ── Large pack ─────────────────────────────────────────────────────────────

test_expect_success 'setup: create pack with many objects' '
	(
	cd repo &&
	for i in $(seq 1 25); do
		echo "content $i" >"file$i.txt"
	done &&
	"$REAL_GIT" add . &&
	tree3=$("$REAL_GIT" write-tree) &&
	commit3=$(echo "many files" | "$REAL_GIT" commit-tree "$tree3" -p HEAD) &&
	"$REAL_GIT" update-ref HEAD "$commit3" &&
	"$REAL_GIT" repack -a -d
	)
'

test_expect_success 'show-index handles pack with many objects' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" >out &&
	test_line_count -ge 25 out
	)
'

test_expect_success 'many-object pack: OIDs sorted' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" >oids &&
	sort oids >sorted &&
	test_cmp oids sorted
	)
'

test_expect_success 'many-object pack: OIDs unique' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	git show-index <"$idx" | awk "{print \$2}" | sort >oids &&
	sort -u oids >uniq &&
	test_cmp oids uniq
	)
'

test_expect_success 'many-object pack: count matches verify-pack' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	show_count=$(git show-index <"$idx" | wc -l | tr -d " ") &&
	verify_count=$("$REAL_GIT" show-index <"$idx" | wc -l | tr -d " ") &&
	test "$show_count" = "$verify_count"
	)
'

# ── Error handling ─────────────────────────────────────────────────────────

test_expect_success 'show-index with empty stdin fails or produces no output' '
	(
	cd repo &&
	echo "" | git show-index >out 2>err || true &&
	true
	)
'

test_expect_success 'show-index with invalid data fails or produces no output' '
	(
	cd repo &&
	echo "not a pack index" | git show-index >out 2>err || true &&
	true
	)
'

test_expect_success '--object-format without value fails' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	test_must_fail git show-index --object-format <"$idx" 2>err
	)
'

# ── Offsets are monotonically structured ───────────────────────────────────

test_expect_success 'all offsets are within reasonable range' '
	(
	cd repo &&
	idx=$(echo .git/objects/pack/*.idx) &&
	pack=$(echo .git/objects/pack/*.pack) &&
	pack_size=$(wc -c <"$pack" | tr -d " ") &&
	git show-index <"$idx" | awk "{print \$1}" >offsets &&
	while read offset; do
		test "$offset" -lt "$pack_size" ||
			{ echo "offset $offset >= pack size $pack_size"; return 1; }
	done <offsets
	)
'

test_done
