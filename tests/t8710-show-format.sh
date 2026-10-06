#!/bin/sh
# Tests for show --format with various placeholders on commits, tags, blobs.

test_description='show --format placeholders'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=committer@test.com
GIT_COMMITTER_NAME='Commit Person'
GIT_AUTHOR_NAME='Author Person'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup -----------------------------------------------------------------

test_expect_success 'setup repository with commits and tags' '
	(
	git init repo &&
	cd repo &&
	git config user.email "tagger@test.com" &&
	git config user.name "Tag Person" &&
	echo "file1 content" >file1.txt &&
	git add file1.txt &&
	test_tick &&
	git commit -m "first commit" &&
	echo "file2 content" >file2.txt &&
	git add file2.txt &&
	test_tick &&
	git commit -m "second commit with body

This is the body of the second commit.
It has multiple lines." &&
	git tag -a v1.0 -m "version 1.0 release" HEAD^1 &&
	git tag -a v2.0 -m "version 2.0 release

With a multi-line tag message body." HEAD &&
	git tag lightweight-tag HEAD
	)
'

# -- %H full hash -------------------------------------------------------------

test_expect_success 'show --format=%H gives full commit hash' '
	(
	cd repo &&
	expected=$(git rev-parse HEAD) &&
	actual=$(git show --format="%H" HEAD | head -1) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'show --format=%H on parent commit' '
	(
	cd repo &&
	expected=$(git rev-parse HEAD^1) &&
	actual=$(git show --format="%H" HEAD^1 | head -1) &&
	test "$expected" = "$actual"
	)
'

# -- %h abbreviated hash -------------------------------------------------------

test_expect_success 'show --format=%h gives abbreviated hash' '
	(
	cd repo &&
	full=$(git rev-parse HEAD) &&
	actual=$(git show --format="%h" HEAD | head -1) &&
	case "$full" in
	${actual}*) true ;;
	*) echo "abbreviated $actual is not prefix of $full"; false ;;
	esac
	)
'

test_expect_success 'show --format=%h is shorter than %H' '
	(
	cd repo &&
	full=$(git show --format="%H" HEAD | head -1) &&
	short=$(git show --format="%h" HEAD | head -1) &&
	full_len=${#full} &&
	short_len=${#short} &&
	test "$short_len" -lt "$full_len"
	)
'

# -- %T tree hash --------------------------------------------------------------

test_expect_success 'show --format=%T gives 40-char tree hash' '
	(
	cd repo &&
	actual=$(git show --format="%T" HEAD | head -1) &&
	len=${#actual} &&
	test "$len" -eq 40 &&
	echo "$actual" | grep -q "^[0-9a-f]*$"
	)
'

test_expect_success 'show --format=%t gives abbreviated tree hash' '
	(
	cd repo &&
	full_tree=$(git show --format="%T" HEAD | head -1) &&
	short_tree=$(git show --format="%t" HEAD | head -1) &&
	case "$full_tree" in
	${short_tree}*) true ;;
	*) false ;;
	esac
	)
'

# -- %P parent hash ------------------------------------------------------------

test_expect_success 'show --format=%P on commit with parent gives parent hash' '
	(
	cd repo &&
	expected=$(git rev-parse HEAD^1) &&
	actual=$(git show --format="%P" HEAD | head -1) &&
	test "$expected" = "$actual"
	)
'

test_expect_success 'show --format=%P on root commit gives empty' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	actual=$(git show --format="%P" "$first" | head -1) &&
	test -z "$actual"
	)
'

test_expect_success 'show --format=%p gives abbreviated parent hash' '
	(
	cd repo &&
	full_parent=$(git show --format="%P" HEAD | head -1) &&
	short_parent=$(git show --format="%p" HEAD | head -1) &&
	case "$full_parent" in
	${short_parent}*) true ;;
	*) false ;;
	esac
	)
'

# -- author fields -------------------------------------------------------------

test_expect_success 'show --format=%an gives author name' '
	(
	cd repo &&
	actual=$(git show --format="%an" HEAD | head -1) &&
	test "$actual" = "Author Person"
	)
'

test_expect_success 'show --format=%ae gives author email' '
	(
	cd repo &&
	actual=$(git show --format="%ae" HEAD | head -1) &&
	test "$actual" = "author@test.com"
	)
'

# -- committer fields ----------------------------------------------------------

test_expect_success 'show --format=%cn gives committer name' '
	(
	cd repo &&
	actual=$(git show --format="%cn" HEAD | head -1) &&
	test "$actual" = "Commit Person"
	)
'

test_expect_success 'show --format=%ce gives committer email' '
	(
	cd repo &&
	actual=$(git show --format="%ce" HEAD | head -1) &&
	test "$actual" = "committer@test.com"
	)
'

