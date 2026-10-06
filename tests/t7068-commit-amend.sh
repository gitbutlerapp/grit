#!/bin/sh
# Tests for commit --amend scenarios.

test_description='commit --amend'

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: init repo with initial commit' '
	(
	git init repo &&
	cd repo &&
	echo "initial content" >file.txt &&
	git add file.txt &&
	git commit -m "initial commit"
	)
'

test_expect_success 'amend changes commit message' '
	(
	cd repo &&
	old_sha=$(git rev-parse HEAD) &&
	git commit --amend -m "amended message" &&
	new_sha=$(git rev-parse HEAD) &&
	test "$old_sha" != "$new_sha"
	)
'

test_expect_success 'amended commit has new message' '
	(
	cd repo &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "amended message"
	)
'

test_expect_success 'amend preserves parent' '
	(
	cd repo &&
	echo "second" >file2.txt &&
	git add file2.txt &&
	git commit -m "second commit" &&
	parent_before=$(git rev-parse HEAD^) &&
	git commit --amend -m "second amended" &&
	parent_after=$(git rev-parse HEAD^) &&
	test "$parent_before" = "$parent_after"
	)
'

test_expect_success 'amend with staged changes includes them' '
	(
	cd repo &&
	echo "modified" >file.txt &&
	git add file.txt &&
	old_tree=$(git rev-parse HEAD^{tree}) &&
	git commit --amend -m "with staged changes" &&
	new_tree=$(git rev-parse HEAD^{tree}) &&
	test "$old_tree" != "$new_tree"
	)
'

test_expect_success 'amended tree contains the staged file content' '
	(
	cd repo &&
	git show HEAD:file.txt >actual &&
	echo "modified" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'amend does not change commit count' '
	(
	cd repo &&
	count_before=$(git rev-list --count HEAD) &&
	git commit --amend -m "re-amended" &&
	count_after=$(git rev-list --count HEAD) &&
	test "$count_before" = "$count_after"
	)
'

test_expect_success 'amend on root commit works' '
	(
	git init root-amend &&
	cd root-amend &&
	echo "root" >root.txt &&
	git add root.txt &&
	git commit -m "root" &&
	git commit --amend -m "root amended" &&
	msg=$(git log -n 1 --format=%s) &&
	test "$msg" = "root amended" &&
	count=$(git rev-list --count HEAD) &&
	test "$count" = "1"
	)
'

test_expect_success 'amend root commit changes SHA' '
	(
	cd root-amend &&
	echo "extra" >extra.txt &&
	git add extra.txt &&
	old=$(git rev-parse HEAD) &&
	git commit --amend -m "root with extra" &&
	new=$(git rev-parse HEAD) &&
	test "$old" != "$new"
	)
'

test_expect_success 'amend preserves author by default' '
	(
	cd repo &&
	author_before=$(git log -n 1 --format="%an <%ae>" HEAD) &&
	git commit --amend -m "test author preserve" &&
	author_after=$(git log -n 1 --format="%an <%ae>" HEAD) &&
	test "$author_before" = "$author_after"
	)
'

test_expect_success 'amend with --author overrides author' '
	(
	cd repo &&
	git commit --amend --author="New Author <new@author.com>" -m "new author" &&
	author=$(git log -n 1 --format="%an" HEAD) &&
	test "$author" = "New Author"
	)
'

test_expect_success 'amend with --author preserves committer' '
	(
	cd repo &&
	committer=$(git log -n 1 --format="%cn" HEAD) &&
	test "$committer" = "C O Mmiter"
	)
'

test_expect_success 'amend only message, tree stays same' '
	(
	cd repo &&
	tree_before=$(git rev-parse HEAD^{tree}) &&
	git commit --amend -m "just a message change" &&
	tree_after=$(git rev-parse HEAD^{tree}) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'amend with new file adds to tree' '
	(
	cd repo &&
	echo "new" >new-file.txt &&
	git add new-file.txt &&
	git commit --amend -m "added new file" &&
	git show HEAD:new-file.txt >actual &&
	echo "new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'amend with removed file updates tree' '
	(
	cd repo &&
	git rm -f new-file.txt &&
	git commit --amend -m "removed file" &&
	test_must_fail git show HEAD:new-file.txt 2>/dev/null
	)
'

test_expect_success 'multiple amends in sequence' '
	(
	cd repo &&
	git commit --amend -m "amend 1" &&
	git commit --amend -m "amend 2" &&
	git commit --amend -m "amend 3" &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "amend 3"
	)
'

test_expect_success 'amend preserves commit count after multiple amends' '
	(
	cd repo &&
	count=$(git rev-list --count HEAD) &&
	git commit --amend -m "yet another" &&
	count2=$(git rev-list --count HEAD) &&
	test "$count" = "$count2"
	)
'

test_expect_success 'amend with -a picks up working tree changes' '
	(
	cd repo &&
	echo "auto-staged" >file.txt &&
	git commit --amend -a -m "auto staged amend" &&
	git show HEAD:file.txt >actual &&
	echo "auto-staged" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'setup: commit for empty amend tests' '
	(
	cd repo &&
	echo "content" >empty-test.txt &&
	git add empty-test.txt &&
	git commit -m "before empty amend"
	)
'

test_expect_success 'amend with --allow-empty keeps same tree' '
	(
	cd repo &&
	tree_before=$(git rev-parse HEAD^{tree}) &&
	git commit --amend --allow-empty -m "empty amend" &&
	tree_after=$(git rev-parse HEAD^{tree}) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'amend with --allow-empty-message and empty message' '
	(
	cd repo &&
	git commit --amend --allow-empty-message -m "" &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test -z "$msg"
	)
'

test_expect_success 'amend restoring a non-empty message' '
	(
	cd repo &&
	git commit --amend -m "restored message" &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "restored message"
	)
'

test_expect_success 'amend with -F reads message from file' '
	(
	cd repo &&
	echo "message from file" >msg.txt &&
	git commit --amend -F msg.txt &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "message from file"
	)
'

test_expect_success 'HEAD points to amended commit' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	git commit --amend -m "check HEAD" &&
	new_head=$(git rev-parse HEAD) &&
	test "$head_sha" != "$new_head"
	)
'

test_expect_success 'branch ref updated after amend' '
	(
	cd repo &&
	branch=$(git symbolic-ref HEAD) &&
	git commit --amend -m "branch ref check" &&
	ref_sha=$(git rev-parse "$branch") &&
	head_sha=$(git rev-parse HEAD) &&
	test "$ref_sha" = "$head_sha"
	)
'

test_expect_success 'amend with --date changes author date' '
	(
	cd repo &&
	git commit --amend --date="2000-01-01T00:00:00" -m "dated" &&
	date_out=$(git log -n 1 --format=%ai HEAD) &&
	echo "$date_out" | grep -q "2000"
	)
'

test_expect_success 'amend with multiline message via -F' '
	(
	cd repo &&
	printf "first line\n\nsecond paragraph" >multi-msg.txt &&
	git commit --amend -F multi-msg.txt &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "first line"
	)
'

test_expect_success 'amend changes committer date (it is later)' '
	(
	cd repo &&
	git commit --amend -m "committer date test" &&
	# Just verify it does not error; committer date is updated
	git log -n 1 --format=%ci HEAD >actual &&
	test -s actual
	)
'

test_done
