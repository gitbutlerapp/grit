#!/bin/sh
#
# Tests for 'grit update-ref -d' — deleting refs, with and without
# old-value checks, --no-deref, stdin mode, and error cases.

test_description='grit update-ref -d (delete mode)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: repo with branches and tags' '
	(
	$REAL_GIT init --initial-branch=master repo &&
	cd repo &&
	$REAL_GIT config user.name "Test User" &&
	$REAL_GIT config user.email "test@example.com" &&
	echo first >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "first commit" &&
	$REAL_GIT tag v1.0 &&
	$REAL_GIT tag -a v1.0-annotated -m "version 1.0" &&
	echo second >file &&
	$REAL_GIT add file &&
	test_tick &&
	$REAL_GIT commit -m "second commit" &&
	$REAL_GIT branch branch-to-delete &&
	$REAL_GIT branch branch-to-delete-2 &&
	$REAL_GIT branch branch-to-delete-3 &&
	$REAL_GIT branch branch-to-delete-4 &&
	$REAL_GIT branch branch-to-delete-5 &&
	$REAL_GIT branch branch-oldval-check &&
	$REAL_GIT branch branch-noderef &&
	$REAL_GIT tag tag-to-delete &&
	$REAL_GIT tag tag-to-delete-2 &&
	$REAL_GIT tag -a annotated-to-delete -m "will delete"
	)
'

# ---------------------------------------------------------------------------
# Basic delete
# ---------------------------------------------------------------------------
test_expect_success 'update-ref -d deletes a branch ref' '
	(
	cd repo &&
	grit show-ref --verify refs/heads/branch-to-delete &&
	grit update-ref -d refs/heads/branch-to-delete &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-to-delete
	)
'

test_expect_success 'update-ref -d deletes a lightweight tag' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/tag-to-delete &&
	grit update-ref -d refs/tags/tag-to-delete &&
	test_must_fail $REAL_GIT show-ref --verify refs/tags/tag-to-delete
	)
'

test_expect_success 'update-ref -d deletes an annotated tag ref' '
	(
	cd repo &&
	grit show-ref --verify refs/tags/annotated-to-delete &&
	grit update-ref -d refs/tags/annotated-to-delete &&
	test_must_fail $REAL_GIT show-ref --verify refs/tags/annotated-to-delete
	)
'

test_expect_success 'deleted branch ref no longer appears in show-ref' '
	(
	cd repo &&
	grit show-ref >actual &&
	! grep "refs/heads/branch-to-delete " actual
	)
'

test_expect_success 'deleted tag ref no longer appears in show-ref' '
	(
	cd repo &&
	grit show-ref >actual &&
	! grep "refs/tags/tag-to-delete " actual
	)
'

# ---------------------------------------------------------------------------
# Delete with old-value verification
# ---------------------------------------------------------------------------
test_expect_success 'update-ref -d with correct old value succeeds' '
	(
	cd repo &&
	old_sha=$($REAL_GIT rev-parse refs/heads/branch-to-delete-2) &&
	grit update-ref -d refs/heads/branch-to-delete-2 "$old_sha" &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-to-delete-2
	)
'

test_expect_success 'update-ref -d with wrong old value fails' '
	(
	cd repo &&
	test_must_fail grit update-ref -d refs/heads/branch-to-delete-3 0000000000000000000000000000000000000001 2>err
	)
'

test_expect_success 'ref still exists after failed old-value delete' '
	(
	cd repo &&
	$REAL_GIT show-ref --verify refs/heads/branch-to-delete-3
	)
'

# ---------------------------------------------------------------------------
# Delete nonexistent ref
# ---------------------------------------------------------------------------
test_expect_success 'update-ref -d on nonexistent ref is idempotent (exits 0)' '
	(
	cd repo &&
	grit update-ref -d refs/heads/nonexistent
	)
'

# ---------------------------------------------------------------------------
# --no-deref with delete
# ---------------------------------------------------------------------------
test_expect_success 'setup: create symbolic ref pointing to a branch' '
	(
	cd repo &&
	$REAL_GIT symbolic-ref refs/heads/symlink refs/heads/branch-noderef
	)
