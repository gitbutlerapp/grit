#!/bin/sh
# Test grit config list, config file (--file/-f), config scoping
# (--local, --global, --system), get/set/unset operations, and
# various config interactions.

test_description='grit config list and --file operations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with basic config' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User"
	)
'

# --- config list ---

test_expect_success 'config list shows user.email' '
	(
	cd repo &&
	grit config list >actual &&
	grep "user.email=test@example.com" actual
	)
'

test_expect_success 'config list shows user.name' '
	(
	cd repo &&
	grit config list >actual &&
	grep "user.name=Test User" actual
	)
'

test_expect_success 'config -l (legacy) lists entries' '
	(
	cd repo &&
	grit config -l >actual &&
	grep "user.email" actual &&
	grep "user.name" actual
	)
'

test_expect_success 'config list shows multiple entries' '
	(
	cd repo &&
	grit config core.autocrlf false &&
	grit config list >actual &&
	grep "core.autocrlf=false" actual &&
	grep "user.email" actual
	)
'

# --- config --file ---

test_expect_success 'config --file writes to custom file' '
	(
	cd repo &&
	grit config --file custom.cfg custom.key "hello" &&
	test -f custom.cfg &&
	grep "hello" custom.cfg
	)
'

test_expect_success 'config --file reads from custom file' '
	(
	cd repo &&
	grit config --file custom.cfg custom.key >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file list shows custom config' '
	(
	cd repo &&
	grit config --file custom.cfg --list >actual &&
	grep "custom.key=hello" actual
	)
'

test_expect_success 'config -f shorthand works for --file' '
	(
	cd repo &&
	grit config -f custom.cfg custom.key >actual &&
	echo "hello" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --file can store multiple keys' '
	(
	cd repo &&
	grit config --file multi.cfg alpha.one "1" &&
	grit config --file multi.cfg alpha.two "2" &&
	grit config --file multi.cfg beta.three "3" &&
	grit config --file multi.cfg --list >actual &&
	grep "alpha.one=1" actual &&
	grep "alpha.two=2" actual &&
	grep "beta.three=3" actual
	)
'

test_expect_success 'config --file does not affect repo config' '
	(
	cd repo &&
	grit config --file isolated.cfg iso.key "secret" &&
	grit config list >actual &&
	! grep "iso.key" actual
	)
'

test_expect_success 'config --file with absolute path' '
	(
	cd repo &&
	grit config --file "$(pwd)/abs.cfg" abs.key "absolute" &&
	grit config --file "$(pwd)/abs.cfg" abs.key >actual &&
	echo "absolute" >expect &&
	test_cmp expect actual
	)
'

# --- config get subcommand ---

test_expect_success 'config get retrieves a value' '
	(
	cd repo &&
	grit config set retrieve.test "found_it" &&
	grit config get retrieve.test >actual &&
	echo "found_it" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config get nonexistent key fails' '
	(
	cd repo &&
	test_must_fail grit config get no.such.key
	)
'

test_expect_success 'config --get (legacy) retrieves a value' '
	(
	cd repo &&
	grit config --get user.email >actual &&
	echo "test@example.com" >expect &&
	test_cmp expect actual
	)
'

# --- config set subcommand ---

test_expect_success 'config set creates new key' '
	(
	cd repo &&
	grit config set brand.new "fresh" &&
	grit config get brand.new >actual &&
	echo "fresh" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set overwrites existing key' '
	(
	cd repo &&
	grit config set user.email "new@example.com" &&
	grit config get user.email >actual &&
	echo "new@example.com" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set with value containing spaces' '
	(
	cd repo &&
	grit config set spaced.key "value with spaces" &&
	grit config get spaced.key >actual &&
	echo "value with spaces" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set with value containing equals sign' '
	(
	cd repo &&
	grit config set eq.key "a=b=c" &&
	grit config get eq.key >actual &&
	echo "a=b=c" >expect &&
	test_cmp expect actual
	)
'

# --- config unset ---

test_expect_success 'config unset removes a key' '
	(
	cd repo &&
	grit config set temp.key "remove_me" &&
	grit config get temp.key >actual &&
	echo "remove_me" >expect &&
	test_cmp expect actual &&
	grit config unset temp.key &&
	test_must_fail grit config get temp.key
	)
'

test_expect_success 'config --unset (legacy) removes a key' '
	(
	cd repo &&
	grit config set legacy.rm "bye" &&
	grit config --unset legacy.rm &&
	test_must_fail grit config get legacy.rm
	)
'

test_expect_success 'config unset nonexistent key fails' '
	(
	cd repo &&
	test_must_fail grit config unset no.such.key
	)
'

# --- config --local ---

test_expect_success 'config --local writes to repo config' '
	(
	cd repo &&
	grit config --local local.key "local_val" &&
	grit config get local.key >actual &&
	echo "local_val" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config --local list shows local entries' '
	(
	cd repo &&
	grit config --local --list >actual &&
	grep "local.key=local_val" actual
	)
'

# --- config with sections ---

test_expect_success 'config set keys in same section' '
	(
	cd repo &&
	grit config set section.alpha "a" &&
	grit config set section.beta "b" &&
	grit config set section.gamma "c" &&
	grit config get section.alpha >actual &&
	echo "a" >expect &&
	test_cmp expect actual &&
	grit config get section.gamma >actual2 &&
	echo "c" >expect2 &&
	test_cmp expect2 actual2
	)
'

test_expect_success 'config list includes section entries' '
	(
	cd repo &&
	grit config list >actual &&
	grep "section.alpha=a" actual &&
	grep "section.beta=b" actual &&
	grep "section.gamma=c" actual
	)
'

# --- config boolean values ---

test_expect_success 'config set boolean true' '
	(
	cd repo &&
	grit config set bool.yes "true" &&
	grit config get bool.yes >actual &&
	echo "true" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'config set boolean false' '
	(
	cd repo &&
	grit config set bool.no "false" &&
	grit config get bool.no >actual &&
	echo "false" >expect &&
	test_cmp expect actual
	)
'

# --- config with empty value ---

test_expect_success 'config set empty value' '
	(
	cd repo &&
	grit config set empty.key "" &&
	grit config get empty.key >actual &&
	echo "" >expect &&
	test_cmp expect actual
	)
'

# --- config rename-section ---

test_expect_success 'config rename-section renames keys' '
	(
	cd repo &&
	grit config set oldsec.key1 "v1" &&
	grit config set oldsec.key2 "v2" &&
	grit config rename-section oldsec newsec &&
	grit config get newsec.key1 >actual &&
	echo "v1" >expect &&
	test_cmp expect actual &&
	test_must_fail grit config get oldsec.key1
	)
'

# --- config remove-section ---

test_expect_success 'config remove-section removes all keys in section' '
	(
	cd repo &&
	grit config set delsec.a "1" &&
	grit config set delsec.b "2" &&
	grit config remove-section delsec &&
	test_must_fail grit config get delsec.a &&
	test_must_fail grit config get delsec.b
	)
'

# --- config --file list count ---

test_expect_success 'config --file list with empty file has no entries' '
	(
	cd repo &&
	>empty.cfg &&
	grit config --file empty.cfg --list >actual &&
	test_line_count = 0 actual
	)
'

test_expect_success 'config --file update existing key' '
	(
	cd repo &&
	grit config --file upd.cfg upd.key "old" &&
	grit config --file upd.cfg upd.key "new" &&
	grit config --file upd.cfg upd.key >actual &&
	echo "new" >expect &&
	test_cmp expect actual
	)
'

# --- config list output format ---

test_expect_success 'config list lines contain key=value format' '
	(
	cd repo &&
	grit config list >actual &&
	# every non-blank line should have an = sign
	grep -v "^$" actual | while read line; do
		echo "$line" | grep "=" || exit 1
	done
	)
'

test_done
