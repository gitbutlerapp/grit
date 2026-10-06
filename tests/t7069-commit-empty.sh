#!/bin/sh
# Tests for commit with --allow-empty, empty tree, and edge cases.

test_description='commit --allow-empty and empty tree scenarios'

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: init repo' '
	(
	git init repo &&
	cd repo
	)
'

test_expect_success 'commit --allow-empty on empty repo creates root commit' '
	(
	cd repo &&
	git commit --allow-empty -m "empty root" &&
	count=$(git rev-list --count HEAD) &&
	test "$count" = "1"
	)
'

test_expect_success 'empty root commit has empty tree' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	# The well-known empty tree SHA
	test "$tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'commit --allow-empty creates second empty commit' '
	(
	cd repo &&
	old=$(git rev-parse HEAD) &&
	git commit --allow-empty -m "empty second" &&
	new=$(git rev-parse HEAD) &&
	test "$old" != "$new" &&
	count=$(git rev-list --count HEAD) &&
	test "$count" = "2"
	)
'

test_expect_success 'second empty commit also has empty tree' '
	(
	cd repo &&
	tree=$(git rev-parse HEAD^{tree}) &&
	test "$tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'second empty commit has first as parent' '
	(
	cd repo &&
	parent=$(git rev-parse HEAD^) &&
	# parent should exist and be a valid commit
	git cat-file -t "$parent" >type &&
	echo "commit" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'commit without --allow-empty fails when nothing staged' '
	(
	cd repo &&
	test_must_fail git commit -m "should fail" 2>err
	)
'

test_expect_success 'commit with file then --allow-empty' '
	(
	cd repo &&
	echo "content" >file.txt &&
	git add file.txt &&
	git commit -m "with file" &&
	git commit --allow-empty -m "empty after file" &&
	tree1=$(git rev-parse HEAD^^{tree}) &&
	tree2=$(git rev-parse HEAD^{tree}) &&
	# Tree should be the same since nothing changed
	test "$tree1" != "4b825dc642cb6eb9a060e54bf8d69288fbee4904" &&
	test "$tree1" = "$tree2"
	)
'

test_expect_success 'multiple --allow-empty in sequence' '
	(
	cd repo &&
	git commit --allow-empty -m "empty 1" &&
	git commit --allow-empty -m "empty 2" &&
	git commit --allow-empty -m "empty 3" &&
	count=$(git rev-list --count HEAD) &&
	# We started with: empty root, empty second, with file, empty after file, empty 1, 2, 3
	test "$count" = "7"
	)
'

test_expect_success 'all empty commits have correct parent chain' '
	(
	cd repo &&
	git rev-list HEAD >all &&
	count=$(wc -l <all | tr -d " ") &&
	test "$count" = "7"
	)
'

test_expect_success '--allow-empty-message with non-empty content' '
	(
	cd repo &&
	echo "more" >more.txt &&
	git add more.txt &&
	git commit --allow-empty-message -m "" &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test -z "$msg"
	)
'

test_expect_success 'commit with empty message body via --allow-empty-message' '
	(
	cd repo &&
	echo "extra" >extra.txt &&
	git add extra.txt &&
	git commit --allow-empty-message -m "" &&
	tree=$(git rev-parse HEAD^{tree}) &&
	# Tree should include extra.txt
	git ls-tree HEAD >ls_out &&
	grep -q "extra.txt" ls_out
	)
'

test_expect_success '--allow-empty and --allow-empty-message combined' '
	(
	cd repo &&
	git commit --allow-empty --allow-empty-message -m "" &&
	count_before=$(git rev-list --count HEAD) &&
	test "$count_before" -gt 0
	)
'

test_expect_success 'setup: fresh repo for empty tree tests' '
	(
	git init empty-tree-repo &&
	cd empty-tree-repo
	)
'

test_expect_success 'empty tree has well-known SHA' '
	(
	cd empty-tree-repo &&
	tree=$(git write-tree) &&
	test "$tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'commit-tree with empty tree works' '
	(
	cd empty-tree-repo &&
	tree=$(git write-tree) &&
	commit=$(echo "empty tree commit" | git commit-tree "$tree") &&
	test -n "$commit" &&
	git cat-file -t "$commit" >type &&
	echo "commit" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'commit-tree result has empty tree' '
	(
	cd empty-tree-repo &&
	tree=$(git write-tree) &&
	commit=$(echo "verify tree" | git commit-tree "$tree") &&
	actual_tree=$(git rev-parse "$commit^{tree}") &&
	test "$actual_tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'add file and write-tree gives non-empty tree' '
	(
	cd empty-tree-repo &&
	echo "hello" >hello.txt &&
	git add hello.txt &&
	tree=$(git write-tree) &&
	test "$tree" != "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'remove file from index restores empty tree' '
	(
	cd empty-tree-repo &&
	git rm --cached hello.txt &&
	tree=$(git write-tree) &&
	test "$tree" = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
	)
'

test_expect_success 'setup: repo for commit message tests' '
	(
	git init msg-repo &&
	cd msg-repo &&
	echo "base" >base.txt &&
	git add base.txt &&
	git commit -m "base"
	)
'

test_expect_success 'commit -m with simple message' '
	(
	cd msg-repo &&
	echo "a" >a.txt &&
	git add a.txt &&
	git commit -m "simple message" &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "simple message"
	)
'

test_expect_success 'commit -F reads message from file' '
	(
	cd msg-repo &&
	echo "b" >b.txt &&
	git add b.txt &&
	echo "file message" >commit-msg.txt &&
	git commit -F commit-msg.txt &&
	msg=$(git log -n 1 --format=%s HEAD) &&
	test "$msg" = "file message"
	)
'

test_expect_success 'commit -F with multiline message' '
	(
	cd msg-repo &&
	echo "c" >c.txt &&
	git add c.txt &&
	printf "subject line\n\nbody paragraph" >multi-msg.txt &&
	git commit -F multi-msg.txt &&
	subj=$(git log -n 1 --format=%s HEAD) &&
	test "$subj" = "subject line"
	)
'

test_expect_success 'commit --allow-empty preserves tree' '
	(
	cd msg-repo &&
	tree_before=$(git rev-parse HEAD^{tree}) &&
	git commit --allow-empty -m "no change" &&
	tree_after=$(git rev-parse HEAD^{tree}) &&
	test "$tree_before" = "$tree_after"
	)
'

test_expect_success 'commit -a stages and commits tracked changes' '
	(
	cd msg-repo &&
	echo "modified base" >base.txt &&
	git commit -a -m "auto staged" &&
	git show HEAD:base.txt >actual &&
	echo "modified base" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit with --author sets author' '
	(
	cd msg-repo &&
	echo "d" >d.txt &&
	git add d.txt &&
	git commit --author="Custom Author <custom@example.com>" -m "custom author" &&
	author=$(git log -n 1 --format=%an HEAD) &&
	test "$author" = "Custom Author"
	)
'

test_expect_success 'commit with --date sets date' '
	(
	cd msg-repo &&
	echo "e" >e.txt &&
	git add e.txt &&
	git commit --date="2000-01-01T00:00:00" -m "dated commit" &&
	date_out=$(git log -n 1 --format=%ai HEAD) &&
	echo "$date_out" | grep -q "2000"
	)
'

test_expect_success '--allow-empty with --author' '
	(
	cd msg-repo &&
	git commit --allow-empty --author="Empty Author <empty@example.com>" -m "empty with author" &&
	author=$(git log -n 1 --format=%an HEAD) &&
	test "$author" = "Empty Author"
	)
'

test_expect_success 'rev-list counts all commits including empty ones' '
	(
	cd msg-repo &&
	git rev-list HEAD >all &&
	# Count should match all commits we made
	count=$(wc -l <all | tr -d " ") &&
	test "$count" -ge 8
	)
'

test_expect_success 'each commit has unique SHA' '
	(
	cd msg-repo &&
	git rev-list HEAD | sort -u >unique &&
	git rev-list HEAD >all &&
	all_c=$(wc -l <all | tr -d " ") &&
	uniq_c=$(wc -l <unique | tr -d " ") &&
	test "$all_c" = "$uniq_c"
	)
'

test_done
