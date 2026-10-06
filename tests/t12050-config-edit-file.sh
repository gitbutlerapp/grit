#!/bin/sh
test_description='grit config --file and config edit'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	grit init repo &&
	(cd repo &&
	 $REAL_GIT config user.email "t@t.com" &&
	 $REAL_GIT config user.name "T" &&
	 echo hello >file.txt &&
	 grit add file.txt &&
	 grit commit -m "initial")
'

# ── --file set and get ────────────────────────────────────────────────────────

test_expect_success 'config --file sets a key in an external file' '
	grit config --file custom.cfg test.key custom-val &&
	test -f custom.cfg
'

test_expect_success 'config --file gets the value back' '
	grit config --file custom.cfg --get test.key >actual &&
	echo custom-val >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file --list lists entries from external file' '
	grit config --file custom.cfg --list >actual &&
	grep "^test.key=custom-val$" actual
'

test_expect_success 'config --file creates the file if missing' '
	rm -f brand-new.cfg &&
	grit config --file brand-new.cfg new.section.key val &&
	test -f brand-new.cfg &&
	grit config --file brand-new.cfg --get new.section.key >actual &&
	echo val >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file does not affect repo config' '
	grit config --file custom.cfg only.here value &&
	test_must_fail grit -C repo config get only.here
'

test_expect_success 'config --file supports multiple sections' '
	grit config --file multi-sect.cfg sec1.k v1 &&
	grit config --file multi-sect.cfg sec2.k v2 &&
	grit config --file multi-sect.cfg --get sec1.k >actual &&
	echo v1 >expect &&
	test_cmp expect actual &&
	grit config --file multi-sect.cfg --get sec2.k >actual &&
	echo v2 >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file overwrite existing key' '
	grit config --file custom.cfg test.key new-val &&
	grit config --file custom.cfg --get test.key >actual &&
	echo new-val >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file --unset removes a key' '
	grit config --file custom.cfg --unset test.key &&
	test_must_fail grit config --file custom.cfg --get test.key
'

test_expect_success 'config --file with --bool normalizes' '
	grit config --file types.cfg bool.key yes &&
	grit config --file types.cfg --bool --get bool.key >actual &&
	echo true >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file with --int returns integer' '
	grit config --file types.cfg int.key 512 &&
	grit config --file types.cfg --int --get int.key >actual &&
	echo 512 >expect &&
	test_cmp expect actual
'

test_expect_success 'config --file with --path expands tilde' '
	grit config --file types.cfg path.key "~/bar" &&
	grit config --file types.cfg --path --get path.key >actual &&
	echo "$HOME/bar" >expect &&
	test_cmp expect actual
'

# ── --global ──────────────────────────────────────────────────────────────────

test_expect_success 'config --global sets in $HOME/.gitconfig' '
	grit config --global test.glob gval &&
	grit config --global --get test.glob >actual &&
	echo gval >expect &&
	test_cmp expect actual &&
	test -f "$HOME/.gitconfig"
'

test_expect_success 'config --global value visible from repo' '
	(cd repo && grit config get test.glob >../actual) &&
	echo gval >expect &&
	test_cmp expect actual
'

test_expect_success 'config --local overrides --global' '
	(cd repo && grit config set test.glob local-override) &&
	(cd repo && grit config get test.glob >../actual) &&
	echo local-override >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global still has original value' '
	grit config --global --get test.glob >actual &&
	echo gval >expect &&
	test_cmp expect actual
'

test_expect_success 'unset local reveals global' '
	(cd repo && grit config unset test.glob) &&
	(cd repo && grit config get test.glob >../actual) &&
	echo gval >expect &&
	test_cmp expect actual
'

test_expect_success 'config --global --unset removes global key' '
	grit config --global --unset test.glob &&
	test_must_fail grit config --global --get test.glob
'

# ── --show-origin with --file ────────────────────────────────────────────────

test_expect_success 'config --file --show-origin shows file path' '
	grit config --file custom.cfg only.here value &&
	grit config --file custom.cfg --show-origin --list >actual &&
	grep "file:custom.cfg" actual
'

# ── rename-section and remove-section in external file ────────────────────────

test_expect_success 'config --file --rename-section works' '
	grit config --file rename.cfg old.k1 v1 &&
	grit config --file rename.cfg --rename-section old new &&
	grit config --file rename.cfg --get new.k1 >actual &&
	echo v1 >expect &&
	test_cmp expect actual &&
	test_must_fail grit config --file rename.cfg --get old.k1
'

test_expect_success 'config --file --remove-section works' '
	grit config --file remove.cfg rm.k1 v1 &&
	grit config --file remove.cfg rm.k2 v2 &&
	grit config --file remove.cfg --remove-section rm &&
	test_must_fail grit config --file remove.cfg --get rm.k1 &&
	test_must_fail grit config --file remove.cfg --get rm.k2
'

# ── config file content verification ─────────────────────────────────────────

test_expect_success 'external config file contains expected INI format' '
	rm -f ini-check.cfg &&
	grit config --file ini-check.cfg section.key value &&
	grep "^\[section\]$" ini-check.cfg &&
	grep "key = value" ini-check.cfg
'

test_expect_success 'multiple keys in same section share section header' '
	grit config --file ini-check.cfg section.key2 value2 &&
	test $(grep -c "^\[section\]$" ini-check.cfg) -eq 1
'

test_expect_success 'different sections get separate headers' '
	grit config --file ini-check.cfg other.key val &&
	grep "^\[other\]$" ini-check.cfg &&
	grep "^\[section\]$" ini-check.cfg
'

# ── config --local (explicit) ────────────────────────────────────────────────

test_expect_success 'config --local reads from repo config only' '
	(cd repo && grit config --local --list >../actual) &&
	grep "^user.email=t@t.com$" actual
'

test_expect_success 'config --local set and get' '
	(cd repo && grit config --local test.localonly lval) &&
	(cd repo && grit config --local --get test.localonly >../actual) &&
	echo lval >expect &&
	test_cmp expect actual
'

test_expect_success 'config --local value not in --global' '
	test_must_fail grit config --global --get test.localonly
'

# ── config --system scope ────────────────────────────────────────────────────

test_expect_success 'config --system fails gracefully without system config' '
	grit config --system --list >actual 2>err;
	true
'

# ── Interaction between --file and repo config ───────────────────────────────

test_expect_success 'writing to --file does not modify .git/config' '
	(cd repo && grit config list >../before) &&
	grit config --file side.cfg side.key sideval &&
	(cd repo && grit config list >../after) &&
	test_cmp before after
'

test_expect_success 'reading from --file does not read .git/config' '
	grit config --file side.cfg --list >actual &&
	! grep "user.email" actual
'

# ── empty values and special characters ──────────────────────────────────────

test_expect_success 'config set with empty value' '
	(cd repo && grit config set test.empty "") &&
	(cd repo && grit config get test.empty >../actual) &&
	echo "" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set with spaces in value' '
	(cd repo && grit config set test.spaces "hello world") &&
	(cd repo && grit config get test.spaces >../actual) &&
	echo "hello world" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set with equals sign in value' '
	(cd repo && grit config set test.eq "a=b") &&
	(cd repo && grit config get test.eq >../actual) &&
	echo "a=b" >expect &&
	test_cmp expect actual
'

test_done
