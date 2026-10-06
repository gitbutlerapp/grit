#!/bin/sh
# Test write-tree with executable file modes, symlinks, mixed permissions,
# and verify the resulting tree entries preserve mode correctly.

test_description='grit write-tree with executable and special modes'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test"
	)
'

###########################################################################
# Section 2: Basic write-tree
###########################################################################

test_expect_success 'write-tree on empty index produces empty tree' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	test -n "$oid"
	)
'

test_expect_success 'write-tree after adding a file' '
	(
	cd repo &&
	echo "hello" >file.txt &&
	grit add file.txt &&
	oid=$(grit write-tree) &&
	test -n "$oid"
	)
'

test_expect_success 'write-tree result is a tree object' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	type=$(grit cat-file -t "$oid") &&
	test "$type" = "tree"
	)
'

test_expect_success 'write-tree matches git write-tree' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 3: Executable files
###########################################################################

test_expect_success 'setup executable file' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	grit add script.sh
	)
'

test_expect_success 'write-tree records executable as 100755' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100755" out | grep "script.sh"
	)
'

test_expect_success 'executable mode matches git' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'non-executable file is 100644' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100644" out | grep "file.txt"
	)
'

###########################################################################
# Section 4: Multiple executable files
###########################################################################

test_expect_success 'add multiple executables' '
	(
	cd repo &&
	echo "#!/bin/bash" >build.sh &&
	chmod +x build.sh &&
	echo "#!/usr/bin/env python3" >run.py &&
	chmod +x run.py &&
	grit add build.sh run.py
	)
'

test_expect_success 'all executables recorded as 100755' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100755.*script.sh" out &&
	grep "100755.*build.sh" out &&
	grep "100755.*run.py" out
	)
'

test_expect_success 'write-tree with executables matches git' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

###########################################################################
# Section 5: Symlinks
###########################################################################

test_expect_success 'add symlink' '
	(
	cd repo &&
	ln -sf file.txt link.txt &&
	grit add link.txt
	)
'

test_expect_success 'write-tree records symlink as 120000' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "120000" out | grep "link.txt"
	)
'

test_expect_success 'symlink mode matches git' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'symlink target is stored as blob content' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	blob_oid=$(grit ls-tree "$oid" | grep "link.txt" | awk "{print \$3}") &&
	grit cat-file -p "$blob_oid" >actual &&
	printf "file.txt" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Mixed modes in one tree
###########################################################################

test_expect_success 'tree has all three mode types' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100644" out &&
	grep "100755" out &&
	grep "120000" out
	)
'

test_expect_success 'entry count is correct' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	test_line_count = 5 out
	)
'

###########################################################################
# Section 7: Subdirectories with executables
###########################################################################

test_expect_success 'add executable in subdirectory' '
	(
	cd repo &&
	mkdir -p bin &&
	echo "#!/bin/sh" >bin/tool &&
	chmod +x bin/tool &&
	echo "readme" >bin/README &&
	grit add bin/tool bin/README
	)
'

test_expect_success 'write-tree with subdir executables matches git' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'subdir tree has correct modes' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	bin_oid=$(grit ls-tree "$oid" | grep "bin" | awk "{print \$3}") &&
	grit ls-tree "$bin_oid" >out &&
	grep "100644.*README" out &&
	grep "100755.*tool" out
	)
'

###########################################################################
# Section 8: Mode change tracking
###########################################################################

test_expect_success 'chmod +x changes tree OID' '
	(
	cd repo &&
	oid_before=$(grit write-tree) &&
	chmod +x file.txt &&
	grit add file.txt &&
	oid_after=$(grit write-tree) &&
	test "$oid_before" != "$oid_after"
	)
'

test_expect_success 'file.txt now shows 100755' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100755.*file.txt" out
	)
'

test_expect_success 'chmod -x reverts mode and tree OID' '
	(
	cd repo &&
	chmod -x file.txt &&
	grit add file.txt &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >out &&
	grep "100644.*file.txt" out
	)
'

###########################################################################
# Section 9: Nested symlinks
###########################################################################

test_expect_success 'symlink in subdirectory' '
	(
	cd repo &&
	mkdir -p lib &&
	echo "module" >lib/core.py &&
	ln -sf core.py lib/alias.py &&
	grit add lib/core.py lib/alias.py
	)
'

test_expect_success 'write-tree with nested symlink matches git' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_expect_success 'nested symlink has 120000 mode' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	lib_oid=$(grit ls-tree "$oid" | grep "lib" | awk "{print \$3}") &&
	grit ls-tree "$lib_oid" >out &&
	grep "120000.*alias.py" out
	)
'

###########################################################################
# Section 10: Idempotency and stability
###########################################################################

test_expect_success 'write-tree is idempotent' '
	(
	cd repo &&
	oid1=$(grit write-tree) &&
	oid2=$(grit write-tree) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'write-tree after commit still works' '
	(
	cd repo &&
	grit commit -m "test commit" &&
	echo "new content" >new.txt &&
	grit add new.txt &&
	oid=$(grit write-tree) &&
	test -n "$oid" &&
	grit cat-file -t "$oid" >type &&
	echo "tree" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'write-tree on unchanged index gives same OID' '
	(
	cd repo &&
	oid1=$(grit write-tree) &&
	oid2=$(grit write-tree) &&
	test "$oid1" = "$oid2"
	)
'

test_expect_success 'ls-tree output matches git ls-tree' '
	(
	cd repo &&
	oid=$(grit write-tree) &&
	grit ls-tree "$oid" >grit_out &&
	git ls-tree "$oid" >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'full tree with all modes matches git exactly' '
	(
	cd repo &&
	grit_oid=$(grit write-tree) &&
	git_oid=$(git write-tree) &&
	test "$grit_oid" = "$git_oid"
	)
'

test_done
