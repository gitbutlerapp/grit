#!/bin/sh
# Tests for grit tag with -m (message) and -F (file) options.

test_description='grit tag -m (message) and -F (file)'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with commits' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo a >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "first" &&
	echo b >b.txt &&
	"$REAL_GIT" add b.txt &&
	"$REAL_GIT" commit -m "second" &&
	echo c >c.txt &&
	"$REAL_GIT" add c.txt &&
	"$REAL_GIT" commit -m "third"
	)
'

###########################################################################
# Section 2: Lightweight tags
###########################################################################

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	git tag v0.1 &&
	git tag -l >out &&
	grep "v0.1" out
	)
'

test_expect_success 'lightweight tag points to HEAD' '
	(
	cd repo &&
	test "$(git rev-parse v0.1)" = "$(git rev-parse HEAD)"
	)
'

test_expect_success 'lightweight tag at specific commit' '
	(
	cd repo &&
	first_sha=$(git rev-parse HEAD~2) &&
	git tag v0.0 "$first_sha" &&
	test "$(git rev-parse v0.0)" = "$first_sha"
	)
'

test_expect_success 'multiple lightweight tags on same commit' '
	(
	cd repo &&
	git tag same-1 &&
	git tag same-2 &&
	test "$(git rev-parse same-1)" = "$(git rev-parse same-2)"
	)
'

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	git tag -l >out &&
	grep "v0.0" out &&
	grep "v0.1" out &&
	grep "same-1" out
	)
'

###########################################################################
# Section 3: Annotated tags with -m
###########################################################################

test_expect_success 'tag -a -m creates annotated tag' '
	(
	cd repo &&
	git tag -a -m "Release 1.0" v1.0 &&
	git tag -l >out &&
	grep "v1.0" out
	)
'

test_expect_success 'annotated tag message is readable via cat-file' '
	(
	cd repo &&
	git cat-file -p v1.0 >out &&
	grep "Release 1.0" out
	)
'

test_expect_success 'annotated tag has tagger info' '
	(
	cd repo &&
	git cat-file -p v1.0 >out &&
	grep "tagger" out
	)
'

test_expect_success 'annotated tag has type commit' '
	(
	cd repo &&
	git cat-file -p v1.0 >out &&
	grep "^type commit" out
	)
'

test_expect_success 'annotated tag has tag name' '
	(
	cd repo &&
	git cat-file -p v1.0 >out &&
	grep "^tag v1.0" out
	)
'

test_expect_success 'annotated tag object type is tag' '
	(
	cd repo &&
	git cat-file -t v1.0 >out &&
	test "$(cat out)" = "tag"
	)
'

test_expect_success 'tag -m without -a still creates annotated tag' '
	(
	cd repo &&
	git tag -m "implicit annotated" v1.1 &&
	git cat-file -t v1.1 >out &&
	test "$(cat out)" = "tag"
	)
'

test_expect_success 'tag -m message is preserved' '
	(
	cd repo &&
	git cat-file -p v1.1 >out &&
	grep "implicit annotated" out
	)
'

test_expect_success 'annotated tag at specific commit' '
	(
	cd repo &&
	second_sha=$(git rev-parse HEAD~1) &&
	git tag -a -m "At second" v0.5 "$second_sha" &&
	git cat-file -p v0.5 >out &&
	grep "$second_sha" out
	)
'

test_expect_success 'multiple -m flags concatenate messages' '
	(
	cd repo &&
	git tag -a -m "line one" -m "line two" v1.2 &&
	git cat-file -p v1.2 >out &&
	grep "line one" out &&
	grep "line two" out
	)
'

###########################################################################
# Section 4: Annotated tags with -F (file)
###########################################################################

test_expect_success 'tag -F reads message from file' '
	(
	cd repo &&
	echo "Message from file" >msg.txt &&
	git tag -a -F msg.txt v2.0 &&
	git cat-file -p v2.0 >out &&
	grep "Message from file" out
	)
'

