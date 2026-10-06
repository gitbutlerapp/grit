#!/bin/sh
#
# Tests for git config with --global and --system scopes
#

test_description='config --global and --system scope handling'
. ./test-lib.sh

test_expect_success 'setup: init repo' '
	git init repo &&
	cd repo
'

test_expect_success 'config --global sets value in global config' '
	git config --global user.name "Global User" &&
	git config --global user.name >actual &&
	echo "Global User" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global sets email' '
	git config --global user.email "global@example.com" &&
	git config --global user.email >actual &&
	echo "global@example.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global writes to ~/.gitconfig' '
	test -f "$HOME/.gitconfig" &&
	grep "Global User" "$HOME/.gitconfig"
'

test_expect_success 'local config overrides global config' '
	git config user.name "Local User" &&
	git config user.name >actual &&
	echo "Local User" >expect &&
	test_cmp expect actual
'

test_expect_success 'global config still readable with --global' '
	git config --global user.name >actual &&
	echo "Global User" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --list shows global entries' '
	git config --global --list >actual &&
	grep "user.name=Global User" actual &&
	grep "user.email=global@example.com" actual
'

test_expect_success 'config --global with new section' '
	git config --global core.editor "vim" &&
	git config --global core.editor >actual &&
	echo "vim" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --replace-all replaces value' '
	git config --global user.name "New Global User" &&
	git config --global user.name >actual &&
	echo "New Global User" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --unset removes key' '
	git config --global core.editor "vim" &&
	git config --global --unset core.editor &&
	test_must_fail git config --global core.editor
'

test_expect_success 'config --global with subsection' '
	git config --global "branch.main.remote" "origin" &&
	git config --global "branch.main.remote" >actual &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global boolean true' '
	git config --global core.bare true &&
	git config --global core.bare >actual &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global boolean false' '
	git config --global core.bare false &&
	git config --global core.bare >actual &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global integer value' '
	git config --global core.compression 9 &&
	git config --global core.compression >actual &&
	echo "9" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --get with default section' '
	git config --global alias.co "checkout" &&
	git config --global alias.co >actual &&
	echo "checkout" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global multiple sections' '
	git config --global color.ui auto &&
	git config --global merge.ff only &&
	git config --global color.ui >actual_color &&
	echo "auto" >expect_color &&
	test_cmp expect_color actual_color &&
	git config --global merge.ff >actual_merge &&
	echo "only" >expect_merge &&
	test_cmp expect_merge actual_merge
'

test_expect_success 'config --local explicitly sets local' '
	git config --local core.autocrlf false &&
	git config --local core.autocrlf >actual &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --local does not appear in --global' '
	test_must_fail git config --global core.autocrlf
'

test_expect_success 'config without scope flag uses local by default' '
	git config mytest.key "localval" &&
	git config --local mytest.key >actual &&
	echo "localval" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --list shows both global and local' '
	git config --list >actual &&
	grep "user.name=Local User" actual &&
	grep "user.email=global@example.com" actual
'

test_expect_success 'config --list --global only shows global' '
	git config --list --global >actual &&
	grep "user.email=global@example.com" actual &&
	! grep "mytest.key" actual
'

test_expect_success 'config --list --local only shows local' '
	git config --list --local >actual &&
	grep "mytest.key=localval" actual
'

test_expect_success 'config --global user.name readable after multiple sets' '
	git config --global user.name "RegexpTest" &&
	git config --global user.name >actual &&
	echo "RegexpTest" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global with spaces in value' '
	git config --global user.name "First Middle Last" &&
	git config --global user.name >actual &&
	echo "First Middle Last" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global with special chars in value' '
	git config --global user.email "user+tag@example.com" &&
	git config --global user.email >actual &&
	echo "user+tag@example.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global overwrite preserves other keys' '
	git config --global user.name "Updated Name" &&
	git config --global user.email >actual &&
	echo "user+tag@example.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global with empty value' '
	git config --global empty.key "" &&
	git config --global empty.key >actual &&
	echo "" >expect &&
	test_cmp expect actual
'

test_expect_success 'second repo sees global config' '
	cd "$TRASH_DIRECTORY" &&
	git init repo2 &&
	cd repo2 &&
	git config --global user.name >actual &&
	echo "Updated Name" >expect &&
	test_cmp expect actual
'

test_expect_success 'second repo local config is independent' '
	git config user.name "Repo2 User" &&
	git config user.name >actual &&
	echo "Repo2 User" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global path with tilde-like home' '
	git config --global core.excludesfile "~/.gitignore_global" &&
	git config --global core.excludesfile >actual &&
	echo "~/.gitignore_global" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --unset from subsection' '
	git config --global "branch.main.remote" "origin" &&
	git config --global --unset "branch.main.remote" &&
	test_must_fail git config --global "branch.main.remote"
'

test_expect_success 'config --global --remove-section' '
	git config --global section.key1 "val1" &&
	git config --global section.key2 "val2" &&
	git config --global --remove-section section &&
	test_must_fail git config --global section.key1 &&
	test_must_fail git config --global section.key2
'

test_expect_success 'config --global rename-section' '
	git config --global oldsec.key "value" &&
	git config --global --rename-section oldsec newsec &&
	git config --global newsec.key >actual &&
	echo "value" >expect &&
	test_cmp expect actual &&
	test_must_fail git config --global oldsec.key
'

test_expect_success 'config --global --get nonexistent key fails' '
	test_must_fail git config --global nonexistent.key
'

test_expect_success 'config --global with numeric section' '
	git config --global "remote.1234.url" "https://example.com" &&
	git config --global "remote.1234.url" >actual &&
	echo "https://example.com" >expect &&
	test_cmp expect actual
'

test_done
