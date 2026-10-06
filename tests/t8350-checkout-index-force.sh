#!/bin/sh
# Tests for checkout-index --force, --no-create, and edge cases.

test_description='checkout-index --force, --no-create, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo alpha >a.txt &&
	echo beta >b.txt &&
	echo gamma >c.txt &&
	mkdir -p sub/deep &&
	echo delta >sub/d.txt &&
	echo epsilon >sub/deep/e.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ── --force basics ──────────────────────────────────────────────────────────

test_expect_success 'checkout-index without --force refuses existing dirty file' '
	(
	cd repo &&
	echo dirty >a.txt &&
	test_must_fail grit checkout-index a.txt 2>err &&
	test "$(cat a.txt)" = "dirty" &&
	grep -i "already exists" err &&
	echo alpha >a.txt
	)
'

test_expect_success 'checkout-index --force overwrites existing file' '
	(
	cd repo &&
	echo dirty >a.txt &&
	grit checkout-index --force a.txt &&
	echo alpha >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index -f is alias for --force' '
	(
	cd repo &&
	echo dirty >b.txt &&
	grit checkout-index -f b.txt &&
	echo beta >expect &&
	test_cmp expect b.txt
	)
'

test_expect_success 'checkout-index --force with multiple files' '
	(
	cd repo &&
	echo dirty >a.txt &&
	echo dirty >b.txt &&
	echo dirty >c.txt &&
	grit checkout-index -f a.txt b.txt c.txt &&
	echo alpha >expect_a &&
	echo beta >expect_b &&
	echo gamma >expect_c &&
	test_cmp expect_a a.txt &&
	test_cmp expect_b b.txt &&
	test_cmp expect_c c.txt
	)
'

test_expect_success 'checkout-index --force with --all' '
	(
	cd repo &&
	echo dirty >a.txt &&
	echo dirty >b.txt &&
	grit checkout-index -f --all &&
	echo alpha >expect &&
	test_cmp expect a.txt &&
	echo beta >expect &&
	test_cmp expect b.txt
	)
'

test_expect_success 'checkout-index --force restores deleted file' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index -f a.txt &&
	echo alpha >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index --force on subdirectory file' '
	(
	cd repo &&
	echo dirty >sub/d.txt &&
	grit checkout-index -f sub/d.txt &&
	echo delta >expect &&
	test_cmp expect sub/d.txt
	)
'

test_expect_success 'checkout-index --force on deep subdirectory file' '
	(
	cd repo &&
	echo dirty >sub/deep/e.txt &&
	grit checkout-index -f sub/deep/e.txt &&
	echo epsilon >expect &&
	test_cmp expect sub/deep/e.txt
	)
'

# ── --no-create ─────────────────────────────────────────────────────────────

test_expect_success 'checkout-index --no-create does not create missing file' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index --no-create a.txt &&
	test_path_is_missing a.txt &&
	grit checkout-index a.txt
	)
'

test_expect_success 'checkout-index -n is alias for --no-create' '
	(
	cd repo &&
	rm -f b.txt &&
	grit checkout-index -n b.txt &&
	test_path_is_missing b.txt &&
	grit checkout-index b.txt
	)
'

test_expect_success 'checkout-index --no-create with existing file does nothing' '
	(
	cd repo &&
	echo alpha >expect &&
	grit checkout-index --no-create a.txt &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index --no-create with existing file keeps it unchanged' '
	(
	cd repo &&
	echo dirty >a.txt &&
	grit checkout-index --no-create a.txt &&
	test "$(cat a.txt)" = "dirty" &&
	echo alpha >a.txt
	)
'

test_expect_success 'checkout-index --no-create with --force and missing file does not create' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index --no-create -f a.txt &&
	test_path_is_missing a.txt &&
	grit checkout-index a.txt
	)
'

test_expect_success 'checkout-index --no-create --all skips missing files' '
	(
	cd repo &&
	rm -f a.txt c.txt &&
	grit checkout-index --no-create --all &&
	test_path_is_missing a.txt &&
	test_path_is_missing c.txt &&
	test -f b.txt &&
	grit checkout-index --all
	)
'

# ── --prefix ────────────────────────────────────────────────────────────────

test_expect_success 'checkout-index --prefix exports to directory' '
	(
	cd repo &&
	mkdir -p export/sub/deep &&
	grit checkout-index --prefix=export/ --all &&
	echo alpha >expect &&
	test_cmp expect export/a.txt &&
	echo beta >expect &&
	test_cmp expect export/b.txt &&
	rm -rf export
	)
