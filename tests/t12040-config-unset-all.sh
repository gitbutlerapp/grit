#!/bin/sh
test_description='grit config unset --all and legacy --unset-all'
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

test_expect_success 'config unset removes a single-valued key' '
	(cd repo && $REAL_GIT config test.single sval) &&
	(cd repo && grit config get test.single >../actual) &&
	echo sval >expect &&
	test_cmp expect actual &&
	(cd repo && grit config unset test.single) &&
	test_must_fail grit -C repo config get test.single
'

test_expect_success 'config unset on missing key fails' '
	test_must_fail grit -C repo config unset no.such.key
'

test_expect_success 'add multiple values with git config --add' '
	(cd repo &&
	 $REAL_GIT config --add multi.key val1 &&
	 $REAL_GIT config --add multi.key val2 &&
	 $REAL_GIT config --add multi.key val3) &&
	(cd repo && grit config --get-all multi.key >../actual) &&
	cat >expect <<-\EOF &&
	val1
	val2
	val3
	EOF
	test_cmp expect actual
'

test_expect_success 'config unset --all removes all occurrences' '
	(cd repo && grit config unset --all multi.key) &&
	test_must_fail grit -C repo config --get-all multi.key
'

test_expect_success 'legacy --unset-all removes all occurrences' '
	(cd repo &&
	 $REAL_GIT config --add leg.key a &&
	 $REAL_GIT config --add leg.key b &&
	 $REAL_GIT config --add leg.key c) &&
	(cd repo && grit config --unset-all leg.key) &&
	test_must_fail grit -C repo config --get-all leg.key
'

test_expect_success 'config unset without --all on multi-valued key warns and fails' '
	(cd repo &&
	 $REAL_GIT config --add partial.key x &&
	 $REAL_GIT config --add partial.key y &&
	 $REAL_GIT config --add partial.key z) &&
	test_must_fail grit -C repo config unset partial.key 2>err &&
	grep -i "multiple values" err &&
	grit -C repo config --get-all partial.key >actual &&
	test_line_count = 3 actual
'

test_expect_success 'config set creates a new key' '
	(cd repo && grit config set new.key newval) &&
	(cd repo && grit config get new.key >../actual) &&
	echo newval >expect &&
	test_cmp expect actual
'

test_expect_success 'config set overwrites an existing key' '
	(cd repo && grit config set new.key updated) &&
	(cd repo && grit config get new.key >../actual) &&
	echo updated >expect &&
	test_cmp expect actual
'

test_expect_success 'unset after set leaves key absent' '
	(cd repo && grit config set tmp.key tmpval) &&
	(cd repo && grit config unset tmp.key) &&
	test_must_fail grit -C repo config get tmp.key
'

test_expect_success 'config --replace-all replaces all values with new value' '
	(cd repo &&
	 $REAL_GIT config --add rep.key r1 &&
	 $REAL_GIT config --add rep.key r2 &&
	 $REAL_GIT config --add rep.key r3) &&
	(cd repo && grit config --replace-all rep.key replaced) &&
	(cd repo && grit config --get-all rep.key >../actual) &&
	cat >expect <<-\EOF &&
	replaced
	EOF
	test_cmp expect actual
'

test_expect_success 'config unset --all after replace-all' '
	(cd repo && grit config unset --all rep.key) &&
	test_must_fail grit -C repo config get rep.key
'

test_expect_success 'config set --all replaces all values with new value' '
	(cd repo &&
	 $REAL_GIT config --add sa.key p &&
	 $REAL_GIT config --add sa.key q) &&
	(cd repo && grit config set --all sa.key replaced) &&
	(cd repo && grit config --get-all sa.key >../actual) &&
	cat >expect <<-\EOF &&
	replaced
	EOF
	test_cmp expect actual &&
	(cd repo && grit config unset --all sa.key)
'

test_expect_success 'config list shows all entries' '
	(cd repo && grit config list >../actual) &&
	grep "^user.email=t@t.com$" actual &&
	grep "^user.name=T$" actual
'

test_expect_success 'config get retrieves correct value' '
	(cd repo && grit config get user.name >../actual) &&
	echo T >expect &&
	test_cmp expect actual
'

test_expect_success 'config --bool normalizes yes to true' '
	(cd repo && $REAL_GIT config test.bool yes) &&
	(cd repo && grit config --bool --get test.bool >../actual) &&
	echo true >expect &&
	test_cmp expect actual
'

