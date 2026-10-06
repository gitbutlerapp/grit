#!/bin/sh

test_description='commit: -m, -F, --amend, --allow-empty, --author, -a, -q, message handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo' '
    grit init repo &&
    (cd repo &&
     grit config user.email "t@t.com" &&
     grit config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial")
'

test_expect_success 'commit -m creates commit with message' '
    (cd repo &&
     echo change1 >>file.txt &&
     grit add file.txt &&
     grit commit -m "first change") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "first change" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit -m with multi-word message' '
    (cd repo &&
     echo change2 >>file.txt &&
     grit add file.txt &&
     grit commit -m "this is a longer commit message") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "this is a longer commit message" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit -F reads message from file' '
    echo "message from file" >msg_file &&
    (cd repo &&
     echo change3 >>file.txt &&
     grit add file.txt &&
     grit commit -F ../msg_file) &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "message from file" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit --amend changes last commit message' '
    (cd repo && grit commit --amend -m "amended message") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "amended message" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit --amend preserves tree when no changes' '
    (cd repo &&
     tree_before=$(grit rev-parse HEAD^{tree}) &&
     grit commit --amend -m "re-amended" &&
     tree_after=$(grit rev-parse HEAD^{tree}) &&
     test "$tree_before" = "$tree_after")
'

test_expect_success 'commit --amend includes staged changes' '
    (cd repo &&
     echo amend_extra >>file.txt &&
     grit add file.txt &&
     grit commit --amend -m "amend with changes") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "amend with changes" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit --amend does not change parent' '
    (cd repo &&
     parent_before=$(grit rev-parse HEAD~1) &&
     grit commit --amend -m "amend again" &&
     parent_after=$(grit rev-parse HEAD~1) &&
     test "$parent_before" = "$parent_after")
'

test_expect_success 'commit --allow-empty creates empty commit' '
    (cd repo &&
     hash_before=$(grit rev-parse HEAD^{tree}) &&
     grit commit --allow-empty -m "empty commit" &&
     hash_after=$(grit rev-parse HEAD^{tree}) &&
     test "$hash_before" = "$hash_after")
'

test_expect_success 'empty commit has correct message' '
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "empty commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit without changes fails' '
    (cd repo && test_must_fail grit commit -m "should fail" 2>../err) &&
    grep -i "nothing to commit" err
'

test_expect_success 'commit --allow-empty-message with empty message' '
    (cd repo &&
     echo emptymsg >>file.txt &&
     grit add file.txt &&
     grit commit --allow-empty-message -m "")
'

test_expect_success 'commit -a stages and commits tracked files' '
    (cd repo &&
     echo auto_add >>file.txt &&
     grit commit -a -m "auto add") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "auto add" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit -a does not add untracked files' '
    (cd repo &&
     echo untracked >untracked.txt &&
     echo more >>file.txt &&
     grit commit -a -m "auto but not untracked") &&
    (cd repo && grit status >../actual) &&
    grep "untracked.txt" actual
'

test_expect_success 'cleanup untracked file' '
    (cd repo && rm -f untracked.txt)
'

test_expect_success 'commit --author overrides author' '
    (cd repo &&
     sane_unset GIT_COMMITTER_NAME &&
     sane_unset GIT_COMMITTER_EMAIL &&
     echo author_test >>file.txt &&
     grit add file.txt &&
     grit commit --author="Other Person <other@test.com>" -m "custom author") &&
    (cd repo && grit log -n 1 --format="%an" >../actual) &&
    echo "Other Person" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit --author sets correct email' '
    (cd repo && grit log -n 1 --format="%ae" >../actual) &&
    echo "other@test.com" >expect &&
    test_cmp expect actual
'

test_expect_success 'committer is still original user' '
    (cd repo && grit log -n 1 --format="%cn" >../actual) &&
    echo "T" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit -q suppresses output' '
    (cd repo &&
     echo quiet >>file.txt &&
     grit add file.txt &&
     grit commit -q -m "quiet commit" >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'quiet commit was actually created' '
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "quiet commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit creates new HEAD' '
    (cd repo &&
     old=$(grit rev-parse HEAD) &&
     echo newhead >>file.txt &&
     grit add file.txt &&
     grit commit -m "new head" &&
     new=$(grit rev-parse HEAD) &&
     test "$old" != "$new")
'

test_expect_success 'commit parent is previous HEAD' '
    (cd repo &&
     grit rev-parse HEAD~1 >../actual &&
     grit log -n 2 --format="%H" >../log_out) &&
    tail -1 log_out >expect &&
    test_cmp expect actual
'

test_expect_success 'commit with special characters in message' '
    (cd repo &&
     echo special >>file.txt &&
     grit add file.txt &&
     grit commit -m "fix: handle edge-case (issue #42)") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "fix: handle edge-case (issue #42)" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit with quotes in message' '
    (cd repo &&
     echo quotes >>file.txt &&
     grit add file.txt &&
     grit commit -m "say \"hello world\"") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "say \"hello world\"" >expect &&
    test_cmp expect actual
'

test_expect_success 'multiple commits maintain chain' '
    (cd repo &&
     echo chain1 >>file.txt && grit add file.txt && grit commit -m "chain1" &&
     echo chain2 >>file.txt && grit add file.txt && grit commit -m "chain2" &&
     echo chain3 >>file.txt && grit add file.txt && grit commit -m "chain3") &&
    (cd repo && grit log -n 3 --format="%s" >../actual) &&
    cat >expect <<-\EOF &&
	chain3
	chain2
	chain1
	EOF
    test_cmp expect actual
'

test_expect_success 'commit --amend with -F' '
    echo "amended from file" >amend_msg &&
    (cd repo && grit commit --amend -F ../amend_msg) &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "amended from file" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit -F from stdin via /dev/stdin' '
    (cd repo &&
     echo stdin_data >>file.txt &&
     grit add file.txt &&
     echo "stdin message" | grit commit -F /dev/stdin) &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "stdin message" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit --allow-empty --amend' '
    (cd repo &&
     grit commit --allow-empty --amend -m "empty amend") &&
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "empty amend" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit on branch creates commit on that branch' '
    (cd repo &&
     grit branch side_br &&
     grit switch side_br &&
     echo side >>file.txt &&
     grit add file.txt &&
     grit commit -m "side commit" &&
     grit branch --show-current >../actual) &&
    echo "side_br" >expect &&
    test_cmp expect actual
'

test_expect_success 'side branch commit message correct' '
    (cd repo && grit log -n 1 --format="%s" >../actual) &&
    echo "side commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'switch back and verify main unchanged' '
    (cd repo && grit switch main &&
     grit log -n 1 --format="%s" >../actual) &&
    echo "empty amend" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit count increases properly' '
    (cd repo && grit rev-list HEAD >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -gt 10
'

test_done
