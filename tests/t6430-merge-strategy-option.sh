#!/bin/sh

test_description='merge -X ours / -X theirs strategy options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: conflicting branches' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "test@example.com" &&
	echo base >file.txt &&
	git add file.txt &&
	git commit -m "base" &&
	git branch feature &&
	echo "ours line" >file.txt &&
	git add file.txt &&
	git commit -m "ours change" &&
	git checkout feature &&
	echo "theirs line" >file.txt &&
	git add file.txt &&
	git commit -m "theirs change" &&
	git checkout master
	)
'

test_expect_success 'merge -X ours resolves conflict with our version' '
	(
	cd repo &&
	git merge -X ours feature -m "merge-ours" &&
	echo "ours line" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'reset and merge -X theirs resolves with their version' '
	(
	cd repo &&
	git reset --hard HEAD~1 &&
	git merge -X theirs feature -m "merge-theirs" &&
	echo "theirs line" >expect &&
	test_cmp expect file.txt
	)
'

test_expect_success 'setup: delete/modify conflict' '
	(
	cd repo &&
	git reset --hard HEAD~1 &&
	git branch -D feature &&
	git branch feature &&
	git rm file.txt &&
	git commit -m "delete file" &&
	git checkout feature &&
	echo "modified" >file.txt &&
	git add file.txt &&
	git commit -m "modify file" &&
	git checkout master
	)
'

test_expect_success '-X ours keeps deletion in delete/modify conflict' '
	(
	cd repo &&
	git merge -X ours feature -m "del-mod-ours" &&
	! test -f file.txt
	)
'

test_expect_success 'reset and -X theirs keeps modification in delete/modify' '
	(
	cd repo &&
	git reset --hard HEAD~1 &&
	git merge -X theirs feature -m "del-mod-theirs" &&
	echo "modified" >expect &&
	test_cmp expect file.txt
	)
'

test_done