# -- subject and body ----------------------------------------------------------

test_expect_success 'show --format=%s gives subject line' '
	(
	cd repo &&
	actual=$(git show --format="%s" HEAD | head -1) &&
	test "$actual" = "second commit with body"
	)
'

test_expect_success 'show --format=%s on first commit' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	actual=$(git show --format="%s" "$first" | head -1) &&
	test "$actual" = "first commit"
	)
'

test_expect_success 'show --format=%b gives body text' '
	(
	cd repo &&
	actual=$(git show --format="%b" HEAD | head -1) &&
	echo "$actual" | grep -q "body of the second commit"
	)
'

test_expect_success 'show --format=%b on bodyless commit is empty' '
	(
	cd repo &&
	first=$(git rev-list HEAD | tail -1) &&
	actual=$(git show --format="%b" "$first" | head -1) &&
	test -z "$actual"
	)
'

# -- composite format strings -------------------------------------------------

test_expect_success 'show --format with multiple placeholders' '
	(
	cd repo &&
	result=$(git show --format="%H %s" HEAD | head -1) &&
	hash=$(git rev-parse HEAD) &&
	expected="$hash second commit with body" &&
	test "$result" = "$expected"
	)
'

test_expect_success 'show --format with literal prefix' '
	(
	cd repo &&
	result=$(git show --format="hash=%H" HEAD | head -1) &&
	hash=$(git rev-parse HEAD) &&
	expected="hash=$hash" &&
	test "$result" = "$expected"
	)
'

test_expect_success 'show --format with pipe separator' '
	(
	cd repo &&
	result=$(git show --format="%an|%ae" HEAD | head -1) &&
	test "$result" = "Author Person|author@test.com"
	)
'

test_expect_success 'show --format with %n gives newline' '
	(
	cd repo &&
	git show --format="line1%nline2" HEAD >out.txt &&
	head -2 out.txt >first_two.txt &&
	grep "^line1$" first_two.txt &&
	grep "^line2$" first_two.txt
	)
'

test_expect_success 'show --format with committer and author together' '
	(
	cd repo &&
	result=$(git show --format="%an <%ae> / %cn <%ce>" HEAD | head -1) &&
	test "$result" = "Author Person <author@test.com> / Commit Person <committer@test.com>"
	)
'

# -- tags ----------------------------------------------------------------------

test_expect_success 'show annotated tag displays tag info' '
	(
	cd repo &&
	git show v1.0 >out.txt &&
	grep "tag v1.0" out.txt &&
	grep "version 1.0 release" out.txt
	)
'

test_expect_success 'show annotated tag v2.0 has body' '
	(
	cd repo &&
	git show v2.0 >out.txt &&
	grep "tag v2.0" out.txt &&
	grep "version 2.0 release" out.txt
	)
'

test_expect_success 'show lightweight tag resolves to commit' '
	(
	cd repo &&
	tag_target=$(git rev-parse lightweight-tag) &&
	head_hash=$(git rev-parse HEAD) &&
	test "$tag_target" = "$head_hash"
	)
'

# -- blobs ---------------------------------------------------------------------

test_expect_success 'show blob displays content' '
	(
	cd repo &&
	blob=$(git hash-object file1.txt) &&
	git show "$blob" >out.txt &&
	echo "file1 content" >expected &&
	test_cmp expected out.txt
	)
'

test_expect_success 'show HEAD:file shows blob content' '
	(
	cd repo &&
	git show HEAD:file1.txt >out.txt &&
	echo "file1 content" >expected &&
	test_cmp expected out.txt
	)
'

# -- oneline -------------------------------------------------------------------

test_expect_success 'show --oneline gives short hash and subject' '
	(
	cd repo &&
	git show --oneline HEAD >out.txt &&
	head -1 out.txt >first.txt &&
	short=$(git show --format="%h" HEAD | head -1) &&
	grep "$short" first.txt &&
	grep "second commit with body" first.txt
	)
'

# -- quiet ---------------------------------------------------------------------

test_expect_success 'show --quiet suppresses diff output' '
	(
	cd repo &&
	git show --quiet HEAD >out.txt &&
	! grep "^diff " out.txt &&
	! grep "^@@" out.txt
	)
'

test_expect_success 'show --quiet still shows header' '
	(
	cd repo &&
	git show --quiet HEAD >out.txt &&
	grep "second commit with body" out.txt
	)
'

# -- unified context -----------------------------------------------------------

test_expect_success 'show -U0 reduces context lines' '
	(
	cd repo &&
	git show -U0 HEAD >out_u0.txt &&
	git show HEAD >out_default.txt &&
	u0_lines=$(wc -l <out_u0.txt) &&
	def_lines=$(wc -l <out_default.txt) &&
	test "$u0_lines" -le "$def_lines"
	)
'

test_done