test_expect_success 'config --bool normalizes no to false' '
	(cd repo && $REAL_GIT config test.bool no) &&
	(cd repo && grit config --bool --get test.bool >../actual) &&
	echo false >expect &&
	test_cmp expect actual
'

test_expect_success 'config --int returns integer' '
	(cd repo && $REAL_GIT config test.ival 42) &&
	(cd repo && grit config --int --get test.ival >../actual) &&
	echo 42 >expect &&
	test_cmp expect actual
'

test_expect_success 'config --type=bool works like --bool' '
	(cd repo && $REAL_GIT config test.tbool on) &&
	(cd repo && grit config --type=bool --get test.tbool >../actual) &&
	echo true >expect &&
	test_cmp expect actual
'

test_expect_success 'config --type=int works like --int' '
	(cd repo && $REAL_GIT config test.tint 1024) &&
	(cd repo && grit config --type=int --get test.tint >../actual) &&
	echo 1024 >expect &&
	test_cmp expect actual
'

test_expect_success 'config --path expands tilde' '
	(cd repo && $REAL_GIT config test.path "~/foo") &&
	(cd repo && grit config --path --get test.path >../actual) &&
	echo "$HOME/foo" >expect &&
	test_cmp expect actual
'

test_expect_success 'config rename-section renames section' '
	(cd repo && $REAL_GIT config old-sect.k1 v1) &&
	(cd repo && grit config --rename-section old-sect new-sect) &&
	(cd repo && grit config get new-sect.k1 >../actual) &&
	echo v1 >expect &&
	test_cmp expect actual &&
	test_must_fail grit -C repo config get old-sect.k1
'

test_expect_success 'config remove-section removes entire section' '
	(cd repo && $REAL_GIT config rm-sect.k1 v1 && $REAL_GIT config rm-sect.k2 v2) &&
	(cd repo && grit config --remove-section rm-sect) &&
	test_must_fail grit -C repo config get rm-sect.k1 &&
	test_must_fail grit -C repo config get rm-sect.k2
'

test_expect_success 'unset --all on non-existent key fails' '
	test_must_fail grit -C repo config unset --all no.such.key
'

test_expect_success 'config --show-origin shows file origin' '
	(cd repo && grit config --show-origin --list >../actual) &&
	grep "file:" actual
'

test_expect_success 'config --show-scope shows scope' '
	(cd repo && grit config --show-scope --list >../actual) &&
	grep "local" actual
'

test_expect_success 'config -z uses NUL delimiters' '
	(cd repo && grit config -z --list >../actual) &&
	tr "\0" "\n" <actual >actual_lines &&
	grep "user.email=t@t.com" actual_lines
'

test_expect_success 'unset then re-set a key' '
	(cd repo && grit config set cycle.key first) &&
	(cd repo && grit config unset cycle.key) &&
	(cd repo && grit config set cycle.key second) &&
	(cd repo && grit config get cycle.key >../actual) &&
	echo second >expect &&
	test_cmp expect actual
'

test_expect_success 'unset --all with two values leaves key gone' '
	(cd repo &&
	 $REAL_GIT config --add two.key a &&
	 $REAL_GIT config --add two.key b) &&
	(cd repo && grit config unset --all two.key) &&
	test_must_fail grit -C repo config get two.key
'

test_expect_success 'set after unset --all works' '
	(cd repo && grit config set two.key reborn) &&
	(cd repo && grit config get two.key >../actual) &&
	echo reborn >expect &&
	test_cmp expect actual
'

test_expect_success 'multiple sections with same key name are independent' '
	(cd repo &&
	 grit config set sec1.k val1 &&
	 grit config set sec2.k val2) &&
	(cd repo && grit config get sec1.k >../actual) &&
	echo val1 >expect &&
	test_cmp expect actual &&
	(cd repo && grit config get sec2.k >../actual) &&
	echo val2 >expect &&
	test_cmp expect actual
'

test_expect_success 'unset one section key does not affect another' '
	(cd repo && grit config unset sec1.k) &&
	test_must_fail grit -C repo config get sec1.k &&
	(cd repo && grit config get sec2.k >../actual) &&
	echo val2 >expect &&
	test_cmp expect actual
'

test_expect_success 'cleanup: remove leftover test keys' '
	(cd repo &&
	 grit config unset new.key &&
	 grit config unset two.key &&
	 grit config unset sec2.k &&
	 grit config unset cycle.key) &&
	(cd repo && grit config list >../actual) &&
	! grep "^new\.key=" actual &&
	! grep "^two\.key=" actual
'

test_done
