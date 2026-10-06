#!/bin/sh
test_description='grit add --intent-to-add (-N): placeholder entries, interactions with status, diff, commit, reset'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Helper: porcelain status without branch header
grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "initial")
'

# Intent-to-add stages an empty blob; worktree has content → AM status
test_expect_success 'add -N marks file as intent-to-add' '
    (cd repo &&
     echo new-content >new.txt &&
     grit add -N new.txt &&
     grit_status >../actual) &&
    echo " A new.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'ls-files shows intent-to-add file' '
    (cd repo &&
     grit ls-files >../actual) &&
    grep "new.txt" actual
'

test_expect_success 'reset to clean state' '
    (cd repo &&
     grit rm --cached new.txt &&
     rm -f new.txt)
'

test_expect_success 'add --intent-to-add long form works' '
    (cd repo &&
     echo another >ita.txt &&
     grit add --intent-to-add ita.txt &&
     grit_status >../actual) &&
    echo " A ita.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup ita file' '
    (cd repo &&
     grit rm --cached ita.txt &&
     rm -f ita.txt)
'

test_expect_success 'add -N on multiple files' '
    (cd repo &&
     echo one >m1.txt &&
     echo two >m2.txt &&
     echo three >m3.txt &&
     grit add -N m1.txt m2.txt m3.txt &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect_sorted <<-\EOF &&
	 A m1.txt
	 A m2.txt
	 A m3.txt
	EOF
    test_cmp expect_sorted actual_sorted
'

test_expect_success 'cleanup multi ita' '
    (cd repo &&
     grit rm --cached m1.txt m2.txt m3.txt &&
     rm -f m1.txt m2.txt m3.txt)
'

test_expect_success 'add -N then full add stages content properly' '
    (cd repo &&
     echo staged >staged.txt &&
     grit add -N staged.txt &&
     grit add staged.txt &&
     grit_status >../actual) &&
    echo "A  staged.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit after full add works' '
    (cd repo &&
     grit commit -m "add staged.txt" &&
     grit log --oneline >../actual) &&
    grep "add staged.txt" actual
'

test_expect_success 'add -N on file in subdirectory' '
    (cd repo &&
     mkdir -p sub/deep &&
     echo nested >sub/deep/nested.txt &&
     grit add -N sub/deep/nested.txt &&
     grit_status >../actual) &&
    echo " A sub/deep/nested.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup nested ita' '
    (cd repo &&
     grit rm --cached sub/deep/nested.txt &&
     rm -rf sub)
'

test_expect_success 'add -N then modify file shows modified status' '
    (cd repo &&
     echo original >modify-me.txt &&
     grit add -N modify-me.txt &&
     echo changed >modify-me.txt &&
     grit_status >../actual) &&
    grep "modify-me.txt" actual
'

test_expect_success 'cleanup modify-me' '
    (cd repo &&
     grit rm --cached modify-me.txt &&
     rm -f modify-me.txt)
'

test_expect_success 'add -N followed by rm --cached removes ita entry' '
    (cd repo &&
     echo remove-me >rm-ita.txt &&
     grit add -N rm-ita.txt &&
     grit rm --cached rm-ita.txt &&
     grit_status >../actual) &&
    echo "?? rm-ita.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup rm-ita' '
    (cd repo && rm -f rm-ita.txt)
'

test_expect_success 'add -N with verbose shows file' '
    (cd repo &&
     echo verbose-test >verbose.txt &&
     grit add -N -v verbose.txt >../actual 2>&1) &&
    grep "verbose.txt" actual
'

test_expect_success 'cleanup verbose' '
    (cd repo &&
     grit rm --cached verbose.txt &&
     rm -f verbose.txt)
'

test_expect_success 'add -N with dry-run does not actually stage' '
    (cd repo &&
     echo dryrun >dry.txt &&
     grit add -N --dry-run dry.txt &&
     grit_status >../actual) &&
    echo "?? dry.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup dryrun file' '
    (cd repo && rm -f dry.txt)
'

test_expect_success 'add -N on already ita file is idempotent' '
    (cd repo &&
     echo idem >idem.txt &&
     grit add -N idem.txt &&
     grit add -N idem.txt &&
     grit_status >../actual) &&
    echo " A idem.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup idem' '
    (cd repo &&
     grit rm --cached idem.txt &&
     rm -f idem.txt)
'

test_expect_success 'add -N then add --all properly stages content' '
    (cd repo &&
     echo alltest >alltest.txt &&
     grit add -N alltest.txt &&
     grit add --all &&
     grit_status >../actual) &&
    echo "A  alltest.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup alltest' '
    (cd repo &&
     grit rm --cached alltest.txt &&
     rm -f alltest.txt)
'

test_expect_success 'add -N then reset HEAD -- path unstages ita' '
    (cd repo &&
     echo resetme >resetme.txt &&
     grit add -N resetme.txt &&
     grit reset HEAD -- resetme.txt &&
     grit_status >../actual) &&
    echo "?? resetme.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup resetme' '
    (cd repo && rm -f resetme.txt)
'

test_expect_success 'add -N with force on ignored file' '
    (cd repo &&
     echo "ignored.txt" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "add gitignore" &&
     echo secret >ignored.txt &&
     grit add -N -f ignored.txt &&
     grit_status >../actual) &&
    grep "ignored.txt" actual
'

test_expect_success 'cleanup ignored test' '
    (cd repo &&
     grit rm --cached ignored.txt &&
     rm -f ignored.txt &&
     grit rm .gitignore &&
     grit commit -m "remove gitignore")
'

test_expect_success 'add -N then status shows file in ls-files' '
    (cd repo &&
     echo check >check-ls.txt &&
     grit add -N check-ls.txt &&
     grit ls-files >../actual) &&
    grep "check-ls.txt" actual
'

test_expect_success 'cleanup check-ls' '
    (cd repo &&
     grit rm --cached check-ls.txt &&
     rm -f check-ls.txt)
'

test_expect_success 'add -N on empty file' '
    (cd repo &&
     : >empty.txt &&
     grit add -N empty.txt &&
     grit ls-files >../actual) &&
    grep "empty.txt" actual
'

test_expect_success 'add -N then full add on empty file stages it' '
    (cd repo &&
     grit add empty.txt &&
     grit_status >../actual) &&
    echo "A  empty.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup empty' '
    (cd repo &&
     grit rm --cached empty.txt &&
     rm -f empty.txt)
'

test_expect_success 'add -N then grit add -u does not fully add ita files' '
    (cd repo &&
     echo u-test >u-test.txt &&
     grit add -N u-test.txt &&
     echo modified >file.txt &&
     grit add -u &&
     grit_status >../actual) &&
    grep "M  file.txt" actual &&
    grep "u-test.txt" actual
'

test_expect_success 'cleanup u-test' '
    (cd repo &&
     grit rm --cached u-test.txt &&
     rm -f u-test.txt &&
     grit reset --hard HEAD)
'

test_expect_success 'add -N then commit --allow-empty does not commit ita content' '
    (cd repo &&
     echo skipme >skipme.txt &&
     grit add -N skipme.txt &&
     grit commit --allow-empty -m "empty commit" &&
     grit log --oneline >../actual) &&
    grep "empty commit" actual
'

test_expect_success 'final cleanup' '
    (cd repo &&
     grit rm --cached skipme.txt 2>/dev/null;
     rm -f skipme.txt)
'

test_done
