#!/bin/sh
# Tests for config with multivalued keys, --get-all, --replace-all, --unset-all.

test_description='config multivalued keys and operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with multivar entries' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	cat >>.git/config <<-\EOF
	[remote "origin"]
		fetch = +refs/heads/*:refs/remotes/origin/*
		fetch = +refs/tags/*:refs/tags/*
		fetch = +refs/notes/*:refs/notes/*
	EOF
	)
'

# ── --get-all (legacy) ──────────────────────────────────────────────────────

test_expect_success 'config --get-all lists all values for multivar' '
	(
	cd repo &&
	git config --get-all remote.origin.fetch >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'config --get-all returns values in order' '
	(
	cd repo &&
	git config --get-all remote.origin.fetch >out &&
	cat >expected <<-\EOF &&
	+refs/heads/*:refs/remotes/origin/*
	+refs/tags/*:refs/tags/*
	+refs/notes/*:refs/notes/*
	EOF
	test_cmp expected out
	)
'

test_expect_success 'config --get-all for single-value key returns one line' '
	(
	cd repo &&
	git config --get-all user.name >out &&
	test_line_count = 1 out &&
	echo "Test User" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config --get-all for missing key fails' '
	(
	cd repo &&
	test_must_fail git config --get-all no.such.key
	)
'

# ── config get --all (new-style) ────────────────────────────────────────────

test_expect_success 'config get --all lists all values' '
	(
	cd repo &&
	git config get --all remote.origin.fetch >out &&
	test_line_count = 3 out
	)
'

test_expect_success 'config get --all preserves insertion order' '
	(
	cd repo &&
	git config get --all remote.origin.fetch >out &&
	head -1 out >first &&
	echo "+refs/heads/*:refs/remotes/origin/*" >expected &&
	test_cmp expected first
	)
'

test_expect_success 'config get without --all returns last value' '
	(
	cd repo &&
	git config remote.origin.fetch >out &&
	echo "+refs/notes/*:refs/notes/*" >expected &&
	test_cmp expected out
	)
'

# ── --replace-all (legacy) ──────────────────────────────────────────────────

test_expect_success 'setup fresh multivar for replace-all test' '
	(
	git init replace-repo &&
	cd replace-repo &&
	cat >>.git/config <<-\EOF
	[multi]
		val = alpha
		val = beta
		val = gamma
	EOF
	)
'

test_expect_success 'config --replace-all replaces last matching value' '
	(
	cd replace-repo &&
	git config --replace-all multi.val "replaced" &&
	git config --get-all multi.val >out &&
	grep "replaced" out
	)
'

# ── config set --all (new-style) ────────────────────────────────────────────

test_expect_success 'setup fresh multivar for set --all test' '
	(
	git init setall-repo &&
	cd setall-repo &&
	cat >>.git/config <<-\EOF
	[stuff]
		item = one
		item = two
		item = three
	EOF
	)
'

test_expect_success 'config set --all replaces last matching value' '
	(
	cd setall-repo &&
	git config set --all stuff.item "new-value" &&
	git config --get-all stuff.item >out &&
	grep "new-value" out
	)
'

test_expect_success 'config set on single-value key replaces it' '
	(
	cd setall-repo &&
	git config user.name "Original" &&
	git config set --all user.name "Replaced" &&
	git config user.name >out &&
	echo "Replaced" >expected &&
	test_cmp expected out
	)
'

# ── --unset-all (legacy) ────────────────────────────────────────────────────

test_expect_success 'setup multivar for unset-all tests' '
	(
	git init unset-repo &&
	cd unset-repo &&
	cat >>.git/config <<-\EOF
	[removeme]
		key = alpha
		key = beta
		key = gamma
	EOF
	)
'

test_expect_success 'config --unset-all removes all values for key' '
	(
	cd unset-repo &&
	git config --unset-all removeme.key &&
	test_must_fail git config removeme.key
	)
'

test_expect_success 'config --unset-all on missing key fails' '
	(
	cd unset-repo &&
	test_must_fail git config --unset-all no.such.multikey
	)
'

test_expect_success 'config --unset-all on single-value key removes it' '
	(
	cd unset-repo &&
	git config single.val "one" &&
	git config --unset-all single.val &&
	test_must_fail git config single.val
	)
'

# ── config unset --all (new-style) ──────────────────────────────────────────

test_expect_success 'setup multivar for unset --all (new-style)' '
	(
	git init unsetall2 &&
	cd unsetall2 &&
	cat >>.git/config <<-\EOF
	[ns]
		k = x
		k = y
		k = z
	EOF
	)
'

test_expect_success 'config unset --all removes all occurrences' '
	(
	cd unsetall2 &&
	git config unset --all ns.k &&
	test_must_fail git config ns.k
	)
'

test_expect_success 'config unset --all leaves section header behind' '
	(
	cd unsetall2 &&
	grep "\[ns\]" .git/config
	)
'

# ── Multivar in different sections ───────────────────────────────────────────

test_expect_success 'multivar keys are section-scoped' '
	(
	git init scope-repo &&
	cd scope-repo &&
	cat >>.git/config <<-\EOF
	[sec1]
		key = val1
	[sec2]
		key = val2
	EOF
	git config --get-all sec1.key >out &&
	test_line_count = 1 out &&
	echo "val1" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'unset-all one section does not affect another' '
	(
	cd scope-repo &&
	git config --unset-all sec1.key &&
	test_must_fail git config sec1.key &&
	git config sec2.key >out &&
	echo "val2" >expected &&
	test_cmp expected out
	)
'

# ── List with multivars ─────────────────────────────────────────────────────

test_expect_success 'config --list shows multivar entries' '
	(
	git init list-repo &&
	cd list-repo &&
	cat >>.git/config <<-\EOF
	[listtest]
		multi = a
		multi = b
		multi = c
	EOF
	git config --list >out &&
	grep "listtest.multi=a" out &&
	grep "listtest.multi=b" out &&
	grep "listtest.multi=c" out
	)
'

test_expect_success 'config list shows multivar entries (new-style)' '
	(
	cd list-repo &&
	git config list >out &&
	grep "listtest.multi=a" out &&
	grep "listtest.multi=b" out &&
	grep "listtest.multi=c" out
	)
'

# ── Edge cases ───────────────────────────────────────────────────────────────

test_expect_success 'multivar with empty values' '
	(
	git init empty-repo &&
	cd empty-repo &&
	cat >>.git/config <<-\EOF
	[empty]
		multi = 
		multi = 
	EOF
	git config --get-all empty.multi >out &&
	test_line_count = 2 out
	)
'

test_expect_success 'multivar with values containing equals signs' '
	(
	git init special-repo &&
	cd special-repo &&
	cat >>.git/config <<-\EOF
	[special]
		multi = foo=bar
		multi = baz=qux
	EOF
	git config --get-all special.multi >out &&
	test_line_count = 2 out &&
	grep "foo=bar" out &&
	grep "baz=qux" out
	)
'

test_expect_success 'multivar with values containing spaces' '
	(
	cd special-repo &&
	cat >>.git/config <<-\EOF
	[spaced]
		multi = hello world
		multi = foo bar baz
	EOF
	git config --get-all spaced.multi >out &&
	test_line_count = 2 out &&
	grep "hello world" out
	)
'

test_expect_success 'unset-all then re-set the key works' '
	(
	git init reuse-repo &&
	cd reuse-repo &&
	cat >>.git/config <<-\EOF
	[reuse]
		key = old1
		key = old2
	EOF
	git config --unset-all reuse.key &&
	test_must_fail git config reuse.key &&
	git config reuse.key "new-value" &&
	git config reuse.key >out &&
	echo "new-value" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'get-all on key with subsection' '
	(
	git init subsec-repo &&
	cd subsec-repo &&
	cat >>.git/config <<-\EOF
	[remote "upstream"]
		fetch = +refs/heads/*:refs/remotes/upstream/*
		fetch = +refs/pull/*/head:refs/remotes/upstream/pr/*
	EOF
	git config get --all remote.upstream.fetch >out &&
	test_line_count = 2 out &&
	grep "refs/pull" out
	)
'

test_done
