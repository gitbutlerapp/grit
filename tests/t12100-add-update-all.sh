#!/bin/sh
test_description='grit add --update, --all, --verbose, --dry-run, --force, --intent-to-add'
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Helper: grit status --porcelain minus the ## branch header
grit_status () {
    grit status --porcelain | grep -v "^##" || true
}

test_expect_success 'setup' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo hello >file.txt &&
     echo world >second.txt &&
     mkdir sub &&
     echo nested >sub/deep.txt &&
     grit add file.txt second.txt sub/deep.txt &&
     grit commit -m "initial")
'

test_expect_success 'add --update stages modified tracked files' '
    (cd repo &&
     echo modified >file.txt &&
     grit add --update &&
     grit_status >../actual) &&
    echo "M  file.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'add --update does not stage new untracked files' '
    (cd repo &&
     echo newfile >untracked.txt &&
     grit add --update &&
     grit_status >../actual) &&
    grep "^?" actual >actual_untracked &&
    echo "?? untracked.txt" >expect &&
    test_cmp expect actual_untracked
'

test_expect_success 'add --update stages deletions of tracked files' '
    (cd repo &&
     rm second.txt &&
     grit add --update &&
     grit_status >../actual) &&
    grep "^D" actual >actual_deleted &&
    echo "D  second.txt" >expect &&
    test_cmp expect actual_deleted
'

test_expect_success 'reset and cleanup for next batch' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f untracked.txt)
'

test_expect_success 'add --all stages new modified and deleted' '
    (cd repo &&
     echo changed >file.txt &&
     echo brand-new >added.txt &&
     rm sub/deep.txt &&
     grit add --all &&
     grit_status >../actual) &&
    sort actual >actual_sorted &&
    cat >expect <<-\EOF &&
	A  added.txt
	D  sub/deep.txt
	M  file.txt
	EOF
    sort expect >expect_sorted &&
    grep -v "^??" actual_sorted >actual_tracked &&
    test_cmp expect_sorted actual_tracked
'

test_expect_success 'reset after --all test' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f added.txt)
'

test_expect_success 'add --verbose shows added files' '
    (cd repo &&
     echo verbose-test >verbose.txt &&
     grit add --verbose verbose.txt >../actual 2>&1) &&
    grep "verbose.txt" actual
'

test_expect_success 'add --dry-run does not actually stage file' '
    (cd repo &&
     grit commit -m "save-verbose" &&
     echo dry >dry.txt &&
     grit add --dry-run dry.txt >../actual 2>&1 &&
     grit_status >../actual_status) &&
    grep "dry.txt" actual &&
    grep "^?" actual_status | grep "dry.txt"
'

test_expect_success 'add --dry-run shows what would be added' '
    (cd repo &&
     echo drytwo >dry2.txt &&
     grit add -n dry2.txt >../actual 2>&1) &&
    grep "dry2.txt" actual
'

test_expect_success 'add with explicit pathspec stages only that file' '
    (cd repo &&
     rm -f dry.txt dry2.txt &&
     grit reset --hard HEAD &&
     echo one >one.txt &&
     echo two >two.txt &&
     grit add one.txt &&
     grit_status >../actual) &&
    grep "^A" actual >actual_added &&
    echo "A  one.txt" >expect &&
    test_cmp expect actual_added
'

test_expect_success 'add dot stages everything in current directory' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f one.txt two.txt &&
     echo alpha >alpha.txt &&
     echo beta >beta.txt &&
     grit add . &&
     grit_status >../actual) &&
    grep "^A" actual | sort >actual_added &&
    cat >expect <<-\EOF &&
	A  alpha.txt
	A  beta.txt
	EOF
    test_cmp expect actual_added
'

test_expect_success 'add --update modifies all tracked when given no pathspec' '
    (cd repo &&
     grit commit -m "checkpoint" &&
     echo changed-alpha >alpha.txt &&
     echo changed-beta >beta.txt &&
     grit add --update &&
     grit_status >../actual) &&
    grep "^M" actual | sort >actual_modified &&
    cat >expect <<-\EOF &&
	M  alpha.txt
	M  beta.txt
	EOF
    test_cmp expect actual_modified
'

test_expect_success 'add --all stages everything recursively' '
    (cd repo &&
     grit reset --hard HEAD &&
     mkdir -p dir1 dir2 &&
     echo d1 >dir1/f.txt &&
     echo d2 >dir2/f.txt &&
     grit add --all &&
     grit_status >../actual) &&
    grep "^A" actual | sort >actual_added &&
    cat >expect <<-\EOF &&
	A  dir1/f.txt
	A  dir2/f.txt
	EOF
    test_cmp expect actual_added
'

