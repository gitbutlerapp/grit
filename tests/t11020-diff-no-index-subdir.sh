#!/bin/sh
# Tests for grit diff with subdirectory structures and various output modes.
# Focuses on --cached (staged) diffs and working-tree diffs in nested repos.

test_description='grit diff: subdirectories, --cached, --stat, --name-only, --name-status, --numstat'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with nested subdirectory structure' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	mkdir -p src/core &&
	mkdir -p src/util &&
	mkdir -p docs/api &&
	echo "fn main() {}" >src/core/main.rs &&
	echo "fn helper() {}" >src/util/helper.rs &&
	echo "fn util() {}" >src/util/util.rs &&
	echo "# API docs" >docs/api/index.md &&
	echo "# README" >docs/api/readme.md &&
	echo "root" >root.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Working-tree diff (no args)
###########################################################################

test_expect_success 'diff shows no output on clean tree' '
	(
	cd repo &&
	grit diff >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff on clean tree matches git' '
	(
	cd repo &&
	grit diff >grit_out &&
	"$REAL_GIT" diff >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff detects modification in subdir file' '
	(
	cd repo &&
	echo "fn main() { println!(\"hello\"); }" >src/core/main.rs &&
	grit diff >actual &&
	grep "src/core/main.rs" actual
	)
'

test_expect_success 'diff --name-only lists changed files' '
	(
	cd repo &&
	echo "changed root" >root.txt &&
	echo "fn new_helper() {}" >src/util/helper.rs &&
	grit diff --name-only >actual &&
	grep "src/core/main.rs" actual &&
	grep "src/util/helper.rs" actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'diff --name-only matches git' '
	(
	cd repo &&
	grit diff --name-only >grit_out &&
	"$REAL_GIT" diff --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --name-status shows M for modified files' '
	(
	cd repo &&
	grit diff --name-status >actual &&
	grep "^M" actual | grep "root.txt" &&
	grep "^M" actual | grep "src/core/main.rs" &&
	grep "^M" actual | grep "src/util/helper.rs"
	)
'

test_expect_success 'diff --name-status matches git' '
	(
	cd repo &&
	grit diff --name-status >grit_out &&
	"$REAL_GIT" diff --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --stat shows changed files in stat output' '
	(
	cd repo &&
	grit diff --stat >actual &&
	grep "root.txt" actual &&
	grep "src/core/main.rs" actual &&
	grep "src/util/helper.rs" actual &&
	grep "3 files changed" actual
	)
'

test_expect_success 'diff --numstat shows numeric stats' '
	(
	cd repo &&
	grit diff --numstat >actual &&
	grep "root.txt" actual &&
	grep "src/core/main.rs" actual
	)
'

test_expect_success 'diff --exit-code returns 1 when there are changes' '
	(
	cd repo &&
	test_must_fail grit diff --exit-code
	)
'

test_expect_success 'diff --quiet suppresses output' '
	(
	cd repo &&
	test_must_fail grit diff --quiet >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --exit-code returns 0 on clean tree' '
	(
	cd repo &&
	"$REAL_GIT" checkout -- . &&
	grit diff --exit-code
	)
'

###########################################################################
# Section 3: --cached diff on staged changes in subdirs
###########################################################################

test_expect_success 'diff --cached shows no output when nothing staged' '
	(
	cd repo &&
	grit diff --cached >actual &&
	test_must_be_empty actual
	)
'

test_expect_success 'diff --cached detects single staged file' '
	(
	cd repo &&
	echo "fn main() { println!(\"hello\"); }" >src/core/main.rs &&
	"$REAL_GIT" add src/core/main.rs &&
	grit diff --cached >actual &&
	grep "src/core/main.rs" actual
	)
'

test_expect_success 'diff --cached matches git for single staged file' '
	(
	cd repo &&
	grit diff --cached >grit_out &&
	"$REAL_GIT" diff --cached >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached with multiple staged subdir files' '
	(
	cd repo &&
	echo "changed root" >root.txt &&
	echo "fn new_helper() {}" >src/util/helper.rs &&
	"$REAL_GIT" add root.txt src/util/helper.rs &&
	grit diff --cached >actual &&
	grep "root.txt" actual &&
	grep "src/core/main.rs" actual &&
	grep "src/util/helper.rs" actual
	)
'

test_expect_success 'diff --cached with multiple files matches git' '
	(
	cd repo &&
	grit diff --cached >grit_out &&
	"$REAL_GIT" diff --cached >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --stat shows diffstat summary' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "root.txt" actual &&
	grep "src/core/main.rs" actual &&
	grep "src/util/helper.rs" actual &&
	grep "3 files changed" actual
	)
'

test_expect_success 'diff --cached --name-only lists staged files' '
	(
	cd repo &&
	grit diff --cached --name-only >actual &&
	grep "root.txt" actual &&
	grep "src/core/main.rs" actual &&
	grep "src/util/helper.rs" actual
	)
'

test_expect_success 'diff --cached --name-only matches git' '
	(
	cd repo &&
	grit diff --cached --name-only >grit_out &&
	"$REAL_GIT" diff --cached --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --name-status shows M' '
	(
	cd repo &&
	grit diff --cached --name-status >actual &&
	grep "^M" actual | grep "root.txt" &&
	grep "^M" actual | grep "src/core/main.rs"
	)
'

test_expect_success 'diff --cached --name-status matches git' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --numstat shows numeric stats' '
	(
	cd repo &&
	grit diff --cached --numstat >actual &&
	grep "root.txt" actual &&
	grep "[0-9]" actual
	)
'

test_expect_success 'diff --cached --numstat matches git' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 4: --cached diff with context lines (-U)
###########################################################################

test_expect_success 'diff --cached -U0 matches git' '
	(
	cd repo &&
	grit diff --cached -U0 >grit_out &&
	"$REAL_GIT" diff --cached -U0 >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached -U10 matches git' '
	(
	cd repo &&
	grit diff --cached -U10 >grit_out &&
	"$REAL_GIT" diff --cached -U10 >git_out &&
	test_cmp git_out grit_out
	)
'

###########################################################################
# Section 5: --cached diff with new and deleted files in subdirs
###########################################################################

test_expect_success 'setup: commit current changes and prepare new/deleted files' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "second" &&
	echo "new file" >src/core/new.rs &&
	"$REAL_GIT" add src/core/new.rs &&
	"$REAL_GIT" rm docs/api/readme.md &&
	true
	)
'

test_expect_success 'diff --cached shows added file in subdir' '
	(
	cd repo &&
	grit diff --cached >actual &&
	grep "src/core/new.rs" actual &&
	grep "new file" actual
	)
'

test_expect_success 'diff --cached shows deleted file in subdir' '
	(
	cd repo &&
	grit diff --cached >actual &&
	grep "docs/api/readme.md" actual &&
	grep "deleted file" actual
	)
'

test_expect_success 'diff --cached --name-status shows A and D' '
	(
	cd repo &&
	grit diff --cached --name-status >actual &&
	grep "^A" actual | grep "src/core/new.rs" &&
	grep "^D" actual | grep "docs/api/readme.md"
	)
'

test_expect_success 'diff --cached --name-status add/delete matches git' '
	(
	cd repo &&
	grit diff --cached --name-status >grit_out &&
	"$REAL_GIT" diff --cached --name-status >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --name-only add/delete matches git' '
	(
	cd repo &&
	grit diff --cached --name-only >grit_out &&
	"$REAL_GIT" diff --cached --name-only >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --numstat add/delete matches git' '
	(
	cd repo &&
	grit diff --cached --numstat >grit_out &&
	"$REAL_GIT" diff --cached --numstat >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --cached --stat shows summary for add/delete' '
	(
	cd repo &&
	grit diff --cached --stat >actual &&
	grep "docs/api/readme.md" actual &&
	grep "src/core/new.rs" actual &&
	grep "2 files changed" actual
	)
'

###########################################################################
# Section 6: diff between two commits
###########################################################################

test_expect_success 'setup: commit and create history' '
	(
	cd repo &&
	"$REAL_GIT" commit -m "third" &&
	echo "updated" >root.txt &&
	"$REAL_GIT" add root.txt &&
	"$REAL_GIT" commit -m "fourth"
	)
'

test_expect_success 'diff HEAD~1 HEAD shows changes between commits' '
	(
	cd repo &&
	grit diff HEAD~1 HEAD >actual &&
	grep "root.txt" actual
	)
'

test_expect_success 'diff HEAD~1 HEAD --name-only matches git' '
	(
	cd repo &&
	grit diff --name-only HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff --name-only HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff HEAD~1 HEAD --name-status matches git' '
	(
	cd repo &&
	grit diff --name-status HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff --name-status HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff HEAD~1 HEAD --numstat matches git' '
	(
	cd repo &&
	grit diff --numstat HEAD~1 HEAD >grit_out &&
	"$REAL_GIT" diff --numstat HEAD~1 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff HEAD~3 HEAD --name-only matches git' '
	(
	cd repo &&
	grit diff --name-only HEAD~3 HEAD >grit_out &&
	"$REAL_GIT" diff --name-only HEAD~3 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff HEAD~3 HEAD --name-status matches git' '
	(
	cd repo &&
	grit diff --name-status HEAD~3 HEAD >grit_out &&
	"$REAL_GIT" diff --name-status HEAD~3 HEAD >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'diff --stat between commits shows summary' '
	(
	cd repo &&
	grit diff --stat HEAD~1 HEAD >actual &&
	grep "root.txt" actual &&
	grep "1 file changed" actual
	)
'

test_done
