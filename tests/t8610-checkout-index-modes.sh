#!/bin/sh
# Tests for checkout-index with different file modes (644, 755, symlinks),
# --prefix, --temp, --stdin, --mkdir, -a, -f combinations.

test_description='checkout-index file modes, prefix, temp, stdin'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup repository with mixed file modes' '
	(
	grit init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&

	echo "normal file" >normal.txt &&
	echo "#!/bin/sh" >script.sh && chmod 755 script.sh &&
	echo "another normal" >readme.md &&
	mkdir -p dir/sub &&
	echo "nested" >dir/sub/nested.txt &&
	ln -s normal.txt symlink.txt &&

	grit add normal.txt script.sh readme.md dir/sub/nested.txt symlink.txt &&
	grit commit -m "initial with mixed modes"
	)
'

###########################################################################
# Section 1: Mode preservation on checkout
###########################################################################

test_expect_success 'checkout-index preserves 644 mode' '
	(
	cd repo &&
	rm -f normal.txt &&
	grit checkout-index -f normal.txt &&
	test -f normal.txt &&
	echo "normal file" >expect &&
	test_cmp expect normal.txt
	)
'

test_expect_success 'checkout-index preserves 755 mode (executable)' '
	(
	cd repo &&
	rm -f script.sh &&
	grit checkout-index -f script.sh &&
	test -x script.sh &&
	echo "#!/bin/sh" >expect &&
	test_cmp expect script.sh
	)
'

test_expect_success 'checkout-index restores symlink' '
	(
	cd repo &&
	rm -f symlink.txt &&
	grit checkout-index -f symlink.txt &&
	test -L symlink.txt &&
	LINK_TARGET=$(readlink symlink.txt) &&
	test "$LINK_TARGET" = "normal.txt"
	)
'

test_expect_success 'checkout-index -a restores all top-level files' '
	(
	cd repo &&
	rm -f normal.txt script.sh readme.md symlink.txt &&
	grit checkout-index -a -f &&
	test -f normal.txt &&
	test -f script.sh &&
	test -f readme.md &&
	test -L symlink.txt
	)
'

test_expect_success 'after -a, executable bit is preserved' '
	(
	cd repo &&
	test -x script.sh
	)
'

test_expect_success 'after -a, symlink target is correct' '
	(
	cd repo &&
	test "$(readlink symlink.txt)" = "normal.txt"
	)
'

###########################################################################
# Section 2: --prefix
###########################################################################

test_expect_success 'checkout-index --prefix writes to subdir' '
	(
	cd repo &&
	rm -rf output/ &&
	mkdir -p output/dir/sub &&
	grit checkout-index --prefix=output/ -a &&
	test -f output/normal.txt &&
	test -f output/script.sh &&
	test -f output/readme.md
	)
'

test_expect_success 'checkout-index --prefix preserves executable mode' '
	(
	cd repo &&
	test -x output/script.sh
	)
'

test_expect_success 'checkout-index --prefix preserves symlinks' '
	(
	cd repo &&
	test -L output/symlink.txt &&
	test "$(readlink output/symlink.txt)" = "normal.txt"
	)
'

test_expect_success 'checkout-index --prefix with nested dirs' '
	(
	cd repo &&
	test -f output/dir/sub/nested.txt
	)
'

test_expect_success 'checkout-index --prefix specific file' '
	(
	cd repo &&
	rm -rf out2/ && mkdir out2/ &&
	grit checkout-index --prefix=out2/ normal.txt &&
	test -f out2/normal.txt &&
	! test -f out2/script.sh
	)
'

test_expect_success 'checkout-index --prefix with --mkdir creates dirs' '
	(
	cd repo &&
	rm -rf out3/ && mkdir out3/ &&
	grit checkout-index --prefix=out3/ --mkdir dir/sub/nested.txt &&
	test -f out3/dir/sub/nested.txt
	)
'

###########################################################################
# Section 3: --temp
###########################################################################

