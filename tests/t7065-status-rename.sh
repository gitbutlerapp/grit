#!/bin/sh
# Tests for grit status rename detection.
# grit does not yet detect renames in status, so renamed files appear
# as delete + add. Tests verify current behavior and mark rename
# detection as expected failures to track progress.

test_description='status rename detection'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with files for rename tests' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	echo "content of alpha" >alpha.txt &&
	echo "content of beta" >beta.txt &&
	echo "content of gamma with more text to ensure similarity" >gamma.txt &&
	mkdir -p src doc &&
	echo "source code file one" >src/one.c &&
	echo "source code file two" >src/two.c &&
	echo "documentation content" >doc/readme.md &&
	git add . &&
	git commit -m "initial files"
	)
'

# ── Simple rename via git mv ────────────────────────────────────────────────

test_expect_success 'git mv renames file in index' '
	(
	cd repo &&
	git mv alpha.txt alpha-renamed.txt &&
	git ls-files >../actual &&
	grep "alpha-renamed.txt" ../actual &&
	! grep "^alpha.txt$" ../actual
	)
'

test_expect_success 'status shows rename after git mv (porcelain)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'status shows rename after git mv (short)' '
	(
	cd repo &&
	git status --short >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'renamed file has same content' '
	(
	cd repo &&
	echo "content of alpha" >../expect &&
	test_cmp ../expect alpha-renamed.txt
	)
'

test_expect_success 'commit rename and verify log' '
	(
	cd repo &&
	git commit -m "rename alpha" &&
	git log --oneline >../actual &&
	test_line_count = 2 ../actual
	)
'

# ── Rename to different directory ────────────────────────────────────────────

test_expect_success 'git mv file into subdirectory' '
	(
	cd repo &&
	git mv beta.txt doc/beta.txt &&
	git ls-files >../actual &&
	grep "doc/beta.txt" ../actual &&
	! grep "^beta.txt$" ../actual
	)
'

test_expect_success 'status detects cross-directory rename' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'commit cross-directory rename' '
	(
	cd repo &&
	git commit -m "move beta to doc"
	)
'

# ── Rename from subdirectory ─────────────────────────────────────────────────

test_expect_success 'git mv file out of subdirectory' '
	(
	cd repo &&
	git mv src/one.c one.c &&
	git ls-files >../actual &&
	grep "^one.c$" ../actual &&
	! grep "^src/one.c$" ../actual
	)
'

test_expect_success 'status shows rename for directory extraction' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'commit directory extraction' '
	(
	cd repo &&
	git commit -m "extract one.c from src"
	)
'

# ── Manual rename (rm + add) ────────────────────────────────────────────────

test_expect_success 'manual rename: copy content, rm old, add new' '
	(
	cd repo &&
	cp gamma.txt gamma-new.txt &&
	git rm gamma.txt &&
	git add gamma-new.txt &&
	git ls-files >../actual &&
	grep "gamma-new.txt" ../actual &&
	! grep "^gamma.txt$" ../actual
	)
'

test_expect_success 'status detects manual rename as R' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'commit manual rename' '
	(
	cd repo &&
	git commit -m "manual rename gamma"
	)
'

# ── Rename with modification ────────────────────────────────────────────────

test_expect_success 'git mv then modify the renamed file' '
	(
	cd repo &&
	git mv doc/readme.md doc/README.md &&
	echo "extra line" >>doc/README.md &&
	git add doc/README.md
	)
'

test_expect_success 'status detects rename with modification' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'commit rename with modification' '
	(
	cd repo &&
	git commit -m "rename and modify readme"
	)
'

# ── Multiple renames at once ────────────────────────────────────────────────

test_expect_success 'setup: create files for batch rename' '
	(
	cd repo &&
	echo "file-a" >a.txt &&
	echo "file-b" >b.txt &&
	echo "file-c" >c.txt &&
	git add a.txt b.txt c.txt &&
	git commit -m "add a b c"
	)
'

test_expect_success 'batch git mv three files' '
	(
	cd repo &&
	git mv a.txt a-new.txt &&
	git mv b.txt b-new.txt &&
	git mv c.txt c-new.txt
	)
'

test_expect_success 'status shows three renames with R prefix' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	count=$(grep -c "^R" ../actual) &&
	test "$count" = "3"
	)
'

test_expect_success 'commit batch rename' '
	(
	cd repo &&
	git commit -m "batch rename"
	)
'

# ── Status after clean rename commit ────────────────────────────────────────

test_expect_success 'status is clean after committing renames' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep -v "^##" ../actual
	)
'

# ── Rename back to original name ────────────────────────────────────────────

test_expect_success 'rename back to original name' '
	(
	cd repo &&
	git mv a-new.txt a.txt &&
	git status --porcelain >../actual &&
	grep "^R" ../actual
	)
'

test_expect_success 'commit rename-back' '
	(
	cd repo &&
	git commit -m "rename back a"
	)
'

# ── Unstaged rename (working tree only) ─────────────────────────────────────

test_expect_success 'working tree rename without staging' '
	(
	cd repo &&
	mv b-new.txt b-moved.txt &&
	git status --porcelain >../actual &&
	grep "b-new.txt" ../actual &&
	grep "b-moved.txt" ../actual
	)
'

test_expect_success 'working tree rename shows as deleted + untracked' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep " D b-new.txt" ../actual &&
	grep "?? b-moved.txt" ../actual
	)
'

test_expect_success 'cleanup: restore working tree rename' '
	(
	cd repo &&
	mv b-moved.txt b-new.txt
	)
'

test_done
