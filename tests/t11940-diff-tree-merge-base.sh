#!/bin/sh
test_description='diff-tree comparisons and merge-base operations'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with linear history' '
	grit init repo &&
	(cd repo &&
		git config user.email "t@t.com" &&
		git config user.name "T" &&
		echo "base" >file.txt &&
		grit add file.txt &&
		grit commit -m "c1" &&
		echo "second" >file2.txt &&
		grit add file2.txt &&
		grit commit -m "c2" &&
		echo "third" >file3.txt &&
		grit add file3.txt &&
		grit commit -m "c3"
	)
'

test_expect_success 'diff-tree -r between two commits' '
	(cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
	grep "A" actual &&
	grep "file3.txt" actual
'

test_expect_success 'diff-tree --name-only between two commits' '
	(cd repo && grit diff-tree --name-only -r HEAD~1 HEAD >../actual) &&
	echo "file3.txt" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree --name-status between two commits' '
	(cd repo && grit diff-tree --name-status -r HEAD~1 HEAD >../actual) &&
	printf "A\tfile3.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree --stat between two commits' '
	(cd repo && grit diff-tree --stat -r HEAD~1 HEAD >../actual) &&
	grep "file3.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'diff-tree -p between two commits shows patch' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
	grep "+third" actual &&
	grep "new file mode" actual
'

test_expect_success 'diff-tree -r with single commit compares to parent' '
	(cd repo && grit diff-tree -r HEAD >../actual) &&
	grep "file3.txt" actual
'

test_expect_success 'diff-tree comparing same commit is empty' '
	(cd repo && grit diff-tree -r HEAD HEAD >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'diff-tree across multiple commits' '
	(cd repo && grit diff-tree --name-only -r HEAD~2 HEAD >../actual) &&
	sort actual >actual_sorted &&
	printf "file2.txt\nfile3.txt\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff-tree --name-status across multiple commits' '
	(cd repo && grit diff-tree --name-status -r HEAD~2 HEAD >../actual) &&
	grep "A" actual &&
	grep "file2.txt" actual &&
	grep "file3.txt" actual
'

test_expect_success 'setup branches for merge-base tests' '
	(cd repo &&
		git checkout -b feature &&
		echo "feat1" >feat.txt &&
		grit add feat.txt &&
		grit commit -m "feature commit" &&
		git checkout main &&
		echo "main-only" >main.txt &&
		grit add main.txt &&
		grit commit -m "main commit"
	)
'

test_expect_success 'merge-base finds common ancestor' '
	(cd repo && grit merge-base main feature >../actual) &&
	(cd repo && grit rev-parse HEAD~1 >../expect) &&
	test_cmp expect actual
'

test_expect_success 'merge-base --is-ancestor for ancestor' '
	(cd repo &&
		BASE=$(grit merge-base main feature) &&
		grit merge-base --is-ancestor "$BASE" main
	)
'

test_expect_success 'merge-base --is-ancestor for non-ancestor fails' '
	(cd repo &&
		test_must_fail grit merge-base --is-ancestor main feature
	)
'

test_expect_success 'diff-tree between diverged branches' '
	(cd repo && grit diff-tree --name-only -r main feature >../actual) &&
	sort actual >actual_sorted &&
	printf "feat.txt\nmain.txt\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff-tree --name-status between diverged branches' '
	(cd repo && grit diff-tree --name-status -r main feature >../actual) &&
	grep "A.*feat.txt" actual &&
	grep "D.*main.txt" actual
'

test_expect_success 'diff-tree -p between diverged branches' '
	(cd repo && grit diff-tree -p main feature >../actual) &&
	grep "+feat1" actual &&
	grep "\-main-only" actual
'

test_expect_success 'diff-tree --stat between diverged branches' '
	(cd repo && grit diff-tree --stat -r main feature >../actual) &&
	grep "2 files changed" actual
'

test_expect_success 'setup deeper branch history' '
	(cd repo &&
		git checkout feature &&
		echo "feat2" >feat2.txt &&
		grit add feat2.txt &&
		grit commit -m "feature commit 2" &&
		echo "feat3" >feat3.txt &&
		grit add feat3.txt &&
		grit commit -m "feature commit 3"
	)
'

test_expect_success 'merge-base still finds original fork point' '
	(cd repo &&
		grit merge-base main feature >../actual_base &&
		grit rev-parse main~1 >../expect_base
	) &&
	test_cmp expect_base actual_base
'

test_expect_success 'diff-tree between main and deep feature' '
	(cd repo && grit diff-tree --name-only -r main feature >../actual) &&
	sort actual >actual_sorted &&
	printf "feat.txt\nfeat2.txt\nfeat3.txt\nmain.txt\n" >expect &&
	test_cmp expect actual_sorted
'

test_expect_success 'diff-tree -r with modified file across branches' '
	(cd repo &&
		git checkout feature &&
		echo "modified-on-feature" >file.txt &&
		grit add file.txt &&
		grit commit -m "modify file on feature" &&
		git checkout main
	) &&
	(cd repo && grit diff-tree --name-status -r main feature >../actual) &&
	grep "M.*file.txt" actual
'

test_expect_success 'setup second branch from same point' '
	(cd repo &&
		git checkout main &&
		git checkout -b branch2 &&
		echo "b2" >b2.txt &&
		grit add b2.txt &&
		grit commit -m "branch2 commit"
	)
'

test_expect_success 'merge-base between two non-main branches' '
	(cd repo && grit merge-base feature branch2 >../actual) &&
	test -s actual
'

test_expect_success 'diff-tree between two non-main branches' '
	(cd repo && grit diff-tree --name-only -r feature branch2 >../actual) &&
	grep "b2.txt" actual
'

test_expect_success 'diff-tree with deleted file between commits' '
	(cd repo &&
		git checkout main &&
		grit rm file3.txt &&
		grit commit -m "delete file3" &&
		grit diff-tree --name-status -r HEAD~1 HEAD >../actual
	) &&
	printf "D\tfile3.txt\n" >expect &&
	test_cmp expect actual
'

test_expect_success 'diff-tree -p shows deletion patch' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
	grep "deleted file mode" actual &&
	grep "\-third" actual
'

test_expect_success 'diff-tree with mode change between commits' '
	(cd repo &&
		chmod 755 file2.txt &&
		grit add file2.txt &&
		grit commit -m "chmod file2" &&
		grit diff-tree -r HEAD~1 HEAD >../actual
	) &&
	grep "100644" actual &&
	grep "100755" actual
'

test_expect_success 'diff-tree -p shows mode change patch' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../actual) &&
	grep "old mode 100644" actual &&
	grep "new mode 100755" actual
'

test_expect_success 'merge-base with HEAD reference' '
	(cd repo &&
		git checkout main &&
		grit merge-base HEAD branch2 >../actual
	) &&
	test -s actual
'

test_expect_success 'merge-base is-ancestor HEAD~1 HEAD' '
	(cd repo && grit merge-base --is-ancestor HEAD~1 HEAD)
'

test_expect_success 'merge-base is-ancestor HEAD HEAD (same commit)' '
	(cd repo && grit merge-base --is-ancestor HEAD HEAD)
'

test_expect_success 'diff-tree --stat for commit with multiple changes' '
	(cd repo &&
		echo "new1" >n1.txt &&
		echo "new2" >n2.txt &&
		echo "new3" >n3.txt &&
		grit add n1.txt n2.txt n3.txt &&
		grit commit -m "add three files" &&
		grit diff-tree --stat -r HEAD~1 HEAD >../actual
	) &&
	grep "3 files changed" actual
'

test_done
