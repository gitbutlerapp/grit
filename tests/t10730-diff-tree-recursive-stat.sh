#!/bin/sh
# Test grit diff-tree with -r (recursive), --stat, --name-only,
# --name-status, and -p options across various tree structures.

test_description='grit diff-tree recursive and stat options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: repo with flat files and subdirectories' '
	(
	grit init dt-repo &&
	cd dt-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "root file" >root.txt &&
	mkdir -p dir/sub &&
	echo "mid file" >dir/mid.txt &&
	echo "deep file" >dir/sub/deep.txt &&
	grit add root.txt dir &&
	test_tick &&
	grit commit -m "initial: flat and nested files"
	)
'

test_expect_success 'setup: second commit adds more files' '
	(
	cd dt-repo &&
	echo "new root" >root2.txt &&
	echo "new deep" >dir/sub/deep2.txt &&
	grit add root2.txt dir/sub/deep2.txt &&
	test_tick &&
	grit commit -m "add root2 and deep2"
	)
'

# --- basic diff-tree ---

test_expect_success 'diff-tree shows raw output between commits' '
	(
	cd dt-repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	grep "root2.txt" actual
	)
'

test_expect_success 'diff-tree shows tree entry for dir without -r' '
	(
	cd dt-repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	grep "dir" actual
	)
'

test_expect_success 'diff-tree without -r does not show deep file' '
	(
	cd dt-repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	! grep "deep2.txt" actual
	)
'

# --- recursive diff-tree ---

test_expect_success 'diff-tree -r shows deep file' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "dir/sub/deep2.txt" actual
	)
'

test_expect_success 'diff-tree -r shows root file' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "root2.txt" actual
	)
'

test_expect_success 'diff-tree -r shows A status for new files' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "A" actual
	)
'

test_expect_success 'diff-tree -r does not show unchanged files' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	! grep "root.txt" actual &&
	! grep "mid.txt" actual
	)
'

# --- diff-tree --stat ---

test_expect_success 'diff-tree --stat shows stat summary' '
	(
	cd dt-repo &&
	grit diff-tree --stat HEAD~1 HEAD >actual &&
	grep "root2.txt" actual &&
	grep "insertion" actual
	)
'

test_expect_success 'diff-tree --stat shows file count in summary' '
	(
	cd dt-repo &&
	grit diff-tree --stat HEAD~1 HEAD >actual &&
	grep "2 files changed" actual
	)
'

test_expect_success 'diff-tree --stat shows deep file via recursive stat' '
	(
	cd dt-repo &&
	grit diff-tree -r --stat HEAD~1 HEAD >actual &&
	grep "dir/sub/deep2.txt" actual
	)
'

# --- diff-tree --name-only ---

test_expect_success 'diff-tree --name-only shows file names' '
	(
	cd dt-repo &&
	grit diff-tree --name-only HEAD~1 HEAD >actual &&
	grep "root2.txt" actual
	)
'

test_expect_success 'diff-tree --name-only shows dir entry without -r' '
	(
	cd dt-repo &&
	grit diff-tree --name-only HEAD~1 HEAD >actual &&
	grep "dir" actual
	)
'

test_expect_success 'diff-tree --name-only has no stat info' '
	(
	cd dt-repo &&
	grit diff-tree --name-only HEAD~1 HEAD >actual &&
	! grep "insertion" actual &&
	! grep "changed" actual
	)
'

# --- diff-tree --name-status ---

test_expect_success 'diff-tree --name-status shows A for added' '
	(
	cd dt-repo &&
	grit diff-tree --name-status HEAD~1 HEAD >actual &&
	grep "^A" actual
	)
'

test_expect_success 'diff-tree --name-status shows both entries' '
	(
	cd dt-repo &&
	grit diff-tree --name-status HEAD~1 HEAD >actual &&
	grep "root2.txt" actual &&
	grep "dir" actual
	)
'

# --- modifications ---

test_expect_success 'setup: modify existing files' '
	(
	cd dt-repo &&
	echo "modified root" >root.txt &&
	echo "modified deep" >dir/sub/deep.txt &&
	grit add root.txt dir/sub/deep.txt &&
	test_tick &&
	grit commit -m "modify root and deep"
	)
'

test_expect_success 'diff-tree -r shows modified files' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "root.txt" actual &&
	grep "dir/sub/deep.txt" actual
	)
