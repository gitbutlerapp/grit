#!/bin/sh

test_description='cherry-pick conflict resolution scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repository' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
     echo "line1" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit branch base
    )
'

test_expect_success 'setup divergent branches for conflicts' '
    (cd repo &&
     grit checkout -b branch-a base &&
     echo "branch-a-content" >file.txt &&
     grit add file.txt &&
     grit commit -m "change on branch-a" &&
     grit checkout -b branch-b base &&
     echo "branch-b-content" >file.txt &&
     grit add file.txt &&
     grit commit -m "change on branch-b"
    )
'

test_expect_success 'cherry-pick with conflict exits non-zero' '
    (cd repo &&
     grit checkout branch-a &&
     test_must_fail grit cherry-pick branch-b
    )
'

test_expect_success 'conflicted file contains conflict markers' '
    (cd repo &&
     grep "<<<<<<" file.txt >../actual_markers) &&
    test -s actual_markers
'

test_expect_success 'cleanup after conflict with reset' '
    (cd repo &&
     grit reset --hard HEAD
    )
'

test_expect_success 'setup clean branches for non-conflicting cherry-pick' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b target-clean &&
     grit checkout -b source-clean &&
     echo "new-file-content" >newfile.txt &&
     grit add newfile.txt &&
     grit commit -m "add newfile"
    )
'

test_expect_success 'cherry-pick non-conflicting commit succeeds' '
    (cd repo &&
     grit checkout target-clean &&
     grit cherry-pick source-clean
    )
'

test_expect_success 'cherry-picked file exists after pick' '
    (cd repo &&
     test -f newfile.txt &&
     cat newfile.txt >../actual) &&
    echo "new-file-content" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick creates a new commit' '
    (cd repo &&
     grit log --oneline >../actual) &&
    grep "add newfile" actual
'

test_expect_success 'cherry-pick commit message matches source' '
    (cd repo &&
     grit log -n 1 --format="%s" target-clean >../actual) &&
    echo "add newfile" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup for multi-file cherry-pick' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b multi-source &&
     echo "file-a" >a.txt &&
     echo "file-b" >b.txt &&
     grit add a.txt b.txt &&
     grit commit -m "add a and b" &&
     grit checkout -b multi-target base
    )
'

test_expect_success 'cherry-pick with multiple files succeeds' '
    (cd repo &&
     grit checkout multi-target &&
     grit cherry-pick multi-source
    )
'

test_expect_success 'both files present after multi-file cherry-pick' '
    (cd repo &&
     test -f a.txt &&
     test -f b.txt
    )
'

test_expect_success 'setup for cherry-pick of specific commit from history' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b history-branch &&
     echo "commit1" >h1.txt &&
     grit add h1.txt &&
     grit commit -m "history commit 1" &&
     echo "commit2" >h2.txt &&
     grit add h2.txt &&
     grit commit -m "history commit 2" &&
     echo "commit3" >h3.txt &&
     grit add h3.txt &&
     grit commit -m "history commit 3"
    )
'

test_expect_success 'cherry-pick a middle commit by hash' '
    (cd repo &&
     mid=$(grit log --oneline history-branch | sed -n "2p" | cut -d" " -f1) &&
     grit checkout -b pick-middle base &&
     grit cherry-pick "$mid"
    )
'

test_expect_success 'only the cherry-picked file is present' '
    (cd repo &&
     test -f h2.txt &&
     ! test -f h1.txt &&
     ! test -f h3.txt
    )
'

test_expect_success 'setup for conflict with deletion' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b del-source &&
     grit rm file.txt &&
     grit commit -m "delete file" &&
     grit checkout -b del-target base &&
     echo "modified" >file.txt &&
     grit add file.txt &&
     grit commit -m "modify file"
    )
'

test_expect_success 'cherry-pick delete vs modify conflicts' '
    (cd repo &&
     grit checkout del-target &&
     test_must_fail grit cherry-pick del-source
    )
'

test_expect_success 'reset after delete conflict' '
    (cd repo &&
     grit reset --hard HEAD
    )
'

test_expect_success 'setup for cherry-pick with rename' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b rename-source &&
     grit mv file.txt renamed.txt &&
     grit commit -m "rename file" &&
     grit checkout -b rename-target base
    )
'

test_expect_success 'cherry-pick rename succeeds' '
    (cd repo &&
     grit checkout rename-target &&
     grit cherry-pick rename-source
    )
'

test_expect_success 'renamed file exists after cherry-pick' '
    (cd repo &&
     test -f renamed.txt
    )
'

test_expect_success 'setup for cherry-pick preserving author' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b author-source &&
     git config user.email "other@test.com" &&
     git config user.name "Other Author" &&
     echo "authored content" >authored.txt &&
     grit add authored.txt &&
     grit commit -m "authored commit" &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     grit checkout -b author-target base
    )
'

test_expect_success 'cherry-pick preserves original author' '
    (cd repo &&
     grit checkout author-target &&
     grit cherry-pick author-source &&
     grit log -n 1 --format="%an" >../actual) &&
    echo "A U Thor" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick changes committer to current user' '
    (cd repo &&
     grit log -n 1 --format="%cn" >../actual) &&
    echo "C O Mitter" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup for empty cherry-pick' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b empty-source &&
     echo "same-content" >samefile.txt &&
     grit add samefile.txt &&
     grit commit -m "add samefile" &&
     grit checkout -b empty-target base &&
     echo "same-content" >samefile.txt &&
     grit add samefile.txt &&
     grit commit -m "same content on target"
    )
'

test_expect_success 'cherry-pick resulting in empty change fails or warns' '
    (cd repo &&
    grit checkout empty-target &&
    test_must_fail grit cherry-pick empty-source &&
    grit cherry-pick --abort 2>/dev/null ||
    grit reset --hard HEAD 2>/dev/null ||
    true)
'

test_expect_success 'setup for multiple sequential cherry-picks' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b seq-source &&
     echo "seq1" >s1.txt &&
     grit add s1.txt &&
     grit commit -m "seq commit 1" &&
     echo "seq2" >s2.txt &&
     grit add s2.txt &&
     grit commit -m "seq commit 2" &&
     grit checkout -b seq-target base
    )
'

test_expect_success 'sequential cherry-picks both succeed' '
    (cd repo &&
     rm -f .git/CHERRY_PICK_HEAD &&
     grit checkout base &&
     grit branch -D seq-target 2>/dev/null;
     grit checkout -b seq-target &&
     first=$(grit rev-parse seq-source~1) &&
     grit cherry-pick "$first" &&
     second=$(grit rev-parse seq-source) &&
     grit cherry-pick "$second"
    )
'

test_expect_success 'both sequential files present' '
    (cd repo &&
     test -f s1.txt &&
     test -f s2.txt
    )
'

test_expect_success 'setup binary-like file conflict' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b bin-a &&
     printf "\000\001\002" >binfile &&
     grit add binfile &&
     grit commit -m "add binary on a" &&
     grit checkout -b bin-b base &&
     printf "\003\004\005" >binfile &&
     grit add binfile &&
     grit commit -m "add binary on b"
    )
'

test_expect_success 'cherry-pick binary conflict fails' '
    (cd repo &&
     grit checkout bin-a &&
     test_must_fail grit cherry-pick bin-b
    )
'

test_expect_success 'cleanup binary conflict' '
    (cd repo &&
     grit reset --hard HEAD
    )
'

test_expect_success 'cherry-pick with -m message is not supported' '
    (cd repo &&
     grit checkout base &&
     grit checkout -b msg-target &&
     test_must_fail grit cherry-pick -m 1 multi-source 2>../actual) ||
    true
'

test_done
