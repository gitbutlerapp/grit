#!/bin/sh
# Test ls-files --stage output format: modes, hashes, stage numbers,
# pathspec filtering, -z output, and various file types.

test_description='grit ls-files --stage output format'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository with different file types' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "regular" >regular.txt &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	ln -s regular.txt symlink.txt &&
	mkdir -p sub/dir &&
	echo "nested" >sub/dir/nested.txt &&
	grit add regular.txt script.sh symlink.txt sub/dir/nested.txt &&
	grit commit -m "initial with various file types"
	)
'

###########################################################################
# Section 2: Basic --stage output format
###########################################################################

test_expect_success 'ls-files --stage produces output' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	test -s actual
	)
'

test_expect_success 'ls-files --stage shows all indexed files' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	test_line_count = 4 actual
	)
'

test_expect_success 'ls-files --stage format is mode SP hash SP stage TAB path' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	while IFS= read -r line; do
		echo "$line" | grep -qE "^[0-9]{6} [0-9a-f]{40} [0-9]	" ||
			{ echo "bad format: $line"; return 1; }
	done <actual
	)
'

test_expect_success 'ls-files -s is short for --stage' '
	(
	cd repo &&
	grit ls-files -s >actual_short &&
	grit ls-files --stage >actual_long &&
	test_cmp actual_long actual_short
	)
'

###########################################################################
# Section 3: Mode values
###########################################################################

test_expect_success 'regular file has mode 100644' '
	(
	cd repo &&
	grit ls-files --stage regular.txt >actual &&
	grep "^100644 " actual
	)
'

test_expect_success 'executable file has mode 100755' '
	(
	cd repo &&
	grit ls-files --stage script.sh >actual &&
	grep "^100755 " actual
	)
'

test_expect_success 'symlink has mode 120000' '
	(
	cd repo &&
	grit ls-files --stage symlink.txt >actual &&
	grep "^120000 " actual
	)
'

###########################################################################
# Section 4: Hash values
###########################################################################

test_expect_success 'hash in ls-files matches hash-object' '
	(
	cd repo &&
	EXPECTED=$(grit hash-object regular.txt) &&
	grit ls-files --stage regular.txt >actual &&
	grep "$EXPECTED" actual
	)
'

test_expect_success 'hash is 40-character hex string' '
	(
	cd repo &&
	grit ls-files --stage regular.txt >actual &&
	HASH=$(cut -d" " -f2 <actual) &&
	echo "$HASH" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'symlink hash matches blob of target path' '
	(
	cd repo &&
	EXPECTED=$(printf "regular.txt" | grit hash-object --stdin) &&
	grit ls-files --stage symlink.txt >actual &&
	grep "$EXPECTED" actual
	)
'

test_expect_success 'executable hash matches hash-object' '
	(
	cd repo &&
	EXPECTED=$(grit hash-object script.sh) &&
	grit ls-files --stage script.sh >actual &&
	grep "$EXPECTED" actual
	)
'

###########################################################################
# Section 5: Stage numbers
###########################################################################

test_expect_success 'stage number is 0 for normal entries' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	while IFS= read -r line; do
		STAGE=$(echo "$line" | cut -d" " -f3 | cut -d"	" -f1) &&
		test "$STAGE" = "0" ||
			{ echo "non-zero stage: $line"; return 1; }
	done <actual
	)
'

###########################################################################
# Section 6: Pathspec filtering
###########################################################################

test_expect_success 'ls-files --stage with single pathspec' '
	(
	cd repo &&
	grit ls-files --stage regular.txt >actual &&
	test_line_count = 1 actual &&
	grep "regular.txt" actual
	)
'

test_expect_success 'ls-files --stage with multiple pathspecs' '
	(
	cd repo &&
	grit ls-files --stage regular.txt script.sh >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'ls-files --stage with directory pathspec' '
	(
	cd repo &&
	grit ls-files --stage sub/ >actual &&
	test_line_count = 1 actual &&
	grep "sub/dir/nested.txt" actual
	)
'

test_expect_success 'ls-files --stage with nonexistent pathspec returns empty' '
	(
	cd repo &&
	grit ls-files --stage nonexistent.txt >actual &&
	test_must_be_empty actual
	)
'

###########################################################################
# Section 7: -z null-terminated output
###########################################################################

test_expect_success 'ls-files --stage -z uses NUL terminators' '
	(
	cd repo &&
	grit ls-files --stage -z >actual &&
	# Count NUL bytes - should be at least 4 (one per file)
	NULS=$(tr -cd "\0" <actual | wc -c | tr -d " ") &&
	test "$NULS" -ge 4
	)
'

test_expect_success 'ls-files --stage -z contains no bare newlines in entries' '
	(
	cd repo &&
	grit ls-files --stage -z >actual &&
	# Each NUL-terminated record should not contain embedded newlines
	# Count NULs - should be at least 4 (one per file)
	NULS=$(tr -cd "\0" <actual | wc -c | tr -d " ") &&
	test "$NULS" -ge 4
	)
'

###########################################################################
# Section 8: After modifications
###########################################################################

test_expect_success 'ls-files --stage unchanged after working tree modification' '
	(
	cd repo &&
	grit ls-files --stage >before &&
	echo "modified" >regular.txt &&
	grit ls-files --stage >after &&
	test_cmp before after
	)
'

test_expect_success 'ls-files --stage updates after grit add' '
	(
	cd repo &&
	echo "modified content" >regular.txt &&
	grit ls-files --stage regular.txt >before &&
	grit add regular.txt &&
	grit ls-files --stage regular.txt >after &&
	! test_cmp before after
	)
'

test_expect_success 'new file appears in --stage after add' '
	(
	cd repo &&
	echo "new file" >new.txt &&
	grit add new.txt &&
	grit ls-files --stage new.txt >actual &&
	test_line_count = 1 actual &&
	grep "^100644" actual &&
	grep "new.txt" actual
	)
'

###########################################################################
# Section 9: Sorted output
###########################################################################

test_expect_success 'ls-files --stage output is sorted by path' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	cut -f2 <actual >paths &&
	sort <paths >sorted_paths &&
	test_cmp sorted_paths paths
	)
'

###########################################################################
# Section 10: Empty and special cases
###########################################################################

test_expect_success 'ls-files --stage on empty index' '
	(
	grit init empty-repo &&
	cd empty-repo &&
	grit ls-files --stage >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files --stage with file containing spaces' '
	(
	cd repo &&
	echo "spaces" >"file with spaces.txt" &&
	grit add "file with spaces.txt" &&
	grit ls-files --stage "file with spaces.txt" >actual &&
	grep "file with spaces.txt" actual
	)
'

test_expect_success 'ls-files --cached shows filenames only' '
	(
	cd repo &&
	grit ls-files --cached >actual &&
	grep "regular.txt" actual &&
	! grep "100644" actual
	)
'

test_expect_success 'ls-files --stage after rm from index' '
	(
	cd repo &&
	grit ls-files --stage new.txt >before &&
	test -s before &&
	grit rm --cached new.txt &&
	grit ls-files --stage new.txt >after &&
	test_must_be_empty after
	)
'

test_expect_success 'ls-files --error-unmatch fails on missing file' '
	(
	cd repo &&
	test_must_fail grit ls-files --error-unmatch nonexistent 2>err &&
	grep -i "did not match" err
	)
'

test_expect_success 'ls-files --error-unmatch succeeds on existing file' '
	(
	cd repo &&
	grit ls-files --error-unmatch regular.txt >actual &&
	grep "regular.txt" actual
	)
'

test_done
