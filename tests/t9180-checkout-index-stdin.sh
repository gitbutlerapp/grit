#!/bin/sh
#
# Tests for 'grit checkout-index --stdin' — reading file paths from stdin
# to check out from the index into the working tree.

test_description='grit checkout-index --stdin'

# When run as `./t9180-checkout-index-stdin.sh`, `$0` has no directory segment, so
# test-lib.sh's default `../../target/...` lookup would miss the workspace binary.
# The harness sets GUST_BIN; discover it here for direct runs from `tests/`.
if test -z "$GUST_BIN"
then
	_here="$(cd "$(dirname "$0")" && pwd)"
	_root="$(cd "$_here/.." && pwd)"
	for _c in "$_root/target/release/grit-git" "$_root/target/debug/grit-git"
	do
		if test -x "$_c"
		then
			GUST_BIN="$_c"
			export GUST_BIN
			break
		fi
	done
fi

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: create repo with multiple files' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "content-a" >a.txt &&
	echo "content-b" >b.txt &&
	echo "content-c" >c.txt &&
	mkdir dir &&
	echo "content-d" >dir/d.txt &&
	echo "content-e" >dir/e.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# ---------------------------------------------------------------------------
# Basic --stdin
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin checks out listed file' '
	(
	cd repo &&
	rm -f a.txt &&
	echo "a.txt" | grit checkout-index --stdin &&
	test -f a.txt &&
	echo "content-a" >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index --stdin with multiple files' '
	(
	cd repo &&
	rm -f a.txt b.txt &&
	printf "a.txt\nb.txt\n" | grit checkout-index --stdin &&
	test -f a.txt &&
	test -f b.txt
	)
'

test_expect_success 'checkout-index --stdin with subdirectory file' '
	(
	cd repo &&
	rm -f dir/d.txt &&
	echo "dir/d.txt" | grit checkout-index --stdin --mkdir &&
	test -f dir/d.txt &&
	echo "content-d" >expect &&
	test_cmp expect dir/d.txt
	)
'

test_expect_success 'checkout-index --stdin all files' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt dir/d.txt dir/e.txt &&
	printf "a.txt\nb.txt\nc.txt\ndir/d.txt\ndir/e.txt\n" |
	grit checkout-index --stdin --mkdir &&
	test -f a.txt &&
	test -f b.txt &&
	test -f c.txt &&
	test -f dir/d.txt &&
	test -f dir/e.txt
	)
'

# ---------------------------------------------------------------------------
# --stdin with -f (force overwrite)
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin -f overwrites existing' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	echo "a.txt" | grit checkout-index --stdin -f &&
	echo "content-a" >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index --stdin without -f skips existing file' '
	(
	cd repo &&
	echo "already here" >a.txt &&
	echo "a.txt" | grit checkout-index --stdin 2>err;
	echo "already here" >expect &&
	test_cmp expect a.txt
	)
'

# ---------------------------------------------------------------------------
# --stdin with -z (NUL-terminated)
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin -z with NUL paths' '
	(
	cd repo &&
	rm -f a.txt b.txt &&
	printf "a.txt\0b.txt\0" | grit checkout-index --stdin -z &&
	test -f a.txt &&
	test -f b.txt
	)
'

test_expect_success 'checkout-index --stdin -z ignores trailing newlines in paths' '
	(
	cd repo &&
	rm -f c.txt &&
	printf "c.txt\0" | grit checkout-index --stdin -z &&
	test -f c.txt
	)
'

# ---------------------------------------------------------------------------
# --stdin with --prefix
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin --prefix writes to prefix dir' '
	(
	cd repo &&
	rm -rf out &&
	mkdir out &&
	echo "a.txt" | grit checkout-index --stdin --prefix=out/ &&
	test -f out/a.txt &&
	echo "content-a" >expect &&
	test_cmp expect out/a.txt
	)
'

test_expect_success 'checkout-index --stdin --prefix with subdirectory' '
	(
	cd repo &&
	rm -rf out &&
	mkdir out &&
	echo "dir/d.txt" | grit checkout-index --stdin --prefix=out/ --mkdir &&
	test -f out/dir/d.txt
	)
'

