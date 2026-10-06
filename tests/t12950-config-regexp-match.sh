#!/bin/sh

test_description='config --get-regexp and config get --regexp: pattern matching, show-names, filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with many config keys' '
    grit init repo &&
    (cd repo &&
     grit config user.email "t@t.com" &&
     grit config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit config set color.ui auto &&
     grit config set color.branch always &&
     grit config set color.diff true &&
     grit config set alias.co checkout &&
     grit config set alias.br branch &&
     grit config set alias.ci commit &&
     grit config set alias.st status &&
     grit config set merge.tool vimdiff &&
     grit config set merge.conflictstyle diff3 &&
     grit config set diff.renames true &&
     grit config set diff.algorithm patience)
'

test_expect_success 'get-regexp matches simple substring' '
    (cd repo && grit config --get-regexp "color" >../actual) &&
    grep "color.ui auto" actual &&
    grep "color.branch always" actual &&
    grep "color.diff true" actual
'

test_expect_success 'get-regexp returns key-space-value format' '
    (cd repo && grit config --get-regexp "alias" >../actual) &&
    grep "alias.co checkout" actual &&
    grep "alias.br branch" actual &&
    grep "alias.ci commit" actual &&
    grep "alias.st status" actual
'

test_expect_success 'get-regexp matches partial key name' '
    (cd repo && grit config --get-regexp "merge" >../actual) &&
    grep "merge.tool vimdiff" actual &&
    grep "merge.conflictstyle diff3" actual
'

test_expect_success 'get-regexp matches diff section' '
    (cd repo && grit config --get-regexp "diff" >../actual) &&
    grep "diff.renames true" actual &&
    grep "diff.algorithm patience" actual
'

test_expect_success 'get-regexp with dot in pattern' '
    (cd repo && grit config --get-regexp "color.ui" >../actual) &&
    echo "color.ui auto" >expect &&
    test_cmp expect actual
'

test_expect_success 'get-regexp no match returns non-zero' '
    (cd repo && test_must_fail grit config --get-regexp "nonexistent")
'

test_expect_success 'get --regexp matches substring' '
    (cd repo && grit config get --regexp "alias" >../actual) &&
    grep "checkout" actual &&
    grep "branch" actual &&
    grep "commit" actual &&
    grep "status" actual
'

test_expect_success 'get --regexp returns values only by default' '
    (cd repo && grit config get --regexp "color.ui" >../actual) &&
    echo "auto" >expect &&
    test_cmp expect actual
'

test_expect_success 'get --regexp --show-names includes key names' '
    (cd repo && grit config get --regexp --show-names "color.ui" >../actual) &&
    echo "color.ui auto" >expect &&
    test_cmp expect actual
'

test_expect_success 'get --regexp --show-names with multiple matches' '
    (cd repo && grit config get --regexp --show-names "alias" >../actual) &&
    grep "alias.co checkout" actual &&
    grep "alias.br branch" actual
'

test_expect_success 'get --regexp no match returns non-zero' '
    (cd repo && test_must_fail grit config get --regexp "zzzzzzz")
'

test_expect_success 'get-regexp with regex metacharacters - dot' '
    (cd repo && grit config --get-regexp "color.ui" >../actual) &&
    echo "color.ui auto" >expect &&
    test_cmp expect actual
'

