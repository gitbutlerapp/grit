#!/bin/sh
# Tests for check-ignore --stdin mode: basic matching, -v (verbose),
# -n (non-matching), -z (NUL I/O), negation patterns, nested
# .gitignore, global excludes, and edge cases.

test_description='check-ignore --stdin mode'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with ignore rules' '
	(
	grit init repo &&
	cd repo &&
	echo "ref: refs/heads/main" >.git/HEAD &&
	mkdir -p src lib .git/info &&

	cat >.gitignore <<-\EOF &&
	*.log
	build/
	*.tmp
	secret.txt
	!keep.log
	EOF

	cat >src/.gitignore <<-\EOF &&
	*.bak
	debug/
	!important.bak
	EOF

	cat >lib/.gitignore <<-\EOF &&
	generated.*
	EOF

	echo "per-repo-ignored" >.git/info/exclude &&

	cat >global-excludes <<-\EOF &&
	*.swp
	.DS_Store
	EOF
	git config core.excludesFile "$(pwd)/global-excludes"
	)
'

# ── basic --stdin matching ───────────────────────────────────────────────────

test_expect_success '--stdin with single ignored path' '
	(
	cd repo &&
	echo "debug.log" | grit check-ignore --stdin >actual &&
	echo "debug.log" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin with non-ignored path produces no output' '
	(
	cd repo &&
	test_expect_code 1 grit check-ignore --stdin >actual <<-\EOF &&
	src/app.js
	EOF
	test_must_be_empty actual
	)
'

test_expect_success '--stdin with multiple paths filters correctly' '
	(
	cd repo &&
	printf "debug.log\nsrc/app.js\nbuild/output\n" |
		grit check-ignore --stdin >actual &&
	grep debug.log actual &&
	grep "build/output" actual &&
	! grep "src/app.js" actual
	)
'

test_expect_success '--stdin matches wildcard *.tmp' '
	(
	cd repo &&
	echo "data.tmp" | grit check-ignore --stdin >actual &&
	echo "data.tmp" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin matches exact filename' '
	(
	cd repo &&
	echo "secret.txt" | grit check-ignore --stdin >actual &&
	echo "secret.txt" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin matches directory pattern build/' '
	(
	cd repo &&
	echo "build/something" | grit check-ignore --stdin >actual &&
	echo "build/something" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin matches deeply nested under directory' '
	(
	cd repo &&
	echo "build/deep/nested/file" | grit check-ignore --stdin >actual &&
	echo "build/deep/nested/file" >expect &&
	test_cmp expect actual
	)
'

# ── negation patterns with verbose ───────────────────────────────────────────

test_expect_success '--stdin -v shows negation pattern for !keep.log' '
	(
	cd repo &&
	echo "keep.log" | grit check-ignore --stdin -v >actual &&
	grep "!keep.log" actual &&
	grep "keep.log" actual
	)
'

test_expect_success '--stdin -v -n shows negation match for keep.log' '
	(
	cd repo &&
	echo "keep.log" | grit check-ignore --stdin -v -n >actual &&
	grep "!keep.log" actual
	)
'

test_expect_success '--stdin -v shows different patterns for log vs keep.log' '
	(
	cd repo &&
	printf "error.log\nkeep.log\n" | grit check-ignore --stdin -v >actual &&
	grep "\\*.log" actual &&
	grep "!keep.log" actual
	)
'

# ── nested .gitignore ────────────────────────────────────────────────────────

test_expect_success '--stdin matches nested .gitignore *.bak' '
	(
	cd repo &&
	echo "src/backup.bak" | grit check-ignore --stdin >actual &&
	echo "src/backup.bak" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin -v shows nested negation for !important.bak' '
	(
	cd repo &&
	echo "src/important.bak" | grit check-ignore --stdin -v >actual &&
	grep "!important.bak" actual
	)
'

test_expect_success '--stdin matches nested directory pattern debug/' '
	(
	cd repo &&
	echo "src/debug/trace.out" | grit check-ignore --stdin >actual &&
	echo "src/debug/trace.out" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin matches lib/.gitignore generated.*' '
	(
	cd repo &&
	echo "lib/generated.js" | grit check-ignore --stdin >actual &&
	echo "lib/generated.js" >expect &&
	test_cmp expect actual
	)
'

# ── .git/info/exclude ────────────────────────────────────────────────────────

test_expect_success '--stdin matches .git/info/exclude' '
	(
	cd repo &&
	echo "per-repo-ignored" | grit check-ignore --stdin >actual &&
	echo "per-repo-ignored" >expect &&
	test_cmp expect actual
	)
'

# ── global excludes ──────────────────────────────────────────────────────────

test_expect_success '--stdin matches global excludes *.swp' '
	(
	cd repo &&
	echo "notes.swp" | grit check-ignore --stdin >actual &&
	echo "notes.swp" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin matches global excludes .DS_Store' '
	(
	cd repo &&
	echo ".DS_Store" | grit check-ignore --stdin >actual &&
	echo ".DS_Store" >expect &&
	test_cmp expect actual
	)
'

# ── --stdin -v (verbose) ─────────────────────────────────────────────────────

test_expect_success '--stdin -v shows source:linenum:pattern' '
	(
	cd repo &&
	echo "debug.log" | grit check-ignore --stdin -v >actual &&
	grep ".gitignore:1:" actual &&
	grep "debug.log" actual
	)
'

test_expect_success '--stdin -v with nested gitignore shows correct source' '
	(
	cd repo &&
	echo "src/old.bak" | grit check-ignore --stdin -v >actual &&
	grep "src/.gitignore" actual
	)
'

test_expect_success '--stdin -v shows global excludes source' '
	(
	cd repo &&
	echo "test.swp" | grit check-ignore --stdin -v >actual &&
	grep "global-excludes" actual
	)
'

# ── --stdin -v -n (verbose non-matching) ─────────────────────────────────────

test_expect_success '--stdin -v -n shows unmatched paths with empty pattern' '
	(
	cd repo &&
	test_expect_code 1 grit check-ignore --stdin -v -n >actual <<-\EOF &&
	src/app.js
	EOF
	grep "::" actual &&
	grep "src/app.js" actual
	)
'

test_expect_success '--stdin -v -n shows both matched and unmatched' '
	(
	cd repo &&
	printf "debug.log\nsrc/app.js\n" | grit check-ignore --stdin -v -n >actual &&
	grep ".gitignore.*debug.log" actual &&
	grep "::	src/app.js" actual
	)
'

test_expect_success '--stdin -v -n with all ignored shows all with sources' '
	(
	cd repo &&
	printf "error.log\ndata.tmp\n" | grit check-ignore --stdin -v -n >actual &&
	test_line_count = 2 actual
	)
'

# ── --stdin -z (NUL I/O) ────────────────────────────────────────────────────

test_expect_success '--stdin -z reads NUL input and writes NUL output' '
	(
	cd repo &&
	printf "debug.log\0error.log\0" | grit check-ignore --stdin -z >actual &&
	printf "debug.log\0error.log\0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin -z with single path' '
	(
	cd repo &&
	printf "debug.log\0" | grit check-ignore --stdin -z >actual &&
	printf "debug.log\0" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin -z filters non-ignored paths' '
	(
	cd repo &&
	printf "debug.log\0src/app.js\0" | grit check-ignore --stdin -z >actual &&
	printf "debug.log\0" >expect &&
	test_cmp expect actual
	)
'

# ── multiple mixed paths ─────────────────────────────────────────────────────

test_expect_success '--stdin correctly filters mixed input' '
	(
	cd repo &&
	printf "README.md\ndebug.log\nsrc/main.rs\nbuild/out\nsecret.txt\n" |
		grit check-ignore --stdin >actual &&
	test_line_count = 3 actual &&
	grep debug.log actual &&
	grep "build/out" actual &&
	grep secret.txt actual
	)
'

test_expect_success '--stdin with all paths ignored' '
	(
	cd repo &&
	printf "error.log\ndata.tmp\nbuild/x\n" |
		grit check-ignore --stdin >actual &&
	test_line_count = 3 actual
	)
'

# ── edge cases ───────────────────────────────────────────────────────────────

test_expect_success '--stdin with trailing newline works' '
	(
	cd repo &&
	printf "debug.log\n" | grit check-ignore --stdin >actual &&
	echo "debug.log" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin path with spaces' '
	(
	cd repo &&
	echo "my file.log" | grit check-ignore --stdin >actual &&
	echo "my file.log" >expect &&
	test_cmp expect actual
	)
'

test_done
