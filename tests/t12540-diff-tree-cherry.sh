#!/bin/sh
# Tests for grit diff-tree (two-tree comparison) and grit cherry
# (finding commits not yet applied upstream).

test_description='grit diff-tree and cherry'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

# ================================================================
# Part 1: diff-tree
# ================================================================

test_expect_success 'setup: repo with several commits' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	echo "hello" >file.txt &&
	mkdir -p sub &&
	echo "nested" >sub/a.txt &&
	echo "nested2" >sub/b.txt &&
	grit add . &&
	grit commit -m "initial" &&
	echo "world" >>file.txt &&
	grit add file.txt &&
	grit commit -m "modify file.txt" &&
	echo "new" >new.txt &&
	grit add new.txt &&
	grit commit -m "add new.txt" &&
	"$REAL_GIT" rm sub/b.txt &&
	grit commit -m "delete sub/b.txt" &&
	echo "changed" >sub/a.txt &&
	grit add sub/a.txt &&
	grit commit -m "modify sub/a.txt"
	)
'

# ---- two-tree raw comparison ----

test_expect_success 'diff-tree -r: HEAD~1 HEAD shows changed file' '
	(cd repo && grit diff-tree -r HEAD~1 HEAD >../actual) &&
	grep "sub/a.txt" actual
'

