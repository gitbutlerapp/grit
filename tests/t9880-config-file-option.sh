#!/bin/sh
# Tests for grit config --file / -f option and related file-based config.

test_description='grit config -f / --file reads and writes to custom config files'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "content" >file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: --file write and read
###########################################################################

test_expect_success 'config --file writes to custom file' '
	(
	cd repo &&
	grit config --file custom.cfg user.name "Custom User" &&
	test -f custom.cfg
	)
'

test_expect_success 'config --file reads back written value' '
	(
	cd repo &&
	grit config --file custom.cfg user.name "Custom User" &&
	grit config --file custom.cfg --get user.name >actual &&
	echo "Custom User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file matches real git' '
	(
	cd repo &&
	grit config --file custom.cfg user.name "Custom User" &&
	"$REAL_GIT" config --file custom.cfg --get user.name >expect &&
	grit config --file custom.cfg --get user.name >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'config -f short form works' '
	(
	cd repo &&
	grit config -f short.cfg user.name "Short Form" &&
	grit config -f short.cfg --get user.name >actual &&
	echo "Short Form" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file can set email' '
	(
	cd repo &&
	grit config --file custom.cfg user.email "custom@example.com" &&
	grit config --file custom.cfg --get user.email >actual &&
	echo "custom@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file multiple keys in same file' '
	(
	cd repo &&
	grit config --file multi.cfg core.bare "false" &&
	grit config --file multi.cfg core.autocrlf "input" &&
	grit config --file multi.cfg --get core.bare >actual_bare &&
	grit config --file multi.cfg --get core.autocrlf >actual_crlf &&
	echo "false" >expect_bare &&
	echo "input" >expect_crlf &&
	test_cmp expect_bare actual_bare &&
	test_cmp expect_crlf actual_crlf
	)
'

test_expect_success 'config --file does not affect local config' '
	(
	cd repo &&
	grit config --file separate.cfg user.name "Separate" &&
	grit config --get user.name >actual &&
	echo "Test User" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file creates file if not exists' '
	(
	cd repo &&
	rm -f brand-new.cfg &&
	grit config --file brand-new.cfg section.key "value" &&
	test -f brand-new.cfg
	)
'

test_expect_success 'config --file list shows entries from file' '
	(
	cd repo &&
	grit config --file list-test.cfg aa.bb "cc" &&
	grit config --file list-test.cfg dd.ee "ff" &&
	grit config --file list-test.cfg --list >actual &&
	grep "aa.bb=cc" actual &&
	grep "dd.ee=ff" actual
	)
'

test_expect_success 'config --file list matches real git' '
	(
	cd repo &&
	grit config --file list-cmp.cfg x.y "z" &&
	grit config --file list-cmp.cfg a.b "c" &&
	grit config --file list-cmp.cfg --list | sort >actual &&
	"$REAL_GIT" config --file list-cmp.cfg --list | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: --file unset
###########################################################################

test_expect_success 'config --file unset removes key' '
	(
	cd repo &&
	grit config --file unset.cfg remove.me "gone" &&
	grit config --file unset.cfg --get remove.me >actual &&
	echo "gone" >expect &&
	test_cmp expect actual &&
	grit config --file unset.cfg --unset remove.me &&
	test_must_fail grit config --file unset.cfg --get remove.me
	)
'

test_expect_success 'config --file unset matches real git' '
	(
	cd repo &&
	grit config --file unset2.cfg del.key "val" &&
	grit config --file unset2.cfg --unset del.key &&
	test_must_fail grit config --file unset2.cfg --get del.key &&
	test_must_fail "$REAL_GIT" config --file unset2.cfg --get del.key
	)
'

###########################################################################
# Section 4: --file overwrite
###########################################################################

test_expect_success 'config --file overwrites existing value' '
	(
	cd repo &&
	grit config --file overwrite.cfg key.val "old" &&
	grit config --file overwrite.cfg key.val "new" &&
	grit config --file overwrite.cfg --get key.val >actual &&
	echo "new" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file overwrite matches real git' '
	(
	cd repo &&
	grit config --file ow2.cfg key.val "first" &&
	grit config --file ow2.cfg key.val "second" &&
	grit config --file ow2.cfg --get key.val >actual &&
	"$REAL_GIT" config --file ow2.cfg --get key.val >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 5: --file with various value types
###########################################################################

test_expect_success 'config --file with boolean true' '
	(
	cd repo &&
	grit config --file types.cfg bool.key "true" &&
	grit config --file types.cfg --get bool.key >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file with integer value' '
	(
	cd repo &&
	grit config --file types.cfg int.key "42" &&
	grit config --file types.cfg --get int.key >actual &&
	echo "42" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file with path-like value' '
	(
	cd repo &&
	grit config --file types.cfg path.key "/some/path/here" &&
	grit config --file types.cfg --get path.key >actual &&
	echo "/some/path/here" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file with value containing spaces' '
	(
	cd repo &&
	grit config --file types.cfg spaced.key "hello world foo" &&
	grit config --file types.cfg --get spaced.key >actual &&
	echo "hello world foo" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file with empty-ish section' '
	(
	cd repo &&
	grit config --file types.cfg my.section.deep "val" &&
	grit config --file types.cfg --get my.section.deep >actual &&
	echo "val" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: --file with absolute path
###########################################################################

test_expect_success 'config --file with absolute path' '
	(
	cd repo &&
	abs_path="$(pwd)/abs-config.cfg" &&
	grit config --file "$abs_path" abs.key "absolute" &&
	grit config --file "$abs_path" --get abs.key >actual &&
	echo "absolute" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file with absolute path outside repo' '
	(
	cd repo &&
	mkdir -p ../outside &&
	abs_path="$(cd ../outside && pwd)/ext.cfg" &&
	grit config --file "$abs_path" ext.key "outside" &&
	grit config --file "$abs_path" --get ext.key >actual &&
	echo "outside" >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: --global vs --file isolation
###########################################################################

test_expect_success 'config --global does not bleed into --file' '
	(
	cd repo &&
	grit config --global user.name "Global User" &&
	grit config --file isolated.cfg other.key "val" &&
	test_must_fail grit config --file isolated.cfg --get user.name
	)
'

test_expect_success 'config --file does not bleed into --global' '
	(
	cd repo &&
	grit config --file isolated2.cfg private.key "secret" &&
	test_must_fail grit config --global --get private.key
	)
'

###########################################################################
# Section 8: --file with --get-regexp
###########################################################################

test_expect_success 'config --file --get-regexp works' '
	(
	cd repo &&
	grit config --file regexp.cfg foo.bar "one" &&
	grit config --file regexp.cfg foo.baz "two" &&
	grit config --file regexp.cfg other.key "three" &&
	grit config --file regexp.cfg --get-regexp "foo" >actual &&
	grep "foo.bar" actual &&
	grep "foo.baz" actual &&
	! grep "other.key" actual
	)
'

test_expect_success 'config --file --get-regexp matches real git' '
	(
	cd repo &&
	grit config --file regexp2.cfg sec.a "1" &&
	grit config --file regexp2.cfg sec.b "2" &&
	grit config --file regexp2.cfg --get-regexp "sec" | sort >actual &&
	"$REAL_GIT" config --file regexp2.cfg --get-regexp "sec" | sort >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 9: --file with sections
###########################################################################

test_expect_success 'config --file remove-section removes entire section' '
	(
	cd repo &&
	grit config --file sec.cfg mysec.a "1" &&
	grit config --file sec.cfg mysec.b "2" &&
	grit config --file sec.cfg other.c "3" &&
	grit config --file sec.cfg --remove-section mysec &&
	test_must_fail grit config --file sec.cfg --get mysec.a &&
	test_must_fail grit config --file sec.cfg --get mysec.b &&
	grit config --file sec.cfg --get other.c >actual &&
	echo "3" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file rename-section works' '
	(
	cd repo &&
	grit config --file ren.cfg old.key "val" &&
	grit config --file ren.cfg --rename-section old new &&
	grit config --file ren.cfg --get new.key >actual &&
	echo "val" >expect &&
	test_cmp expect actual &&
	test_must_fail grit config --file ren.cfg --get old.key
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'config --file nonexistent file for read fails' '
	(
	cd repo &&
	test_must_fail grit config --file no-such-file.cfg --get some.key
	)
'

test_expect_success 'config --file with nested subsection key' '
	(
	cd repo &&
	grit config --file nested.cfg "branch.main.remote" "origin" &&
	grit config --file nested.cfg --get "branch.main.remote" >actual &&
	echo "origin" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file round-trip preserves format' '
	(
	cd repo &&
	grit config --file rt.cfg section.key "value" &&
	"$REAL_GIT" config --file rt.cfg --get section.key >expect &&
	grit config --file rt.cfg --get section.key >actual &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file set and list single entry' '
	(
	cd repo &&
	grit config --file single.cfg only.key "only" &&
	grit config --file single.cfg --list >actual &&
	test_line_count = 1 actual
	)
'

test_done
