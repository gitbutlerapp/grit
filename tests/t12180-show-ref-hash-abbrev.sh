#!/bin/sh

test_description='show-ref --hash, --abbrev, --verify, --exists, and filtering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "hello" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch br1 &&
     grit branch br2 &&
     grit tag v1.0 &&
     grit tag -a v2.0 -m "annotated"
    )
'

test_expect_success 'show-ref lists all refs' '
    (cd repo &&
     grit show-ref >../actual) &&
    grep "refs/heads/" actual &&
    grep "refs/tags/" actual
'

test_expect_success 'show-ref output has hash and refname' '
    (cd repo &&
     grit show-ref >../actual) &&
    head -1 actual | grep -E "^[0-9a-f]{40} refs/"
'

test_expect_success 'show-ref --hash shows only hashes' '
    (cd repo &&
     grit show-ref --hash >../actual) &&
    head -1 actual | grep -E "^[0-9a-f]{40}$"
'

test_expect_success 'show-ref --hash output has no ref names' '
    (cd repo &&
     grit show-ref --hash >../actual) &&
    ! grep "refs/" actual
'

test_expect_success 'show-ref --hash line count matches full output' '
    (cd repo &&
     grit show-ref >../full &&
     grit show-ref --hash >../hashes) &&
    test_line_count = $(wc -l <full | tr -d " ") hashes
'

test_expect_success 'show-ref --abbrev shows abbreviated hashes' '
    (cd repo &&
     grit show-ref --abbrev >../actual) &&
    head -1 actual >first_line &&
    hash=$(awk "{print \$1}" <first_line) &&
    len=${#hash} &&
    test "$len" -lt 40
'

test_expect_success 'show-ref --abbrev=4 abbreviates to minimum 4 chars' '
    (cd repo &&
     grit show-ref --abbrev=4 >../actual) &&
    head -1 actual >first_line &&
    hash=$(awk "{print \$1}" <first_line) &&
    len=${#hash} &&
    test "$len" -ge 4 &&
    test "$len" -lt 40
'

test_expect_success 'show-ref --abbrev=40 shows full hash' '
    (cd repo &&
     grit show-ref --abbrev=40 >../actual) &&
    head -1 actual >first_line &&
    hash=$(awk "{print \$1}" <first_line) &&
    len=${#hash} &&
    test "$len" -eq 40
'

test_expect_success 'show-ref --hash --abbrev combines both flags' '
    (cd repo &&
     grit show-ref --hash --abbrev >../actual) &&
    head -1 actual >first_line &&
    ! grep "refs/" first_line &&
    hash=$(cat first_line | tr -d "\n") &&
    len=${#hash} &&
    test "$len" -lt 40
'

test_expect_success 'show-ref --hash=7 abbreviates hash to 7' '
    (cd repo &&
     grit show-ref --hash=7 >../actual) &&
    head -1 actual >first_line &&
    hash=$(cat first_line | tr -d "\n") &&
    len=${#hash} &&
    test "$len" -ge 7 &&
    test "$len" -le 10
'

test_expect_success 'show-ref --branches shows only branches' '
    (cd repo &&
     grit show-ref --branches >../actual) &&
    grep "refs/heads/" actual &&
    ! grep "refs/tags/" actual
'

test_expect_success 'show-ref --tags shows only tags' '
    (cd repo &&
     grit show-ref --tags >../actual) &&
    grep "refs/tags/" actual &&
    ! grep "refs/heads/" actual
'

test_expect_success 'show-ref with pattern filters output' '
    (cd repo &&
     grit show-ref refs/heads/br1 >../actual) &&
    test_line_count = 1 actual &&
    grep "refs/heads/br1" actual
'

test_expect_success 'show-ref with non-matching pattern returns empty' '
    (cd repo &&
     test_must_fail grit show-ref refs/heads/nonexistent >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'show-ref --verify checks exact ref' '
    (cd repo &&
     grit show-ref --verify refs/heads/br1 >../actual) &&
    test_line_count = 1 actual
'

test_expect_success 'show-ref --verify fails on nonexistent ref' '
    (cd repo &&
     test_must_fail grit show-ref --verify refs/heads/nonexistent)
'

test_expect_success 'show-ref --verify --quiet suppresses output' '
    (cd repo &&
     grit show-ref --verify --quiet refs/heads/br1 >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'show-ref --verify --quiet fails silently on missing' '
    (cd repo &&
     test_must_fail grit show-ref --verify --quiet refs/heads/nonexistent >../actual) &&
    test_line_count = 0 actual
'

test_expect_success 'show-ref --exists checks ref existence' '
    (cd repo &&
     grit show-ref --exists refs/heads/br1)
'

test_expect_success 'show-ref --exists fails for missing ref' '
    (cd repo &&
     test_must_fail grit show-ref --exists refs/heads/nonexistent)
'

test_expect_success 'show-ref --head includes HEAD' '
    (cd repo &&
     grit show-ref --head >../actual) &&
    grep "^[0-9a-f]* HEAD$" actual
'

test_expect_success 'show-ref --head --hash includes HEAD hash' '
    (cd repo &&
     grit show-ref --head --hash >../actual &&
     grit show-ref --hash >../no_head) &&
    head_lines=$(wc -l <actual | tr -d " ") &&
    no_head_lines=$(wc -l <no_head | tr -d " ") &&
    test "$head_lines" -gt "$no_head_lines"
'

test_expect_success 'show-ref -d dereferences annotated tags' '
    (cd repo &&
     grit show-ref -d refs/tags/v2.0 >../actual) &&
    grep "refs/tags/v2.0$" actual &&
    grep "refs/tags/v2.0\\^{}$" actual
'

test_expect_success 'show-ref -d does not dereference lightweight tags' '
    (cd repo &&
     grit show-ref -d refs/tags/v1.0 >../actual) &&
    grep "refs/tags/v1.0$" actual &&
    ! grep "v1.0\\^{}" actual
'

test_expect_success 'show-ref -d --hash shows both hashes for annotated' '
    (cd repo &&
     grit show-ref -d --hash refs/tags/v2.0 >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'show-ref main ref matches rev-parse' '
    (cd repo &&
     grit show-ref --hash refs/heads/main >../show_hash &&
     grit rev-parse main >../rev_hash) &&
    test_cmp show_hash rev_hash
'

test_expect_success 'show-ref with multiple patterns' '
    (cd repo &&
     grit show-ref refs/heads/br1 refs/heads/br2 >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'show-ref --hash --abbrev=7 on tag' '
    (cd repo &&
     grit show-ref --hash --abbrev=7 refs/tags/v1.0 >../actual) &&
    hash=$(cat actual | tr -d "\n") &&
    len=${#hash} &&
    test "$len" -ge 7 &&
    test "$len" -le 10
'

test_expect_success 'setup more branches for count tests' '
    (cd repo &&
     grit branch br3 &&
     grit branch br4 &&
     grit branch br5
    )
'

test_expect_success 'show-ref --branches lists all branches' '
    (cd repo &&
     grit show-ref --branches >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'show-ref --tags counts tags correctly' '
    (cd repo &&
     grit show-ref --tags >../actual) &&
    test_line_count = 2 actual
'

test_expect_success 'show-ref full output includes both branches and tags' '
    (cd repo &&
     grit show-ref >../actual) &&
    br_count=$(grep -c "refs/heads/" actual) &&
    tag_count=$(grep -c "refs/tags/" actual) &&
    test "$br_count" -eq 6 &&
    test "$tag_count" -eq 2
'

test_done
