#!/bin/sh
# Tests for grit ls-files with -z, --stage, and various filter flags.

test_description='grit ls-files: -z, --stage, --cached, --modified, --deleted, --others, pathspecs'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with tracked and untracked files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "tracked1" >a.txt &&
	echo "tracked2" >b.txt &&
	mkdir -p sub &&
	echo "nested" >sub/c.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic --cached (default)
###########################################################################

test_expect_success 'ls-files lists tracked files' '
	(
	cd repo &&
	grit ls-files >actual &&
	grep "a.txt" actual &&
	grep "b.txt" actual &&
	grep "sub/c.txt" actual
	)
'

test_expect_success 'ls-files matches git ls-files' '
	(
	cd repo &&
	grit ls-files >grit_out &&
	"$REAL_GIT" ls-files >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files --cached is same as default' '
	(
	cd repo &&
	grit ls-files >default_out &&
	grit ls-files --cached >cached_out &&
	test_cmp default_out cached_out
	)
'

test_expect_success 'ls-files file count is correct' '
	(
	cd repo &&
	grit ls-files >actual &&
	test_line_count = 3 actual
	)
'

###########################################################################
# Section 3: --stage / -s
###########################################################################

test_expect_success 'ls-files --stage shows mode and hash' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "100644" actual &&
	grep "[0-9a-f]\{40\}" actual
	)
'

test_expect_success 'ls-files --stage matches git' '
	(
	cd repo &&
	grit ls-files --stage >grit_out &&
	"$REAL_GIT" ls-files --stage >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files -s is same as --stage' '
	(
	cd repo &&
	grit ls-files -s >short_out &&
	grit ls-files --stage >long_out &&
	test_cmp long_out short_out
	)
'

test_expect_success 'ls-files --stage shows stage number 0 for normal files' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "a.txt" actual | grep "0"
	)
'

test_expect_success 'ls-files --stage entry count matches file count' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	test_line_count = 3 actual
	)
'

###########################################################################
# Section 4: -z (NUL termination)
###########################################################################

test_expect_success 'ls-files -z output contains NUL bytes' '
	(
	cd repo &&
	grit ls-files -z >actual &&
	tr "\0" "\n" <actual >decoded &&
	grep "a.txt" decoded
	)
'

test_expect_success 'ls-files -z matches git -z' '
	(
	cd repo &&
	grit ls-files -z >grit_out &&
	"$REAL_GIT" ls-files -z >git_out &&
	cmp grit_out git_out
	)
'

test_expect_success 'ls-files -z -s matches git -z -s' '
	(
	cd repo &&
	grit ls-files -z -s >grit_out &&
	"$REAL_GIT" ls-files -z -s >git_out &&
	cmp grit_out git_out
	)
'

test_expect_success 'ls-files -z entry count matches' '
	(
	cd repo &&
	grit ls-files -z >actual &&
	tr "\0" "\n" <actual >decoded &&
	grit ls-files >normal &&
	wc -l <normal | tr -d " " >expect_count &&
	grep -c . decoded >actual_count || true &&
	test_cmp expect_count actual_count
	)
'

###########################################################################
# Section 5: --others (untracked) — grit currently returns all files
###########################################################################

test_expect_success 'ls-files --others shows only untracked files' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	grit ls-files --others >actual &&
	grep "untracked.txt" actual &&
	! grep "a.txt" actual
	)
'

