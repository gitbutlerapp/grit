#!/bin/sh
# Tests for grit status --ignored and .gitignore integration.
# grit does not yet fully respect .gitignore in status, so most
# ignore-related tests are marked as expected failures.

test_description='status --ignored and .gitignore handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with .gitignore' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test" &&
	git config user.email "t@t.com" &&
	cat >.gitignore <<-\EOF &&
	*.o
	*.a
	build/
	tmp/
	*.log
	EOF
	echo "hello" >main.c &&
	echo "world" >util.c &&
	git add .gitignore main.c util.c &&
	git commit -m "initial with gitignore"
	)
'

# ── Create various ignored and untracked files ──────────────────────────────

test_expect_success 'create ignored and untracked files' '
	(
	cd repo &&
	echo "obj" >main.o &&
	echo "obj2" >util.o &&
	echo "archive" >lib.a &&
	mkdir -p build &&
	echo "binary" >build/output &&
	echo "binary2" >build/debug &&
	mkdir -p tmp &&
	echo "scratch" >tmp/scratch.txt &&
	echo "log" >error.log &&
	echo "log2" >debug.log &&
	echo "untracked" >notes.txt &&
	echo "untracked2" >TODO
	)
'

# ── Basic status should not show ignored files ──────────────────────────────

test_expect_success 'status does not show *.o files (ignored by pattern)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "main.o" ../actual &&
	! grep "util.o" ../actual
	)
'

test_expect_success 'status does not show *.a files (ignored by pattern)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "lib.a" ../actual
	)
'

test_expect_success 'status does not show build/ directory (ignored)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "build/" ../actual
	)
'

test_expect_success 'status does not show tmp/ directory (ignored)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "tmp/" ../actual
	)
'

test_expect_success 'status does not show *.log files (ignored)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "error.log" ../actual &&
	! grep "debug.log" ../actual
	)
'

test_expect_success 'status shows only non-ignored untracked files' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "?? TODO" ../actual &&
	grep "?? notes.txt" ../actual &&
	grep -v "^##" ../actual >../actual_filtered &&
	test_line_count = 2 ../actual_filtered
	)
'

# ── status --ignored should show ignored files ──────────────────────────────

test_expect_success 'status --ignored shows *.o in ignored section' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! main.o" ../actual &&
	grep "!! util.o" ../actual
	)
'

test_expect_success 'status --ignored shows *.a in ignored section' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! lib.a" ../actual
	)
'

test_expect_success 'status --ignored shows build/ in ignored section' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! build/" ../actual
	)
'

test_expect_success 'status --ignored shows tmp/ in ignored section' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! tmp/" ../actual
	)
'

test_expect_success 'status --ignored shows *.log in ignored section' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! error.log" ../actual &&
	grep "!! debug.log" ../actual
	)
'

test_expect_success 'status --ignored still shows untracked files' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "TODO" ../actual &&
	grep "notes.txt" ../actual
	)
'

# ── Nested .gitignore ───────────────────────────────────────────────────────

test_expect_success 'setup: nested .gitignore' '
	(
	cd repo &&
	mkdir -p src &&
	echo "*.tmp" >src/.gitignore &&
	echo "code" >src/main.c &&
	echo "temp" >src/scratch.tmp &&
	git add src/.gitignore src/main.c &&
	git commit -m "add src with nested gitignore"
	)
'

test_expect_success 'status does not show *.tmp in src (nested gitignore)' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "scratch.tmp" ../actual
	)
'

test_expect_success 'status --ignored shows *.tmp in src' '
	(
	cd repo &&
	git status --porcelain --ignored >../actual &&
	grep "!! src/scratch.tmp" ../actual
	)
'

# ── Negation patterns ───────────────────────────────────────────────────────

test_expect_success 'setup: gitignore with negation' '
	(
	cd repo &&
	cat >.gitignore <<-\EOF &&
	*.o
	*.a
	build/
	tmp/
	*.log
	!important.log
	EOF
	git add .gitignore &&
	echo "important" >important.log &&
	git commit -m "add negation pattern"
	)
'

test_expect_success 'status shows important.log as untracked' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	grep "important.log" ../actual
	)
'

test_expect_success 'status still hides non-negated *.log files' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "error.log" ../actual &&
	! grep "debug.log" ../actual
	)
'

# ── git add respects .gitignore ─────────────────────────────────────────────

test_expect_success 'git add . does not stage ignored files' '
	(
	cd repo &&
	git add . &&
	git diff --cached --name-only >../actual &&
	! grep "\.o$" ../actual &&
	! grep "\.a$" ../actual &&
	! grep "error\.log" ../actual &&
	! grep "debug\.log" ../actual
	)
'

test_expect_success 'reset index for next tests' '
	(
	cd repo &&
	git reset HEAD 2>/dev/null; true
	)
'

# ── ls-files --ignored ─────────────────────────────────────────────────────

test_expect_success 'ls-files --ignored shows ignored files' '
	(
	cd repo &&
	git ls-files --ignored >../actual &&
	grep "main.o" ../actual &&
	grep "util.o" ../actual &&
	grep "lib.a" ../actual
	)
'

test_expect_success 'ls-files --others --exclude-standard shows untracked non-ignored files' '
	(
	cd repo &&
	git ls-files --others --exclude-standard >../actual &&
	grep "notes.txt" ../actual &&
	grep "TODO" ../actual &&
	! grep "main.o" ../actual
	)
'

# ── Tracked file not affected by gitignore ──────────────────────────────────

test_expect_success 'tracked file matching ignore pattern still shows in ls-files' '
	(
	cd repo &&
	echo "tracked-obj" >special.o &&
	git add -f special.o &&
	git commit -m "force-add ignored pattern file" &&
	git ls-files >../actual &&
	grep "special.o" ../actual
	)
'

test_expect_success 'status shows modifications to force-added ignored file' '
	(
	cd repo &&
	echo "modified" >>special.o &&
	git status --porcelain >../actual &&
	grep "special.o" ../actual
	)
'

test_expect_success 'commit modification to force-added file' '
	(
	cd repo &&
	git add special.o &&
	git commit -m "update special.o"
	)
'

# ── Empty directory with only ignored files ─────────────────────────────────

test_expect_success 'setup: directory with only ignored content' '
	(
	cd repo &&
	mkdir -p onlyignored &&
	echo "obj" >onlyignored/file.o &&
	echo "archive" >onlyignored/lib.a
	)
'

test_expect_success 'status does not show directory containing only ignored files' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "onlyignored" ../actual
	)
'

# ── Global gitignore pattern: comment and blank lines ───────────────────────

test_expect_success 'setup: gitignore with comments and blank lines' '
	(
	cd repo &&
	cat >.gitignore <<-\EOF &&
	# This is a comment
	*.o
	
	# Another comment
	*.a
	build/
	
	tmp/
	*.log
	!important.log
	EOF
	git add .gitignore &&
	git commit -m "gitignore with comments"
	)
'

test_expect_success 'status still ignores *.o with comments in gitignore' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "main.o" ../actual &&
	! grep "util.o" ../actual
	)
'

test_expect_success 'status still ignores build/ with comments in gitignore' '
	(
	cd repo &&
	git status --porcelain >../actual &&
	! grep "build/" ../actual
	)
'

test_done
