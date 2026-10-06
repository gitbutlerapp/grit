#!/bin/sh

test_description='status --porcelain and --short output format'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
    grit init repo && cd repo &&
    git config user.email "t@t.com" && git config user.name "T" &&
    echo "hello" >file.txt &&
    echo "world" >other.txt &&
    grit add . &&
    grit commit -m "initial"
	)
'

test_expect_success 'porcelain shows nothing for clean repo' '
    (cd repo && grit status --porcelain >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'short shows nothing for clean repo' '
    (cd repo && grit status -s >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'untracked file shown as ??' '
    (cd repo && echo "new" >untracked.txt &&
     grit status --porcelain >../actual) &&
    grep "^?? untracked.txt$" actual
'

test_expect_success 'short format also shows ??' '
    (cd repo && grit status -s >../actual) &&
    grep "^?? untracked.txt$" actual
'

test_expect_success 'staged modification shows M in first column' '
    (cd repo && echo "modified" >file.txt && grit add file.txt &&
     grit status --porcelain >../actual) &&
    grep "^M  file.txt$" actual
'

test_expect_success 'staged addition shows A in first column' '
    (cd repo && grit add untracked.txt &&
     grit status --porcelain >../actual) &&
    grep "^A  untracked.txt$" actual
'

test_expect_success 'staged deletion shows D in first column' '
    (cd repo && grit commit -m "staged changes" &&
     grit rm other.txt &&
     grit status --porcelain >../actual) &&
    grep "^D  other.txt$" actual
'

test_expect_success 'commit and verify clean porcelain' '
    (cd repo && grit commit -m "del other" &&
     grit status --porcelain >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'both staged and unstaged shows MM' '
    (cd repo && echo "staged" >file.txt && grit add file.txt &&
     echo "unstaged" >file.txt &&
     grit status --porcelain >../actual) &&
    grep "^MM file.txt$" actual
'

test_expect_success 'short format shows MM too' '
    (cd repo && grit status -s >../actual) &&
    grep "^MM file.txt$" actual
'

test_expect_success 'porcelain -b includes branch header' '
    (cd repo && grit status --porcelain -b >../actual) &&
    head -1 actual >branch_line &&
    echo "## main" >expect &&
    test_cmp expect branch_line
'

test_expect_success 'porcelain without -b omits branch header' '
    (cd repo && grit status --porcelain >../actual) &&
    ! grep "^## main$" actual
'

test_expect_success 'short -b includes branch header' '
    (cd repo && grit status -s -b >../actual) &&
    grep "^## main$" actual
'

test_expect_success 'reset and test multiple untracked files' '
    (cd repo && git checkout -- . && grit add . && grit commit -m "save" &&
     echo "u1" >u1.txt && echo "u2" >u2.txt && echo "u3" >u3.txt &&
     grit status --porcelain >../actual) &&
    grep "^?? u1.txt$" actual &&
    grep "^?? u2.txt$" actual &&
    grep "^?? u3.txt$" actual
'

test_expect_success 'untracked in nested dir' '
    (cd repo && mkdir -p sub && echo "nested-new" >sub/new.txt &&
     grit status --porcelain >../actual) &&
    grep "sub/" actual
'

test_expect_success 'stage some and leave others untracked' '
    (cd repo && grit add u1.txt &&
     grit status --porcelain >../actual) &&
    grep "^A  u1.txt$" actual &&
    grep "^?? u2.txt$" actual &&
    grep "^?? u3.txt$" actual
'

test_expect_success 'untracked-files=no hides untracked' '
    (cd repo && grit status --porcelain -u no >../actual) &&
    grep "^A  u1.txt$" actual &&
    ! grep "^??" actual
'

test_expect_success 'mixed states: A, M, D, ??' '
    (cd repo && echo "mod" >file.txt && grit add file.txt &&
     grit status --porcelain >../actual) &&
    grep "^A  u1.txt$" actual &&
    grep "^M  file.txt$" actual &&
    grep "^?? u2.txt$" actual
'

test_expect_success 'commit and test -z NUL termination' '
    (cd repo && grit add . && grit commit -m "add all" &&
     echo "z1" >z1.txt && echo "z2" >z2.txt &&
     grit status --porcelain -z >../actual_raw) &&
    tr "\0" "\n" <actual_raw >actual &&
    grep "^?? z1.txt$" actual &&
    grep "^?? z2.txt$" actual
'

test_expect_success 'long format shows branch info' '
    (cd repo && grit status >../actual) &&
    grep "On branch main" actual
'

test_expect_success 'long format shows untracked section' '
    (cd repo && grit status >../actual) &&
    grep "Untracked files" actual &&
    grep "z1.txt" actual
'

test_expect_success 'long format shows nothing to commit when clean' '
    (cd repo && rm -f z1.txt z2.txt && grit status >../actual) &&
    grep "nothing to commit" actual
'

test_expect_success 'long format shows staged changes' '
    (cd repo && echo "staged" >file.txt && grit add file.txt &&
     grit status >../actual) &&
    grep "Changes to be committed" actual
'

test_expect_success 'long format shows unstaged changes' '
    (cd repo && echo "unstaged2" >file.txt &&
     grit status >../actual) &&
    grep "Changes not staged" actual
'

test_expect_success 'porcelain with nested directory additions' '
    (cd repo && git checkout -- . && grit add . && grit commit -m "save2" &&
     mkdir -p deep/path &&
     echo "d" >deep/path/d.txt &&
     grit add deep/path/d.txt &&
     grit status --porcelain >../actual) &&
    grep "^A  deep/path/d.txt$" actual
'

test_expect_success 'porcelain after committing nested addition' '
    (cd repo && grit commit -m "add deep" &&
     grit status --porcelain >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'modify and delete in same status' '
    (cd repo && echo "mod2" >file.txt && grit add file.txt &&
     grit rm deep/path/d.txt &&
     grit status --porcelain >../actual) &&
    grep "^M  file.txt$" actual &&
    grep "^D  deep/path/d.txt$" actual
'

test_expect_success 'porcelain entry count matches expected' '
    (cd repo && grit status --porcelain >../actual) &&
    grep -v "^##" actual >entries &&
    test_line_count = 2 entries
'

test_expect_success 'commit pending changes to reach clean state' '
    (cd repo && grit commit -m "pending" &&
     grit status --porcelain >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'gitignore setup' '
    (cd repo && echo "*.log" >.gitignore && grit add .gitignore &&
     grit commit -m "add gitignore" &&
     echo "ignored" >test.log &&
     grit status --porcelain >../actual) &&
    ! grep "test.log" actual
'

test_expect_success 'untracked-files=no hides untracked including ignored' '
    (cd repo && grit status --porcelain -u no >../actual) &&
    ! grep "test.log" actual
'

test_done
