#!/bin/sh

test_description='config set --all and --replace-all: multi-valued keys, replacement, type coercion'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo' '
    grit init repo &&
    (cd repo &&
     grit config user.email "t@t.com" &&
     grit config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial")
'

test_expect_success 'config set basic key-value' '
    (cd repo && grit config set core.editor vim) &&
    (cd repo && grit config get core.editor >../actual) &&
    echo "vim" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set overwrites existing key' '
    (cd repo && grit config set core.editor nano) &&
    (cd repo && grit config get core.editor >../actual) &&
    echo "nano" >expect &&
    test_cmp expect actual
'

test_expect_success 'config legacy positional set key value' '
    (cd repo && grit config test.key1 "value1") &&
    (cd repo && grit config --get test.key1 >../actual) &&
    echo "value1" >expect &&
    test_cmp expect actual
'

test_expect_success 'get-all reads multi-valued keys' '
    (cd repo && printf "[mtest]\n\tmulti = alpha\n\tmulti = beta\n\tmulti = gamma\n" >>.git/config) &&
    (cd repo && grit config --get-all mtest.multi >../actual) &&
    cat >expect <<-\EOF &&
	alpha
	beta
	gamma
	EOF
    test_cmp expect actual
'

test_expect_success 'replace-all replaces all occurrences of multi-valued key' '
    (cd repo && grit config --replace-all mtest.multi "replaced") &&
    (cd repo && grit config --get-all mtest.multi >../actual) &&
    echo "replaced" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set --all replaces all of multi-valued key' '
    (cd repo && printf "[setmulti]\n\tkey = one\n\tkey = two\n\tkey = three\n" >>.git/config) &&
    (cd repo && grit config set --all setmulti.key "unified") &&
    (cd repo && grit config get --all setmulti.key >../actual) &&
    echo "unified" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set creates section if needed' '
    (cd repo && grit config set brand.new "fresh") &&
    (cd repo && grit config get brand.new >../actual) &&
    echo "fresh" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool sets canonical boolean true' '
    (cd repo && grit config --bool core.filemode true) &&
    (cd repo && grit config get core.filemode >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool sets canonical boolean false' '
    (cd repo && grit config --bool core.bare false) &&
    (cd repo && grit config get core.bare >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --int stores integer' '
    (cd repo && grit config --int pack.windowmemory 1024) &&
    (cd repo && grit config get pack.windowmemory >../actual) &&
    echo "1024" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set replaces last occurrence of multi-valued key' '
    (cd repo && printf "[repl]\n\ttest = first\n\ttest = second\n" >>.git/config) &&
    (cd repo && grit config set repl.test "newest") &&
    (cd repo && grit config get repl.test >../actual) &&
    echo "newest" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set --all with multi-valued entries replaces all' '
    (cd repo && printf "[boolm]\n\tmulti = yes\n\tmulti = on\n" >>.git/config) &&
    (cd repo && grit config set --all boolm.multi "true") &&
    (cd repo && grit config get --all boolm.multi >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'replace-all with single value is same as set' '
    (cd repo && grit config set single.val "old") &&
    (cd repo && grit config --replace-all single.val "new") &&
    (cd repo && grit config --get single.val >../actual) &&
    echo "new" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set with dotted subsection' '
    (cd repo && grit config set remote.origin.url "https://example.com/repo.git") &&
    (cd repo && grit config get remote.origin.url >../actual) &&
    echo "https://example.com/repo.git" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set with deeply nested key' '
    (cd repo && grit config set a.b.c "deep") &&
    (cd repo && grit config get a.b.c >../actual) &&
    echo "deep" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set value with spaces' '
    (cd repo && grit config set test.spaces "hello world") &&
    (cd repo && grit config get test.spaces >../actual) &&
    echo "hello world" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set value with special characters' '
    (cd repo && grit config set test.special "a=b;c#d") &&
    (cd repo && grit config get test.special >../actual) &&
    echo "a=b;c#d" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set empty string value' '
    (cd repo && grit config set test.empty "") &&
    (cd repo && grit config get test.empty >../actual) &&
    echo "" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set numeric string value' '
    (cd repo && grit config set test.numeric "42") &&
    (cd repo && grit config get test.numeric >../actual) &&
    echo "42" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set url-like value' '
    (cd repo && grit config set test.url "git@github.com:user/repo.git") &&
    (cd repo && grit config get test.url >../actual) &&
    echo "git@github.com:user/repo.git" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set path value' '
    (cd repo && grit config set test.path "/usr/local/bin/editor") &&
    (cd repo && grit config get test.path >../actual) &&
    echo "/usr/local/bin/editor" >expect &&
    test_cmp expect actual
'

test_expect_success 'config list shows set values' '
    (cd repo && grit config set visible.key "visible-val") &&
    (cd repo && grit config --list >../actual) &&
    grep "visible.key=visible-val" actual
'

test_expect_success 'config set --local stores in local config' '
    (cd repo && grit config --local local.test "localval") &&
    (cd repo && grit config get local.test >../actual) &&
    echo "localval" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set to file with -f' '
    (cd repo && grit config -f ../custom.cfg set custom.key "fileval") &&
    (cd repo && grit config -f ../custom.cfg get custom.key >../actual) &&
    echo "fileval" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set multiple keys in same section' '
    (cd repo &&
     grit config set multi2.alpha "a" &&
     grit config set multi2.beta "b" &&
     grit config set multi2.gamma "g") &&
    (cd repo && grit config get multi2.alpha >../actual) &&
    echo "a" >expect &&
    test_cmp expect actual &&
    (cd repo && grit config get multi2.beta >../actual) &&
    echo "b" >expect &&
    test_cmp expect actual &&
    (cd repo && grit config get multi2.gamma >../actual) &&
    echo "g" >expect &&
    test_cmp expect actual
'

test_expect_success 'replace-all with --int type replaces all' '
    (cd repo && printf "[intm]\n\tmulti = 10\n\tmulti = 20\n" >>.git/config) &&
    (cd repo && grit config --replace-all --int intm.multi "99") &&
    (cd repo && grit config --get-all intm.multi >../actual) &&
    echo "99" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set overwrites previous value in same session' '
    (cd repo &&
     grit config set session.test "first" &&
     grit config set session.test "second" &&
     grit config set session.test "third" &&
     grit config get session.test >../actual) &&
    echo "third" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool canonicalizes yes to true' '
    (cd repo && grit config --bool norm.bool yes) &&
    (cd repo && grit config get norm.bool >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool canonicalizes no to false' '
    (cd repo && grit config --bool norm.boolno no) &&
    (cd repo && grit config get norm.boolno >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'replace-all on nonexistent key creates it' '
    (cd repo && grit config --replace-all newkey.replaceall "created") &&
    (cd repo && grit config --get newkey.replaceall >../actual) &&
    echo "created" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set --all on single-valued key works' '
    (cd repo && grit config set singleval.key "original") &&
    (cd repo && grit config set --all singleval.key "replaced") &&
    (cd repo && grit config get singleval.key >../actual) &&
    echo "replaced" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set preserves other keys in section' '
    (cd repo &&
     grit config set preserve.keep "keepme" &&
     grit config set preserve.change "old") &&
    (cd repo && grit config set preserve.change "new") &&
    (cd repo && grit config get preserve.keep >../actual) &&
    echo "keepme" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool canonicalizes on to true' '
    (cd repo && grit config --bool norm.on on) &&
    (cd repo && grit config get norm.on >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool canonicalizes off to false' '
    (cd repo && grit config --bool norm.off off) &&
    (cd repo && grit config get norm.off >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_done
