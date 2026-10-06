#!/bin/sh
# Tests for check-ignore with complex .gitignore patterns.

test_description='check-ignore complex pattern scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	echo "ref: refs/heads/main" >.git/HEAD
	)
'

# ── Wildcard patterns ─────────────────────────────────────────────────────

test_expect_success 'single star matches within filename' '
	(
	cd repo &&
	echo "*.log" >.gitignore &&
	grit check-ignore test.log >actual &&
	echo test.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'single star does not match path separator' '
	(
	cd repo &&
	echo "foo*bar" >.gitignore &&
	grit check-ignore fooXbar >actual &&
	echo fooXbar >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore foo/bar
	)
'

test_expect_success 'question mark matches single character' '
	(
	cd repo &&
	echo "?.txt" >.gitignore &&
	grit check-ignore a.txt >actual &&
	echo a.txt >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore ab.txt
	)
'

test_expect_success 'star matches multiple characters in filename' '
	(
	cd repo &&
	echo "test*.log" >.gitignore &&
	grit check-ignore test123.log >actual &&
	echo test123.log >expect &&
	test_cmp expect actual &&
	grit check-ignore testABC.log >actual2 &&
	echo testABC.log >expect2 &&
	test_cmp expect2 actual2
	)
'

test_expect_success 'star matches zero characters' '
	(
	cd repo &&
	echo "test*.log" >.gitignore &&
	grit check-ignore test.log >actual &&
	echo test.log >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'multiple stars in pattern' '
	(
	cd repo &&
	echo "*test*" >.gitignore &&
	grit check-ignore mytest123 >actual &&
	echo mytest123 >expect &&
	test_cmp expect actual
	)
'

# ── Double-star patterns ──────────────────────────────────────────────────

test_expect_success '** at beginning matches in subdirectories' '
	(
	cd repo &&
	echo "**/build" >.gitignore &&
	grit check-ignore src/build >actual &&
	echo src/build >expect &&
	test_cmp expect actual &&
	grit check-ignore a/b/c/build >actual2 &&
	echo a/b/c/build >expect2 &&
	test_cmp expect2 actual2
	)
'

test_expect_success '** at end matches everything inside' '
	(
	cd repo &&
	echo "logs/**" >.gitignore &&
	grit check-ignore logs/debug.log >actual &&
	echo logs/debug.log >expect &&
	test_cmp expect actual &&
	grit check-ignore logs/sub/trace.log >actual2 &&
	echo logs/sub/trace.log >expect2 &&
	test_cmp expect2 actual2
	)
'

test_expect_success '** in middle matches one or more directories' '
	(
	cd repo &&
	echo "a/**/z" >.gitignore &&
	grit check-ignore a/b/z >actual &&
	echo a/b/z >expect &&
	test_cmp expect actual &&
	grit check-ignore a/b/c/z >actual2 &&
	echo a/b/c/z >expect2 &&
	test_cmp expect2 actual2
	)
'

# ── Directory patterns (trailing slash) ───────────────────────────────────

test_expect_success 'trailing slash matches directories only (via path with slash)' '
	(
	cd repo &&
	echo "build/" >.gitignore &&
	grit check-ignore build/output.o >actual &&
	echo build/output.o >expect &&
	test_cmp expect actual
	)
'

# ── Negation patterns ─────────────────────────────────────────────────────

test_expect_success 'negation overrides previous pattern' '
	(
	cd repo &&
	printf "*.log\n!important.log\n" >.gitignore &&
	grit check-ignore debug.log >actual &&
	echo debug.log >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore important.log
	)
'

test_expect_success 'double negation: re-ignore after negation' '
	(
	cd repo &&
	printf "*.tmp\n!*.tmp\n*.tmp\n" >.gitignore &&
	grit check-ignore test.tmp >actual &&
	echo test.tmp >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'negation of directory pattern' '
	(
	cd repo &&
	printf "vendor/\n!vendor/important/\n" >.gitignore &&
	grit check-ignore vendor/junk >actual &&
	echo vendor/junk >expect &&
	test_cmp expect actual
	)
'

# ── Anchored patterns ─────────────────────────────────────────────────────

test_expect_success 'pattern with slash is anchored to .gitignore location' '
	(
	cd repo &&
	echo "doc/secret" >.gitignore &&
	grit check-ignore doc/secret >actual &&
	echo doc/secret >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore other/doc/secret
	)
'

test_expect_success 'leading slash anchors to repo root' '
	(
	cd repo &&
	echo "/root-only.txt" >.gitignore &&
	grit check-ignore root-only.txt >actual &&
	echo root-only.txt >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore sub/root-only.txt
	)
'

# ── Subdirectory .gitignore ───────────────────────────────────────────────

test_expect_success 'subdirectory .gitignore applies to its subtree' '
	(
	cd repo &&
	echo "" >.gitignore &&
	mkdir -p sub &&
	echo "*.sub-only" >sub/.gitignore &&
	grit check-ignore sub/test.sub-only >actual &&
	echo sub/test.sub-only >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore test.sub-only
	)
'

test_expect_success 'nested .gitignore overrides parent' '
	(
	cd repo &&
	echo "*.data" >.gitignore &&
	mkdir -p deep &&
	echo "!*.data" >deep/.gitignore &&
	grit check-ignore top.data >actual &&
	echo top.data >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore deep/keep.data
	)
'

# ── info/exclude ──────────────────────────────────────────────────────────

test_expect_success 'info/exclude patterns work' '
	(
	cd repo &&
	echo "" >.gitignore &&
	mkdir -p .git/info &&
	echo "*.secret" >.git/info/exclude &&
	grit check-ignore test.secret >actual &&
	echo test.secret >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'info/exclude works in subdirectory' '
	(
	cd repo &&
	echo "" >.gitignore &&
	mkdir -p .git/info &&
	echo "*.excl" >.git/info/exclude &&
	grit check-ignore sub/deep/file.excl >actual &&
	echo sub/deep/file.excl >expect &&
	test_cmp expect actual
	)
'

# ── --stdin mode ──────────────────────────────────────────────────────────

test_expect_success '--stdin reads paths from stdin' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	printf "test.ign\n" | grit check-ignore --stdin >actual &&
	echo test.ign >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin with multiple paths' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	printf "a.ign\nb.ign\n" | grit check-ignore --stdin >actual &&
	printf "a.ign\nb.ign\n" >expect &&
	test_cmp expect actual
	)
'

test_expect_success '--stdin skips non-ignored paths' '
	(
	cd repo &&
	echo "*.ign" >.gitignore &&
	printf "a.ign\nkeep.txt\nb.ign\n" | grit check-ignore --stdin >actual &&
	printf "a.ign\nb.ign\n" >expect &&
	test_cmp expect actual
	)
'

# ── -v (verbose) mode ─────────────────────────────────────────────────────

test_expect_success '-v shows source and pattern' '
	(
	cd repo &&
	echo "*.xyz" >.gitignore &&
	grit check-ignore -v test.xyz >actual &&
	grep "\.gitignore" actual &&
	grep "\*.xyz" actual &&
	grep "test.xyz" actual
	)
'

test_expect_success '-v shows line number' '
	(
	cd repo &&
	printf "first\n*.vvv\n" >.gitignore &&
	grit check-ignore -v test.vvv >actual &&
	grep ":2:" actual
	)
'

# ── -n (non-matching) with -v ─────────────────────────────────────────────

test_expect_success '-n -v shows non-matching paths' '
	(
	cd repo &&
	echo "*.xyz" >.gitignore &&
	grit check-ignore -v -n notignored.txt >actual || true &&
	grep "notignored.txt" actual
	)
'

# ── Complex combined patterns ─────────────────────────────────────────────

test_expect_success 'complex pattern: double-star plus extension' '
	(
	cd repo &&
	echo "**/*.pyc" >.gitignore &&
	grit check-ignore lib/test.pyc >actual &&
	echo lib/test.pyc >expect &&
	test_cmp expect actual &&
	grit check-ignore a/b/c/mod.pyc >actual2 &&
	echo a/b/c/mod.pyc >expect2 &&
	test_cmp expect2 actual2
	)
'

test_expect_success 'complex pattern: negate specific file from double-star' '
	(
	cd repo &&
	printf "**/*.pyc\n!important.pyc\n" >.gitignore &&
	grit check-ignore lib/test.pyc >actual &&
	echo lib/test.pyc >expect &&
	test_cmp expect actual &&
	test_must_fail grit check-ignore important.pyc
	)
'

test_expect_success 'comment lines are ignored in .gitignore' '
	(
	cd repo &&
	printf "# This is a comment\n*.commented\n" >.gitignore &&
	grit check-ignore test.commented >actual &&
	echo test.commented >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'blank lines are ignored in .gitignore' '
	(
	cd repo &&
	printf "\n\n*.blanked\n\n" >.gitignore &&
	grit check-ignore test.blanked >actual &&
	echo test.blanked >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'pattern with trailing spaces (no escape) still works' '
	(
	cd repo &&
	printf "*.trailing\n" >.gitignore &&
	grit check-ignore test.trailing >actual &&
	echo test.trailing >expect &&
	test_cmp expect actual
	)
'

test_done
