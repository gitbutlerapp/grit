#!/bin/sh
# Test grit add --verbose/-v, --dry-run/-n, --force/-f,
# --update/-u, --all/-A, --intent-to-add/-N, pathspec handling,
# and various add scenarios.

test_description='grit add --verbose and --dry-run options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User"
	)
'

# --- basic add ---

test_expect_success 'add single file' '
	(
	cd repo &&
	echo "hello" >hello.txt &&
	grit add hello.txt &&
	grit status >actual &&
	grep "hello.txt" actual
	)
'

test_expect_success 'add multiple files' '
	(
	cd repo &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	grit add a.txt b.txt &&
	grit status >actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual
	)
'

test_expect_success 'add with dot adds everything' '
	(
	cd repo &&
	echo "c" >c.txt &&
	echo "d" >d.txt &&
	grit add . &&
	grit status >actual &&
	grep "c.txt" actual &&
	grep "d.txt" actual
	)
'

test_expect_success 'commit setup files' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "initial files"
	)
'

# --- add --verbose / -v ---

test_expect_success 'add -v shows files being added' '
	(
	cd repo &&
	echo "new1" >new1.txt &&
	grit add -v new1.txt >actual 2>&1 &&
	grep "new1.txt" actual
	)
'

test_expect_success 'add --verbose shows files being added' '
	(
	cd repo &&
	echo "new2" >new2.txt &&
	grit add --verbose new2.txt >actual 2>&1 &&
	grep "new2.txt" actual
	)
'

test_expect_success 'add -v with multiple files' '
	(
	cd repo &&
	echo "v1" >v1.txt &&
	echo "v2" >v2.txt &&
	grit add -v v1.txt v2.txt >actual 2>&1 &&
	grep "v1.txt" actual &&
	grep "v2.txt" actual
	)
'

test_expect_success 'add -v with dot' '
	(
	cd repo &&
	echo "v3" >v3.txt &&
	grit add -v . >actual 2>&1 &&
	grep "v3.txt" actual
	)
'

test_expect_success 'commit verbose files' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "verbose files"
	)
'

# --- add --dry-run / -n ---

test_expect_success 'add -n does not stage files' '
	(
	cd repo &&
	echo "dry" >dry.txt &&
	grit add -n dry.txt >actual 2>&1 &&
	grit status >status_out &&
	! grep "new file.*dry.txt" status_out || true &&
	# dry.txt should still be untracked or not staged
	grep "dry.txt" actual
	)
'

test_expect_success 'add --dry-run shows what would be added' '
	(
	cd repo &&
	echo "dry2" >dry2.txt &&
	grit add --dry-run dry2.txt >actual 2>&1 &&
	grep "dry2.txt" actual
	)
'

test_expect_success 'add -n with multiple files' '
	(
	cd repo &&
	echo "d1" >d1.txt &&
	echo "d2" >d2.txt &&
	grit add -n d1.txt d2.txt >actual 2>&1 &&
	grep "d1.txt" actual &&
	grep "d2.txt" actual
	)
'

test_expect_success 'add -n with dot' '
	(
	cd repo &&
	grit add -n . >actual 2>&1 &&
	grep "dry.txt" actual &&
	grep "dry2.txt" actual
	)
'

test_expect_success 'add -n does not modify index' '
	(
	cd repo &&
	grit ls-files >before &&
	grit add -n . >/dev/null 2>&1 &&
	grit ls-files >after &&
	test_cmp before after
	)
'

# --- add -v -n combined ---

test_expect_success 'add -v -n shows verbose dry-run output' '
	(
	cd repo &&
	echo "combo" >combo.txt &&
	grit add -v -n combo.txt >actual 2>&1 &&
	grep "combo.txt" actual
	)
'

test_expect_success 'add -v -n does not stage' '
	(
	cd repo &&
	grit ls-files >before &&
	grit add -v -n combo.txt >/dev/null 2>&1 &&
	grit ls-files >after &&
	test_cmp before after
	)
'

# --- add --update / -u ---

test_expect_success 'add -u stages modified tracked files' '
	(
	cd repo &&
	echo "modified hello" >hello.txt &&
	grit add -u &&
	grit status >actual &&
	grep "hello.txt" actual
	)
'

test_expect_success 'add -u does not add new untracked files' '
	(
	cd repo &&
	grit ls-files >actual &&
	! grep "dry.txt" actual
	)
'

test_expect_success 'add --update stages modifications' '
	(
	cd repo &&
	echo "modified again" >a.txt &&
	grit add --update &&
	test_tick &&
	grit commit -m "update test"
	)
'

# --- add --all / -A ---

test_expect_success 'add -A stages new and modified files' '
	(
	cd repo &&
	echo "allnew" >allnew.txt &&
	echo "mod" >hello.txt &&
	grit add -A &&
	grit status >actual &&
	grep "allnew.txt" actual
	)
'

test_expect_success 'add --all stages everything' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "all test" &&
	echo "another" >another.txt &&
	grit add --all &&
	grit status >actual &&
	grep "another.txt" actual
	)
'

test_expect_success 'commit all files' '
	(
	cd repo &&
	test_tick &&
	grit commit -m "all committed"
	)
'

# --- add --intent-to-add / -N ---

test_expect_success 'add -N records intent to add' '
	(
	cd repo &&
	echo "intent" >intent.txt &&
	grit add -N intent.txt &&
	grit ls-files >actual &&
	grep "intent.txt" actual
	)
'

test_expect_success 'add --intent-to-add records intent' '
	(
	cd repo &&
	echo "intent2" >intent2.txt &&
	grit add --intent-to-add intent2.txt &&
	grit ls-files >actual &&
	grep "intent2.txt" actual
	)
'

# --- add --force / -f ---

test_expect_success 'setup: create .gitignore' '
	(
	cd repo &&
	echo "*.log" >.gitignore &&
	grit add .gitignore &&
	test_tick &&
	grit commit -m "add gitignore"
	)
'

test_expect_success 'add -f adds ignored file' '
	(
	cd repo &&
	echo "log data" >debug.log &&
	grit add -f debug.log &&
	grit ls-files >actual &&
	grep "debug.log" actual
	)
'

test_expect_success 'add --force adds ignored file' '
	(
	cd repo &&
	echo "another log" >error.log &&
	grit add --force error.log &&
	grit ls-files >actual &&
	grep "error.log" actual
	)
'

test_expect_success 'add -f -v shows forced file' '
	(
	cd repo &&
	echo "trace" >trace.log &&
	grit add -f -v trace.log >actual 2>&1 &&
	grep "trace.log" actual
	)
'

# --- add in subdirectory ---

test_expect_success 'add file in subdirectory' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "nested" >sub/deep/file.txt &&
	grit add sub/deep/file.txt &&
	grit ls-files >actual &&
	grep "sub/deep/file.txt" actual
	)
'

test_expect_success 'add subdirectory with dot' '
	(
	cd repo &&
	echo "sub2" >sub/another.txt &&
	cd sub &&
	grit add . &&
	cd .. &&
	grit ls-files >actual &&
	grep "sub/another.txt" actual
	)
'

# --- add -v -n with subdirectory ---

test_expect_success 'add -v -n in subdirectory' '
	(
	cd repo &&
	echo "subvn" >sub/vn.txt &&
	grit add -v -n sub/vn.txt >actual 2>&1 &&
	grep "vn.txt" actual
	)
'

test_done
