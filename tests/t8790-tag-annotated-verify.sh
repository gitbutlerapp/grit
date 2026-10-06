#!/bin/sh
# Tests for tag creation, listing, annotated tags, deletion, and options.

test_description='tag annotated and listing options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

OUT="$TRASH_DIRECTORY/output"
mkdir -p "$OUT"

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup repository with commits' '
	(
	git init repo &&
	cd repo &&
	echo "first" >file.txt &&
	git add file.txt &&
	git commit -m "first commit" &&
	echo "second" >file.txt &&
	git add file.txt &&
	git commit -m "second commit" &&
	echo "third" >file.txt &&
	git add file.txt &&
	git commit -m "third commit"
	)
'

# -- lightweight tags ----------------------------------------------------------

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	git tag v1.0
	)
'

test_expect_success 'tag points to HEAD' '
	(
	cd repo &&
	tag_target=$(git rev-parse v1.0) &&
	head_sha=$(git rev-parse HEAD) &&
	test "$tag_target" = "$head_sha"
	)
'

test_expect_success 'list tags shows v1.0' '
	(
	cd repo &&
	git tag -l >"$OUT/t1" &&
	grep "^v1.0$" "$OUT/t1"
	)
'

test_expect_success 'create second lightweight tag' '
	(
	cd repo &&
	git tag v0.9 HEAD~1
	)
'

test_expect_success 'list tags shows both tags' '
	(
	cd repo &&
	git tag -l >"$OUT/t2" &&
	grep "v1.0" "$OUT/t2" &&
	grep "v0.9" "$OUT/t2"
	)
'

test_expect_success 'tag on older commit points correctly' '
	(
	cd repo &&
	tag_target=$(git rev-parse v0.9) &&
	parent_sha=$(git rev-parse HEAD~1) &&
	test "$tag_target" = "$parent_sha"
	)
'

# -- annotated tags ------------------------------------------------------------

test_expect_success 'create annotated tag with -m' '
	(
	cd repo &&
	git tag -a -m "Release 2.0" v2.0
	)
'

test_expect_success 'annotated tag resolves to HEAD via ^{}' '
	(
	cd repo &&
	tag_commit=$(git rev-parse "v2.0^{}") &&
	head_sha=$(git rev-parse HEAD) &&
	test "$tag_commit" = "$head_sha"
	)
'

test_expect_success 'list tags includes annotated tag' '
	(
	cd repo &&
	git tag -l >"$OUT/t3" &&
	grep "v2.0" "$OUT/t3"
	)
'

test_expect_success 'create annotated tag with -m on older commit' '
	(
	cd repo &&
	git tag -a -m "Beta release" v1.0-beta HEAD~2
	)
'

test_expect_success 'annotated tag on older commit resolves correctly' '
	(
	cd repo &&
	tag_commit=$(git rev-parse "v1.0-beta^{}") &&
	old_sha=$(git rev-parse HEAD~2) &&
	test "$tag_commit" = "$old_sha"
	)
'

# -- tag -n: show annotation --------------------------------------------------

test_expect_success 'tag -n shows annotation for annotated tags' '
	(
	cd repo &&
	git tag -n >"$OUT/t4" &&
	grep "v2.0" "$OUT/t4" | grep "Release 2.0"
	)
'

test_expect_success 'tag -n shows lightweight tag without annotation' '
	(
	cd repo &&
	git tag -n >"$OUT/t5" &&
	grep "v1.0" "$OUT/t5"
	)
'

# -- tag -l with pattern -------------------------------------------------------

test_expect_success 'tag -l "v1*" matches only v1 tags' '
	(
	cd repo &&
	git tag -l "v1*" >"$OUT/t6" &&
	grep "v1.0" "$OUT/t6" &&
	grep "v1.0-beta" "$OUT/t6" &&
	! grep "v2.0" "$OUT/t6" &&
	! grep "v0.9" "$OUT/t6"
	)
'

test_expect_success 'tag -l "v2*" matches only v2 tags' '
	(
	cd repo &&
	git tag -l "v2*" >"$OUT/t7" &&
	grep "v2.0" "$OUT/t7" &&
	! grep "v1.0" "$OUT/t7"
	)
'

test_expect_success 'tag -l with non-matching pattern shows nothing' '
	(
	cd repo &&
	git tag -l "xyz*" >"$OUT/t8" &&
	test_line_count = 0 "$OUT/t8"
	)
'

# -- tag deletion --------------------------------------------------------------

test_expect_success 'delete lightweight tag' '
	(
	cd repo &&
	git tag -d v0.9
	)
