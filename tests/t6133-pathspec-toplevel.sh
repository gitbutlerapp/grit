#!/bin/sh
# Tests for pathspec resolution from the repository root.
# Verifies that pathspecs resolve correctly in ls-files and add
# regardless of where in the tree the command is run from.
# Also marks diff/log/status pathspec support as expected failures
# since grit does not yet implement pathspec filtering for those.

test_description='pathspec resolution from repo root'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with nested directories' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	mkdir -p src/lib src/bin doc/api doc/guide &&
	echo "root" >README.md &&
	echo "lib1" >src/lib/one.c &&
	echo "lib2" >src/lib/two.c &&
	echo "bin1" >src/bin/main.c &&
	echo "bin2" >src/bin/helper.c &&
	echo "api" >doc/api/ref.md &&
	echo "guide" >doc/guide/intro.md &&
	echo "top" >top.txt &&
	echo "cfg" >config.yml &&
	git add . &&
	git commit -m "initial structure"
	)
'

# ── ls-files from root with directory pathspecs ─────────────────────────────

test_expect_success 'ls-files with directory pathspec from root' '
	(
	cd repo &&
	git ls-files src/ >../actual &&
	test_line_count = 4 ../actual
	)
'

test_expect_success 'ls-files with nested directory pathspec from root' '
	(
	cd repo &&
	git ls-files src/lib/ >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'ls-files with exact file pathspec from root' '
	(
	cd repo &&
	git ls-files README.md >../actual &&
	echo "README.md" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'ls-files with nested file pathspec from root' '
	(
	cd repo &&
	git ls-files src/lib/one.c >../actual &&
	echo "src/lib/one.c" >../expect &&
	test_cmp ../expect ../actual
	)
'

test_expect_success 'ls-files with multiple directory pathspecs' '
	(
	cd repo &&
	git ls-files src/lib/ doc/ >../actual &&
	test_line_count = 4 ../actual
	)
'

test_expect_success 'ls-files with file and directory pathspecs' '
	(
	cd repo &&
	git ls-files README.md src/bin/ >../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'ls-files nonexistent pathspec returns empty' '
	(
	cd repo &&
	git ls-files nonexistent/ >../actual &&
	test_must_be_empty ../actual
	)
'

test_expect_success 'ls-files with two nonexistent returns empty' '
	(
	cd repo &&
	git ls-files nope1 nope2 >../actual &&
	test_must_be_empty ../actual
	)
'

# ── ls-files from subdirectory ──────────────────────────────────────────────

test_expect_success 'ls-files from src shows files relative to src' '
	(
	cd repo/src &&
	git ls-files >../../actual &&
	test_line_count = 4 ../../actual &&
	grep "^lib/one.c" ../../actual &&
	grep "^lib/two.c" ../../actual &&
	grep "^bin/main.c" ../../actual &&
	grep "^bin/helper.c" ../../actual
	)
'

test_expect_success 'ls-files from src with lib/ pathspec' '
	(
	cd repo/src &&
	git ls-files lib/ >../../actual &&
	test_line_count = 2 ../../actual
	)
'

test_expect_success 'ls-files from deep subdirectory lists local files' '
	(
	cd repo/src/lib &&
	git ls-files >../../../actual &&
	test_line_count = 2 ../../../actual &&
	grep "^one.c" ../../../actual &&
	grep "^two.c" ../../../actual
	)
'

test_expect_success 'ls-files from deep subdirectory with filename' '
	(
	cd repo/src/lib &&
	git ls-files one.c >../../../actual &&
	echo "one.c" >../../../expect &&
	test_cmp ../../../expect ../../../actual
	)
'

test_expect_success 'ls-files from doc shows doc-relative paths' '
	(
	cd repo/doc &&
	git ls-files >../../actual &&
	test_line_count = 2 ../../actual &&
	grep "^api/ref.md" ../../actual &&
	grep "^guide/intro.md" ../../actual
	)
'

test_expect_success 'ls-files from doc/api shows single file' '
	(
	cd repo/doc/api &&
	git ls-files >../../../actual &&
	echo "ref.md" >../../../expect &&
	test_cmp ../../../expect ../../../actual
	)
'

# ── ls-files dot pathspec ───────────────────────────────────────────────────

test_expect_success 'ls-files . from subdirectory scopes to subtree' '
	(
	cd repo/src/lib &&
	git ls-files . >../../../actual &&
	test_line_count = 2 ../../../actual
	)
'

test_expect_success 'ls-files . from src scopes to src tree' '
	(
	cd repo/src &&
	git ls-files . >../../actual &&
	test_line_count = 4 ../../actual
	)
'

# ── add with pathspec from root ─────────────────────────────────────────────

test_expect_success 'modify files for add tests' '
	(
	cd repo &&
	echo "mod1" >>src/lib/one.c &&
	echo "mod2" >>doc/api/ref.md &&
	echo "mod3" >>README.md
	)
'

test_expect_success 'add with directory pathspec stages only that dir' '
	(
	cd repo &&
	git add src/ &&
	git diff --cached --name-only >../actual &&
	test_line_count = 1 ../actual &&
	grep "src/lib/one.c" ../actual
	)
'

test_expect_success 'add more with file pathspec' '
	(
	cd repo &&
	git add README.md &&
	git diff --cached --name-only >../actual &&
	test_line_count = 2 ../actual &&
	grep "README.md" ../actual &&
	grep "src/lib/one.c" ../actual
	)
'

test_expect_success 'add remaining with directory pathspec' '
	(
	cd repo &&
	git add doc/ &&
	git diff --cached --name-only >../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'commit staged changes' '
	(
	cd repo &&
	git commit -m "modifications"
	)
'

# ── add from subdirectory ───────────────────────────────────────────────────

test_expect_success 'setup: modify two files in src' '
	(
	cd repo &&
	echo "again1" >>src/lib/one.c &&
	echo "again2" >>src/bin/main.c
	)
'

test_expect_success 'add from src/lib stages only lib files' '
	(
	cd repo/src/lib &&
	git add . &&
	cd ../../.. &&
	cd repo &&
	git diff --cached --name-only >../actual &&
	test_line_count = 1 ../actual &&
	grep "src/lib/one.c" ../actual
	)
'

test_expect_success 'add from src stages remaining src files' '
	(
	cd repo/src &&
	git add . &&
	cd ../.. &&
	cd repo &&
	git diff --cached --name-only >../actual &&
	test_line_count = 2 ../actual
	)
'

test_expect_success 'commit from subdirectory' '
	(
	cd repo &&
	git commit -m "more mods"
	)
'

# ── ls-files -C flag ────────────────────────────────────────────────────────

test_expect_success 'ls-files -C targets repo from outside' '
	git -C repo ls-files src/lib/ >actual &&
	test_line_count = 2 actual
'

test_expect_success 'ls-files -C with nested pathspec' '
	git -C repo ls-files doc/ >actual &&
	test_line_count = 2 actual
'

test_expect_success 'ls-files -C with exact file' '
	git -C repo ls-files config.yml >actual &&
	echo "config.yml" >expect &&
	test_cmp expect actual
'

# ── diff/log/status with pathspec (expected failures — not yet supported) ──

test_expect_success 'setup: modify for diff pathspec tests' '
	(
	cd repo &&
	echo "diffmod" >>src/lib/one.c
	)
'

test_expect_success 'diff with -- pathspec restricts output' '
	(
	cd repo &&
	git diff --name-only -- src/ >../actual &&
	test_line_count = 1 ../actual &&
	grep "src/lib/one.c" ../actual
	)
'

test_expect_success 'log with -- pathspec restricts commits' '
	(
	cd repo &&
	git log --oneline -- src/ >../actual &&
	test_line_count = 3 ../actual
	)
'

test_expect_success 'log with -- file pathspec' '
	(
	cd repo &&
	git log --oneline -- config.yml >../actual &&
	test_line_count = 1 ../actual
	)
'

test_done
