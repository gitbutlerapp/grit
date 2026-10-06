#!/bin/sh
test_description='grit mv: cross-directory moves, rename, -f, -n, -k, -v, multi-source'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

# Filter to only tracked changes (exclude ??)
grit_status_tracked () {
    grit status --porcelain | grep -v "^##" | grep -v "^??" || true
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
     mkdir -p src dst sub/deep &&
     echo a >src/a.txt &&
     echo b >src/b.txt &&
     echo c >src/c.txt &&
     echo d >sub/deep/d.txt &&
     echo top >top.txt &&
     grit add . &&
     grit commit -m "initial")
'

test_expect_success 'mv renames a file in same directory' '
    (cd repo &&
     grit mv top.txt renamed.txt &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "renamed.txt" actual
'

test_expect_success 'renamed file exists, original does not' '
    test -f repo/renamed.txt &&
    test ! -f repo/top.txt
'

test_expect_success 'commit rename' '
    (cd repo && grit commit -m "rename top")
'

test_expect_success 'mv moves file across directories' '
    (cd repo &&
     grit mv src/a.txt dst/a.txt &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "dst/a.txt" actual
'

test_expect_success 'file is in new location on disk' '
    test -f repo/dst/a.txt &&
    test ! -f repo/src/a.txt
'

test_expect_success 'commit cross-dir move' '
    (cd repo && grit commit -m "move a to dst")
'

test_expect_success 'mv file into directory (destination is dir)' '
    (cd repo &&
     grit mv src/b.txt dst/ &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "dst/b.txt" actual
'

test_expect_success 'commit move to dir' '
    (cd repo && grit commit -m "move b to dst")
'

test_expect_success 'mv multiple files into directory' '
    (cd repo &&
     grit mv src/c.txt sub/deep/d.txt dst/ &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "dst/c.txt" actual &&
    grep "dst/d.txt" actual
'

test_expect_success 'commit multi move' '
    (cd repo && grit commit -m "move c and d to dst" && rm -rf src sub)
'

test_expect_success 'mv --dry-run shows what would happen' '
    (cd repo &&
     grit mv --dry-run dst/a.txt dst/a-renamed.txt >../actual 2>&1) &&
    grep "a.txt" actual
'

test_expect_success 'mv --dry-run does not actually move' '
    test -f repo/dst/a.txt &&
    test ! -f repo/dst/a-renamed.txt
'

test_expect_success 'mv --verbose shows action' '
    (cd repo &&
     grit mv -v dst/a.txt dst/a-moved.txt >../actual 2>&1) &&
    grep "a.txt" actual
'

test_expect_success 'commit verbose move' '
    (cd repo && grit commit -m "verbose move a")
'

test_expect_success 'mv refuses to overwrite existing tracked file' '
    (cd repo &&
     echo existing >dst/target.txt &&
     grit add dst/target.txt &&
     grit commit -m "add target" &&
     test_must_fail grit mv dst/b.txt dst/target.txt 2>../err) &&
    test -s err
'

test_expect_success 'mv --force overwrites existing file' '
    (cd repo &&
     grit mv --force dst/b.txt dst/target.txt &&
     grit_status_tracked >../actual) &&
    grep "dst/target.txt" actual
'

test_expect_success 'overwritten file has correct content' '
    (cd repo && cat dst/target.txt >../actual) &&
    echo "b" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit force overwrite' '
    (cd repo && grit commit -m "force overwrite target")
'

test_expect_success 'mv file from deep to top level' '
    (cd repo &&
     grit mv dst/c.txt c-top.txt &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "c-top.txt" actual
'

test_expect_success 'commit deep to top' '
    (cd repo && grit commit -m "move c to top")
'

test_expect_success 'mv file to same name is an error' '
    (cd repo &&
     test_must_fail grit mv c-top.txt c-top.txt 2>../err) &&
    test -s err
'

test_expect_success 'mv preserves file content' '
    (cd repo &&
     grit mv c-top.txt moved-content.txt &&
     cat moved-content.txt >../actual) &&
    echo "c" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit content preservation' '
    (cd repo && grit commit -m "move c-top to moved-content")
'

test_expect_success 'mv with -n -v shows dry-run verbose output' '
    (cd repo &&
     grit mv -n -v dst/d.txt dst/d-renamed.txt >../actual 2>&1) &&
    grep "d.txt" actual &&
    test -f repo/dst/d.txt &&
    test ! -f repo/dst/d-renamed.txt
'

test_expect_success 'mv file then commit' '
    (cd repo &&
     grit mv dst/d.txt d-final.txt &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "d-final.txt" actual
'

test_expect_success 'commit d move' '
    (cd repo && grit commit -m "move d to top")
'

test_expect_success 'mv newly added file works' '
    (cd repo &&
     echo newfile >new.txt &&
     grit add new.txt &&
     grit mv new.txt new-moved.txt &&
     grit_status_tracked >../actual) &&
    echo "A  new-moved.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit new moved file' '
    (cd repo && grit commit -m "add and move new file")
'

test_expect_success 'mv into freshly created directory' '
    (cd repo &&
     mkdir newdir &&
     grit mv renamed.txt newdir/ &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "newdir/renamed.txt" actual
'

test_expect_success 'commit to newdir' '
    (cd repo && grit commit -m "move to newdir")
'

test_expect_success 'ls-files reflects all moves' '
    (cd repo && grit ls-files >../actual) &&
    grep "newdir/renamed.txt" actual &&
    grep "moved-content.txt" actual &&
    ! grep "^top.txt$" actual
'

test_expect_success 'mv file with spaces in name' '
    (cd repo &&
     echo spacey >"file with spaces.txt" &&
     grit add "file with spaces.txt" &&
     grit commit -m "add spaced file" &&
     grit mv "file with spaces.txt" "no spaces.txt" &&
     grit_status_tracked >../actual) &&
    grep "no spaces.txt" actual
'

test_expect_success 'commit spaced rename' '
    (cd repo && grit commit -m "rename spaced file")
'

test_expect_success 'mv back and forth leaves file in final location' '
    (cd repo &&
     grit mv moved-content.txt temp-name.txt &&
     grit mv temp-name.txt final-name.txt &&
     grit_status_tracked >../actual) &&
    grep "R" actual &&
    grep "final-name.txt" actual
'

test_expect_success 'commit back-and-forth' '
    (cd repo && grit commit -m "move back and forth")
'

test_expect_success 'final state has expected files' '
    (cd repo && grit ls-files >../actual) &&
    grep "dst/a-moved.txt" actual &&
    grep "dst/target.txt" actual &&
    grep "newdir/renamed.txt" actual &&
    grep "final-name.txt" actual
'

test_done
