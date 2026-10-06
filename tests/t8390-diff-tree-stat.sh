#!/bin/sh
# Tests for diff-tree --stat with various scenarios: subdirectories, many files,
# large changes, tree OIDs, binary content, combined flags.

test_description='diff-tree --stat comprehensive'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

SYS_GIT=$(command -v git)

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with history' '
	(
	"$SYS_GIT" init repo &&
	cd repo &&
	"$SYS_GIT" config user.name "Test User" &&
	"$SYS_GIT" config user.email "test@example.com" &&
	echo "original line" >file1.txt &&
	echo "another file" >file2.txt &&
	echo "stay the same" >unchanged.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "initial"
	)
'

# ── Basic --stat output ─────────────────────────────────────────────────

test_expect_success 'setup: modify one file' '
	(
	cd repo &&
	echo "modified line" >file1.txt &&
	"$SYS_GIT" add file1.txt &&
	"$SYS_GIT" commit -m "modify file1"
	)
'

test_expect_success 'diff-tree --stat shows modified file' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "file1.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat does not show unchanged files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	! grep "unchanged.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat shows insertion/deletion counts' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep -E "[0-9]+ insertion" ../stat_out &&
	grep -E "[0-9]+ deletion" ../stat_out
	)
'

test_expect_success 'diff-tree --stat shows summary line with changed' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "changed" ../stat_out
	)
'

# ── Multiple files changed ──────────────────────────────────────────────

test_expect_success 'setup: modify multiple files' '
	(
	cd repo &&
	echo "new content for file1" >file1.txt &&
	echo "new content for file2" >file2.txt &&
	"$SYS_GIT" add . &&
	"$SYS_GIT" commit -m "modify both files"
	)
'

test_expect_success 'diff-tree --stat lists all changed files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "file1.txt" ../stat_out &&
	grep "file2.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat summary reflects file count' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "2 files changed" ../stat_out
	)
'

# ── File additions ──────────────────────────────────────────────────────

test_expect_success 'setup: add new files' '
	(
	cd repo &&
	echo "brand new" >newfile.txt &&
	echo "also new" >newfile2.txt &&
	"$SYS_GIT" add newfile.txt newfile2.txt &&
	"$SYS_GIT" commit -m "add new files"
	)
'

test_expect_success 'diff-tree --stat shows new files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "newfile.txt" ../stat_out &&
	grep "newfile2.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat shows insertions for new files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "insertion" ../stat_out
	)
'

# ── File deletions ──────────────────────────────────────────────────────

test_expect_success 'setup: delete files' '
	(
	cd repo &&
	"$SYS_GIT" rm newfile.txt newfile2.txt &&
	"$SYS_GIT" commit -m "remove new files"
	)
'

test_expect_success 'diff-tree --stat shows deleted files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "newfile.txt" ../stat_out &&
	grep "newfile2.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat shows deletions for removed files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "deletion" ../stat_out
	)
'

# ── Subdirectory changes ────────────────────────────────────────────────

test_expect_success 'setup: add files in subdirectories' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	echo "sub content" >sub/subfile.txt &&
	echo "deep content" >sub/deep/deepfile.txt &&
	"$SYS_GIT" add sub &&
	"$SYS_GIT" commit -m "add subdirectory files"
	)
'

test_expect_success 'diff-tree --stat shows collapsed subdirectory' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "sub" ../stat_out
	)
'

test_expect_success 'diff-tree --stat -r shows full subdirectory paths' '
	(
	cd repo &&
	git diff-tree --stat -r HEAD~1 HEAD >../stat_out &&
	grep "sub/subfile.txt" ../stat_out &&
	grep "sub/deep/deepfile.txt" ../stat_out
	)
'

test_expect_success 'setup: modify file in subdirectory' '
	(
	cd repo &&
	echo "updated sub content" >sub/subfile.txt &&
	"$SYS_GIT" add sub/subfile.txt &&
	"$SYS_GIT" commit -m "update sub file"
	)
'

test_expect_success 'diff-tree --stat -r shows modified subdirectory file' '
	(
	cd repo &&
	git diff-tree --stat -r HEAD~1 HEAD >../stat_out &&
	grep "sub/subfile.txt" ../stat_out &&
	! grep "sub/deep/deepfile.txt" ../stat_out
	)
'

# ── Single commit arg (diff against parent) ─────────────────────────────

test_expect_success 'diff-tree --stat with single commit diffs parent' '
	(
	cd repo &&
	git diff-tree --stat -r HEAD >../stat_out &&
	grep "sub/subfile.txt" ../stat_out
	)
