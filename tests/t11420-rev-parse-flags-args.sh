#!/bin/sh
# Tests for grit rev-parse with various revision arguments.

test_description='grit rev-parse: refs, tags, HEAD~N, HEAD^N, --verify'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with branches and tags' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "first" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "first commit" &&
	"$REAL_GIT" tag v0.1 &&
	echo "second" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "second commit" &&
	"$REAL_GIT" tag v0.2 &&
	"$REAL_GIT" checkout -b feature &&
	echo "feature" >feat.txt &&
	"$REAL_GIT" add feat.txt &&
	"$REAL_GIT" commit -m "feature commit" &&
	"$REAL_GIT" checkout main &&
	echo "third" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "third commit" &&
	"$REAL_GIT" merge feature -m "merge feature" --no-edit &&
	echo "fourth" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "fourth commit" &&
	"$REAL_GIT" tag v1.0
	)
'

###########################################################################
# Section 2: Basic HEAD resolution
###########################################################################

test_expect_success 'rev-parse HEAD returns valid SHA-1' '
	(
	cd repo &&
	git rev-parse HEAD >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse HEAD matches real git' '
	(
	cd repo &&
	git rev-parse HEAD >grit_out &&
	"$REAL_GIT" rev-parse HEAD >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse HEAD is deterministic' '
	(
	cd repo &&
	git rev-parse HEAD >run1 &&
	git rev-parse HEAD >run2 &&
	test_cmp run1 run2
	)
'

###########################################################################
# Section 3: Branch resolution
###########################################################################

test_expect_success 'rev-parse main returns valid SHA-1' '
	(
	cd repo &&
	git rev-parse main >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse main matches HEAD' '
	(
	cd repo &&
	git rev-parse main >main_hash &&
	git rev-parse HEAD >head_hash &&
	test_cmp main_hash head_hash
	)
'

test_expect_success 'rev-parse feature returns valid SHA-1' '
	(
	cd repo &&
	git rev-parse feature >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse feature matches real git' '
	(
	cd repo &&
	git rev-parse feature >grit_out &&
	"$REAL_GIT" rev-parse feature >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse feature differs from main' '
	(
	cd repo &&
	git rev-parse feature >feat_hash &&
	git rev-parse main >main_hash &&
	! test_cmp feat_hash main_hash
	)
'

###########################################################################
# Section 4: Tag resolution
###########################################################################

test_expect_success 'rev-parse v0.1 returns valid SHA-1' '
	(
	cd repo &&
	git rev-parse v0.1 >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse v0.1 matches real git' '
	(
	cd repo &&
	git rev-parse v0.1 >grit_out &&
	"$REAL_GIT" rev-parse v0.1 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse v0.2 matches real git' '
	(
	cd repo &&
	git rev-parse v0.2 >grit_out &&
	"$REAL_GIT" rev-parse v0.2 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse v1.0 matches HEAD' '
	(
	cd repo &&
	git rev-parse v1.0 >tag_hash &&
	git rev-parse HEAD >head_hash &&
	test_cmp tag_hash head_hash
	)
'

test_expect_success 'rev-parse v0.1 differs from v0.2' '
	(
	cd repo &&
	git rev-parse v0.1 >v1 &&
	git rev-parse v0.2 >v2 &&
	! test_cmp v1 v2
	)
'

###########################################################################
# Section 5: Ancestor notation (HEAD~N)
###########################################################################

test_expect_success 'rev-parse HEAD~0 equals HEAD' '
	(
	cd repo &&
	git rev-parse HEAD >head_hash &&
	git rev-parse HEAD~0 >tilde0 &&
	test_cmp head_hash tilde0
	)
'

test_expect_success 'rev-parse HEAD~1 returns parent' '
	(
	cd repo &&
	git rev-parse HEAD~1 >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse HEAD~1 matches real git' '
	(
	cd repo &&
	git rev-parse HEAD~1 >grit_out &&
	"$REAL_GIT" rev-parse HEAD~1 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse HEAD~2 matches real git' '
	(
	cd repo &&
	git rev-parse HEAD~2 >grit_out &&
	"$REAL_GIT" rev-parse HEAD~2 >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'rev-parse HEAD~1 differs from HEAD' '
	(
	cd repo &&
	git rev-parse HEAD >head_hash &&
	git rev-parse HEAD~1 >parent_hash &&
	! test_cmp head_hash parent_hash
	)
'

###########################################################################
# Section 6: Parent notation (HEAD^N)
###########################################################################

test_expect_success 'rev-parse HEAD^1 returns first parent' '
	(
	cd repo &&
	git rev-parse HEAD^1 >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse HEAD^1 matches HEAD~1' '
	(
	cd repo &&
	git rev-parse HEAD^1 >caret &&
	git rev-parse HEAD~1 >tilde &&
	test_cmp caret tilde
	)
'

test_expect_success 'rev-parse HEAD^1 matches real git' '
	(
	cd repo &&
	git rev-parse HEAD^1 >grit_out &&
	"$REAL_GIT" rev-parse HEAD^1 >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 7: --verify flag
###########################################################################

test_expect_success 'rev-parse --verify HEAD succeeds' '
	(
	cd repo &&
	git rev-parse --verify HEAD >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse --verify HEAD matches rev-parse HEAD' '
	(
	cd repo &&
	git rev-parse HEAD >plain &&
	git rev-parse --verify HEAD >verified &&
	test_cmp plain verified
	)
'

test_expect_success 'rev-parse --verify main succeeds' '
	(
	cd repo &&
	git rev-parse --verify main >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse --verify nonexistent fails' '
	(
	cd repo &&
	test_must_fail git rev-parse --verify nonexistent-ref 2>/dev/null
	)
'

test_expect_success 'rev-parse --verify with tag' '
	(
	cd repo &&
	git rev-parse --verify v1.0 >output &&
	git rev-parse v1.0 >plain &&
	test_cmp plain output
	)
'

###########################################################################
# Section 8: Multiple arguments
###########################################################################

test_expect_success 'rev-parse with two refs outputs two lines' '
	(
	cd repo &&
	git rev-parse main feature >output &&
	test $(wc -l <output) -eq 2
	)
'

test_expect_success 'rev-parse main feature matches individual calls' '
	(
	cd repo &&
	git rev-parse main >m &&
	git rev-parse feature >f &&
	cat m f >expected &&
	git rev-parse main feature >actual &&
	test_cmp expected actual
	)
'

###########################################################################
# Section 9: Edge cases
###########################################################################

test_expect_success 'rev-parse on nonexistent ref fails' '
	(
	cd repo &&
	test_must_fail git rev-parse does-not-exist 2>/dev/null
	)
'

test_expect_success 'rev-parse with full SHA-1 returns same hash' '
	(
	cd repo &&
	full_hash=$(git rev-parse HEAD) &&
	git rev-parse "$full_hash" >output &&
	test "$(cat output)" = "$full_hash"
	)
'

test_expect_success 'rev-parse tag~1 returns parent of tagged commit' '
	(
	cd repo &&
	git rev-parse v1.0~1 >tag_parent &&
	git rev-parse HEAD~1 >head_parent &&
	test_cmp tag_parent head_parent
	)
'

test_expect_success 'rev-parse on single-commit repo' '
	(
	"$REAL_GIT" init single &&
	cd single &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "only" >only.txt &&
	"$REAL_GIT" add only.txt &&
	"$REAL_GIT" commit -m "only" &&
	git rev-parse HEAD >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_done