test_expect_success 'ls-files --others matches git' '
	(
	cd repo &&
	grit ls-files --others >grit_out &&
	"$REAL_GIT" ls-files --others >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files --others includes untracked file (grit bug)' '
	(
	cd repo &&
	echo "untracked" >untracked.txt &&
	grit ls-files --others >actual &&
	grep "untracked.txt" actual
	)
'

###########################################################################
# Section 6: --modified (grit currently lists all files, not just modified)
###########################################################################

test_expect_success 'ls-files --modified on clean repo is empty' '
	(
	cd repo &&
	grit ls-files --modified >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files --modified includes modified tracked file' '
	(
	cd repo &&
	echo "changed" >a.txt &&
	grit ls-files --modified >actual &&
	grep "a.txt" actual
	)
'

test_expect_success 'ls-files --modified matches git' '
	(
	cd repo &&
	grit ls-files --modified >grit_out &&
	"$REAL_GIT" ls-files --modified >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'restore a.txt after modified test' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- a.txt
	)
'

###########################################################################
# Section 7: --deleted (grit currently lists all files, not just deleted)
###########################################################################

test_expect_success 'ls-files --deleted on clean repo is empty' '
	(
	cd repo &&
	grit ls-files --deleted >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'ls-files --deleted includes deleted tracked file' '
	(
	cd repo &&
	rm b.txt &&
	grit ls-files --deleted >actual &&
	grep "b.txt" actual
	)
'

test_expect_success 'ls-files --deleted matches git' '
	(
	cd repo &&
	grit ls-files --deleted >grit_out &&
	"$REAL_GIT" ls-files --deleted >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'restore b.txt after deleted test' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- b.txt
	)
'

###########################################################################
# Section 8: Pathspec filtering
###########################################################################

test_expect_success 'ls-files with pathspec restricts output' '
	(
	cd repo &&
	grit ls-files sub/ >actual &&
	grep "sub/c.txt" actual &&
	! grep "a.txt" actual
	)
'

test_expect_success 'ls-files pathspec matches git' '
	(
	cd repo &&
	grit ls-files sub/ >grit_out &&
	"$REAL_GIT" ls-files sub/ >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files with specific file pathspec' '
	(
	cd repo &&
	grit ls-files a.txt >actual &&
	grep "a.txt" actual &&
	test_line_count = 1 actual
	)
'

test_expect_success 'ls-files specific file matches git' '
	(
	cd repo &&
	grit ls-files a.txt >grit_out &&
	"$REAL_GIT" ls-files a.txt >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 9: After adding new files to index
###########################################################################

test_expect_success 'ls-files shows newly staged file' '
	(
	cd repo &&
	echo "new" >new.txt &&
	"$REAL_GIT" add new.txt &&
	grit ls-files >actual &&
	grep "new.txt" actual
	)
'

test_expect_success 'ls-files --stage for newly staged file' '
	(
	cd repo &&
	grit ls-files --stage >actual &&
	grep "new.txt" actual &&
	grep "100644" actual | grep "new.txt"
	)
'

test_expect_success 'ls-files after staging matches git' '
	(
	cd repo &&
	grit ls-files --stage >grit_out &&
	"$REAL_GIT" ls-files --stage >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 10: Executable and symlink modes in stage
###########################################################################

test_expect_success 'ls-files --stage shows 100755 for executable' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	"$REAL_GIT" add script.sh &&
	grit ls-files --stage >actual &&
	grep "100755" actual | grep "script.sh"
	)
'

test_expect_success 'ls-files --stage executable matches git' '
	(
	cd repo &&
	grit ls-files --stage script.sh >grit_out &&
	"$REAL_GIT" ls-files --stage script.sh >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'ls-files --stage shows 120000 for symlink' '
	(
	cd repo &&
	ln -sf a.txt link.txt &&
	"$REAL_GIT" add link.txt &&
	grit ls-files --stage >actual &&
	grep "120000" actual | grep "link.txt"
	)
'

test_expect_success 'ls-files --stage symlink matches git' '
	(
	cd repo &&
	grit ls-files --stage link.txt >grit_out &&
	"$REAL_GIT" ls-files --stage link.txt >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 11: Deduplicate
###########################################################################

test_expect_success 'ls-files --deduplicate matches git' '
	(
	cd repo &&
	grit ls-files --deduplicate >grit_out &&
	"$REAL_GIT" ls-files --deduplicate >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 12: Combined flags
###########################################################################

test_expect_success 'ls-files -z --stage with pathspec' '
	(
	cd repo &&
	grit ls-files -z --stage a.txt >grit_out &&
	"$REAL_GIT" ls-files -z --stage a.txt >git_out &&
	cmp grit_out git_out
	)
'

test_expect_success 'ls-files --others with pathspec matches git' '
	(
	cd repo &&
	echo "extra" >sub/extra.txt &&
	grit ls-files --others sub/ >grit_out &&
	"$REAL_GIT" ls-files --others sub/ >git_out &&
	test_cmp git_out grit_out
	)
'

test_done