'

# ── Same tree produces zero-change summary ───────────────────────────────

test_expect_success 'diff-tree --stat on same tree shows 0 files changed' '
	(
	cd repo &&
	git diff-tree --stat HEAD HEAD >../stat_out &&
	grep "0 files changed" ../stat_out
	)
'

# ── Large number of changes ─────────────────────────────────────────────

test_expect_success 'setup: add many files' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "content $i" >multi_$i.txt
	done &&
	"$SYS_GIT" add multi_*.txt &&
	"$SYS_GIT" commit -m "add 20 files"
	)
'

test_expect_success 'diff-tree --stat shows all 20 new files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	count=$(grep -c "multi_" ../stat_out) &&
	test "$count" = "20"
	)
'

test_expect_success 'diff-tree --stat summary for 20 files' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "20 files changed" ../stat_out
	)
'

# ── Larger content changes ──────────────────────────────────────────────

test_expect_success 'setup: create file with many lines' '
	(
	cd repo &&
	for i in $(seq 1 50); do
		echo "line $i"
	done >bigfile.txt &&
	"$SYS_GIT" add bigfile.txt &&
	"$SYS_GIT" commit -m "add bigfile"
	)
'

test_expect_success 'diff-tree --stat shows + chars for big addition' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "bigfile.txt" ../stat_out &&
	grep "+" ../stat_out
	)
'

test_expect_success 'setup: replace half the lines' '
	(
	cd repo &&
	for i in $(seq 1 50); do
		if test $((i % 2)) -eq 0; then
			echo "replaced $i"
		else
			echo "line $i"
		fi
	done >bigfile.txt &&
	"$SYS_GIT" add bigfile.txt &&
	"$SYS_GIT" commit -m "replace half of bigfile"
	)
'

test_expect_success 'diff-tree --stat shows both + and - for modification' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "bigfile.txt" ../stat_out &&
	grep "insertion" ../stat_out &&
	grep "deletion" ../stat_out
	)
'

# ── Tree OIDs directly ──────────────────────────────────────────────────

test_expect_success 'diff-tree --stat with tree OIDs' '
	(
	cd repo &&
	tree1=$("$SYS_GIT" rev-parse HEAD~1^{tree}) &&
	tree2=$("$SYS_GIT" rev-parse HEAD^{tree}) &&
	git diff-tree --stat "$tree1" "$tree2" >../stat_out &&
	grep "bigfile.txt" ../stat_out
	)
'

# ── Combined flags ──────────────────────────────────────────────────────

test_expect_success 'diff-tree --stat --no-commit-id suppresses commit line' '
	(
	cd repo &&
	git diff-tree --stat --no-commit-id HEAD >../stat_out &&
	! grep "^[0-9a-f]\{40\}$" ../stat_out &&
	grep "bigfile.txt" ../stat_out
	)
'

test_expect_success 'diff-tree --stat -r across multiple commits' '
	(
	cd repo &&
	git diff-tree --stat -r HEAD~5 HEAD >../stat_out &&
	test -s ../stat_out
	)
'

# ── Binary file ─────────────────────────────────────────────────────────

test_expect_success 'setup: add binary file' '
	(
	cd repo &&
	printf "\x00\x01\x02\x03\x04\x05" >binary.bin &&
	"$SYS_GIT" add binary.bin &&
	"$SYS_GIT" commit -m "add binary"
	)
'

test_expect_success 'diff-tree --stat shows binary file' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "binary.bin" ../stat_out
	)
'

# ── Single file addition vs modification ─────────────────────────────────

test_expect_success 'setup: add then modify a single file' '
	(
	cd repo &&
	echo "first version" >single.txt &&
	"$SYS_GIT" add single.txt &&
	"$SYS_GIT" commit -m "add single" &&
	echo "second version" >single.txt &&
	"$SYS_GIT" add single.txt &&
	"$SYS_GIT" commit -m "modify single"
	)
'

test_expect_success 'diff-tree --stat: addition shows file with + only' '
	(
	cd repo &&
	git diff-tree --stat HEAD~2 HEAD~1 >../stat_out &&
	grep "single.txt" ../stat_out &&
	grep "+" ../stat_out
	)
'

test_expect_success 'diff-tree --stat: modification shows +/-' '
	(
	cd repo &&
	git diff-tree --stat HEAD~1 HEAD >../stat_out &&
	grep "single.txt" ../stat_out
	)
'

test_done