'

test_expect_success 'deleted tag no longer listed' '
	(
	cd repo &&
	git tag -l >"$OUT/t9" &&
	! grep "v0.9" "$OUT/t9"
	)
'

test_expect_success 'delete annotated tag' '
	(
	cd repo &&
	git tag -d v2.0
	)
'

test_expect_success 'deleted annotated tag no longer listed' '
	(
	cd repo &&
	git tag -l >"$OUT/t10" &&
	! grep "v2.0" "$OUT/t10"
	)
'

# -- tag -f: force overwrite ---------------------------------------------------

test_expect_success 'cannot create tag with existing name without -f' '
	(
	cd repo &&
	test_must_fail git tag v1.0
	)
'

test_expect_success 'force create tag overwrites existing' '
	(
	cd repo &&
	git tag -f v1.0 HEAD~1
	)
'

test_expect_success 'forced tag now points to different commit' '
	(
	cd repo &&
	tag_target=$(git rev-parse v1.0) &&
	parent_sha=$(git rev-parse HEAD~1) &&
	test "$tag_target" = "$parent_sha"
	)
'

test_expect_success 'restore v1.0 to HEAD' '
	(
	cd repo &&
	git tag -f v1.0 HEAD
	)
'

# -- --contains ----------------------------------------------------------------

test_expect_success 'tag --contains HEAD shows tags on HEAD' '
	(
	cd repo &&
	git tag -l --contains HEAD >"$OUT/t11" &&
	grep "v1.0" "$OUT/t11"
	)
'

test_expect_success 'tag --contains older commit shows tags reachable from it' '
	(
	cd repo &&
	git tag -l --contains HEAD~2 >"$OUT/t12" &&
	grep "v1.0" "$OUT/t12" &&
	grep "v1.0-beta" "$OUT/t12"
	)
'

# -- --sort options ------------------------------------------------------------

test_expect_success 'tag --sort=refname lists alphabetically' '
	(
	cd repo &&
	git tag -l --sort=refname >"$OUT/t13" &&
	first=$(head -1 "$OUT/t13") &&
	last=$(tail -1 "$OUT/t13") &&
	test "$first" = "v1.0" &&
	test "$last" = "v1.0-beta"
	)
'

test_expect_success 'recreate v2.0 for more sort tests' '
	(
	cd repo &&
	git tag -a -m "Release 2.0 final" v2.0
	)
'

test_expect_success 'tag --sort=version:refname does version sorting' '
	(
	cd repo &&
	git tag -l --sort=version:refname >"$OUT/t14" &&
	head -1 "$OUT/t14" | grep "v1.0"
	)
'

# -- tag with -F (file message) ------------------------------------------------

test_expect_success 'create annotated tag with message from file' '
	(
	cd repo &&
	echo "Tag from file" >"$OUT/msg" &&
	echo "Second line of message" >>"$OUT/msg" &&
	git tag -a -F "$OUT/msg" v3.0
	)
'

test_expect_success 'tag -n shows file-sourced message' '
	(
	cd repo &&
	git tag -n >"$OUT/t15" &&
	grep "v3.0" "$OUT/t15" | grep "Tag from file"
	)
'

# -- multiple annotated tags on same commit ------------------------------------

test_expect_success 'create multiple annotated tags on same commit' '
	(
	cd repo &&
	git tag -a -m "alias tag A" alias-a &&
	git tag -a -m "alias tag B" alias-b
	)
'

test_expect_success 'both alias tags resolve to same commit' '
	(
	cd repo &&
	a_sha=$(git rev-parse "alias-a^{}") &&
	b_sha=$(git rev-parse "alias-b^{}") &&
	test "$a_sha" = "$b_sha"
	)
'

test_expect_success 'listing shows all tags including aliases' '
	(
	cd repo &&
	git tag -l >"$OUT/t16" &&
	grep "alias-a" "$OUT/t16" &&
	grep "alias-b" "$OUT/t16"
	)
'

# -- tag on tag (edge case) ---------------------------------------------------

test_expect_success 'tag object type is tag for annotated' '
	(
	cd repo &&
	tag_sha=$(git rev-parse v3.0) &&
	obj_type=$(git cat-file -t "$tag_sha") &&
	test "$obj_type" = "tag"
	)
'

test_expect_success 'lightweight tag object type is commit' '
	(
	cd repo &&
	tag_sha=$(git rev-parse v1.0) &&
	obj_type=$(git cat-file -t "$tag_sha") &&
	test "$obj_type" = "commit"
	)
'

test_done