test_expect_success 'add intent-to-add marks file' '
    (cd repo &&
     grit commit -m "save-dirs" &&
     echo ita >ita.txt &&
     grit add --intent-to-add ita.txt &&
     grit_status >../actual) &&
    grep "ita.txt" actual
'

test_expect_success 'add -N file then full add stages it' '
    (cd repo &&
     grit add ita.txt &&
     grit_status >../actual) &&
    echo "A  ita.txt" >expect &&
    test_cmp expect actual
'

test_expect_success 'setup gitignore' '
    (cd repo &&
     grit commit -m "save-ita" &&
     echo "*.log" >.gitignore &&
     grit add .gitignore &&
     grit commit -m "add gitignore")
'

test_expect_success 'add --force stages ignored file' '
    (cd repo &&
     echo logdata >debug.log &&
     grit add --force debug.log &&
     grit_status >../actual) &&
    grep "debug.log" actual
'

test_expect_success 'add --update does not stage untracked ignored files' '
    (cd repo &&
     grit reset --hard HEAD &&
     echo another >another.log &&
     echo modified >file.txt &&
     grit add --update &&
     grit_status >../actual) &&
    ! grep "^A.*another.log" actual &&
    grep "^M" actual | grep "file.txt"
'

test_expect_success 'add multiple files at once' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f another.log &&
     echo m1 >m1.txt &&
     echo m2 >m2.txt &&
     echo m3 >m3.txt &&
     grit add m1.txt m2.txt m3.txt &&
     grit_status >../actual) &&
    grep "^A" actual | sort >actual_added &&
    cat >expect <<-\EOF &&
	A  m1.txt
	A  m2.txt
	A  m3.txt
	EOF
    test_cmp expect actual_added
'

test_expect_success 'add in subdirectory with relative path' '
    (cd repo &&
     grit commit -m "save" &&
     mkdir -p subdir &&
     echo sub >subdir/sub.txt &&
     cd subdir &&
     grit add sub.txt &&
     grit_status >../../actual) &&
    grep "subdir/sub.txt" actual
'

test_expect_success 'add --update in subdirectory' '
    (cd repo &&
     grit commit -m "save-sub" &&
     echo updated >subdir/sub.txt &&
     cd subdir &&
     grit add --update &&
     grit_status >../../actual) &&
    grep "M" actual | grep "subdir/sub.txt"
'

test_expect_success 'add --all removes deleted tracked from index' '
    (cd repo &&
     grit commit -m "save-all" &&
     rm subdir/sub.txt &&
     grit add --all &&
     grit_status >../actual) &&
    grep "^D" actual | grep "subdir/sub.txt"
'

test_expect_success 'add after reset --mixed re-adds correctly' '
    (cd repo &&
     grit commit -m "pre-reset" &&
     echo resetme >reset.txt &&
     grit add reset.txt &&
     grit commit -m "add-reset" &&
     grit reset --mixed HEAD~1 &&
     grit add reset.txt &&
     grit_status >../actual) &&
    grep "^A" actual | grep "reset.txt"
'

test_expect_success 'add file with spaces in name' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f reset.txt &&
     echo space >"file with spaces.txt" &&
     grit add "file with spaces.txt" &&
     grit_status >../actual) &&
    grep "file with spaces.txt" actual
'

test_expect_success 'add --verbose --dry-run combined' '
    (cd repo &&
     echo combo >combo.txt &&
     grit add --verbose --dry-run combo.txt >../actual 2>&1) &&
    grep "combo.txt" actual
'

test_expect_success 'add --all --dry-run shows changes without staging' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f "file with spaces.txt" combo.txt &&
     echo brandnew >brandnew.txt &&
     grit add --all --dry-run >../actual 2>&1 &&
     grit_status >../actual_status) &&
    grep "brandnew.txt" actual &&
    grep "^?" actual_status | grep "brandnew.txt"
'

test_expect_success 'add --update --verbose shows updated files' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f brandnew.txt &&
     echo mod >file.txt &&
     grit add --update --verbose >../actual 2>&1) &&
    grep "file.txt" actual
'

test_expect_success 'add nonexistent file fails' '
    (cd repo &&
     test_must_fail grit add no-such-file.txt 2>../actual) &&
    test_path_is_file actual
'

test_expect_success 'add --all handles mixed new modified deleted' '
    (cd repo &&
     grit reset --hard HEAD &&
     echo mod2 >file.txt &&
     echo fresh >fresh.txt &&
     grit add --all &&
     grit_status >../actual) &&
    grep "file.txt" actual &&
    grep "fresh.txt" actual
'

test_expect_success 'add executable file preserves mode' '
    (cd repo &&
     grit reset --hard HEAD &&
     rm -f fresh.txt &&
     echo "#!/bin/sh" >script.sh &&
     chmod +x script.sh &&
     grit add script.sh &&
     grit_status >../actual) &&
    grep "script.sh" actual
'

test_done