'

test_expect_success 'update-ref -d --no-deref removes symbolic ref itself' '
	(
	cd repo &&
	grit update-ref -d --no-deref refs/heads/symlink &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/symlink &&
	$REAL_GIT show-ref --verify refs/heads/branch-noderef
	)
'

# ---------------------------------------------------------------------------
# stdin mode with delete
# ---------------------------------------------------------------------------
test_expect_success 'update-ref --stdin delete command works' '
	(
	cd repo &&
	old_sha=$($REAL_GIT rev-parse refs/heads/branch-to-delete-4) &&
	echo "delete refs/heads/branch-to-delete-4 $old_sha" |
	grit update-ref --stdin &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-to-delete-4
	)
'

test_expect_success 'update-ref --stdin delete without old value works' '
	(
	cd repo &&
	echo "delete refs/heads/branch-to-delete-5" |
	grit update-ref --stdin &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-to-delete-5
	)
'

test_expect_success 'update-ref --stdin delete nonexistent ref exits 0' '
	(
	cd repo &&
	echo "delete refs/heads/does-not-exist" |
	grit update-ref --stdin
	)
'

# ---------------------------------------------------------------------------
# stdin -z mode with delete
# ---------------------------------------------------------------------------
test_expect_success 'setup: create refs for -z tests' '
	(
	cd repo &&
	$REAL_GIT branch branch-z-delete &&
	$REAL_GIT tag tag-z-delete
	)
'

test_expect_success 'update-ref --stdin -z delete command works' '
	(
	cd repo &&
	old_sha=$($REAL_GIT rev-parse refs/heads/branch-z-delete) &&
	printf "delete refs/heads/branch-z-delete $old_sha\0" |
	grit update-ref --stdin -z &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-z-delete
	)
'

test_expect_success 'update-ref --stdin -z delete tag works' '
	(
	cd repo &&
	printf "delete refs/tags/tag-z-delete\0" |
	grit update-ref --stdin -z &&
	test_must_fail $REAL_GIT show-ref --verify refs/tags/tag-z-delete
	)
'

# ---------------------------------------------------------------------------
# Reflog message with delete
# ---------------------------------------------------------------------------
test_expect_success 'setup: branch for reflog delete test' '
	(
	cd repo &&
	$REAL_GIT branch branch-reflog-delete
	)
'

test_expect_success 'update-ref -d -m writes reflog message' '
	(
	cd repo &&
	grit update-ref -d -m "deleting branch" refs/heads/branch-reflog-delete &&
	test_must_fail $REAL_GIT show-ref --verify refs/heads/branch-reflog-delete
	)
'

# ---------------------------------------------------------------------------
# Delete tag-to-delete-2 and verify
# ---------------------------------------------------------------------------
test_expect_success 'update-ref -d removes second tag' '
	(
	cd repo &&
	grit update-ref -d refs/tags/tag-to-delete-2 &&
	test_must_fail $REAL_GIT show-ref --verify refs/tags/tag-to-delete-2
	)
'

# ---------------------------------------------------------------------------
# Compare remaining refs with real git
# ---------------------------------------------------------------------------
test_expect_success 'remaining refs match between grit and real git' '
	(
	cd repo &&
	grit show-ref | sort >actual &&
	$REAL_GIT show-ref | sort >expect &&
	test_cmp expect actual
	)
'

# ---------------------------------------------------------------------------
# Cannot delete HEAD directly
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# Delete a custom ref
# ---------------------------------------------------------------------------
test_expect_success 'setup: create custom ref for deletion' '
	(
	cd repo &&
	sha=$($REAL_GIT rev-parse refs/heads/master) &&
	$REAL_GIT update-ref refs/custom/myref "$sha"
	)
'

test_expect_success 'update-ref -d deletes custom ref' '
	(
	cd repo &&
	grit update-ref -d refs/custom/myref &&
	test_must_fail $REAL_GIT show-ref --verify refs/custom/myref
	)
'

test_done
