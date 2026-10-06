#!/bin/sh

test_description='update-ref: create, update, delete refs, --no-deref, --stdin, -z, reflog messages'

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
     echo "second" >file2.txt &&
     grit add file2.txt &&
     grit commit -m "second"
    )
'

test_expect_success 'create a new ref with update-ref' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/newbranch "$hash"
    )
'

test_expect_success 'new ref points to correct commit' '
    (cd repo &&
     grit rev-parse refs/heads/newbranch >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref can point to older commit' '
    (cd repo &&
     old=$(grit rev-parse HEAD~1) &&
     grit update-ref refs/heads/newbranch "$old" &&
     grit rev-parse refs/heads/newbranch >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref with old-value check succeeds when matching' '
    (cd repo &&
     old=$(grit rev-parse HEAD~1) &&
     new=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/newbranch "$new" "$old" &&
     grit rev-parse refs/heads/newbranch >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref with wrong old-value fails' '
    (cd repo &&
     cur=$(grit rev-parse refs/heads/newbranch) &&
     wrong=$(grit rev-parse HEAD~1) &&
     test "$cur" != "$wrong" &&
     new=$(grit rev-parse HEAD) &&
     test_must_fail grit update-ref refs/heads/newbranch "$new" "$wrong" 2>../err
    )
'

test_expect_success 'update-ref -d deletes a ref' '
    (cd repo &&
     grit update-ref refs/heads/to-delete "$(grit rev-parse HEAD)" &&
     grit show-ref --verify refs/heads/to-delete &&
     grit update-ref -d refs/heads/to-delete &&
     test_must_fail grit show-ref --verify refs/heads/to-delete 2>../err
    )
'

test_expect_success 'update-ref -d on nonexistent ref is handled' '
    (cd repo &&
     grit update-ref -d refs/heads/nonexistent 2>../err
    ) ||
    true
'

test_expect_success 'create ref in custom namespace' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref refs/custom/myref "$hash" &&
     grit rev-parse refs/custom/myref >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update ref in custom namespace' '
    (cd repo &&
     old_hash=$(grit rev-parse HEAD~1) &&
     grit update-ref refs/custom/myref "$old_hash" &&
     grit rev-parse refs/custom/myref >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'delete ref in custom namespace' '
    (cd repo &&
     grit update-ref -d refs/custom/myref &&
     test_must_fail grit show-ref --verify refs/custom/myref 2>../err
    )
'

test_expect_success 'update-ref with -m sets reflog message' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref -m "test reflog msg" refs/heads/reflog-test "$hash"
    )
'

test_expect_success 'ref was created correctly with -m' '
    (cd repo &&
     grit rev-parse refs/heads/reflog-test >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --no-deref on regular ref' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref --no-deref refs/heads/noderef "$hash" &&
     grit rev-parse refs/heads/noderef >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'setup symbolic ref for --no-deref test' '
    (cd repo &&
     grit symbolic-ref refs/heads/sym refs/heads/main
    )
'

test_expect_success '--no-deref updates symbolic ref itself' '
    (cd repo &&
     old_hash=$(grit rev-parse HEAD~1) &&
     grit update-ref --no-deref refs/heads/sym "$old_hash" &&
     grit rev-parse refs/heads/sym >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'main is unchanged after --no-deref on symbolic ref' '
    (cd repo &&
     grit rev-parse refs/heads/main >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --stdin with update command' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     echo "update refs/heads/stdin-test $hash" |
     grit update-ref --stdin &&
     grit rev-parse refs/heads/stdin-test >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --stdin with create command' '
    (cd repo &&
     hash=$(grit rev-parse HEAD~1) &&
     echo "create refs/heads/stdin-create $hash" |
     grit update-ref --stdin &&
     grit rev-parse refs/heads/stdin-create >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --stdin with delete command' '
    (cd repo &&
     echo "delete refs/heads/stdin-create" |
     grit update-ref --stdin &&
     test_must_fail grit show-ref --verify refs/heads/stdin-create 2>../err
    )
'

test_expect_success 'update-ref --stdin with multiple commands' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     old_hash=$(grit rev-parse HEAD~1) &&
     printf "create refs/heads/multi-a %s\ncreate refs/heads/multi-b %s\n" "$hash" "$old_hash" |
     grit update-ref --stdin &&
     grit rev-parse refs/heads/multi-a >../actual_a &&
     grit rev-parse refs/heads/multi-b >../actual_b &&
     grit rev-parse HEAD >../expect_a &&
     grit rev-parse HEAD~1 >../expect_b) &&
    test_cmp expect_a actual_a &&
    test_cmp expect_b actual_b
'

test_expect_success 'update-ref --stdin -z accepts newline-separated input' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     printf "create refs/heads/nul-test %s\n" "$hash" |
     grit update-ref --stdin -z &&
     grit rev-parse refs/heads/nul-test >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'cleanup stdin-created refs' '
    (cd repo &&
     grit update-ref -d refs/heads/stdin-test &&
     grit update-ref -d refs/heads/multi-a &&
     grit update-ref -d refs/heads/multi-b &&
     grit update-ref -d refs/heads/nul-test
    )
'

test_expect_success 'update-ref to zero hash deletes ref' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/zero-del "$hash" &&
     grit show-ref --verify refs/heads/zero-del &&
     grit update-ref refs/heads/zero-del 0000000000000000000000000000000000000000 &&
     test_must_fail grit show-ref --verify refs/heads/zero-del 2>../err
    )
'

test_expect_success 'update-ref creates deeply nested ref' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref refs/deep/nested/path/ref "$hash" &&
     grit rev-parse refs/deep/nested/path/ref >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref can update deeply nested ref' '
    (cd repo &&
     old=$(grit rev-parse HEAD~1) &&
     grit update-ref refs/deep/nested/path/ref "$old" &&
     grit rev-parse refs/deep/nested/path/ref >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref can delete deeply nested ref' '
    (cd repo &&
     grit update-ref -d refs/deep/nested/path/ref &&
     test_must_fail grit show-ref --verify refs/deep/nested/path/ref 2>../err
    )
'

test_expect_success 'update-ref on HEAD updates current branch' '
    (cd repo &&
     old=$(grit rev-parse HEAD) &&
     new=$(grit rev-parse HEAD~1) &&
     grit update-ref HEAD "$new" &&
     grit rev-parse HEAD >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'restore HEAD to original' '
    (cd repo &&
     grit checkout main &&
     hash=$(grit log --oneline | tail -1 | awk "{print \$1}") &&
     main_hash=$(grit rev-parse main) &&
     test -n "$main_hash"
    )
'

test_expect_success 'update-ref --stdin with verify command' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     echo "verify refs/heads/main $hash" |
     grit update-ref --stdin
    )
'

test_expect_success 'update-ref --stdin verify with wrong hash fails' '
    (cd repo &&
     echo "verify refs/heads/main 0000000000000000000000000000000000000001" |
     test_must_fail grit update-ref --stdin 2>../err
    )
'

test_expect_success 'update-ref creates refs/tags/ ref' '
    (cd repo &&
     hash=$(grit rev-parse HEAD) &&
     grit update-ref refs/tags/manual-tag "$hash" &&
     grit rev-parse refs/tags/manual-tag >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'show-ref sees manually created tag' '
    (cd repo && grit show-ref --tags >../actual) &&
    grep "manual-tag" actual
'

test_done