test_expect_success 'diff-tree -r: HEAD~1 HEAD matches git' '
	(cd repo && grit diff-tree -r HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree -r HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree: HEAD~4 HEAD shows all changes' '
	(cd repo && grit diff-tree HEAD~4 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree HEAD~4 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree -r: HEAD~4 HEAD recurses into subtrees' '
	(cd repo && grit diff-tree -r HEAD~4 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree -r HEAD~4 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff-tree output formats ----

test_expect_success 'diff-tree --name-only: HEAD~2 HEAD' '
	(cd repo && grit diff-tree --name-only HEAD~2 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree --name-only HEAD~2 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree --name-status: shows M/A/D' '
	(cd repo && grit diff-tree --name-status HEAD~4 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree --name-status HEAD~4 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree -r --name-status: D for deleted file' '
	(cd repo && grit diff-tree -r --name-status HEAD~2 HEAD~1 >../actual) &&
	grep "^D" actual | grep "sub/b.txt"
'

test_expect_success 'diff-tree --name-status: A for added file' '
	(cd repo && grit diff-tree --name-status HEAD~3 HEAD~2 >../actual) &&
	grep "^A" actual | grep "new.txt"
'

test_expect_success 'diff-tree --stat: summary output' '
	(cd repo && grit diff-tree --stat HEAD~4 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree --stat HEAD~4 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree -p: patch output matches git' '
	(cd repo && grit diff-tree -p HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree -p HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree -p: multi-commit range matches git' '
	(cd repo && grit diff-tree -p HEAD~4 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree -p HEAD~4 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff-tree --root ----

test_expect_success 'diff-tree --root: root commit shows all files' '
	(cd repo && grit diff-tree --root -r HEAD~4 >../actual) &&
	grep "file.txt" actual &&
	grep "sub/a.txt" actual &&
	grep "sub/b.txt" actual
'

test_expect_success 'diff-tree --root: matches git' '
	(cd repo && grit diff-tree --root -r HEAD~4 >../grit_out &&
	 "$REAL_GIT" diff-tree --root -r HEAD~4 >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'diff-tree: without --root, root commit is empty' '
	(cd repo && grit diff-tree -r HEAD~4 >../actual) &&
	test_must_be_empty actual
'

# ---- diff-tree with path filter ----

test_expect_success 'diff-tree: path filter limits output' '
	(cd repo && grit diff-tree HEAD~4 HEAD file.txt >../actual) &&
	grep "file.txt" actual &&
	! grep "new.txt" actual
'

test_expect_success 'diff-tree: path filter matches git' '
	(cd repo && grit diff-tree HEAD~4 HEAD file.txt >../grit_out &&
	 "$REAL_GIT" diff-tree HEAD~4 HEAD file.txt >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff-tree same tree ----

test_expect_success 'diff-tree: same commit twice produces no output' '
	(cd repo && grit diff-tree HEAD HEAD >../actual) &&
	test_must_be_empty actual
'

# ================================================================
# Part 2: cherry
# ================================================================

test_expect_success 'setup: create branch for cherry tests' '
	(cd repo &&
	 "$REAL_GIT" checkout -b feature &&
	 echo "feature1" >feat1.txt &&
	 grit add feat1.txt &&
	 grit commit -m "feature commit 1" &&
	 echo "feature2" >feat2.txt &&
	 grit add feat2.txt &&
	 grit commit -m "feature commit 2" &&
	 echo "feature3" >feat3.txt &&
	 grit add feat3.txt &&
	 grit commit -m "feature commit 3")
'

test_expect_success 'cherry: lists unmerged commits with + prefix' '
	(cd repo && grit cherry main >../actual) &&
	grep "^+" actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" = "3"
'

test_expect_success 'cherry: matches git' '
	(cd repo && grit cherry main >../grit_out &&
	 "$REAL_GIT" cherry main >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'cherry -v: shows commit subjects' '
	(cd repo && grit cherry -v main >../actual) &&
	grep "feature commit 1" actual &&
	grep "feature commit 2" actual &&
	grep "feature commit 3" actual
'

test_expect_success 'cherry -v: matches git' '
	(cd repo && grit cherry -v main >../grit_out &&
	 "$REAL_GIT" cherry -v main >../git_out) &&
	test_cmp git_out grit_out
'

# ---- cherry after cherry-picking ----

test_expect_success 'setup: advance main so cherry-pick creates a new commit' '
	(cd repo &&
	 "$REAL_GIT" checkout main &&
	 echo "main-only" >main-only.txt &&
	 "$REAL_GIT" add main-only.txt &&
	 "$REAL_GIT" commit -m "main diverges")
'

test_expect_success 'setup: cherry-pick one commit to main' '
	(cd repo &&
	 feature_first=$("$REAL_GIT" rev-list main..feature | tail -1) &&
	 "$REAL_GIT" cherry-pick "$feature_first")
'

test_expect_success 'cherry: cherry-picked commit shows - prefix' '
	(cd repo &&
	 "$REAL_GIT" checkout feature &&
	 grit cherry main >../actual) &&
	grep "^-" actual &&
	grep "^+" actual
'

test_expect_success 'cherry: after cherry-pick matches git' '
	(cd repo && grit cherry main >../grit_out &&
	 "$REAL_GIT" cherry main >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'cherry -v: after cherry-pick matches git' '
	(cd repo && grit cherry -v main >../grit_out &&
	 "$REAL_GIT" cherry -v main >../git_out) &&
	test_cmp git_out grit_out
'

# ---- cherry with explicit HEAD argument ----

test_expect_success 'cherry: explicit HEAD arg matches git' '
	(cd repo && grit cherry main feature >../grit_out &&
	 "$REAL_GIT" cherry main feature >../git_out) &&
	test_cmp git_out grit_out
'

test_expect_success 'cherry -v: explicit HEAD arg matches git' '
	(cd repo && grit cherry -v main feature >../grit_out &&
	 "$REAL_GIT" cherry -v main feature >../git_out) &&
	test_cmp git_out grit_out
'

# ---- cherry with limit ----

test_expect_success 'setup: create limit ref' '
	(cd repo &&
	 limit_sha=$("$REAL_GIT" rev-list main..feature | tail -1) &&
	 "$REAL_GIT" tag limit-tag "$limit_sha")
'

test_expect_success 'cherry: with limit excludes earlier commits' '
	(cd repo && grit cherry main feature limit-tag >../actual) &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -lt 3
'

test_expect_success 'cherry: with limit matches git' '
	(cd repo && grit cherry main feature limit-tag >../grit_out &&
	 "$REAL_GIT" cherry main feature limit-tag >../git_out) &&
	test_cmp git_out grit_out
'

test_done
