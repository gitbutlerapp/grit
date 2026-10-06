#!/bin/sh

test_description='cherry-pick sequences: multiple commits, abort, continue, skip, and ordering'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---- PART 1: Non-conflicting sequences (repo) ----

test_expect_success 'setup base repository' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch base
    )
'

test_expect_success 'setup source branch with five commits' '
    (cd repo &&
     grit checkout -b source base &&
     echo "a" >a.txt && grit add a.txt && grit commit -m "add a" &&
     echo "b" >b.txt && grit add b.txt && grit commit -m "add b" &&
     echo "c" >c.txt && grit add c.txt && grit commit -m "add c" &&
     echo "d" >d.txt && grit add d.txt && grit commit -m "add d" &&
     echo "e" >e.txt && grit add e.txt && grit commit -m "add e"
    )
'

test_expect_success 'cherry-pick single commit' '
    (cd repo &&
     grit checkout -b pick-one base &&
     hash=$(grit rev-parse source~4) &&
     grit cherry-pick "$hash"
    )
'

test_expect_success 'only first file present after single pick' '
    (cd repo &&
     test -f a.txt &&
     ! test -f b.txt
    )
'

test_expect_success 'cherry-pick two consecutive commits' '
    (cd repo &&
     grit checkout -b pick-two base &&
     grit cherry-pick "$(grit rev-parse source~4)" &&
     grit cherry-pick "$(grit rev-parse source~3)"
    )
'

test_expect_success 'both files present after two picks' '
    (cd repo &&
     test -f a.txt &&
     test -f b.txt &&
     ! test -f c.txt
    )
'

test_expect_success 'log shows cherry-picked commits' '
    (cd repo &&
     grit log --oneline pick-two >../actual) &&
    grep "add a" actual &&
    grep "add b" actual
'

test_expect_success 'cherry-pick three commits preserves order' '
    (cd repo &&
     grit checkout -b pick-three base &&
     grit cherry-pick "$(grit rev-parse source~4)" &&
     grit cherry-pick "$(grit rev-parse source~3)" &&
     grit cherry-pick "$(grit rev-parse source~2)" &&
     grit log --oneline >../actual) &&
    head -1 actual | grep "add c" &&
    sed -n 2p actual | grep "add b" &&
    sed -n 3p actual | grep "add a"
'

test_expect_success 'three files present, no extras' '
    (cd repo &&
     test -f a.txt && test -f b.txt && test -f c.txt &&
     ! test -f d.txt
    )
'

test_expect_success 'cherry-pick all five commits' '
    (cd repo &&
     grit checkout -b pick-five base &&
     for i in 4 3 2 1 0; do
         grit cherry-pick "$(grit rev-parse source~$i)" || return 1
     done
    )
'

test_expect_success 'all five files present' '
    (cd repo &&
     test -f a.txt && test -f b.txt && test -f c.txt &&
     test -f d.txt && test -f e.txt
    )
'

test_expect_success 'pick-five has six commits total' '
    (cd repo &&
     grit log --oneline pick-five >../actual) &&
    test_line_count = 6 actual
'

test_expect_success 'cherry-pick onto diverged base produces new hash' '
    (cd repo &&
     grit checkout base &&
     echo "diverge" >extra.txt && grit add extra.txt && grit commit -m "diverge" &&
     grit checkout -b pick-diverged &&
     h=$(grit rev-parse source~4) &&
     grit cherry-pick "$h" &&
     grit rev-parse HEAD >../pick_h &&
     grit rev-parse source~4 >../src_h) &&
    ! test_cmp pick_h src_h
'

test_expect_success 'cherry-pick middle commit only' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b pick-mid &&
     grit cherry-pick "$(grit rev-parse source~2)" &&
     test -f c.txt &&
     ! test -f b.txt &&
     ! test -f d.txt
    )
'

test_expect_success 'cherry-pick file addition does not touch other files' '
    (cd repo &&
     cat file.txt >../actual) &&
    echo "base" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick same commit twice yields empty pick' '
    (cd repo &&
    grit checkout -b double base &&
    h=$(grit rev-parse source~4) &&
    grit cherry-pick "$h" &&
    test_must_fail grit cherry-pick "$h" &&
    rm -f .git/CHERRY_PICK_HEAD)
'

test_expect_success 'cherry-pick source branch is unmodified' '
    (cd repo &&
     grit log --oneline source >../actual) &&
    test_line_count = 6 actual &&
    grep "add e" actual
'