'

test_expect_success 'diff-tree --name-status shows M for modified' '
	(
	cd dt-repo &&
	grit diff-tree --name-status HEAD~1 HEAD >actual &&
	grep "^M" actual
	)
'

test_expect_success 'diff-tree --stat shows modification stats' '
	(
	cd dt-repo &&
	grit diff-tree --stat HEAD~1 HEAD >actual &&
	grep "2 files changed" actual
	)
'

# --- deletions ---

test_expect_success 'setup: delete a file' '
	(
	cd dt-repo &&
	grit rm root2.txt &&
	test_tick &&
	grit commit -m "delete root2"
	)
'

test_expect_success 'diff-tree --name-status shows D for deleted' '
	(
	cd dt-repo &&
	grit diff-tree --name-status HEAD~1 HEAD >actual &&
	grep "^D" actual &&
	grep "root2.txt" actual
	)
'

test_expect_success 'diff-tree --stat shows deletion count' '
	(
	cd dt-repo &&
	grit diff-tree --stat HEAD~1 HEAD >actual &&
	grep "deletion" actual
	)
'

# --- diff-tree -p (patch output) ---

test_expect_success 'diff-tree -p shows patch output' '
	(
	cd dt-repo &&
	grit diff-tree -p HEAD~1 HEAD >actual &&
	grep "^diff --git" actual
	)
'

test_expect_success 'diff-tree -p shows deleted file content' '
	(
	cd dt-repo &&
	grit diff-tree -p HEAD~1 HEAD >actual &&
	grep "^-" actual
	)
'

# --- comparing non-adjacent commits ---

test_expect_success 'diff-tree -r between first and last commit' '
	(
	cd dt-repo &&
	first=$(grit log --oneline --reverse | head -1 | cut -d" " -f1) &&
	grit diff-tree -r "$first" HEAD >actual &&
	grep "dir/sub/deep.txt" actual
	)
'

test_expect_success 'diff-tree --stat between non-adjacent commits' '
	(
	cd dt-repo &&
	first=$(grit log --oneline --reverse | head -1 | cut -d" " -f1) &&
	grit diff-tree --stat "$first" HEAD >actual &&
	grep "files changed" actual
	)
'

# --- same commit comparison ---

test_expect_success 'diff-tree between same commit is empty' '
	(
	cd dt-repo &&
	grit diff-tree HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff-tree -r between same commit is empty' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD HEAD >actual &&
	test_must_be_empty actual
	)
'

# --- tree objects directly ---

test_expect_success 'diff-tree works with tree objects' '
	(
	cd dt-repo &&
	t1=$(grit rev-parse HEAD~1^{tree}) &&
	t2=$(grit rev-parse HEAD^{tree}) &&
	grit diff-tree "$t1" "$t2" >actual &&
	test -s actual
	)
'

# --- multiple levels of nesting ---

test_expect_success 'setup: deeply nested structure' '
	(
	cd dt-repo &&
	mkdir -p a/b/c/d &&
	echo "very deep" >a/b/c/d/leaf.txt &&
	grit add a &&
	test_tick &&
	grit commit -m "add deeply nested file"
	)
'

test_expect_success 'diff-tree -r shows very deep file' '
	(
	cd dt-repo &&
	grit diff-tree -r HEAD~1 HEAD >actual &&
	grep "a/b/c/d/leaf.txt" actual
	)
'

test_expect_success 'diff-tree -r --name-only shows deep path' '
	(
	cd dt-repo &&
	grit diff-tree -r --name-only HEAD~1 HEAD >actual &&
	grep "a/b/c/d/leaf.txt" actual
	)
'

test_expect_success 'diff-tree -r --stat shows deep path' '
	(
	cd dt-repo &&
	grit diff-tree -r --stat HEAD~1 HEAD >actual &&
	grep "a/b/c/d/leaf.txt" actual
	)
'

test_expect_success 'diff-tree -r --name-status A for deep file' '
	(
	cd dt-repo &&
	grit diff-tree -r --name-status HEAD~1 HEAD >actual &&
	grep "^A" actual &&
	grep "a/b/c/d/leaf.txt" actual
	)
'

test_expect_success 'diff-tree without -r shows tree for a/' '
	(
	cd dt-repo &&
	grit diff-tree HEAD~1 HEAD >actual &&
	grep "a" actual
	)
'

test_done
