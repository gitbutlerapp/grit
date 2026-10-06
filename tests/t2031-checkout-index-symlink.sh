#!/bin/sh
# Test checkout-index behaviour with symlinks: creation, content,
# --force, --prefix, --temp, core.symlinks=false, and edge cases.

test_description='grit checkout-index with symlinks'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with symlinks' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "target content" >target.txt &&
	ln -s target.txt link.txt &&
	mkdir -p sub &&
	echo "sub content" >sub/file.txt &&
	ln -s file.txt sub/link.txt &&
	grit add target.txt link.txt sub/file.txt sub/link.txt &&
	grit commit -m "initial with symlinks"
	)
'

###########################################################################
# Section 2: Basic symlink checkout
###########################################################################

test_expect_success 'checkout-index creates symlink' '
	(
	cd repo &&
	rm -f link.txt &&
	grit checkout-index link.txt &&
	test -L link.txt
	)
'

test_expect_success 'symlink points to correct target' '
	(
	cd repo &&
	rm -f link.txt &&
	grit checkout-index link.txt &&
	TARGET=$(readlink link.txt) &&
	test "$TARGET" = "target.txt"
	)
'

test_expect_success 'checkout-index restores symlink in subdirectory' '
	(
	cd repo &&
	rm -f sub/link.txt &&
	grit checkout-index sub/link.txt &&
	test -L sub/link.txt &&
	TARGET=$(readlink sub/link.txt) &&
	test "$TARGET" = "file.txt"
	)
'

test_expect_success 'checkout-index --all restores symlinks' '
	(
	cd repo &&
	rm -f link.txt sub/link.txt &&
	grit checkout-index --all &&
	test -L link.txt &&
	test -L sub/link.txt
	)
'

###########################################################################
# Section 3: ls-files shows symlink mode
###########################################################################

test_expect_success 'ls-files --stage shows 120000 for symlink' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "^120000" actual >symlinks &&
	test_line_count = 2 symlinks
	)
'

test_expect_success 'ls-files --stage shows 100644 for regular files' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "^100644" actual >regular &&
	test_line_count = 2 regular
	)
'

