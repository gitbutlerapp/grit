#!/bin/sh

test_description='config handling of null/implicit-true values, booleans, integers, empty values'

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

test_expect_success 'null-value key (no = sign) treated as implicit true' '
    (cd repo &&
     printf "[section]\n\tnullkey\n" >>.git/config &&
     grit config --get section.nullkey >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'null-value key with --bool returns true' '
    (cd repo && grit config --bool --get section.nullkey >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'empty value (key =) returns empty string' '
    (cd repo &&
     printf "[section]\n\temptykey =\n" >>.git/config &&
     grit config --get section.emptykey >../actual) &&
    echo "" >expect &&
    test_cmp expect actual
'

test_expect_success 'empty value with --bool returns true' '
    (cd repo && grit config --bool --get section.emptykey >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'config list shows null-value as key=true' '
    (cd repo && grit config -l --local >../actual) &&
    grep "^section.nullkey=true$" actual
'

test_expect_success 'config list shows empty-value as key=' '
    (cd repo && grit config -l --local >../actual) &&
    grep "^section.emptykey=$" actual
'

test_expect_success 'bool: on is true' '
    (cd repo && git config test.bool "on" &&
     grit config --bool --get test.bool >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: off is false' '
    (cd repo && git config test.bool "off" &&
     grit config --bool --get test.bool >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: yes is true' '
    (cd repo && git config test.bool "yes" &&
     grit config --bool --get test.bool >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: no is false' '
    (cd repo && git config test.bool "no" &&
     grit config --bool --get test.bool >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: true is true' '
    (cd repo && git config test.bool "true" &&
     grit config --bool --get test.bool >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: false is false' '
    (cd repo && git config test.bool "false" &&
     grit config --bool --get test.bool >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: 1 is true' '
    (cd repo && git config test.bool "1" &&
     grit config --bool --get test.bool >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: 0 is false' '
    (cd repo && git config test.bool "0" &&
     grit config --bool --get test.bool >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'bool: non-boolean value fails' '
    (cd repo && git config test.bool "notabool" &&
     ! grit config --bool --get test.bool 2>../actual_err) &&
    grep "bad boolean" actual_err
'

test_expect_success 'int: small integer' '
    (cd repo && git config test.int "42" &&
     grit config --int --get test.int >../actual) &&
    echo "42" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: zero' '
    (cd repo && git config test.int "0" &&
     grit config --int --get test.int >../actual) &&
    echo "0" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: negative' '
    (cd repo && git config test.int "-1" &&
     grit config --int --get test.int >../actual) &&
    echo "-1" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: k suffix is 1024' '
    (cd repo && git config test.int "1k" &&
     grit config --int --get test.int >../actual) &&
    echo "1024" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: m suffix is 1048576' '
    (cd repo && git config test.int "1m" &&
     grit config --int --get test.int >../actual) &&
    echo "1048576" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: g suffix is 1073741824' '
    (cd repo && git config test.int "1g" &&
     grit config --int --get test.int >../actual) &&
    echo "1073741824" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set creates new key' '
    (cd repo && grit config set mytest.newkey "hello" &&
     grit config --get mytest.newkey >../actual) &&
    echo "hello" >expect &&
    test_cmp expect actual
'

test_expect_success 'config set overwrites existing key' '
    (cd repo && grit config set mytest.newkey "world" &&
     grit config --get mytest.newkey >../actual) &&
    echo "world" >expect &&
    test_cmp expect actual
'

test_expect_success 'config unset removes existing key' '
    (cd repo && grit config unset mytest.newkey &&
     ! grit config --get mytest.newkey)
'

test_expect_success 'config unset nonexistent key returns error' '
    (cd repo && ! grit config unset totally.nonexistent 2>../actual_err)
'

test_expect_success 'config get nonexistent key returns exit 1' '
    (cd repo && ! grit config --get nonexistent.key)
'

test_expect_success 'multiple null-value keys are all true' '
    (cd repo &&
     printf "[multi]\n\ta\n\tb\n\tc\n" >>.git/config &&
     grit config --get multi.a >../actual_a &&
     grit config --get multi.b >../actual_b &&
     grit config --get multi.c >../actual_c) &&
    echo "true" >expect &&
    test_cmp expect actual_a &&
    test_cmp expect actual_b &&
    test_cmp expect actual_c
'

test_expect_success 'config list shows all null-value keys' '
    (cd repo && grit config -l --local >../actual) &&
    grep "^multi.a=true$" actual &&
    grep "^multi.b=true$" actual &&
    grep "^multi.c=true$" actual
'

test_expect_success 'mixed null and valued keys in same section' '
    (cd repo &&
     printf "[mixed]\n\tnullish\n\tvalued = hello\n\tnullish2\n" >>.git/config &&
     grit config --get mixed.nullish >../actual_n &&
     grit config --get mixed.valued >../actual_v &&
     grit config --get mixed.nullish2 >../actual_n2) &&
    echo "true" >expect_t &&
    echo "hello" >expect_h &&
    test_cmp expect_t actual_n &&
    test_cmp expect_h actual_v &&
    test_cmp expect_t actual_n2
'

test_expect_success 'get-regexp finds null-value keys' '
    (cd repo && grit config --get-regexp "multi" >../actual) &&
    grep "multi.a true" actual &&
    grep "multi.b true" actual &&
    grep "multi.c true" actual
'

test_expect_success 'name-only get-regexp with null values' '
    (cd repo && grit config --name-only --get-regexp "multi" >../actual) &&
    grep "^multi.a$" actual &&
    grep "^multi.b$" actual &&
    ! grep "true" actual
'

test_expect_success 'int: 2k suffix' '
    (cd repo && git config test.int "2k" &&
     grit config --int --get test.int >../actual) &&
    echo "2048" >expect &&
    test_cmp expect actual
'

test_expect_success 'int: 5m suffix' '
    (cd repo && git config test.int "5m" &&
     grit config --int --get test.int >../actual) &&
    echo "5242880" >expect &&
    test_cmp expect actual
'

test_done
