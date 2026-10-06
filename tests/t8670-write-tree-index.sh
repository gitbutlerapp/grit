#!/bin/sh
# Tests for write-tree with complex index states.

test_description='write-tree with complex index states'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ──────────────────────────────────────────────────────────────────

test_expect_success 'setup: repository with files' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	echo "world" >file2.txt &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deeper" >sub/deep/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

# ── Basic write-tree ───────────────────────────────────────────────────────

test_expect_success 'write-tree produces a valid OID' '
	(
	cd repo &&
	git write-tree >tree_oid &&
	grep -qE "^[0-9a-f]{40}$" tree_oid
	)
'

test_expect_success 'write-tree output is a tree object' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	type=$(git cat-file -t "$tree") &&
	test "$type" = "tree"
	)
'

test_expect_success 'write-tree matches HEAD tree when index is clean' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	head_tree=$(git rev-parse HEAD^{tree}) &&
	test "$tree" = "$head_tree"
	)
'

test_expect_success 'write-tree is deterministic' '
	(
	cd repo &&
	git write-tree >out1 &&
	git write-tree >out2 &&
	test_cmp out1 out2
	)
'

# ── write-tree after staging changes ───────────────────────────────────────

test_expect_success 'write-tree reflects staged changes' '
	(
	cd repo &&
	head_tree=$(git rev-parse HEAD^{tree}) &&
	echo "modified" >file.txt &&
	"$REAL_GIT" add file.txt &&
	tree=$(git write-tree) &&
	test "$tree" != "$head_tree"
	)
'

test_expect_success 'write-tree tree contains updated blob' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- file.txt >entry &&
	blob_oid=$(awk "{print \$3}" entry) &&
	content=$(git cat-file -p "$blob_oid") &&
	test "$content" = "modified"
	)
'

test_expect_success 'write-tree preserves unmodified entries' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- file2.txt >entry &&
	blob_oid=$(awk "{print \$3}" entry) &&
	content=$(git cat-file -p "$blob_oid") &&
	test "$content" = "world"
	)
'

# ── write-tree with new files ─────────────────────────────────────────────

test_expect_success 'write-tree includes newly staged files' '
	(
	cd repo &&
	echo "brand new" >new.txt &&
	"$REAL_GIT" add new.txt &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- new.txt >entry &&
	test_line_count -eq 1 entry
	)
'

test_expect_success 'write-tree new file has correct content' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- new.txt >entry &&
	blob_oid=$(awk "{print \$3}" entry) &&
	content=$(git cat-file -p "$blob_oid") &&
	test "$content" = "brand new"
	)
'

# ── write-tree with deleted files ──────────────────────────────────────────

test_expect_success 'write-tree excludes removed files' '
	(
	cd repo &&
	"$REAL_GIT" rm -f file2.txt &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >entries &&
	! grep "file2.txt" entries
	)
'

# ── write-tree with subdirectories ─────────────────────────────────────────

test_expect_success 'write-tree preserves subdirectory structure' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- sub >entry &&
	grep "040000 tree" entry
	)
'

test_expect_success 'write-tree subtree contains correct files' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree -r "$tree" -- sub >entries &&
	grep "sub/nested.txt" entries &&
	grep "sub/deep/file.txt" entries
	)
'

test_expect_success 'write-tree with new file in subdirectory' '
	(
	cd repo &&
	echo "new nested" >sub/new.txt &&
	"$REAL_GIT" add sub/new.txt &&
	tree=$(git write-tree) &&
	git ls-tree -r "$tree" -- sub >entries &&
	grep "sub/new.txt" entries
	)
'

# ── write-tree with executable file ────────────────────────────────────────

test_expect_success 'write-tree preserves executable mode' '
	(
	cd repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	"$REAL_GIT" add script.sh &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" -- script.sh >entry &&
	grep "100755" entry
	)
'

test_expect_success 'write-tree with multiple new subdirectory files' '
	(
	cd repo &&
	mkdir -p other &&
	echo "a" >other/a.txt &&
	echo "b" >other/b.txt &&
	"$REAL_GIT" add other/ &&
	tree=$(git write-tree) &&
	git ls-tree -r "$tree" -- other >entries &&
	test_line_count -eq 2 entries
	)
'

test_expect_success 'write-tree subtree OID is consistent with ls-tree' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	sub_oid=$(git ls-tree "$tree" -- sub | awk "{print \$3}") &&
	git ls-tree "$sub_oid" >sub_entries &&
	test_line_count -gt 0 sub_entries
	)
'

# ── write-tree ls-tree round-trip ──────────────────────────────────────────

test_expect_success 'ls-tree of write-tree matches mktree round-trip' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >ls_out &&
	rebuilt=$(git mktree <ls_out) &&
	test "$tree" = "$rebuilt"
	)
'

test_expect_success 'write-tree tree entries have valid modes' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >entries &&
	awk "{print \$1}" entries >modes &&
	while read mode; do
		case "$mode" in
		100644|100755|120000|040000) ;;
		*) echo "bad mode: $mode"; return 1 ;;
		esac
	done <modes
	)
'

test_expect_success 'write-tree tree entries have valid OIDs' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree -r "$tree" >entries &&
	awk "{print \$3}" entries >oids &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad OID: $oid"; return 1; }
	done <oids
	)
'

# ── write-tree after reset ────────────────────────────────────────────────

test_expect_success 'setup: commit current state and reset' '
	(
	cd repo &&
	"$REAL_GIT" add -A &&
	"$REAL_GIT" commit -m "intermediate" &&
	echo "post-reset" >file.txt &&
	"$REAL_GIT" add file.txt
	)
'

test_expect_success 'write-tree after staging reflects new content' '
	(
	cd repo &&
	head_tree=$(git rev-parse HEAD^{tree}) &&
	tree=$(git write-tree) &&
	test "$tree" != "$head_tree"
	)
'

# ── write-tree with many files ────────────────────────────────────────────

test_expect_success 'write-tree with many files in index' '
	(
	cd repo &&
	for i in $(seq 1 20); do
		echo "file $i" >"batch_$i.txt"
	done &&
	"$REAL_GIT" add batch_*.txt &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >entries &&
	count=$(grep "batch_" entries | wc -l) &&
	test "$count" -eq 20
	)
'

test_expect_success 'write-tree with many files produces valid tree' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git cat-file -t "$tree" >type &&
	test "$(cat type)" = "tree"
	)
'

# ── write-tree --missing-ok ───────────────────────────────────────────────

test_expect_success 'write-tree without --missing-ok on clean index succeeds' '
	(
	cd repo &&
	git write-tree >tree_oid &&
	grep -qE "^[0-9a-f]{40}$" tree_oid
	)
'

# ── Empty index ────────────────────────────────────────────────────────────

test_expect_success 'setup: empty index' '
	(
	cd repo &&
	"$REAL_GIT" add -A &&
	"$REAL_GIT" commit -m "save everything" &&
	"$REAL_GIT" rm -rf . &&
	"$REAL_GIT" add -A
	)
'

test_expect_success 'write-tree on empty index produces empty tree' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >entries &&
	test_must_be_empty entries
	)
'

test_done
