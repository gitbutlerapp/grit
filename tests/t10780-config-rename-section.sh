#!/bin/sh
# Test grit config rename-section and remove-section subcommands,
# verifying section manipulation, edge cases, and parity with git.

test_description='grit config rename-section and remove-section'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup repository' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "test@test.com" &&
	git config user.name "Test" &&
	echo "content" >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic rename-section
###########################################################################

test_expect_success 'config set creates section' '
	(
	cd repo &&
	grit config set foo.bar baz &&
	grit config get foo.bar >out &&
	echo "baz" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'rename-section renames a section' '
	(
	cd repo &&
	grit config rename-section foo newfoo &&
	grit config get newfoo.bar >out &&
	echo "baz" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'old section no longer exists after rename' '
	(
	cd repo &&
	test_must_fail grit config get foo.bar
	)
'

test_expect_success 'rename-section matches git behavior' '
	(
	cd repo &&
	grit config set alpha.key1 val1 &&
	grit config rename-section alpha beta &&
	grit config get beta.key1 >grit_out &&
	git config get beta.key1 >git_out &&
	test_cmp git_out grit_out
	)
'

test_expect_success 'rename-section with multiple keys' '
	(
	cd repo &&
	grit config set sect.a 1 &&
	grit config set sect.b 2 &&
	grit config set sect.c 3 &&
	grit config rename-section sect newsect &&
	grit config get newsect.a >out_a &&
	grit config get newsect.b >out_b &&
	grit config get newsect.c >out_c &&
	echo "1" >exp_a &&
	echo "2" >exp_b &&
	echo "3" >exp_c &&
	test_cmp exp_a out_a &&
	test_cmp exp_b out_b &&
	test_cmp exp_c out_c
	)
'

test_expect_success 'rename-section preserves values with special characters' '
	(
	cd repo &&
	grit config set special.path "/home/user/my dir/file" &&
	grit config rename-section special moved &&
	grit config get moved.path >out &&
	echo "/home/user/my dir/file" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'rename-section with subsection' '
	(
	cd repo &&
	grit config set "remote.origin.url" "https://example.com/repo.git" &&
	grit config rename-section "remote.origin" "remote.upstream" &&
	grit config get "remote.upstream.url" >out &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'rename-section fails for nonexistent section' '
	(
	cd repo &&
	test_must_fail grit config rename-section nosuchsection something
	)
'

test_expect_success 'rename-section result matches git' '
	(
	cd repo &&
	grit config set comp.x hello &&
	grit config rename-section comp comp2 &&
	grit config list >grit_list &&
	git config --list >git_list &&
	grep "comp2.x=hello" grit_list &&
	grep "comp2.x=hello" git_list
	)
'

###########################################################################
# Section 3: remove-section
###########################################################################

test_expect_success 'remove-section removes a section' '
	(
	cd repo &&
	grit config set removeme.key val &&
	grit config get removeme.key >out &&
	echo "val" >expect &&
	test_cmp expect out &&
	grit config remove-section removeme &&
	test_must_fail grit config get removeme.key
	)
'

test_expect_success 'remove-section removes all keys in section' '
	(
	cd repo &&
	grit config set delsect.a 1 &&
	grit config set delsect.b 2 &&
	grit config set delsect.c 3 &&
	grit config remove-section delsect &&
	test_must_fail grit config get delsect.a &&
	test_must_fail grit config get delsect.b &&
	test_must_fail grit config get delsect.c
	)
'

test_expect_success 'remove-section does not affect other sections' '
	(
	cd repo &&
	grit config set keep.key kept &&
	grit config set drop.key dropped &&
	grit config remove-section drop &&
	grit config get keep.key >out &&
	echo "kept" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'remove-section fails for nonexistent section' '
	(
	cd repo &&
	test_must_fail grit config remove-section nonexistent
	)
'

test_expect_success 'remove-section matches git' '
	(
	cd repo &&
	grit config set gitsync.val 42 &&
	grit config remove-section gitsync &&
	test_must_fail grit config get gitsync.val &&
	test_must_fail git config get gitsync.val
	)
'

###########################################################################
# Section 4: Legacy --rename-section / --remove-section flags
###########################################################################

test_expect_success 'legacy --rename-section flag works' '
	(
	cd repo &&
	grit config set oldflag.k v &&
	grit config --rename-section oldflag newflag &&
	grit config get newflag.k >out &&
	echo "v" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'legacy --remove-section flag works' '
	(
	cd repo &&
	grit config set legdel.k v &&
	grit config --remove-section legdel &&
	test_must_fail grit config get legdel.k
	)
'

###########################################################################
# Section 5: Config scopes
###########################################################################

test_expect_success 'rename-section with --local scope' '
	(
	cd repo &&
	grit config --local set scopetest.key val &&
	grit config --local rename-section scopetest scopemoved &&
	grit config --local get scopemoved.key >out &&
	echo "val" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'remove-section with --local scope' '
	(
	cd repo &&
	grit config --local set scopedel.key val &&
	grit config --local remove-section scopedel &&
	test_must_fail grit config --local get scopedel.key
	)
'

###########################################################################
# Section 6: Config list after operations
###########################################################################

test_expect_success 'config list does not show removed section' '
	(
	cd repo &&
	grit config set listdel.x 1 &&
	grit config remove-section listdel &&
	grit config list >out &&
	! grep "listdel" out
	)
'

test_expect_success 'config list shows renamed section' '
	(
	cd repo &&
	grit config set listrn.x 1 &&
	grit config rename-section listrn listrenamed &&
	grit config list >out &&
	grep "listrenamed.x=1" out &&
	! grep "listrn.x" out
	)
'

test_expect_success 'config list matches git after rename' '
	(
	cd repo &&
	grit config set cmplist.y 2 &&
	grit config rename-section cmplist cmplist2 &&
	grit config list >grit_out &&
	git config --list >git_out &&
	grep "cmplist2.y" grit_out &&
	grep "cmplist2.y" git_out
	)
'

###########################################################################
# Section 7: Edge cases with booleans and types
###########################################################################

test_expect_success 'rename-section preserves boolean values' '
	(
	cd repo &&
	grit config set boolsect.flag true &&
	grit config rename-section boolsect boolmoved &&
	grit config --bool get boolmoved.flag >out &&
	echo "true" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'rename-section preserves integer values' '
	(
	cd repo &&
	grit config set intsect.count 42 &&
	grit config rename-section intsect intmoved &&
	grit config --int get intmoved.count >out &&
	echo "42" >expect &&
	test_cmp expect out
	)
'

###########################################################################
# Section 8: Multiple renames
###########################################################################

test_expect_success 'rename a section twice' '
	(
	cd repo &&
	grit config set first.k v &&
	grit config rename-section first second &&
	grit config rename-section second third &&
	grit config get third.k >out &&
	echo "v" >expect &&
	test_cmp expect out &&
	test_must_fail grit config get first.k &&
	test_must_fail grit config get second.k
	)
'

test_expect_success 'remove then re-create section' '
	(
	cd repo &&
	grit config set ephemeral.k 1 &&
	grit config remove-section ephemeral &&
	grit config set ephemeral.k 2 &&
	grit config get ephemeral.k >out &&
	echo "2" >expect &&
	test_cmp expect out
	)
'

###########################################################################
# Section 9: Config file contents verification
###########################################################################

test_expect_success 'rename-section updates .git/config file' '
	(
	cd repo &&
	grit config set filecheck.key val &&
	grit config rename-section filecheck filemoved &&
	grep "\[filemoved\]" .git/config &&
	! grep "\[filecheck\]" .git/config
	)
'

test_expect_success 'remove-section cleans .git/config file' '
	(
	cd repo &&
	grit config set fileclean.key val &&
	grit config remove-section fileclean &&
	! grep "\[fileclean\]" .git/config &&
	! grep "fileclean" .git/config
	)
'

###########################################################################
# Section 10: Additional edge cases
###########################################################################

test_expect_success 'rename-section with dotted subsection name' '
	(
	cd repo &&
	grit config set "branch.main.remote" origin &&
	grit config rename-section "branch.main" "branch.dev" &&
	grit config get "branch.dev.remote" >out &&
	echo "origin" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'remove-section with subsection' '
	(
	cd repo &&
	grit config set "submodule.lib.path" lib &&
	grit config set "submodule.lib.url" https://example.com &&
	grit config remove-section "submodule.lib" &&
	test_must_fail grit config get "submodule.lib.path" &&
	test_must_fail grit config get "submodule.lib.url"
	)
'

test_expect_success 'rename-section idempotent when same name' '
	(
	cd repo &&
	grit config set same.key val &&
	grit config rename-section same same &&
	grit config get same.key >out &&
	echo "val" >expect &&
	test_cmp expect out
	)
'

test_expect_success 'config set after rename adds to new section' '
	(
	cd repo &&
	grit config set addafter.a 1 &&
	grit config rename-section addafter addafter2 &&
	grit config set addafter2.b 2 &&
	grit config get addafter2.a >out_a &&
	grit config get addafter2.b >out_b &&
	echo "1" >exp_a &&
	echo "2" >exp_b &&
	test_cmp exp_a out_a &&
	test_cmp exp_b out_b
	)
'

test_expect_success 'rename-section with empty value key' '
	(
	cd repo &&
	grit config set emptyval.key "" &&
	grit config rename-section emptyval emptyval2 &&
	grit config get emptyval2.key >out &&
	echo "" >expect &&
	test_cmp expect out
	)
'

test_done
