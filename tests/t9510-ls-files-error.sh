#!/bin/sh
# Tests for grit ls-files: --error-unmatch, --stage, -z, pathspec
# filtering, and various cached-file listing scenarios.

test_description='grit ls-files error-unmatch, stage, and pathspecs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	cd "$TRASH_DIRECTORY" &&
	grit init repo &&
	cd repo &&
	echo "tracked1" >tracked1.txt &&
	echo "tracked2" >tracked2.txt &&
	mkdir -p sub/deep &&
	echo "sub file" >sub/deep.txt &&
	echo "deeper" >sub/deep/leaf.txt &&
	grit add tracked1.txt tracked2.txt sub/deep.txt sub/deep/leaf.txt
'

###########################################################################
# Section 2: --error-unmatch
###########################################################################

test_expect_success 'ls-files --error-unmatch succeeds for tracked file' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --error-unmatch tracked1.txt >actual &&
	grep "tracked1.txt" actual
'

test_expect_success 'ls-files --error-unmatch fails for untracked file' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	test_must_fail grit ls-files --error-unmatch nonexistent.txt
'

test_expect_success 'ls-files --error-unmatch with multiple tracked files' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --error-unmatch tracked1.txt tracked2.txt >actual &&
	grep "tracked1.txt" actual &&
	grep "tracked2.txt" actual
'

test_expect_success 'ls-files --error-unmatch fails if any file is untracked' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	test_must_fail grit ls-files --error-unmatch tracked1.txt missing.txt
'

test_expect_success 'ls-files --error-unmatch with subdirectory file' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --error-unmatch sub/deep.txt >actual &&
	grep "sub/deep.txt" actual
'

test_expect_success 'ls-files --error-unmatch with deeply nested file' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --error-unmatch sub/deep/leaf.txt >actual &&
	grep "sub/deep/leaf.txt" actual
'

###########################################################################
# Section 3: --stage / -s
###########################################################################

test_expect_success 'ls-files --stage shows mode, OID, stage' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --stage >actual &&
	grep "^100644 [0-9a-f]\{40\} 0	tracked1.txt" actual
'

test_expect_success 'ls-files -s is same as --stage' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files -s >s_out &&
	grit ls-files --stage >stage_out &&
	test_cmp s_out stage_out
'

test_expect_success 'ls-files --stage shows all tracked files' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --stage >actual &&
	test $(wc -l <actual) -eq 4
'

test_expect_success 'ls-files --stage OIDs are 40 hex chars' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --stage >actual &&
	while IFS="	" read info path; do
		oid=$(echo "$info" | awk "{print \$2}") &&
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" || return 1
	done <actual
'

test_expect_success 'ls-files --stage all stages are 0' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --stage >actual &&
	while IFS="	" read info path; do
		stage=$(echo "$info" | awk "{print \$3}") &&
		test "$stage" = "0" || return 1
	done <actual
'

test_expect_success 'ls-files --stage OID matches hash-object' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	expected_oid=$(grit hash-object tracked1.txt) &&
	grit ls-files --stage >actual &&
	grep "tracked1.txt" actual | awk "{print \$2}" >stage_oid &&
	echo "$expected_oid" >expect &&
	test_cmp expect stage_oid
'

test_expect_success 'ls-files --stage OID for sub file matches hash-object' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	expected_oid=$(grit hash-object sub/deep.txt) &&
	grit ls-files --stage >actual &&
	grep "sub/deep.txt$" actual | awk "{print \$2}" >stage_oid &&
	echo "$expected_oid" >expect &&
	test_cmp expect stage_oid
'

###########################################################################
# Section 4: -z (NUL-terminated output)
###########################################################################

test_expect_success 'ls-files -z uses NUL terminators' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files -z >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "tracked1.txt" decoded &&
	grep "tracked2.txt" decoded
'

test_expect_success 'ls-files -z --stage uses NUL terminators' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files -z --stage >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "tracked1.txt" decoded
'

test_expect_success 'ls-files -z entry count matches normal mode' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files >normal &&
	grit ls-files -z | tr "\0" "\n" >z_decoded &&
	test $(grep -c . normal) -eq $(grep -c . z_decoded)
'

###########################################################################
# Section 5: Pathspec filtering
###########################################################################

test_expect_success 'ls-files with pathspec restricts output' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files tracked1.txt >actual &&
	echo "tracked1.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'ls-files with directory pathspec shows files in dir' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files sub/ >actual &&
	grep "sub/deep.txt" actual &&
	grep "sub/deep/leaf.txt" actual
'

test_expect_success 'ls-files with directory pathspec excludes other files' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files sub/ >actual &&
	! grep "tracked1.txt" actual &&
	! grep "tracked2.txt" actual
'

test_expect_success 'ls-files with non-matching pathspec shows nothing' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files nonexistent/ >actual &&
	test_must_be_empty actual
'

test_expect_success 'ls-files with multiple pathspecs' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files tracked1.txt sub/deep.txt >actual &&
	grep "tracked1.txt" actual &&
	grep "sub/deep.txt" actual &&
	test $(wc -l <actual) -eq 2
'

###########################################################################
# Section 6: Default behavior (cached)
###########################################################################

test_expect_success 'ls-files default lists all cached files' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files >actual &&
	test $(wc -l <actual) -eq 4
'

test_expect_success 'ls-files -c same as default' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files -c >cached &&
	grit ls-files >default_out &&
	test_cmp cached default_out
'

test_expect_success 'ls-files --cached same as default' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files --cached >cached &&
	grit ls-files >default_out &&
	test_cmp cached default_out
'

test_expect_success 'ls-files output is sorted' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit ls-files >actual &&
	sort actual >sorted &&
	test_cmp actual sorted
'

test_expect_success 'ls-files after adding another file' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	echo "extra" >extra.txt &&
	grit add extra.txt &&
	grit ls-files >actual &&
	grep "extra.txt" actual &&
	test $(wc -l <actual) -eq 5
'

test_expect_success 'ls-files after removing a file from index' '
	cd "$TRASH_DIRECTORY" && cd repo &&
	grit rm --cached extra.txt &&
	grit ls-files >actual &&
	! grep "extra.txt" actual &&
	test $(wc -l <actual) -eq 4
'

###########################################################################
# Section 7: Cross-check with real git
###########################################################################

test_expect_success 'setup cross-check repo' '
	cd "$TRASH_DIRECTORY" &&
	$REAL_GIT init cross-repo &&
	cd cross-repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo "a" >a.txt &&
	echo "b" >b.txt &&
	mkdir sub &&
	echo "c" >sub/c.txt &&
	$REAL_GIT add .
'

test_expect_success 'ls-files output matches real git' '
	cd "$TRASH_DIRECTORY" && cd cross-repo &&
	grit ls-files >grit_out &&
	$REAL_GIT ls-files >git_out &&
	test_cmp grit_out git_out
'

test_expect_success 'ls-files --stage output matches real git' '
	cd "$TRASH_DIRECTORY" && cd cross-repo &&
	grit ls-files --stage >grit_out &&
	$REAL_GIT ls-files --stage >git_out &&
	test_cmp grit_out git_out
'

test_expect_success 'ls-files with pathspec matches real git' '
	cd "$TRASH_DIRECTORY" && cd cross-repo &&
	grit ls-files a.txt >grit_out &&
	$REAL_GIT ls-files a.txt >git_out &&
	test_cmp grit_out git_out
'

test_done
