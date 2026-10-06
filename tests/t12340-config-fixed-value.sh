#!/bin/sh

test_description='grit config get/set/unset with fixed values'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T"
	)
'

test_expect_success 'config set and get a simple key' '
	(cd repo && grit config set test.key "hello" &&
	 grit config get test.key >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set overwrites existing value' '
	(cd repo && grit config set test.key "world" &&
	 grit config get test.key >../actual) &&
	echo "world" >expect &&
	test_cmp expect actual
'

test_expect_success 'config get returns user.email from git config' '
	(cd repo && grit config get user.email >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_expect_success 'config get returns user.name from git config' '
	(cd repo && grit config get user.name >../actual) &&
	echo "T" >expect &&
	test_cmp expect actual
'

test_expect_success 'config unset removes a key' '
	(cd repo && grit config set test.removeme "gone" &&
	 grit config unset test.removeme &&
	 test_must_fail grit config get test.removeme)
'

test_expect_success 'config set with dotted subsection' '
	(cd repo && grit config set my.sub.key "value1" &&
	 grit config get my.sub.key >../actual) &&
	echo "value1" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set numeric value' '
	(cd repo && grit config set test.num "42" &&
	 grit config get test.num >../actual) &&
	echo "42" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set boolean true' '
	(cd repo && grit config set test.flag "true" &&
	 grit config get test.flag >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set boolean false' '
	(cd repo && grit config set test.flag2 "false" &&
	 grit config get test.flag2 >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set value with spaces' '
	(cd repo && grit config set test.spaced "hello world" &&
	 grit config get test.spaced >../actual) &&
	echo "hello world" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set value with special chars' '
	(cd repo && grit config set test.special "a=b;c" &&
	 grit config get test.special >../actual) &&
	echo "a=b;c" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set empty string value' '
	(cd repo && grit config set test.empty "" &&
	 grit config get test.empty >../actual) &&
	echo "" >expect &&
	test_cmp expect actual
'

test_expect_success 'config list includes set keys' '
	(cd repo && grit config set test.listcheck "found" &&
	 grit config list >../actual) &&
	grep "test.listcheck=found" actual
'

test_expect_success 'config list shows multiple keys' '
	(cd repo && grit config set alpha.key "a" &&
	 grit config set beta.key "b" &&
	 grit config list >../actual) &&
	grep "alpha.key=a" actual &&
	grep "beta.key=b" actual
'

test_expect_success 'config get nonexistent key fails' '
	(cd repo && test_must_fail grit config get no.such.key)
'

test_expect_success 'config set then unset then get fails' '
	(cd repo && grit config set test.cycle "val" &&
	 grit config unset test.cycle &&
	 test_must_fail grit config get test.cycle)
'

test_expect_success 'config --bool returns canonical true' '
	(cd repo && grit config set test.boolcanon "yes" &&
	 grit config --bool test.boolcanon >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --bool returns canonical false' '
	(cd repo && grit config set test.boolcanon2 "no" &&
	 grit config --bool test.boolcanon2 >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --bool on on returns true' '
	(cd repo && grit config set test.boolon "on" &&
	 grit config --bool test.boolon >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --bool on off returns false' '
	(cd repo && grit config set test.booloff "off" &&
	 grit config --bool test.booloff >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config --int canonicalizes integer' '
	(cd repo && grit config set test.intval "100" &&
	 grit config --int test.intval >../actual) &&
	echo "100" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set core.bare to false' '
	(cd repo && grit config get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set and get core.repositoryformatversion' '
	(cd repo && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set path value' '
	(cd repo && grit config set test.path "/some/path/to/file" &&
	 grit config get test.path >../actual) &&
	echo "/some/path/to/file" >expect &&
	test_cmp expect actual
'

test_expect_success 'config set url-like value' '
	(cd repo && grit config set remote.origin.url "https://example.com/repo.git" &&
	 grit config get remote.origin.url >../actual) &&
	echo "https://example.com/repo.git" >expect &&
	test_cmp expect actual
'

test_expect_success 'config rename-section renames keys' '
	(cd repo && grit config set old.key1 "v1" &&
	 grit config set old.key2 "v2" &&
	 grit config rename-section old new &&
	 grit config get new.key1 >../actual) &&
	echo "v1" >expect &&
	test_cmp expect actual
'

test_expect_success 'config rename-section old section is gone' '
	(cd repo && test_must_fail grit config get old.key1)
'

test_expect_success 'config remove-section removes all keys' '
	(cd repo && grit config set removeme.a "1" &&
	 grit config set removeme.b "2" &&
	 grit config remove-section removeme &&
	 test_must_fail grit config get removeme.a &&
	 test_must_fail grit config get removeme.b)
'

test_expect_success 'config set replaces value in file' '
	(cd repo && grit config set test.replace "first" &&
	 grit config set test.replace "second" &&
	 grit config get test.replace >../actual) &&
	echo "second" >expect &&
	test_cmp expect actual
'

test_expect_success 'config list output is consistent after many operations' '
	(cd repo && grit config set final.a "1" &&
	 grit config set final.b "2" &&
	 grit config set final.c "3" &&
	 grit config list >../actual) &&
	grep "final.a=1" actual &&
	grep "final.b=2" actual &&
	grep "final.c=3" actual
'

test_expect_success 'config --show-origin shows file path' '
	(cd repo && grit config --show-origin --list >../actual) &&
	grep "^file:" actual
'

test_expect_success 'config --show-scope shows scope names' '
	(cd repo && grit config --show-scope --list >../actual) &&
	grep "^local" actual
'

test_expect_success 'config set with --local scope' '
	(cd repo && grit config --local test.scope "local-val" &&
	 grit config --local test.scope >../actual) &&
	echo "local-val" >expect &&
	test_cmp expect actual
'

test_done
