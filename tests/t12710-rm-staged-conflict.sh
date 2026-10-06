#!/bin/sh
test_description='grit rm: staged files, --cached, --force, -r, --dry-run, --quiet, --ignore-unmatch'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     mkdir -p dir/sub &&
     echo a >file1.txt &&
     echo b >file2.txt &&
     echo c >file3.txt &&
     echo d >dir/nested.txt &&
     echo e >dir/sub/deep.txt &&
     grit add . &&
     grit commit -m "initial")
'

test_expect_success 'rm removes file from index and worktree' '
    (cd repo &&
     grit rm file1.txt &&
     grit_status >../actual) &&
    echo "D  file1.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'removed file no longer on disk' '
    test ! -f repo/file1.txt
'

test_expect_success 'commit removal' '
    (cd repo && grit commit -m "remove file1")
'

test_expect_success 'rm --cached removes from index but keeps worktree' '
    (cd repo &&
     grit rm --cached file2.txt &&
     grit_status >../actual) &&
    grep "^D  file2.txt" actual &&
    test -f repo/file2.txt
'

test_expect_success 'commit cached removal and clean up leftover' '
    (cd repo &&
     grit commit -m "remove file2 from index" &&
     rm -f file2.txt)
'

test_expect_success 'rm on file with staged changes requires --force' '
    (cd repo &&
     echo modified >file3.txt &&
     grit add file3.txt &&
     test_must_fail grit rm file3.txt 2>../err) &&
    grep -i "staged" err || grep -i "force" err || grep -i "change" err
'

test_expect_success 'rm --force removes file with staged changes' '
    (cd repo &&
     grit rm --force file3.txt &&
     grit_status >../actual) &&
    echo "D  file3.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'commit force removal' '
    (cd repo && grit commit -m "force remove file3")
'

test_expect_success 'rm -r removes directory recursively' '
    (cd repo &&
     grit rm -r dir &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect <<-\EOF &&
	D  dir/nested.txt
	D  dir/sub/deep.txt
	EOF
    sort expect >expect_sorted &&
    test_cmp expect_sorted actual_sorted
'

test_expect_success 'rm -r removed files from disk' '
    test ! -f repo/dir/nested.txt &&
    test ! -f repo/dir/sub/deep.txt
'

test_expect_success 'commit recursive removal' '
    (cd repo && grit commit -m "remove dir")
'

test_expect_success 'recreate files for more tests' '
    (cd repo &&
     echo aa >a.txt &&
     echo bb >b.txt &&
     echo cc >c.txt &&
     mkdir -p sub &&
     echo dd >sub/d.txt &&
     grit add . &&
     grit commit -m "recreate files")
'

test_expect_success 'rm --dry-run shows what would be removed' '
    (cd repo &&
     grit rm --dry-run a.txt >../actual 2>&1) &&
    grep "a.txt" actual
'

test_expect_success 'rm --dry-run does not actually remove file' '
    test -f repo/a.txt &&
    (cd repo && grit ls-files >../actual) &&
    grep "a.txt" actual
'

test_expect_success 'rm --quiet suppresses output' '
    (cd repo &&
     grit rm --quiet b.txt >../actual 2>&1) &&
    test_must_be_empty actual
'

test_expect_success 'commit quiet removal' '
    (cd repo && grit commit -m "remove b.txt quietly")
'

test_expect_success 'rm nonexistent file fails' '
    (cd repo &&
     test_must_fail grit rm nonexistent.txt 2>../err) &&
    test -s err
'

test_expect_success 'rm --ignore-unmatch on nonexistent file succeeds' '
    (cd repo &&
     grit rm --ignore-unmatch nonexistent.txt)
'

test_expect_success 'rm --cached with --dry-run shows but does not remove' '
    (cd repo &&
     grit rm --cached --dry-run a.txt >../actual 2>&1) &&
    grep "a.txt" actual &&
    (cd repo && grit ls-files >../ls_actual) &&
    grep "a.txt" ls_actual
'

test_expect_success 'rm --cached removes from index only' '
    (cd repo &&
     grit rm --cached a.txt &&
     grit_status >../actual) &&
    grep "^D  a.txt" actual &&
    test -f repo/a.txt
'

test_expect_success 'commit and re-add for more tests' '
    (cd repo &&
     grit commit -m "remove a from index" &&
     grit add a.txt &&
     grit commit -m "re-add a")
'

test_expect_success 'rm on file with local modifications requires force' '
    (cd repo &&
     echo changed >a.txt &&
     test_must_fail grit rm a.txt 2>../err) &&
    test -s err
'

test_expect_success 'rm --force on locally modified file succeeds' '
    (cd repo &&
     grit rm -f a.txt &&
     grit_status >../actual) &&
    grep "^D  a.txt" actual
'

test_expect_success 'commit force rm' '
    (cd repo && grit commit -m "force rm a")
'

test_expect_success 'rm -r --cached on directory removes index entries only' '
    (cd repo &&
     grit rm -r --cached sub &&
     grit_status >../actual) &&
    grep "^D  sub/d.txt" actual &&
    test -f repo/sub/d.txt
'

test_expect_success 'commit cached dir removal' '
    (cd repo && grit commit -m "rm cached sub dir")
'

test_expect_success 'recreate for remaining tests' '
    (cd repo &&
     echo x >x.txt &&
     echo y >y.txt &&
     echo z >z.txt &&
     grit add . &&
     grit commit -m "add xyz")
'

test_expect_success 'rm multiple files at once' '
    (cd repo &&
     grit rm x.txt y.txt &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect <<-\EOF &&
	D  x.txt
	D  y.txt
	EOF
    sort expect >expect_sorted &&
    test_cmp expect_sorted actual_sorted
'

test_expect_success 'commit multi-rm' '
    (cd repo && grit commit -m "remove x and y")
'

test_expect_success 'rm --cached on newly added file leaves it untracked' '
    (cd repo &&
     echo brand-new >brand.txt &&
     grit add brand.txt &&
     grit rm --cached brand.txt &&
     grit_status >../actual) &&
    echo "?? brand.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'cleanup brand' '
    (cd repo && rm -f brand.txt)
'

test_expect_success 'rm on file not in index fails' '
    (cd repo &&
     echo untracked >unk.txt &&
     test_must_fail grit rm unk.txt 2>../err) &&
    test -s err
'

test_expect_success 'cleanup untracked' '
    (cd repo && rm -f unk.txt)
'

test_expect_success 'rm --ignore-unmatch on untracked file succeeds' '
    (cd repo &&
     grit rm --ignore-unmatch untracked-nope.txt)
'

test_expect_success 'rm on last tracked file' '
    (cd repo &&
     grit rm z.txt &&
     grit_status >../actual) &&
    grep "^D  z.txt" actual
'

test_expect_success 'commit final rm' '
    (cd repo && grit commit -m "remove z" --allow-empty)
'

test_done
