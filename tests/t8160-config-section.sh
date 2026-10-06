#!/bin/sh
# Tests for config section operations: --rename-section, --remove-section,
# and their new-style subcommand equivalents.

test_description='config section operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository with several sections' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	git config color.ui auto &&
	git config color.diff always &&
	git config alias.co checkout &&
	git config alias.br branch &&
	git config alias.st status
	)
'

# ── rename-section (new-style subcommand) ────────────────────────────────────

test_expect_success 'rename-section renames simple section' '
	(
	cd repo &&
	git config rename-section alias shortcuts &&
	git config shortcuts.co >out &&
	echo "checkout" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'rename-section preserves all keys' '
	(
	cd repo &&
	git config shortcuts.br >out1 &&
	echo "branch" >expected1 &&
	test_cmp expected1 out1 &&
	git config shortcuts.st >out2 &&
	echo "status" >expected2 &&
	test_cmp expected2 out2
	)
'

test_expect_success 'rename-section removes old section' '
	(
	cd repo &&
	test_must_fail git config alias.co
	)
'

test_expect_success 'rename-section with subsection' '
	(
	cd repo &&
	git config "remote.origin.url" "https://example.com/repo.git" &&
	git config "remote.origin.fetch" "+refs/heads/*:refs/remotes/origin/*" &&
	git config rename-section "remote.origin" "remote.upstream" &&
	git config remote.upstream.url >out &&
	echo "https://example.com/repo.git" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'rename-section with subsection removes old' '
	(
	cd repo &&
	test_must_fail git config remote.origin.url
	)
'

# ── --rename-section (legacy flag) ──────────────────────────────────────────

test_expect_success 'setup for legacy rename-section' '
	(
	git init legacy-rename &&
	cd legacy-rename &&
	git config sec1.key1 val1 &&
	git config sec1.key2 val2
	)
'

test_expect_success '--rename-section renames section' '
	(
	cd legacy-rename &&
	git config --rename-section sec1 sec2 &&
	git config sec2.key1 >out &&
	echo "val1" >expected &&
	test_cmp expected out
	)
'

test_expect_success '--rename-section removes old section' '
	(
	cd legacy-rename &&
	test_must_fail git config sec1.key1
	)
'

# ── remove-section (new-style subcommand) ────────────────────────────────────

test_expect_success 'remove-section removes a section entirely' '
	(
	cd repo &&
	git config remove-section shortcuts &&
	test_must_fail git config shortcuts.co &&
	test_must_fail git config shortcuts.br &&
	test_must_fail git config shortcuts.st
	)
'

test_expect_success 'remove-section does not affect other sections' '
	(
	cd repo &&
	git config user.name >out &&
	echo "Test User" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'remove-section with subsection' '
	(
	cd repo &&
	git config remove-section "remote.upstream" &&
	test_must_fail git config remote.upstream.url &&
	test_must_fail git config remote.upstream.fetch
	)
'

test_expect_success 'remove-section on nonexistent section fails' '
	(
	cd repo &&
	test_must_fail git config remove-section nosuch
	)
'

# ── --remove-section (legacy flag) ──────────────────────────────────────────

test_expect_success 'setup for legacy remove-section' '
	(
	git init legacy-remove &&
	cd legacy-remove &&
	git config old.key1 val1 &&
	git config old.key2 val2 &&
	git config keep.key1 stay
	)
'

test_expect_success '--remove-section removes the section' '
	(
	cd legacy-remove &&
	git config --remove-section old &&
	test_must_fail git config old.key1 &&
	test_must_fail git config old.key2
	)
'

test_expect_success '--remove-section preserves other sections' '
	(
	cd legacy-remove &&
	git config keep.key1 >out &&
	echo "stay" >expected &&
	test_cmp expected out
	)
'

# ── rename then remove ──────────────────────────────────────────────────────

test_expect_success 'rename then remove works' '
	(
	git init chain-repo &&
	cd chain-repo &&
	git config temp.k1 v1 &&
	git config temp.k2 v2 &&
	git config rename-section temp renamed &&
	git config renamed.k1 >out &&
	echo "v1" >expected &&
	test_cmp expected out &&
	git config remove-section renamed &&
	test_must_fail git config renamed.k1
	)
'

# ── section with special characters ─────────────────────────────────────────

test_expect_success 'rename section with hyphenated name' '
	(
	git init hyphen-repo &&
	cd hyphen-repo &&
	git config my-section.key value &&
	git config rename-section my-section your-section &&
	git config your-section.key >out &&
	echo "value" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'remove section with hyphenated name' '
	(
	cd hyphen-repo &&
	git config remove-section your-section &&
	test_must_fail git config your-section.key
	)
'

test_expect_success 'rename section with numeric name' '
	(
	git init num-repo &&
	cd num-repo &&
	git config sec123.key value &&
	git config rename-section sec123 sec456 &&
	git config sec456.key >out &&
	echo "value" >expected &&
	test_cmp expected out
	)
'

# ── verify config file structure after operations ────────────────────────────

test_expect_success 'remove-section cleans config file' '
	(
	git init clean-repo &&
	cd clean-repo &&
	git config extra.key value &&
	git config remove-section extra &&
	! grep "\[extra\]" .git/config
	)
'

test_expect_success 'rename-section updates header in config file' '
	(
	git init header-repo &&
	cd header-repo &&
	git config oldsec.key value &&
	git config rename-section oldsec newsec &&
	! grep "\[oldsec\]" .git/config &&
	grep "\[newsec\]" .git/config
	)
'

test_expect_success 'rename-section with subsection updates header' '
	(
	cd header-repo &&
	git config "branch.main.remote" origin &&
	git config rename-section "branch.main" "branch.develop" &&
	grep "branch.*develop" .git/config &&
	! grep "branch.*main" .git/config
	)
'

# ── remove-section on core does not break repo ──────────────────────────────

test_expect_success 'rename-section to name that already has entries merges' '
	(
	git init merge-repo &&
	cd merge-repo &&
	git config src.key1 val1 &&
	git config dst.key2 val2 &&
	git config rename-section src dst &&
	git config dst.key1 >out &&
	echo "val1" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'remove-section on section with many keys removes all' '
	(
	git init many-repo &&
	cd many-repo &&
	git config bulk.a 1 &&
	git config bulk.b 2 &&
	git config bulk.c 3 &&
	git config bulk.d 4 &&
	git config bulk.e 5 &&
	git config remove-section bulk &&
	test_must_fail git config bulk.a &&
	test_must_fail git config bulk.e
	)
'

test_expect_success 'can remove non-core sections without breaking repo' '
	(
	git init safe-repo &&
	cd safe-repo &&
	git config custom.key value &&
	git config remove-section custom &&
	git config core.bare >out &&
	echo "false" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'rename-section twice in sequence' '
	(
	git init twice-repo &&
	cd twice-repo &&
	git config first.key value &&
	git config rename-section first second &&
	git config rename-section second third &&
	git config third.key >out &&
	echo "value" >expected &&
	test_cmp expected out &&
	test_must_fail git config first.key &&
	test_must_fail git config second.key
	)
'

test_done
