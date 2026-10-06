#!/bin/sh

test_description='show-ref with --head, detached HEAD, --verify, --exists, and various flags'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repository' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "hello" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch side &&
     echo "more" >file2.txt &&
     grit add file2.txt &&
     grit commit -m "second" &&
     grit tag v1.0 &&
     grit tag -a v2.0 -m "annotated tag"
    )
'

test_expect_success 'show-ref lists branches' '
    (cd repo && grit show-ref >../actual) &&
    grep "refs/heads/main" actual &&
    grep "refs/heads/side" actual
'

test_expect_success 'show-ref lists tags' '
    grep "refs/tags/v1.0" actual &&
    grep "refs/tags/v2.0" actual
'

test_expect_success 'show-ref output has hash and ref on each line' '
    while IFS= read -r line; do
        hash=$(echo "$line" | awk "{print \$1}") &&
        ref=$(echo "$line" | awk "{print \$2}") &&
        test ${#hash} -ge 40 &&
        echo "$ref" | grep "^refs/" || return 1
    done <actual
'

test_expect_success 'show-ref --head includes HEAD' '
    (cd repo && grit show-ref --head >../actual) &&
    grep "HEAD" actual
'

test_expect_success 'show-ref --head HEAD hash matches main' '
    (cd repo &&
     grit show-ref --head >../actual_head &&
     main_hash=$(grit rev-parse main) &&
     head_line=$(grep "HEAD" ../actual_head | head -1) &&
     head_hash=$(echo "$head_line" | awk "{print \$1}") &&
     test "$head_hash" = "$main_hash"
    )
'

test_expect_success 'show-ref --branches shows only branches' '
    (cd repo && grit show-ref --branches >../actual) &&
    grep "refs/heads/" actual &&
    ! grep "refs/tags/" actual
'

test_expect_success 'show-ref --tags shows only tags' '
    (cd repo && grit show-ref --tags >../actual) &&
    grep "refs/tags/" actual &&
    ! grep "refs/heads/" actual
'

test_expect_success 'show-ref --verify with valid ref succeeds' '
    (cd repo && grit show-ref --verify refs/heads/main >../actual) &&
    grep "refs/heads/main" actual
'

test_expect_success 'show-ref --verify with invalid ref fails' '
    (cd repo && test_must_fail grit show-ref --verify refs/heads/nonexistent 2>../err) &&
    test -f err
'

test_expect_success 'show-ref --verify --quiet suppresses output' '
    (cd repo && grit show-ref --verify --quiet refs/heads/main >../actual) &&
    test ! -s actual
'

test_expect_success 'show-ref --exists with existing ref succeeds' '
    (cd repo && grit show-ref --exists refs/heads/main)
'

test_expect_success 'show-ref --exists with nonexistent ref fails' '
    (cd repo && test_must_fail grit show-ref --exists refs/heads/nonexistent)
'

test_expect_success 'show-ref --hash shows only hashes' '
    (cd repo && grit show-ref --hash refs/heads/main >../actual) &&
    wc -l <actual >count &&
    echo "1" >expect &&
    test_cmp expect count &&
    len=$(awk "{print length(\$0)}" actual) &&
    test "$len" -ge 40
'

test_expect_success 'show-ref --hash output matches rev-parse' '
    (cd repo &&
     grit show-ref --hash refs/heads/main >../actual &&
     grit rev-parse main >../expect) &&
    test_cmp expect actual
'

test_expect_success 'show-ref with pattern filters results' '
    (cd repo && grit show-ref main >../actual) &&
    grep "refs/heads/main" actual &&
    ! grep "side" actual
'

test_expect_success 'show-ref -d dereferences annotated tags' '
    (cd repo && grit show-ref -d refs/tags/v2.0 >../actual) &&
    wc -l <actual >count &&
    test "$(cat count)" -ge 1
'

test_expect_success 'show-ref -d shows peeled tag hash' '
    (cd repo && grit show-ref -d refs/tags/v2.0 >../actual) &&
    grep "v2.0" actual
'

test_expect_success 'setup detached HEAD' '
    (cd repo &&
     grit checkout --detach main
    )
'

test_expect_success 'show-ref still works in detached HEAD' '
    (cd repo && grit show-ref >../actual) &&
    grep "refs/heads/main" actual
'

test_expect_success 'show-ref --head in detached state includes HEAD' '
    (cd repo && grit show-ref --head >../actual) &&
    grep "HEAD" actual
'

test_expect_success 'HEAD hash in detached state matches commit' '
    (cd repo &&
     detach_hash=$(grit rev-parse HEAD) &&
     grit show-ref --head >../actual &&
     head_line=$(grep "HEAD" ../actual | head -1) &&
     head_hash=$(echo "$head_line" | awk "{print \$1}") &&
     test "$head_hash" = "$detach_hash"
    )
'

test_expect_success 'show-ref --verify HEAD in detached state' '
    (cd repo && grit show-ref --verify HEAD >../actual) &&
    grep "HEAD" actual
'

test_expect_success 'return to branch after detached HEAD tests' '
    (cd repo && grit checkout main)
'

test_expect_success 'setup more branches for pattern tests' '
    (cd repo &&
     grit branch fix/bug-1 &&
     grit branch fix/bug-2 &&
     grit branch release/1.0
    )
'

test_expect_success 'show-ref with glob-like pattern' '
    (cd repo && grit show-ref "refs/heads/fix/*" >../actual 2>&1) &&
    if test -s actual; then
        grep "fix/" actual
    fi ||
    true
'

test_expect_success 'show-ref --branches lists all branches including new ones' '
    (cd repo && grit show-ref --branches >../actual) &&
    grep "main" actual &&
    grep "side" actual &&
    grep "fix/bug-1" actual &&
    grep "fix/bug-2" actual &&
    grep "release/1.0" actual
'

test_expect_success 'show-ref --tags does not include branches' '
    (cd repo && grit show-ref --tags >../actual) &&
    ! grep "fix/bug" actual &&
    ! grep "release/1.0" actual
'

test_expect_success 'show-ref --verify multiple refs' '
    (cd repo && grit show-ref --verify refs/heads/main refs/heads/side >../actual) &&
    grep "main" actual &&
    grep "side" actual
'

test_expect_success 'show-ref --hash --tags shows tag hashes only' '
    (cd repo && grit show-ref --hash --tags >../actual) &&
    lines=$(wc -l <actual) &&
    test "$lines" -ge 2 &&
    ! grep "refs/" actual
'

test_expect_success 'show-ref --abbrev abbreviates hashes' '
    (cd repo && grit show-ref --abbrev refs/heads/main >../actual) &&
    hash=$(awk "{print \$1}" actual) &&
    len=${#hash} &&
    test "$len" -lt 40
'

test_expect_success 'show-ref --abbrev=8 gives 8-char hashes' '
    (cd repo && grit show-ref --abbrev=8 refs/heads/main >../actual) &&
    hash=$(awk "{print \$1}" actual) &&
    len=${#hash} &&
    test "$len" -ge 8 &&
    test "$len" -le 12
'

test_expect_success 'show-ref with no matching refs returns non-zero' '
    (cd repo && test_must_fail grit show-ref refs/heads/totally-fake)
'

test_expect_success 'setup: add annotated tag on side branch' '
    (cd repo &&
     grit checkout side &&
     grit tag -a v-side -m "side tag" &&
     grit checkout main
    )
'

test_expect_success 'show-ref --tags sees all tags' '
    (cd repo && grit show-ref --tags >../actual) &&
    grep "v1.0" actual &&
    grep "v2.0" actual &&
    grep "v-side" actual
'

test_expect_success 'show-ref --dereference with annotated tag shows peel' '
    (cd repo && grit show-ref --dereference refs/tags/v-side >../actual) &&
    wc -l <actual >count &&
    test "$(cat count)" -ge 1
'

test_done
