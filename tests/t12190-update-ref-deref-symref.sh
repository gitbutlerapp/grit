#!/bin/sh

test_description='update-ref with --no-deref, symbolic refs, delete, and stdin'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "hello" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'update-ref creates a new ref' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/new-branch "$head"
    )
'

test_expect_success 'new ref points to correct commit' '
    (cd repo &&
     grit rev-parse refs/heads/new-branch >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref with old value succeeds when matching' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/new-branch "$head" "$head"
    )
'

test_expect_success 'update-ref with wrong old value fails' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     test_must_fail grit update-ref refs/heads/new-branch "$head" 0000000000000000000000000000000000000001
    )
'

test_expect_success 'update-ref -d deletes a ref' '
    (cd repo &&
     grit update-ref -d refs/heads/new-branch
    )
'

test_expect_success 'deleted ref no longer exists' '
    (cd repo &&
     test_must_fail grit show-ref --verify refs/heads/new-branch)
'

test_expect_success 'update-ref -d with old value succeeds when matching' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/del-test "$head" &&
     grit update-ref -d refs/heads/del-test "$head"
    )
'

test_expect_success 'update-ref -d with wrong old value fails' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/del-test2 "$head" &&
     test_must_fail grit update-ref -d refs/heads/del-test2 0000000000000000000000000000000000000001
    )
'

test_expect_success 'cleanup del-test2' '
    (cd repo &&
     grit update-ref -d refs/heads/del-test2
    )
'

test_expect_success 'setup symbolic ref for deref tests' '
    (cd repo &&
     grit symbolic-ref refs/heads/symlink refs/heads/main
    )
'

test_expect_success 'update-ref through symref updates target by default' '
    (cd repo &&
     echo "second" >file.txt &&
     grit add file.txt &&
     grit commit -m "second" &&
     new_head=$(grit rev-parse HEAD) &&
     grit checkout main &&
     old_head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/symlink "$new_head" &&
     grit rev-parse refs/heads/main >../actual &&
     echo "$new_head" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --no-deref on symref updates symref itself' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit symbolic-ref refs/heads/symlink2 refs/heads/main &&
     grit update-ref --no-deref refs/heads/symlink2 "$head" &&
     grit rev-parse refs/heads/symlink2 >../actual &&
     echo "$head" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'after --no-deref symref is no longer symbolic' '
    (cd repo &&
     test_must_fail grit symbolic-ref refs/heads/symlink2)
'

test_expect_success 'update-ref with -m sets reflog message' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref -m "test reflog msg" refs/heads/logged "$head"
    )
'

test_expect_success 'ref created with -m message exists' '
    (cd repo &&
     grit show-ref --verify refs/heads/logged)
'

test_expect_success 'update-ref can create refs under custom namespace' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/custom/myref "$head"
    )
'

test_expect_success 'custom namespace ref resolves correctly' '
    (cd repo &&
     grit rev-parse refs/custom/myref >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref can update existing ref to new value' '
    (cd repo &&
     echo "third" >file.txt &&
     grit add file.txt &&
     grit commit -m "third" &&
     new=$(grit rev-parse HEAD) &&
     grit update-ref refs/custom/myref "$new" &&
     grit rev-parse refs/custom/myref >../actual &&
     echo "$new" >../expect) &&
    test_cmp expect actual
'

test_expect_success 'update-ref --stdin with update command' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/stdin-test "$head" &&
     printf "update refs/heads/stdin-test %s %s\n" "$head" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'update-ref --stdin with create command' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     printf "create refs/heads/stdin-created %s\n" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'stdin-created ref exists' '
    (cd repo &&
     grit show-ref --verify refs/heads/stdin-created)
'

test_expect_success 'update-ref --stdin with delete command' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     printf "delete refs/heads/stdin-created %s\n" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'stdin-deleted ref is gone' '
    (cd repo &&
     test_must_fail grit show-ref --verify refs/heads/stdin-created)
'

test_expect_success 'update-ref --stdin with verify command' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     printf "verify refs/heads/main %s\n" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'update-ref --stdin verify with wrong hash fails' '
    (cd repo &&
     printf "verify refs/heads/main %s\n" "0000000000000000000000000000000000000001" |
     test_must_fail grit update-ref --stdin
    )
'

test_expect_success 'update-ref --stdin with empty line is ignored' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     printf "create refs/heads/empty-line-test %s\n\n" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'empty-line-test ref exists' '
    (cd repo &&
     grit show-ref --verify refs/heads/empty-line-test)
'

test_expect_success 'update-ref --stdin multiple commands' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     printf "create refs/heads/multi1 %s\ncreate refs/heads/multi2 %s\n" "$head" "$head" |
     grit update-ref --stdin
    )
'

test_expect_success 'both multi refs exist' '
    (cd repo &&
     grit show-ref --verify refs/heads/multi1 &&
     grit show-ref --verify refs/heads/multi2)
'

test_expect_success 'update-ref -d on nonexistent ref succeeds silently' '
    (cd repo &&
     grit update-ref -d refs/heads/does-not-exist)
'

test_expect_success 'update-ref to zero hash deletes ref' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/heads/zero-del "$head" &&
     grit update-ref refs/heads/zero-del 0000000000000000000000000000000000000000 &&
     test_must_fail grit show-ref --verify refs/heads/zero-del)
'

test_expect_success 'update-ref works on tag refs' '
    (cd repo &&
     head=$(grit rev-parse HEAD) &&
     grit update-ref refs/tags/manual-tag "$head"
    )
'

test_expect_success 'manual tag ref points correctly' '
    (cd repo &&
     grit rev-parse refs/tags/manual-tag >../actual &&
     grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'cleanup manual tag' '
    (cd repo &&
     grit update-ref -d refs/tags/manual-tag)
'

test_done