'

test_expect_success 'checkout-index --prefix with single file' '
	(
	cd repo &&
	mkdir -p out &&
	grit checkout-index --prefix=out/ a.txt &&
	echo alpha >expect &&
	test_cmp expect out/a.txt &&
	rm -rf out
	)
'

test_expect_success 'checkout-index --prefix with subdirectory file and --mkdir' '
	(
	cd repo &&
	grit checkout-index --prefix=pfx/ --mkdir sub/d.txt &&
	echo delta >expect &&
	test_cmp expect pfx/sub/d.txt &&
	rm -rf pfx
	)
'

# ── --temp ──────────────────────────────────────────────────────────────────

test_expect_success 'checkout-index --temp writes to temp files' '
	(
	cd repo &&
	grit checkout-index --temp a.txt >tmpout &&
	TMPFILE=$(awk "{print \$1}" tmpout) &&
	echo alpha >expect &&
	test_cmp expect "$TMPFILE" &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp --all writes multiple temp files' '
	(
	cd repo &&
	grit checkout-index --temp --all >tmpout &&
	LINES=$(wc -l <tmpout) &&
	test "$LINES" -ge 5 &&
	rm -f $(awk "{print \$1}" tmpout)
	)
'

# ── --stdin ─────────────────────────────────────────────────────────────────

test_expect_success 'checkout-index --stdin reads paths from stdin' '
	(
	cd repo &&
	rm -f a.txt b.txt &&
	printf "a.txt\nb.txt\n" | grit checkout-index --stdin &&
	echo alpha >expect &&
	test_cmp expect a.txt &&
	echo beta >expect &&
	test_cmp expect b.txt
	)
'

test_expect_success 'checkout-index --stdin -z reads NUL-terminated paths' '
	(
	cd repo &&
	rm -f a.txt c.txt &&
	printf "a.txt\0c.txt\0" | grit checkout-index --stdin -z &&
	echo alpha >expect_a &&
	echo gamma >expect_c &&
	test_cmp expect_a a.txt &&
	test_cmp expect_c c.txt
	)
'

test_expect_success 'checkout-index --stdin with --force' '
	(
	cd repo &&
	echo dirty >a.txt &&
	printf "a.txt\n" | grit checkout-index --stdin -f &&
	echo alpha >expect &&
	test_cmp expect a.txt
	)
'

# ── --quiet ─────────────────────────────────────────────────────────────────

test_expect_success 'checkout-index --quiet suppresses output' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index -q a.txt >out 2>&1 &&
	test_must_be_empty out
	)
'

# ── Edge cases ──────────────────────────────────────────────────────────────

test_expect_success 'checkout-index for file not in index fails' '
	(
	cd repo &&
	test_must_fail grit checkout-index nonexistent.txt 2>err &&
	grep -i "not in" err
	)
'

test_expect_success 'checkout-index --all restores all tracked files' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt sub/d.txt sub/deep/e.txt &&
	grit checkout-index --all &&
	test -f a.txt &&
	test -f b.txt &&
	test -f c.txt &&
	test -f sub/d.txt &&
	test -f sub/deep/e.txt
	)
'

test_expect_success 'checkout-index restores correct content after staging' '
	(
	cd repo &&
	echo staged-content >a.txt &&
	grit add a.txt &&
	rm -f a.txt &&
	grit checkout-index a.txt &&
	echo staged-content >expect &&
	test_cmp expect a.txt &&
	grit restore --staged a.txt &&
	grit restore a.txt
	)
'

test_expect_success 'checkout-index with --mkdir creates leading directories' '
	(
	cd repo &&
	rm -rf sub &&
	grit checkout-index --mkdir sub/d.txt &&
	echo delta >expect &&
	test_cmp expect sub/d.txt
	)
'

test_expect_success 'checkout-index --force does not affect untracked files' '
	(
	cd repo &&
	echo untracked >untracked.txt &&
	grit checkout-index -f --all --mkdir &&
	test -f untracked.txt &&
	test "$(cat untracked.txt)" = "untracked" &&
	rm -f untracked.txt
	)
'

test_expect_success 'checkout-index --prefix with --force overwrites in prefix dir' '
	(
	cd repo &&
	rm -rf out &&
	mkdir -p out &&
	echo old >out/a.txt &&
	grit checkout-index --prefix=out/ -f a.txt &&
	echo alpha >expect &&
	test_cmp expect out/a.txt &&
	rm -rf out
	)
'

test_done