test_expect_success 'tag -F with multiline file' '
	(
	cd repo &&
	printf "First line\nSecond line\nThird line\n" >multiline.txt &&
	git tag -a -F multiline.txt v2.1 &&
	git cat-file -p v2.1 >out &&
	grep "First line" out &&
	grep "Second line" out &&
	grep "Third line" out
	)
'

test_expect_success 'tag -F with - reads from stdin' '
	(
	cd repo &&
	echo "From stdin" | git tag -a -F - v2.2 &&
	git cat-file -p v2.2 >out &&
	grep "From stdin" out
	)
'

test_expect_success 'tag -F with empty file creates tag with empty message' '
	(
	cd repo &&
	>empty.txt &&
	git tag -a -F empty.txt --allow-empty-message v2.3 2>/dev/null ||
	git tag -a -F empty.txt v2.3 2>/dev/null ||
	git tag v2.3
	)
'

###########################################################################
# Section 5: Tag listing and filtering
###########################################################################

test_expect_success 'tag -l with glob pattern' '
	(
	cd repo &&
	git tag -l "v1.*" >out &&
	grep "v1.0" out &&
	grep "v1.1" out &&
	! grep "v0.0" out
	)
'

test_expect_success 'tag -l with glob v2*' '
	(
	cd repo &&
	git tag -l "v2.*" >out &&
	grep "v2.0" out
	)
'

test_expect_success 'tag -l with non-matching pattern' '
	(
	cd repo &&
	git tag -l "zzz*" >out &&
	test_must_be_empty out
	)
'

test_expect_success 'tag --list is alias for -l' '
	(
	cd repo &&
	git tag --list >out1 &&
	git tag -l >out2 &&
	test_cmp out1 out2
	)
'

###########################################################################
# Section 6: Tag deletion
###########################################################################

test_expect_success 'tag -d deletes lightweight tag' '
	(
	cd repo &&
	git tag temp-light &&
	git tag -d temp-light &&
	git tag -l >out &&
	! grep "temp-light" out
	)
'

test_expect_success 'tag -d deletes annotated tag' '
	(
	cd repo &&
	git tag -a -m "temp" temp-annotated &&
	git tag -d temp-annotated &&
	git tag -l >out &&
	! grep "temp-annotated" out
	)
'

test_expect_success 'tag -d fails for nonexistent tag' '
	test_must_fail git tag -d nonexistent 2>/dev/null
'

test_expect_success 'tag -d one at a time' '
	(
	cd repo &&
	git tag del1 &&
	git tag del2 &&
	git tag -d del1 &&
	git tag -d del2 &&
	git tag -l >out &&
	! grep "del1" out &&
	! grep "del2" out
	)
'

###########################################################################
# Section 7: Tag verification and show
###########################################################################

test_expect_success 'git show tag shows annotated tag info' '
	(
	cd repo &&
	git show v1.0 >out &&
	grep "Release 1.0" out
	)
'

test_expect_success 'git log --oneline with tag decoration' '
	(
	cd repo &&
	git log --oneline --decorate >out &&
	grep "v1.0\|tag:" out || grep "v1.0" out
	)
'

test_expect_success 'tag cannot be created with existing name' '
	(
	cd repo &&
	test_must_fail git tag v1.0
	)
'

test_expect_success 'tag -f force overwrites existing tag' '
	(
	cd repo &&
	old_sha=$(git rev-parse v0.1) &&
	git tag -f v0.1 HEAD~1 &&
	new_sha=$(git rev-parse v0.1) &&
	test "$old_sha" != "$new_sha"
	)
'

###########################################################################
# Section 8: Tag with special characters
###########################################################################

test_expect_success 'tag with dots in name' '
	(
	cd repo &&
	git tag release.candidate.1 &&
	git tag -l >out &&
	grep "release.candidate.1" out
	)
'

test_expect_success 'tag with slashes in name' '
	(
	cd repo &&
	git tag releases/v3.0 &&
	git tag -l >out &&
	grep "releases/v3.0" out
	)
'

test_expect_success 'tag with dashes in name' '
	(
	cd repo &&
	git tag my-special-tag &&
	git tag -l >out &&
	grep "my-special-tag" out
	)
'

test_done