test_expect_success 'symlink blob contains target path' '
	(
	cd repo &&
	BLOB_OID=$(grit ls-files --stage link.txt | cut -d" " -f2) &&
	grit cat-file -p "$BLOB_OID" >actual &&
	echo -n "target.txt" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: --force with symlinks
###########################################################################

test_expect_success 'checkout-index refuses to overwrite existing symlink without --force' '
	(
	cd repo &&
	rm -f link.txt &&
	ln -s something_else link.txt &&
	grit checkout-index link.txt 2>err &&
	TARGET=$(readlink link.txt) &&
	test "$TARGET" = "something_else"
	)
'

test_expect_success 'checkout-index --force overwrites symlink' '
	(
	cd repo &&
	rm -f link.txt &&
	ln -s something_else link.txt &&
	grit checkout-index --force link.txt &&
	test -L link.txt &&
	TARGET=$(readlink link.txt) &&
	test "$TARGET" = "target.txt"
	)
'

test_expect_success 'checkout-index --force replaces regular file with symlink' '
	(
	cd repo &&
	rm -f link.txt &&
	echo "not a link" >link.txt &&
	grit checkout-index --force link.txt &&
	test -L link.txt &&
	TARGET=$(readlink link.txt) &&
	test "$TARGET" = "target.txt"
	)
'

test_expect_success 'checkout-index --force replaces symlink with regular file' '
	(
	cd repo &&
	rm -f target.txt &&
	ln -s bogus target.txt &&
	grit checkout-index --force target.txt &&
	test -f target.txt &&
	! test -L target.txt &&
	echo "target content" >expect &&
	test_cmp expect target.txt
	)
'

###########################################################################
# Section 5: --prefix with symlinks
###########################################################################

test_expect_success 'checkout-index --prefix creates symlinks in prefix dir' '
	(
	cd repo &&
	rm -rf out &&
	grit checkout-index --all --mkdir --prefix=out/ &&
	test -L out/link.txt &&
	TARGET=$(readlink out/link.txt) &&
	test "$TARGET" = "target.txt"
	)
'

test_expect_success 'checkout-index --prefix preserves subdirectory symlinks' '
	(
	cd repo &&
	rm -rf out &&
	grit checkout-index --all --mkdir --prefix=out/ &&
	test -L out/sub/link.txt &&
	TARGET=$(readlink out/sub/link.txt) &&
	test "$TARGET" = "file.txt"
	)
'

test_expect_success 'prefixed symlink target is relative (same as index)' '
	(
	cd repo &&
	rm -rf export &&
	grit checkout-index --mkdir --prefix=export/ link.txt &&
	TARGET=$(readlink export/link.txt) &&
	test "$TARGET" = "target.txt"
	)
'

###########################################################################
# Section 6: --temp with symlinks
###########################################################################

test_expect_success 'checkout-index --temp with symlink creates temp file' '
	(
	cd repo &&
	grit checkout-index --temp link.txt >temp_out &&
	TMPFILE=$(cut -f1 <temp_out | tr -d " ") &&
	test -f "$TMPFILE" &&
	cat "$TMPFILE" >actual &&
	echo -n "target.txt" >expect &&
	test_cmp expect actual &&
	rm -f "$TMPFILE"
	)
'

test_expect_success 'checkout-index --temp output references original name' '
	(
	cd repo &&
	grit checkout-index --temp link.txt >temp_out &&
	grep "link.txt" temp_out &&
	TMPFILE=$(cut -f1 <temp_out | tr -d " ") &&
	rm -f "$TMPFILE"
	)
'

###########################################################################
# Section 7: core.symlinks=false
###########################################################################

test_expect_success 'setup repo with core.symlinks=false' '
	(
	grit init nosym-repo &&
	cd nosym-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	git config core.symlinks false &&
	BLOB=$(printf "target.txt" | grit hash-object -t blob -w --stdin) &&
	printf "120000 %s\tsymlink\n" "$BLOB" | grit update-index --index-info
	)
'

test_expect_success 'checkout-index writes plain file with core.symlinks=false' '
	(
	cd nosym-repo &&
	grit checkout-index symlink &&
	test -f symlink &&
	! test -L symlink
	)
'

test_expect_success 'plain file content matches symlink target' '
	(
	cd nosym-repo &&
	cat symlink >actual &&
	printf "target.txt" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Dangling and special symlinks
###########################################################################

test_expect_success 'checkout-index creates dangling symlink' '
	(
	grit init dangle-repo &&
	cd dangle-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	BLOB=$(printf "nonexistent" | grit hash-object -t blob -w --stdin) &&
	printf "120000 %s\tdangling\n" "$BLOB" | grit update-index --index-info &&
	grit checkout-index dangling &&
	test -L dangling &&
	TARGET=$(readlink dangling) &&
	test "$TARGET" = "nonexistent"
	)
'

test_expect_success 'absolute symlink target is preserved' '
	(
	grit init abs-repo &&
	cd abs-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	BLOB=$(printf "/tmp/absolute-target" | grit hash-object -t blob -w --stdin) &&
	printf "120000 %s\tabslink\n" "$BLOB" | grit update-index --index-info &&
	grit checkout-index abslink &&
	test -L abslink &&
	TARGET=$(readlink abslink) &&
	test "$TARGET" = "/tmp/absolute-target"
	)
'

test_expect_success 'symlink to directory name is preserved' '
	(
	grit init dirlink-repo &&
	cd dirlink-repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	BLOB=$(printf "realdir" | grit hash-object -t blob -w --stdin) &&
	printf "120000 %s\tdirlink\n" "$BLOB" | grit update-index --index-info &&
	grit checkout-index dirlink &&
	test -L dirlink &&
	TARGET=$(readlink dirlink) &&
	test "$TARGET" = "realdir"
	)
'

###########################################################################
# Section 9: Mixed symlinks and regular files
###########################################################################

test_expect_success 'checkout-index --all handles mix of types correctly' '
	(
	cd repo &&
	rm -f target.txt link.txt sub/file.txt sub/link.txt &&
	grit checkout-index --all &&
	test -f target.txt && ! test -L target.txt &&
	test -L link.txt &&
	test -f sub/file.txt && ! test -L sub/file.txt &&
	test -L sub/link.txt
	)
'

test_expect_success 'hash-object of checked out symlink matches index' '
	(
	cd repo &&
	rm -f link.txt &&
	grit checkout-index link.txt &&
	EXPECTED=$(grit ls-files --stage link.txt | cut -d" " -f2) &&
	ACTUAL=$(printf "%s" "$(readlink link.txt)" | grit hash-object --stdin) &&
	test "$EXPECTED" = "$ACTUAL"
	)
'

test_done
