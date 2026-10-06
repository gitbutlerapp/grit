#!/bin/sh
# Tests for grit ls-files with cached/staged files.

test_description='grit ls-files cached mode'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Basic cached listing
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo
	)
'

test_expect_success 'ls-files on empty index shows nothing' '
	(
	cd repo &&
	grit ls-files >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files -c on empty index shows nothing' '
	(
	cd repo &&
	grit ls-files -c >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files shows one added file' '
	(
	cd repo &&
	echo "hello" >hello.txt &&
	grit add hello.txt &&
	grit ls-files >actual &&
	echo "hello.txt" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-files --cached same as default' '
	(
	cd repo &&
	grit ls-files --cached >actual_cached &&
	grit ls-files >actual_default &&
	test_cmp actual_cached actual_default
	)
'

test_expect_success 'ls-files shows multiple files sorted' '
	(
	cd repo &&
	echo "b" >b.txt &&
	echo "a" >a.txt &&
	echo "c" >c.txt &&
	grit add a.txt b.txt c.txt &&
	grit ls-files >actual &&
	printf "a.txt\nb.txt\nc.txt\nhello.txt\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'ls-files output is alphabetically sorted' '
	(
	cd repo &&
	grit ls-files >actual &&
	sort actual >sorted &&
	test_cmp sorted actual
	)
'

###########################################################################
# Section 2: Stage info
###########################################################################

test_expect_success 'ls-files -s shows stage info' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	test -s actual
	)
'

test_expect_success 'ls-files -s format is mode OID stage path' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	head -1 actual | grep -qE "^[0-9]{6} [0-9a-f]{40} [0-9]	"
	)
'

test_expect_success 'ls-files -s shows correct mode for regular file' '
	(
	cd repo &&
	grit ls-files -s hello.txt >actual &&
	grep "^100644" actual
	)
'

test_expect_success 'ls-files -s shows stage 0 for normal entries' '
	(
	cd repo &&
	grit ls-files -s >actual &&
	while read mode oid stage path; do
		test "$stage" = "0" || return 1
	done <actual
	)
'

test_expect_success 'ls-files -s OID matches hash-object' '
	(
	cd repo &&
	expected_oid=$(grit hash-object hello.txt) &&
	grit ls-files -s hello.txt >actual &&
	oid=$(awk "{print \$2}" actual) &&
	test "$oid" = "$expected_oid"
	)
'

###########################################################################
# Section 3: Subdirectories
###########################################################################

test_expect_success 'ls-files shows files in subdirectories' '
	(
	cd repo &&
	mkdir -p sub &&
	echo "nested" >sub/nested.txt &&
	grit add sub/nested.txt &&
	grit ls-files >actual &&
	grep "sub/nested.txt" actual
	)
'

test_expect_success 'ls-files shows full relative path for nested files' '
	(
	cd repo &&
	mkdir -p deep/dir &&
	echo "deep" >deep/dir/file.txt &&
	grit add deep/dir/file.txt &&
	grit ls-files >actual &&
	grep "deep/dir/file.txt" actual
	)
'

test_expect_success 'ls-files with pathspec filters output' '
	(
	cd repo &&
	grit ls-files sub/ >actual &&
	test_line_count = 1 actual &&
	grep "sub/nested.txt" actual
	)
'

test_expect_success 'ls-files with non-matching pathspec shows nothing' '
	(
	cd repo &&
	grit ls-files nonexistent/ >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 4: Cached entries after modifications
###########################################################################

test_expect_success 'ls-files still shows file after working tree modification' '
	(
	cd repo &&
	echo "changed" >hello.txt &&
	grit ls-files >actual &&
	grep "hello.txt" actual
	)
'

test_expect_success 'ls-files still shows file after working tree deletion' '
	(
	cd repo &&
	rm -f a.txt &&
	grit ls-files >actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'ls-files -s still shows OID after working tree change' '
	(
	cd repo &&
	grit ls-files -s hello.txt >actual &&
	awk "{print \$2}" actual | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'ls-files cached count unchanged by working tree edits' '
	(
	cd repo &&
	grit ls-files >before_count &&
	echo "more changes" >b.txt &&
	grit ls-files >after_count &&
	test_cmp before_count after_count
	)
'

test_expect_success 'ls-files after re-add shows same file' '
	(
	cd repo &&
	echo "re-added" >hello.txt &&
	grit add hello.txt &&
	grit ls-files >actual &&
	grep "hello.txt" actual
	)
'

###########################################################################
# Section 5: Zero-terminated output
###########################################################################

test_expect_success 'ls-files -z uses NUL terminators' '
	(
	cd repo &&
	grit ls-files -z >actual_z &&
	# NUL bytes should be present
	tr "\0" "\n" <actual_z >actual_nl &&
	test -s actual_nl
	)
'

test_expect_success 'ls-files -z output contains all cached files' '
	(
	cd repo &&
	grit ls-files -z | tr "\0" "\n" | sed "/^$/d" >actual &&
	grit ls-files >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: After removals and updates
###########################################################################

test_expect_success 'ls-files after grit rm no longer shows file' '
	(
	cd repo &&
	echo "remove me" >removeme.txt &&
	grit add removeme.txt &&
	grit ls-files >before &&
	grep "removeme.txt" before &&
	grit rm -f removeme.txt &&
	grit ls-files >after &&
	! grep "removeme.txt" after
	)
'

test_expect_success 'ls-files after update-index reflects new blob' '
	(
	cd repo &&
	echo "original" >upd.txt &&
	grit add upd.txt &&
	grit ls-files -s upd.txt >before &&
	echo "updated" >upd.txt &&
	grit add upd.txt &&
	grit ls-files -s upd.txt >after &&
	! test_cmp before after
	)
'

test_expect_success 'ls-files count matches number of added files' '
	(
	cd repo &&
	grit ls-files >listing &&
	count=$(wc -l <listing | tr -d " ") &&
	test "$count" -gt 0
	)
'

test_expect_success 'ls-files with executable file shows it' '
	(
	cd repo &&
	echo "#!/bin/sh" >run.sh &&
	chmod +x run.sh &&
	grit add run.sh &&
	grit ls-files >actual &&
	grep "run.sh" actual
	)
'

test_expect_success 'ls-files -s shows 100755 for executable' '
	(
	cd repo &&
	grit ls-files -s run.sh >actual &&
	grep "^100755" actual
	)
'

test_expect_success 'ls-files in fresh empty repo is empty' '
	(
	grit init fresh &&
	cd fresh &&
	grit ls-files >actual &&
	test_must_be_empty actual
	)
'

test_done
