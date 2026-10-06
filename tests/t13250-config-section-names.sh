#!/bin/sh

test_description='grit config: section and subsection name handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

# ── section names are case-insensitive ───────────────────────────────────

test_expect_success 'section name is case-insensitive on set and get' '
	(cd repo && grit config set Core.TestKey "val1" &&
	 grit config get core.testkey >../actual) &&
	echo "val1" >expect &&
	test_cmp expect actual
'

test_expect_success 'uppercase section name retrieves lowercase set' '
	(cd repo && grit config set lowersec.key "lo" &&
	 grit config get LOWERSEC.key >../actual) &&
	echo "lo" >expect &&
	test_cmp expect actual
'

test_expect_success 'mixed case section round-trips' '
	(cd repo && grit config set MiXeD.key "mix" &&
	 grit config get mixed.key >../actual) &&
	echo "mix" >expect &&
	test_cmp expect actual
'

# ── variable names are case-insensitive ──────────────────────────────────

test_expect_success 'variable name is case-insensitive' '
	(cd repo && grit config set test.VarName "upper" &&
	 grit config get test.varname >../actual) &&
	echo "upper" >expect &&
	test_cmp expect actual
'

test_expect_success 'all-uppercase variable name works' '
	(cd repo && grit config set test.ALLCAPS "caps" &&
	 grit config get test.allcaps >../actual) &&
	echo "caps" >expect &&
	test_cmp expect actual
'

# ── subsections are case-sensitive ───────────────────────────────────────

test_expect_success 'subsection name is case-sensitive' '
	(cd repo &&
	 $REAL_GIT config "branch.MyBranch.remote" "origin" &&
	 grit config get branch.MyBranch.remote >../actual) &&
	echo "origin" >expect &&
	test_cmp expect actual
'

test_expect_success 'subsection case mismatch fails' '
	(cd repo && test_must_fail grit config get branch.mybranch.remote)
'

test_expect_success 'subsection preserves dots in name' '
	(cd repo &&
	 $REAL_GIT config "url.https://example.com/.insteadOf" "ex:" &&
	 grit config get "url.https://example.com/.insteadOf" >../actual) &&
	echo "ex:" >expect &&
	test_cmp expect actual
'

# ── section listing via config list ──────────────────────────────────────

test_expect_success 'config list shows all entries' '
	(cd repo && grit config list >../actual) &&
	grep "core.bare=false" actual
'

test_expect_success 'config list includes user-set keys' '
	(cd repo && grit config list >../actual) &&
	grep "lowersec.key=lo" actual
'

test_expect_success 'config list includes subsection entries' '
	(cd repo && grit config list >../actual) &&
	grep "branch.MyBranch.remote=origin" actual
'

# ── rename-section ───────────────────────────────────────────────────────

test_expect_success 'rename-section renames section' '
	(cd repo &&
	 grit config set oldsec.key1 "v1" &&
	 grit config set oldsec.key2 "v2" &&
	 grit config rename-section oldsec newsec &&
	 grit config get newsec.key1 >../actual) &&
	echo "v1" >expect &&
	test_cmp expect actual
'

test_expect_success 'rename-section old section no longer exists' '
	(cd repo && test_must_fail grit config get oldsec.key1)
'

test_expect_success 'rename-section preserves all keys' '
	(cd repo && grit config get newsec.key2 >../actual) &&
	echo "v2" >expect &&
	test_cmp expect actual
'

test_expect_success 'rename-section fails for nonexistent section' '
	(cd repo && test_must_fail grit config rename-section nosuch.section another)
'

# ── remove-section ───────────────────────────────────────────────────────

test_expect_success 'remove-section removes all keys' '
	(cd repo &&
	 grit config set removeme.a "1" &&
	 grit config set removeme.b "2" &&
	 grit config remove-section removeme &&
	 test_must_fail grit config get removeme.a &&
	 test_must_fail grit config get removeme.b)
'

test_expect_success 'remove-section fails for nonexistent section' '
	(cd repo && test_must_fail grit config remove-section nonexistent)
'

# ── section with numeric characters ──────────────────────────────────────

test_expect_success 'section name with digits' '
	(cd repo && grit config set sec123.key "digits" &&
	 grit config get sec123.key >../actual) &&
	echo "digits" >expect &&
	test_cmp expect actual
'

test_expect_success 'section name with hyphens' '
	(cd repo && grit config set my-section.key "hyphen" &&
	 grit config get my-section.key >../actual) &&
	echo "hyphen" >expect &&
	test_cmp expect actual
'

# ── multiple subsections under same section ──────────────────────────────

test_expect_success 'multiple subsections under same section' '
	(cd repo &&
	 $REAL_GIT config "remote.origin.url" "https://origin.example" &&
	 $REAL_GIT config "remote.upstream.url" "https://upstream.example" &&
	 grit config get remote.origin.url >../actual) &&
	echo "https://origin.example" >expect &&
	test_cmp expect actual
'

test_expect_success 'each subsection is independent' '
	(cd repo && grit config get remote.upstream.url >../actual) &&
	echo "https://upstream.example" >expect &&
	test_cmp expect actual
'

# ── core section defaults ────────────────────────────────────────────────

test_expect_success 'core.repositoryformatversion is set by init' '
	(cd repo && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'core.filemode is set by init' '
	(cd repo && grit config get core.filemode >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'core.logallrefupdates is set by init' '
	(cd repo && grit config get core.logallrefupdates >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

# ── legacy --get with section names ──────────────────────────────────────

test_expect_success 'legacy --get works with sections' '
	(cd repo && grit config --get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'legacy --get is case-insensitive for section' '
	(cd repo && grit config --get CORE.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'legacy -l lists all entries' '
	(cd repo && grit config -l >../actual) &&
	grep "core.bare=false" actual
'

# ── edge cases ───────────────────────────────────────────────────────────

test_expect_success 'get nonexistent section.key fails' '
	(cd repo && test_must_fail grit config get totally.made.up)
'

test_expect_success 'set and get key with long section name' '
	(cd repo && grit config set averylongsectionname.key "longname" &&
	 grit config get averylongsectionname.key >../actual) &&
	echo "longname" >expect &&
	test_cmp expect actual
'

test_expect_success 'set and get key with single-char section' '
	(cd repo && grit config set x.y "tiny" &&
	 grit config get x.y >../actual) &&
	echo "tiny" >expect &&
	test_cmp expect actual
'

test_done