test_expect_success 'get-regexp matches all alias entries' '
    (cd repo && grit config --get-regexp "alias" >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -eq 4
'

test_expect_success 'get-regexp matches all color entries' '
    (cd repo && grit config --get-regexp "color" >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -eq 3
'

test_expect_success 'get-regexp matches all merge entries' '
    (cd repo && grit config --get-regexp "merge" >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -eq 2
'

test_expect_success 'get --regexp with show-names on all diff entries' '
    (cd repo && grit config get --regexp --show-names "diff" >../actual) &&
    grep "diff.renames" actual &&
    grep "diff.algorithm" actual
'

test_expect_success 'setup more config for advanced regexp' '
    (cd repo &&
     grit config set http.proxy "http://proxy:8080" &&
     grit config set http.sslverify false &&
     grit config set https.proxy "https://proxy:8443" &&
     grit config set remote.origin.url "https://example.com/repo.git" &&
     grit config set remote.origin.fetch "+refs/heads/*:refs/remotes/origin/*" &&
     grit config set remote.upstream.url "https://upstream.com/repo.git" &&
     grit config set remote.upstream.fetch "+refs/heads/*:refs/remotes/upstream/*")
'

test_expect_success 'get-regexp matches http section' '
    (cd repo && grit config --get-regexp "http" >../actual) &&
    grep "http.proxy" actual &&
    grep "http.sslverify" actual
'

test_expect_success 'get-regexp matches remote entries' '
    (cd repo && grit config --get-regexp "remote" >../actual) &&
    grep "remote.origin.url" actual &&
    grep "remote.upstream.url" actual
'

test_expect_success 'get-regexp with remote.origin prefix' '
    (cd repo && grit config --get-regexp "remote.origin" >../actual) &&
    grep "remote.origin.url" actual &&
    grep "remote.origin.fetch" actual &&
    ! grep "upstream" actual
'

test_expect_success 'get-regexp with remote.upstream prefix' '
    (cd repo && grit config --get-regexp "remote.upstream" >../actual) &&
    grep "remote.upstream.url" actual &&
    grep "remote.upstream.fetch" actual &&
    ! grep "origin" actual
'

test_expect_success 'get --regexp matches url keys across remotes' '
    (cd repo && grit config get --regexp --show-names "url" >../actual) &&
    grep "remote.origin.url" actual &&
    grep "remote.upstream.url" actual
'

test_expect_success 'get --regexp matches fetch keys across remotes' '
    (cd repo && grit config get --regexp --show-names "fetch" >../actual) &&
    grep "remote.origin.fetch" actual &&
    grep "remote.upstream.fetch" actual
'

test_expect_success 'get-regexp with ssl substring' '
    (cd repo && grit config --get-regexp "ssl" >../actual) &&
    grep "http.sslverify false" actual
'

test_expect_success 'get-regexp with proxy substring' '
    (cd repo && grit config --get-regexp "proxy" >../actual) &&
    grep "http.proxy" actual &&
    grep "https.proxy" actual
'

test_expect_success 'get --regexp values-only for proxy' '
    (cd repo && grit config get --regexp "proxy" >../actual) &&
    grep "http://proxy:8080" actual &&
    grep "https://proxy:8443" actual
'

test_expect_success 'get-regexp case sensitivity - lowercase matches' '
    (cd repo && grit config set CamelCase.Key "value") &&
    (cd repo && grit config --get-regexp "camelcase" >../actual) &&
    grep "value" actual
'

test_expect_success 'get-regexp with single-char pattern' '
    (cd repo && grit config --get-regexp "u" >../actual) &&
    grep "user" actual
'

test_expect_success 'get --regexp on newly added key' '
    (cd repo && grit config set brand.newkey "newval") &&
    (cd repo && grit config get --regexp "brand" >../actual) &&
    echo "newval" >expect &&
    test_cmp expect actual
'

test_expect_success 'get --regexp --show-names on newly added key' '
    (cd repo && grit config get --regexp --show-names "brand.newkey" >../actual) &&
    echo "brand.newkey newval" >expect &&
    test_cmp expect actual
'

test_expect_success 'get-regexp after config unset still matches remaining' '
    (cd repo && grit config unset alias.st) &&
    (cd repo && grit config --get-regexp "alias" >../actual) &&
    ! grep "alias.st" actual &&
    grep "alias.co" actual
'

test_expect_success 'get-regexp count decreases after unset' '
    (cd repo && grit config --get-regexp "alias" >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -eq 3
'

test_done
