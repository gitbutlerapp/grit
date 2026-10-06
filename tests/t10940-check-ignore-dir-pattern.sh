#!/bin/sh
# Tests for check-ignore with directory patterns, negation, nested
# .gitignore, verbose mode, --stdin, and edge cases.

test_description='check-ignore directory patterns and options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with .gitignore' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&

	cat >.gitignore <<-\EOF &&
	*.log
	*.tmp
	build/
	dist/
	!important.log
	secret.*
	EOF

	mkdir -p src build dist
	)
'

# ── Basic pattern matching ───────────────────────────────────────────────────

test_expect_success 'file matching *.log is ignored' '
	(
	cd repo &&
	grit check-ignore debug.log
	)
'

test_expect_success 'file matching *.tmp is ignored' '
	(
	cd repo &&
	grit check-ignore data.tmp
	)
'

test_expect_success 'file not matching any pattern is not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore README.md
	)
'

test_expect_success 'another non-ignored file' '
	(
	cd repo &&
	test_must_fail grit check-ignore src/main.rs
	)
'

test_expect_success '.gitignore itself is not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore .gitignore
	)
'

# ── Directory patterns ───────────────────────────────────────────────────────

test_expect_success 'directory pattern build/ matches files inside' '
	(
	cd repo &&
	grit check-ignore build/output.o
	)
'

test_expect_success 'directory pattern dist/ matches files inside' '
	(
	cd repo &&
	grit check-ignore dist/bundle.js
	)
'

test_expect_success 'directory pattern matches nested files' '
	(
	cd repo &&
	grit check-ignore build/sub/deep/file.txt
	)
'

test_expect_success 'file named like dir but not inside is not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore buildfile
	)
'

# ── Negation pattern ────────────────────────────────────────────────────────

test_expect_success 'negated pattern !important.log is not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore important.log
	)
'

test_expect_success 'other .log files still ignored despite negation' '
	(
	cd repo &&
	grit check-ignore error.log
	)
'

test_expect_success 'similar name to negation still ignored' '
	(
	cd repo &&
	grit check-ignore important.log.bak.log &&
	grit check-ignore not-important.log
	)
'

# ── Wildcard patterns ────────────────────────────────────────────────────────

test_expect_success 'secret.* matches various extensions' '
	(
	cd repo &&
	grit check-ignore secret.key &&
	grit check-ignore secret.txt &&
	grit check-ignore secret.env
	)
'

test_expect_success 'file starting with secret but no dot is not ignored' '
	(
	cd repo &&
	test_must_fail grit check-ignore secrets
	)
'

# ── Verbose mode (-v) ───────────────────────────────────────────────────────

test_expect_success '-v shows source file and pattern' '
	(
	cd repo &&
	grit check-ignore -v debug.log >actual &&
	grep ".gitignore" actual &&
	grep "\\*.log" actual
	)
'

test_expect_success '-v shows line info for directory pattern' '
	(
	cd repo &&
	grit check-ignore -v build/out.o >actual &&
	grep ".gitignore" actual &&
	grep "build/" actual
	)
'

test_expect_success '-v with non-matching and -n shows empty source' '
	(
	cd repo &&
	grit check-ignore -v -n README.md >actual || true &&
	grep "README.md" actual
	)
'

# ── --stdin mode ─────────────────────────────────────────────────────────────

test_expect_success '--stdin reads paths from stdin' '
	(
	cd repo &&
	printf "debug.log\nbuild/x\n" | grit check-ignore --stdin >actual &&
	grep "debug.log" actual &&
	grep "build/x" actual
	)
'

test_expect_success '--stdin with non-ignored paths filters them out' '
	(
	cd repo &&
	printf "README.md\ndebug.log\nsrc/main.rs\n" | grit check-ignore --stdin >actual &&
	grep "debug.log" actual &&
	! grep "README.md" actual &&
	! grep "src/main.rs" actual
	)
'

test_expect_success '--stdin with all non-ignored returns exit 1' '
	(
	cd repo &&
	printf "README.md\nsrc/main.rs\n" | test_must_fail grit check-ignore --stdin >actual &&
	test_must_be_empty actual
	)
'

test_expect_success '--stdin with empty input returns exit 1' '
	(
	cd repo &&
	printf "" | test_must_fail grit check-ignore --stdin
	)
'

# ── Nested .gitignore ────────────────────────────────────────────────────────

test_expect_success 'setup nested .gitignore' '
	(
	cd repo &&
	mkdir -p sub/deep &&
	cat >sub/.gitignore <<-\EOF &&
	*.dat
	!keep.dat
	EOF
	cat >sub/deep/.gitignore <<-\EOF
	*.cache
	EOF
	)
'

test_expect_success 'nested .gitignore ignores *.dat in subdir' '
	(
	cd repo &&
	grit check-ignore sub/data.dat
	)
'

test_expect_success 'nested negation !keep.dat works' '
	(
	cd repo &&
	test_must_fail grit check-ignore sub/keep.dat
	)
'

test_expect_success 'deeply nested .gitignore ignores *.cache' '
	(
	cd repo &&
	grit check-ignore sub/deep/foo.cache
	)
'

test_expect_success 'parent pattern still applies in subdir' '
	(
	cd repo &&
	grit check-ignore sub/debug.log
	)
'

test_expect_success 'subdir pattern does not leak to root' '
	(
	cd repo &&
	test_must_fail grit check-ignore data.dat
	)
'

test_expect_success 'subdir pattern does not leak to sibling' '
	(
	cd repo &&
	test_must_fail grit check-ignore src/data.dat
	)
'

# ── Multiple paths on command line ───────────────────────────────────────────

test_expect_success 'multiple paths: mixed ignored and not' '
	(
	cd repo &&
	grit check-ignore debug.log build/x README.md >actual || true &&
	grep "debug.log" actual &&
	grep "build/x" actual &&
	! grep "README.md" actual
	)
'

test_expect_success 'multiple paths: all ignored returns 0' '
	(
	cd repo &&
	grit check-ignore debug.log build/x data.tmp
	)
'

test_expect_success 'multiple paths: none ignored returns 1' '
	(
	cd repo &&
	test_must_fail grit check-ignore README.md src/main.rs
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'path with spaces' '
	(
	cd repo &&
	grit check-ignore "my file.log"
	)
'

test_expect_success 'path with special chars in extension' '
	(
	cd repo &&
	test_must_fail grit check-ignore "file.log.bak"
	)
'

test_expect_success 'dotfile not ignored unless matched' '
	(
	cd repo &&
	test_must_fail grit check-ignore .hidden
	)
'

test_done
