#!/bin/sh
# Test checkout-index: --all, --force, --prefix, --temp, --mkdir, --quiet,
# --no-create, --stdin, and various edge cases.

test_description='grit checkout-index basic operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with multiple files' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "alpha" >a.txt &&
	echo "beta" >b.txt &&
	echo "gamma" >c.txt &&
	mkdir -p sub &&
	echo "delta" >sub/d.txt &&
	grit add a.txt b.txt c.txt sub/d.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic checkout-index
###########################################################################

test_expect_success 'checkout-index single file' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index a.txt &&
	test -f a.txt &&
	echo "alpha" >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index multiple files' '
	(
	cd repo &&
	rm -f a.txt b.txt &&
	grit checkout-index a.txt b.txt &&
	echo "alpha" >expect_a &&
	echo "beta" >expect_b &&
	test_cmp expect_a a.txt &&
	test_cmp expect_b b.txt
	)
'

test_expect_success 'checkout-index file in subdirectory' '
	(
	cd repo &&
	rm -f sub/d.txt &&
	grit checkout-index sub/d.txt &&
	echo "delta" >expect &&
	test_cmp expect sub/d.txt
	)
'

test_expect_success 'checkout-index fails for file not in index' '
	(
	cd repo &&
	test_must_fail grit checkout-index nonexistent.txt 2>err &&
	grep -i "not in" err
	)
'

###########################################################################
# Section 3: --all
###########################################################################

test_expect_success 'checkout-index --all restores all files' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt sub/d.txt &&
	grit checkout-index --all &&
	test -f a.txt &&
	test -f b.txt &&
	test -f c.txt &&
	test -f sub/d.txt
	)
'

test_expect_success 'checkout-index --all content is correct' '
	(
	cd repo &&
	rm -f a.txt b.txt c.txt sub/d.txt &&
	grit checkout-index --all &&
	echo "alpha" >expect &&
	test_cmp expect a.txt &&
	echo "delta" >expect_d &&
	test_cmp expect_d sub/d.txt
	)
'

test_expect_success 'checkout-index --all does not overwrite existing' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	grit checkout-index --all 2>err &&
	echo "modified" >expect &&
	test_cmp expect a.txt
	)
'

###########################################################################
# Section 4: --force
###########################################################################

test_expect_success 'checkout-index --force overwrites existing file' '
	(
	cd repo &&
	echo "modified" >a.txt &&
	grit checkout-index --force a.txt &&
	echo "alpha" >expect &&
	test_cmp expect a.txt
	)
'

test_expect_success 'checkout-index -f is short for --force' '
	(
	cd repo &&
	echo "changed" >b.txt &&
	grit checkout-index -f b.txt &&
	echo "beta" >expect &&
	test_cmp expect b.txt
	)
'

test_expect_success 'checkout-index --all --force overwrites everything' '
	(
	cd repo &&
	echo "x" >a.txt &&
	echo "y" >b.txt &&
	echo "z" >c.txt &&
	grit checkout-index --all --force &&
	echo "alpha" >expect_a &&
	echo "beta" >expect_b &&
	echo "gamma" >expect_c &&
	test_cmp expect_a a.txt &&
	test_cmp expect_b b.txt &&
	test_cmp expect_c c.txt
	)
'

###########################################################################
# Section 5: --prefix
###########################################################################

test_expect_success 'checkout-index --prefix writes to prefixed path' '
	(
	cd repo &&
	rm -rf out &&
	mkdir out &&
	grit checkout-index --all --mkdir --prefix=out/ &&
	test -f out/a.txt &&
	test -f out/b.txt &&
	test -f out/c.txt &&
	echo "alpha" >expect &&
	test_cmp expect out/a.txt
	)
'

test_expect_success 'checkout-index --prefix creates subdirectories with --mkdir' '
	(
	cd repo &&
	rm -rf export &&
	grit checkout-index --all --mkdir --prefix=export/ &&
	test -f export/sub/d.txt &&
	echo "delta" >expect &&
	test_cmp expect export/sub/d.txt
	)
'

test_expect_success 'checkout-index --prefix with trailing slash only' '
	(
	cd repo &&
	rm -rf pfx &&
	grit checkout-index --mkdir --prefix=pfx/ a.txt &&
	test -f pfx/a.txt
	)
'

test_expect_success 'checkout-index --prefix does not affect working tree originals' '
	(
	cd repo &&
	echo "original" >a.txt &&
	rm -rf copy &&
	grit checkout-index --mkdir --prefix=copy/ a.txt &&
	echo "original" >expect &&
	test_cmp expect a.txt &&
	echo "alpha" >expect_copy &&
	test_cmp expect_copy copy/a.txt
	)
'

###########################################################################
# Section 6: --temp
###########################################################################

test_expect_success 'checkout-index --temp writes to temp file' '
	(
	cd repo &&
	grit checkout-index --temp a.txt >temp_out &&
	TMPFILE=$(cut -f1 <temp_out | tr -d " ") &&
	test -f "$TMPFILE" &&
	echo "alpha" >expect &&
	test_cmp expect "$TMPFILE" &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp output contains filename' '
	(
	cd repo &&
	grit checkout-index --temp a.txt >temp_out &&
	grep "a.txt" temp_out &&
	TMPFILE=$(cut -f1 <temp_out | tr -d " ") &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp with multiple files' '
	(
	cd repo &&
	grit checkout-index --temp a.txt b.txt >temp_out &&
	test_line_count = 2 temp_out &&
	while read line; do
		TMPFILE=$(echo "$line" | cut -f1 | tr -d " ") &&
		test -f "$TMPFILE" &&
		rm -f "$TMPFILE"
	done <temp_out
	)
'

test_expect_success 'checkout-index --temp does not modify working tree files' '
	(
	cd repo &&
	echo "keep me" >a.txt &&
	grit checkout-index --temp a.txt >temp_out &&
	echo "keep me" >expect &&
	test_cmp expect a.txt &&
	TMPFILE=$(cut -f1 <temp_out | tr -d " ") &&
	rm -f "$TMPFILE"
	)
'

###########################################################################
# Section 7: --quiet and --no-create
###########################################################################

test_expect_success 'checkout-index --quiet with existing file succeeds silently' '
	(
	cd repo &&
	grit checkout-index --quiet --force a.txt 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'checkout-index --no-create does not create files' '
	(
	cd repo &&
	rm -f a.txt &&
	grit checkout-index --no-create a.txt &&
	! test -f a.txt
	)
'

test_expect_success 'checkout-index -n is short for --no-create' '
	(
	cd repo &&
	rm -f b.txt &&
	grit checkout-index -n b.txt &&
	! test -f b.txt
	)
'

###########################################################################
# Section 8: --stdin
###########################################################################

test_expect_success 'checkout-index --stdin reads paths from stdin' '
	(
	cd repo &&
	rm -f a.txt b.txt &&
	printf "a.txt\0b.txt\0" | grit checkout-index --stdin -z &&
	test -f a.txt &&
	test -f b.txt
	)
'

test_expect_success 'checkout-index --stdin with newline-terminated input' '
	(
	cd repo &&
	rm -f a.txt &&
	echo "a.txt" | grit checkout-index --stdin &&
	test -f a.txt
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'checkout-index without args and without --all fails or no-ops' '
	(
	cd repo &&
	grit checkout-index 2>err
	)
'

test_expect_success 'checkout-index preserves executable bit' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit add script.sh &&
	rm script.sh &&
	grit checkout-index script.sh &&
	test -x script.sh
	)
'

test_expect_success 'checkout-index with empty index' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	grit checkout-index --all 2>err
	)
'

test_done