test_expect_success 'cherry-pick preserves author name' '
    (cd repo &&
     grit checkout -b author-check base &&
     grit cherry-pick "$(grit rev-parse source~4)" &&
     grit log -n 1 --format="%an" >../actual) &&
    echo "A U Thor" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick preserves author email' '
    (cd repo &&
     grit log -n 1 --format="%ae" >../actual) &&
    echo "author@example.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup and pick from different author' '
    (cd repo &&
     grit checkout -b other-author base &&
     git config user.name "Other" &&
     git config user.email "other@test.com" &&
     echo "other" >other.txt && grit add other.txt && grit commit -m "other commit" &&
     git config user.name "T" &&
     git config user.email "t@t.com" &&
     grit checkout -b pick-other base &&
     grit cherry-pick "$(grit rev-parse other-author)" &&
     grit log -n 1 --format="%an" >../actual) &&
    echo "A U Thor" >expect &&
    test_cmp expect actual
'

test_expect_success 'committer is current user' '
    (cd repo &&
     grit log -n 1 --format="%cn" >../actual) &&
    echo "C O Mitter" >expect &&
    test_cmp expect actual
'

test_expect_success 'diff-tree shows cherry-picked file' '
    (cd repo &&
     grit diff-tree --name-only -r HEAD >../actual) &&
    grep "other.txt" actual
'

# ---- PART 2: Conflict + abort (separate repo) ----

test_expect_success 'setup abort repo' '
    grit init abort-repo &&
    (cd abort-repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch base &&
     grit checkout -b csrc &&
     echo "src-change" >file.txt && grit add file.txt && grit commit -m "src" &&
     grit checkout -b ctgt base &&
     echo "tgt-change" >file.txt && grit add file.txt && grit commit -m "tgt"
    )
'

test_expect_success 'cherry-pick conflict exits non-zero' '
    (cd abort-repo &&
     h=$(grit rev-parse csrc) &&
     test_must_fail grit cherry-pick "$h"
    )
'

test_expect_success 'CHERRY_PICK_HEAD exists after conflict' '
    (cd abort-repo &&
     test -f .git/CHERRY_PICK_HEAD
    )
'

test_expect_success 'git cherry-pick --abort restores state' '
    (cd abort-repo &&
     /usr/bin/git cherry-pick --abort
    )
'

test_expect_success 'no CHERRY_PICK_HEAD after abort' '
    (cd abort-repo &&
     ! test -f .git/CHERRY_PICK_HEAD
    )
'

test_expect_success 'HEAD unchanged after abort' '
    (cd abort-repo &&
     grit log -n 1 --format="%s" >../actual) &&
    echo "tgt" >expect &&
    test_cmp expect actual
'

# ---- PART 3: Conflict + continue (separate repo) ----

test_expect_success 'setup continue repo' '
    grit init cont-repo &&
    (cd cont-repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch base &&
     grit checkout -b csrc &&
     echo "src-change" >file.txt && grit add file.txt && grit commit -m "src" &&
     grit checkout -b ctgt base &&
     echo "tgt-change" >file.txt && grit add file.txt && grit commit -m "tgt"
    )
'

test_expect_success 'cherry-pick conflict then resolve and continue' '
    (cd cont-repo &&
     h=$(grit rev-parse csrc) &&
     test_must_fail grit cherry-pick "$h" &&
     echo "resolved" >file.txt &&
     /usr/bin/git add file.txt &&
     /usr/bin/git cherry-pick --continue --no-edit
    )
'

test_expect_success 'continue created commit with source message' '
    (cd cont-repo &&
     grit log -n 1 --format="%s" >../actual) &&
    echo "src" >expect &&
    test_cmp expect actual
'

test_expect_success 'resolved content is correct' '
    (cd cont-repo &&
     cat file.txt >../actual) &&
    echo "resolved" >expect &&
    test_cmp expect actual
'

# ---- PART 4: Conflict + skip (separate repo) ----

test_expect_success 'setup skip repo' '
    grit init skip-repo &&
    (cd skip-repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch base &&
     grit checkout -b csrc &&
     echo "src-change" >file.txt && grit add file.txt && grit commit -m "src" &&
     grit checkout -b ctgt base &&
     echo "tgt-change" >file.txt && grit add file.txt && grit commit -m "tgt"
    )
'

test_expect_success 'cherry-pick conflict then skip' '
    (cd skip-repo &&
     h=$(grit rev-parse csrc) &&
     test_must_fail grit cherry-pick "$h" &&
     /usr/bin/git cherry-pick --skip
    )
'

test_expect_success 'skip leaves HEAD at target commit' '
    (cd skip-repo &&
     grit log -n 1 --format="%s" >../actual) &&
    echo "tgt" >expect &&
    test_cmp expect actual
'

test_expect_success 'no CHERRY_PICK_HEAD after skip' '
    (cd skip-repo &&
     ! test -f .git/CHERRY_PICK_HEAD
    )
'

test_done
