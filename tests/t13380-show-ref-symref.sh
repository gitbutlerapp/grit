#!/bin/sh
# Tests for grit show-ref with symbolic refs, --head, --verify, --exists, etc.

test_description='grit show-ref symref and listing behavior'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Setup
###########################################################################

test_expect_success 'setup: create repo with branches and tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" branch feature &&
	"$REAL_GIT" branch release &&
	echo "update" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" tag -a v1.0 -m "release v1.0" HEAD~1 &&
	"$REAL_GIT" tag lightweight-tag
	)
'

###########################################################################
# Basic listing
###########################################################################

test_expect_success 'show-ref lists all refs' '
	(cd repo && grit show-ref >../actual) &&
	(cd repo && "$REAL_GIT" show-ref >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref output has SHA and refname' '
	(cd repo && grit show-ref >../actual) &&
	grep "^[0-9a-f]\{40\} refs/" actual
'

test_expect_success 'show-ref lists branches and tags' '
	(cd repo && grit show-ref >../actual) &&
	grep "refs/heads/" actual &&
	grep "refs/tags/" actual
'

###########################################################################
# --head flag
###########################################################################

test_expect_success 'show-ref --head includes HEAD' '
	(cd repo && grit show-ref --head >../actual) &&
	grep "^[0-9a-f]\{40\} HEAD$" actual
'

test_expect_success 'show-ref --head matches git' '
	(cd repo && grit show-ref --head >../actual) &&
	(cd repo && "$REAL_GIT" show-ref --head >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref --head puts HEAD first' '
	(cd repo && grit show-ref --head >../actual) &&
	head -1 actual >first &&
	grep "HEAD" first
'

###########################################################################
# --branches and --tags filters
###########################################################################

test_expect_success 'show-ref --branches only shows branches' '
	(cd repo && grit show-ref --branches >../actual) &&
	(cd repo && "$REAL_GIT" show-ref --heads >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref --branches excludes tags' '
	(cd repo && grit show-ref --branches >../actual) &&
	! grep "refs/tags/" actual
'

test_expect_success 'show-ref --tags only shows tags' '
	(cd repo && grit show-ref --tags >../actual) &&
	(cd repo && "$REAL_GIT" show-ref --tags >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref --tags excludes branches' '
	(cd repo && grit show-ref --tags >../actual) &&
	! grep "refs/heads/" actual
'

###########################################################################
# --verify mode
###########################################################################

test_expect_success 'show-ref --verify with exact ref succeeds' '
	(cd repo && grit show-ref --verify refs/heads/main >../actual) &&
	grep "refs/heads/main" actual
'

test_expect_success 'show-ref --verify matches git' '
	(cd repo && grit show-ref --verify refs/heads/main >../actual) &&
	(cd repo && "$REAL_GIT" show-ref --verify refs/heads/main >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref --verify with nonexistent ref fails' '
	(cd repo && test_must_fail grit show-ref --verify refs/heads/nonexistent)
'

test_expect_success 'show-ref --verify requires full ref path' '
	(cd repo && test_must_fail grit show-ref --verify main)
'

###########################################################################
# --exists mode
###########################################################################

test_expect_success 'show-ref --exists returns 0 for existing ref' '
	(cd repo && grit show-ref --exists refs/heads/main)
'

test_expect_success 'show-ref --exists returns non-zero for missing ref' '
	(cd repo && test_must_fail grit show-ref --exists refs/heads/nonexistent)
'

test_expect_success 'show-ref --exists works for tags' '
	(cd repo && grit show-ref --exists refs/tags/v1.0)
'

###########################################################################
# --hash / -s mode
###########################################################################

test_expect_success 'show-ref --hash shows only SHA' '
	(cd repo && grit show-ref --hash >../actual) &&
	(cd repo && "$REAL_GIT" show-ref | cut -d" " -f1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref -s is alias for --hash' '
	(cd repo && grit show-ref -s >../actual) &&
	(cd repo && grit show-ref --hash >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref --hash=7 abbreviates SHA' '
	(cd repo && grit show-ref --hash=7 >../actual) &&
	while read line; do
		len=$(printf "%s" "$line" | wc -c)
		test "$len" -ge 7 || exit 1
	done <actual
'

###########################################################################
# --dereference / -d
###########################################################################

test_expect_success 'show-ref -d shows peeled annotated tags' '
	(cd repo && grit show-ref -d >../actual) &&
	grep "v1.0\^{}" actual
'

test_expect_success 'show-ref -d matches git' '
	(cd repo && grit show-ref -d >../actual) &&
	(cd repo && "$REAL_GIT" show-ref -d >../expect) &&
	test_cmp expect actual
'

test_expect_success 'show-ref -d peeled tag points to commit' '
	(cd repo &&
	 PEELED=$(grit show-ref -d | grep "v1.0\^{}" | cut -d" " -f1) &&
	 COMMIT=$("$REAL_GIT" rev-parse v1.0^{}) &&
	 test "$PEELED" = "$COMMIT")
'

###########################################################################
# --abbrev
###########################################################################

test_expect_success 'show-ref --abbrev abbreviates SHA' '
	(cd repo && grit show-ref --abbrev >../actual) &&
	(cd repo && "$REAL_GIT" show-ref --abbrev >../expect) &&
	test_cmp expect actual
'

###########################################################################
# --quiet / -q
###########################################################################

test_expect_success 'show-ref --verify -q produces no output on success' '
	(cd repo && grit show-ref --verify -q refs/heads/main >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'show-ref --verify -q returns 0 for existing ref' '
	(cd repo && grit show-ref --verify -q refs/heads/main)
'

test_expect_success 'show-ref --verify -q returns non-zero for missing ref' '
	(cd repo && test_must_fail grit show-ref --verify -q refs/heads/nonexistent)
'

###########################################################################
# Pattern matching
###########################################################################

test_expect_success 'show-ref with pattern filters refs' '
	(cd repo && grit show-ref refs/heads/main >../actual) &&
	test_line_count = 1 actual &&
	grep "refs/heads/main" actual
'

test_expect_success 'show-ref with nonexistent pattern returns empty' '
	(cd repo && test_must_fail grit show-ref refs/nonexistent/)
'

###########################################################################
# Symbolic ref scenarios
###########################################################################

test_expect_success 'HEAD is a symref to main' '
	(cd repo &&
	 "$REAL_GIT" symbolic-ref HEAD >../actual) &&
	echo "refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'show-ref --head after switching branch' '
	(cd repo &&
	 "$REAL_GIT" checkout feature &&
	 grit show-ref --head >../actual &&
	 "$REAL_GIT" show-ref --head >../expect &&
	 "$REAL_GIT" checkout main) &&
	test_cmp expect actual
'

test_done