# ---------------------------------------------------------------------------
# --stdin with --temp
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin --temp creates temp files' '
	(
	cd repo &&
	echo "a.txt" | grit checkout-index --stdin --temp >output &&
	tmpfile=$(awk "{print \$1}" output | head -1) &&
	test -f "$tmpfile" &&
	echo "content-a" >expect &&
	test_cmp expect "$tmpfile"
	)
'

test_expect_success 'checkout-index --stdin --temp lists tab-separated path' '
	(
	cd repo &&
	echo "a.txt" | grit checkout-index --stdin --temp >output &&
	grep "	a.txt" output
	)
'

# ---------------------------------------------------------------------------
# Edge cases
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin with empty input does nothing' '
	(
	cd repo &&
	echo "" | grit checkout-index --stdin
	)
'

test_expect_success 'checkout-index --stdin with non-indexed file fails' '
	(
	cd repo &&
	echo "nonexistent.txt" | test_must_fail grit checkout-index --stdin
	)
'

test_expect_success 'checkout-index --stdin file content matches index' '
	(
	cd repo &&
	rm -f b.txt &&
	echo "b.txt" | grit checkout-index --stdin &&
	echo "content-b" >expect &&
	test_cmp expect b.txt
	)
'

# ---------------------------------------------------------------------------
# Multiple calls
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin can be called repeatedly' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt &&
	echo "a.txt" | grit checkout-index --stdin &&
	echo "b.txt" | grit checkout-index --stdin &&
	echo "c.txt" | grit checkout-index --stdin &&
	test -f a.txt &&
	test -f b.txt &&
	test -f c.txt
	)
'

# ---------------------------------------------------------------------------
# --stdin with --all (--all should take precedence or conflict)
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --all restores all files' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt &&
	rm -rf dir &&
	grit checkout-index -a -f --mkdir &&
	test -f a.txt &&
	test -f b.txt &&
	test -f c.txt &&
	test -f dir/d.txt &&
	test -f dir/e.txt
	)
'

# ---------------------------------------------------------------------------
# --quiet suppresses messages
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin -q is quiet on success' '
	(
	cd repo &&
	rm -f a.txt &&
	echo "a.txt" | grit checkout-index --stdin -q >stdout_out 2>&1 &&
	test_must_be_empty stdout_out
	)
'

# ---------------------------------------------------------------------------
# --stdin -f with multiple files
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin -f with multiple existing files' '
	(
	cd repo &&
	echo "overwritten" >a.txt &&
	echo "overwritten" >b.txt &&
	printf "a.txt\nb.txt\n" | grit checkout-index --stdin -f &&
	echo "content-a" >expect_a &&
	echo "content-b" >expect_b &&
	test_cmp expect_a a.txt &&
	test_cmp expect_b b.txt
	)
'

# ---------------------------------------------------------------------------
# Verify file permissions after checkout
# ---------------------------------------------------------------------------
test_expect_success 'setup: add executable file' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	git add script.sh &&
	git commit -m "add script"
	)
'

test_expect_success 'checkout-index --stdin restores executable bit' '
	(
	cd repo &&
	rm -f script.sh &&
	echo "script.sh" | grit checkout-index --stdin &&
	test -x script.sh
	)
'

# ---------------------------------------------------------------------------
# Large batch via stdin
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin with many files' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "file-$i" >"f$i.txt" || return 1
	done &&
	git add f*.txt &&
	git commit -m "add many files" &&
	for i in $(seq 1 20); do
		rm -f "f$i.txt" || return 1
	done &&
	for i in $(seq 1 20); do
		echo "f$i.txt"
	done | grit checkout-index --stdin &&
	for i in $(seq 1 20); do
		test -f "f$i.txt" || return 1
	done
	)
'

# ---------------------------------------------------------------------------
# --stdin with -z and --prefix combined
# ---------------------------------------------------------------------------
test_expect_success 'checkout-index --stdin single file preserves content exactly' '
	(
	cd repo &&
	rm -f c.txt &&
	echo "c.txt" | grit checkout-index --stdin &&
	echo "content-c" >expect &&
	test_cmp expect c.txt
	)
'

test_expect_success 'checkout-index --stdin -z --prefix combined' '
	(
	cd repo &&
	rm -rf zout &&
	mkdir zout &&
	printf "a.txt\0b.txt\0" | grit checkout-index --stdin -z --prefix=zout/ &&
	test -f zout/a.txt &&
	test -f zout/b.txt
	)
'

test_done
