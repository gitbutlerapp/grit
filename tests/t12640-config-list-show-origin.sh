#!/bin/sh

test_description='config --show-origin and --show-scope with list, get, get-regexp'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'config -l lists local entries' '
    (cd repo && grit config -l --local >../actual) &&
    grep "core.bare=false" actual
'

test_expect_success 'config --show-origin -l shows file prefix' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "^file:.*\.git/config" actual
'

test_expect_success 'config --show-origin -l has tab separator' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "	core.bare=false" actual
'

test_expect_success 'config --show-scope -l shows local scope' '
    (cd repo && grit config --show-scope -l --local >../actual) &&
    grep "^local	" actual
'

test_expect_success 'config --show-scope all entries have scope prefix' '
    (cd repo && grit config --show-scope -l --local >../actual) &&
    ! grep -v "^local	" actual
'

test_expect_success 'config --show-origin --show-scope combined' '
    (cd repo && grit config --show-origin --show-scope -l --local >../actual) &&
    grep "^local	file:" actual
'

test_expect_success 'show-origin includes user.email' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "user.email=t@t.com" actual
'

test_expect_success 'show-origin includes user.name' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "user.name=T" actual
'

test_expect_success 'add custom config value' '
    (cd repo && git config test.key "myvalue") &&
    (cd repo && grit config --get test.key >../actual) &&
    echo "myvalue" >expect &&
    test_cmp expect actual
'

test_expect_success 'show-origin for custom key in list' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "test.key=myvalue" actual
'

test_expect_success 'show-scope for custom key in list' '
    (cd repo && grit config --show-scope -l --local >../actual) &&
    grep "local	test.key=myvalue" actual
'

test_expect_success 'config --show-origin --get shows value only' '
    (cd repo && grit config --show-origin --get test.key >../actual) &&
    echo "myvalue" >expect &&
    test_cmp expect actual
'

test_expect_success 'add multiple config keys' '
    (cd repo &&
     git config section.alpha "one" &&
     git config section.beta "two" &&
     git config section.gamma "three"
    )
'

test_expect_success 'get-regexp matches multiple keys' '
    (cd repo && grit config --get-regexp "section" >../actual) &&
    grep "section.alpha one" actual &&
    grep "section.beta two" actual &&
    grep "section.gamma three" actual
'

test_expect_success 'name-only with get-regexp' '
    (cd repo && grit config --name-only --get-regexp "section" >../actual) &&
    grep "^section.alpha$" actual &&
    grep "^section.beta$" actual &&
    ! grep "one" actual
'

test_expect_success 'config -z uses NUL delimiter for list' '
    (cd repo && grit config -z -l --local >../actual) &&
    tr "\0" "\n" <actual >actual_lines &&
    grep "test.key=myvalue" actual_lines
'

test_expect_success 'config -z --show-origin uses NUL and tab' '
    (cd repo && grit config -z --show-origin -l --local >../actual) &&
    tr "\0" "\n" <actual >actual_lines &&
    grep "	test.key=myvalue" actual_lines
'

test_expect_success 'config list without --local includes all scopes' '
    (cd repo && grit config -l >../actual) &&
    grep "core.bare=false" actual
'

test_expect_success 'config --show-origin for get single key' '
    (cd repo && grit config --show-origin --get user.email >../actual) &&
    echo "t@t.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --bool for boolean values' '
    (cd repo && grit config --bool --get core.bare >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --int for integer values' '
    (cd repo && grit config --int --get core.repositoryformatversion >../actual) &&
    echo "0" >expect &&
    test_cmp expect actual
'

test_expect_success 'config --get nonexistent key fails' '
    (cd repo && ! grit config --get nonexistent.key)
'

test_expect_success 'setup multi-value key via raw config edit' '
    (cd repo &&
     printf "[multi]\n\tkey = val1\n\tkey = val2\n" >>.git/config
    )
'

test_expect_success 'config --get-all returns multiple values' '
    (cd repo && grit config --get-all multi.key >../actual) &&
    echo "val1" >expect &&
    echo "val2" >>expect &&
    test_cmp expect actual
'

test_expect_success 'show-origin list shows multi-value entries' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "multi.key=val1" actual &&
    grep "multi.key=val2" actual
'

test_expect_success 'config set and retrieve new key' '
    (cd repo && grit config set fresh.key "newval") &&
    (cd repo && grit config --get fresh.key >../actual) &&
    echo "newval" >expect &&
    test_cmp expect actual
'

test_expect_success 'config show-origin reflects new key' '
    (cd repo && grit config --show-origin -l --local >../actual) &&
    grep "fresh.key=newval" actual
'

test_expect_success 'config unset removes key' '
    (cd repo && grit config unset fresh.key) &&
    (cd repo && ! grit config --get fresh.key)
'

test_expect_success 'config list no longer shows unset key' '
    (cd repo && grit config -l --local >../actual) &&
    ! grep "fresh.key" actual
'

test_expect_success 'config --show-scope with --get single key' '
    (cd repo && grit config --show-scope --get user.email >../actual) &&
    echo "t@t.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'config file option reads from specific file' '
    echo "[custom]" >custom.cfg &&
    echo "	val = hello" >>custom.cfg &&
    (cd repo && grit config -f ../custom.cfg --get custom.val >../actual) &&
    echo "hello" >expect &&
    test_cmp expect actual
'

test_expect_success 'show-origin with file option shows file path' '
    (cd repo && grit config --show-origin -f ../custom.cfg -l >../actual) &&
    grep "custom.val=hello" actual
'

test_done