test_expect_success 'checkout-index --temp creates temp file' '
	(
	cd repo &&
	grit checkout-index --temp normal.txt >actual &&
	TMPFILE=$(cut -f1 actual | tr -d " ") &&
	test -f "$TMPFILE" &&
	echo "normal file" >expect &&
	test_cmp expect "$TMPFILE" &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp output has tab-separated name' '
	(
	cd repo &&
	grit checkout-index --temp normal.txt >actual &&
	grep "	normal.txt" actual &&
	TMPFILE=$(cut -f1 actual | tr -d " ") &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp with executable' '
	(
	cd repo &&
	grit checkout-index --temp script.sh >actual &&
	TMPFILE=$(cut -f1 actual | tr -d " ") &&
	test -f "$TMPFILE" &&
	echo "#!/bin/sh" >expect &&
	test_cmp expect "$TMPFILE" &&
	rm -f "$TMPFILE"
	)
'

###########################################################################
# Section 4: --stdin
###########################################################################

test_expect_success 'checkout-index --stdin reads paths from stdin' '
	(
	cd repo &&
	rm -f normal.txt script.sh &&
	printf "normal.txt\nscript.sh\n" | grit checkout-index --stdin -f &&
	test -f normal.txt &&
	test -f script.sh
	)
'

test_expect_success 'checkout-index --stdin -z reads NUL-terminated paths' '
	(
	cd repo &&
	rm -f normal.txt readme.md &&
	printf "normal.txt\0readme.md\0" | grit checkout-index --stdin -z -f &&
	test -f normal.txt &&
	test -f readme.md
	)
'

###########################################################################
# Section 5: --force and existing files
###########################################################################

test_expect_success 'checkout-index without --force refuses existing dirty file' '
	(
	cd repo &&
	echo "dirty" >normal.txt &&
	test_must_fail grit checkout-index normal.txt 2>err &&
	test "$(cat normal.txt)" = "dirty" &&
	echo "normal file" >normal.txt
	)
'

test_expect_success 'checkout-index -f overwrites existing file' '
	(
	cd repo &&
	echo "dirty" >normal.txt &&
	grit checkout-index -f normal.txt &&
	echo "normal file" >expect &&
	test_cmp expect normal.txt
	)
'

test_expect_success 'checkout-index --force overwrites existing file' '
	(
	cd repo &&
	echo "dirty2" >script.sh &&
	grit checkout-index --force script.sh &&
	echo "#!/bin/sh" >expect &&
	test_cmp expect script.sh
	)
'

###########################################################################
# Section 6: Index state verification via ls-files
###########################################################################

test_expect_success 'ls-files --stage shows correct modes' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "100644.*normal.txt" actual &&
	grep "100755.*script.sh" actual &&
	grep "120000.*symlink.txt" actual
	)
'

test_expect_success 'ls-files --stage shows nested file' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "100644.*dir/sub/nested.txt" actual
	)
'

###########################################################################
# Section 7: Multiple files and edge cases
###########################################################################

test_expect_success 'checkout-index multiple named files' '
	(
	cd repo &&
	rm -f normal.txt readme.md &&
	grit checkout-index -f normal.txt readme.md &&
	test -f normal.txt &&
	test -f readme.md
	)
'

test_expect_success 'checkout-index nonexistent file fails' '
	(
	cd repo &&
	test_must_fail grit checkout-index no-such-file 2>err &&
	test -s err
	)
'

test_expect_success 'checkout-index -q suppresses errors on missing' '
	(
	cd repo &&
	test_must_fail grit checkout-index -q no-such-file 2>err &&
	test_must_be_empty err
	)
'

test_expect_success 'checkout-index --prefix with trailing slash required' '
	(
	cd repo &&
	rm -rf pfx/ && mkdir pfx/ &&
	grit checkout-index --prefix=pfx/ normal.txt &&
	test -f pfx/normal.txt
	)
'

test_done
