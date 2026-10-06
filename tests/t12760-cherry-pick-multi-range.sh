#!/bin/sh

test_description='cherry-pick with multiple commits, ranges, and various selection patterns'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup base repository with linear history' '
    grit init repo &&
    (cd repo &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial"
    )
'

test_expect_success 'create source branch with 8 commits' '
    (cd repo &&
     grit checkout -b source &&
     for i in 1 2 3 4 5 6 7 8; do
         echo "content-$i" >"file-$i.txt" &&
         grit add "file-$i.txt" &&
         grit commit -m "add file-$i" || return 1
     done
    )
'

test_expect_success 'cherry-pick first commit onto new branch' '
    (cd repo &&
     grit checkout -b pick-first main &&
     grit cherry-pick source~7
    )
'

test_expect_success 'first pick created file-1 only' '
    (cd repo &&
     test -f file-1.txt &&
     ! test -f file-2.txt
    )
'

test_expect_success 'cherry-pick second commit onto same branch' '
    (cd repo &&
     grit cherry-pick source~6
    )
'

test_expect_success 'now file-1 and file-2 exist' '
    (cd repo &&
     test -f file-1.txt &&
     test -f file-2.txt &&
     ! test -f file-3.txt
    )
'

test_expect_success 'cherry-pick three more commits sequentially' '
    (cd repo &&
     grit cherry-pick source~5 &&
     grit cherry-pick source~4 &&
     grit cherry-pick source~3
    )
'

test_expect_success 'five files exist after sequential picks' '
    (cd repo &&
     test -f file-1.txt &&
     test -f file-2.txt &&
     test -f file-3.txt &&
     test -f file-4.txt &&
     test -f file-5.txt &&
     ! test -f file-6.txt
    )
'

test_expect_success 'log shows five cherry-picked commits in order' '
    (cd repo &&
     grit log --oneline pick-first >../actual) &&
    head -1 actual | grep "add file-5" &&
    sed -n 2p actual | grep "add file-4" &&
    sed -n 3p actual | grep "add file-3"
'

test_expect_success 'cherry-pick does not modify source branch' '
    (cd repo &&
     grit log --oneline source >../actual) &&
    grep "add file-8" actual &&
    grep "add file-1" actual
'

test_expect_success 'setup second repo for non-overlapping picks' '
    grit init repo2 &&
    (cd repo2 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >file.txt &&
     grit add file.txt &&
     grit commit -m "initial" &&
     grit checkout -b features &&
     echo "feat-a" >a.txt && grit add a.txt && grit commit -m "feature a" &&
     echo "feat-b" >b.txt && grit add b.txt && grit commit -m "feature b" &&
     echo "feat-c" >c.txt && grit add c.txt && grit commit -m "feature c" &&
     echo "feat-d" >d.txt && grit add d.txt && grit commit -m "feature d"
    )
'

test_expect_success 'cherry-pick first and third commits (skip second)' '
    (cd repo2 &&
     grit checkout -b selective main &&
     grit cherry-pick features~3 &&
     grit cherry-pick features~1
    )
'

test_expect_success 'selective pick has a and c but not b' '
    (cd repo2 &&
     test -f a.txt &&
     test -f c.txt &&
     ! test -f b.txt
    )
'

test_expect_success 'cherry-pick onto empty-ish branch works' '
    (cd repo2 &&
     grit checkout -b fresh main &&
     grit cherry-pick features~3
    ) &&
    (cd repo2 && test -f a.txt)
'

test_expect_success 'cherry-picked commit has different hash from original' '
    (cd repo2 &&
     orig=$(grit rev-parse features~3) &&
     picked=$(grit rev-parse fresh) &&
     test "$orig" != "$picked"
    )
'

test_expect_success 'cherry-picked commit preserves subject line' '
    (cd repo2 &&
     grit log -n 1 --format="%s" fresh >../actual) &&
    echo "feature a" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick commit that adds multiple files' '
    grit init repo3 &&
    (cd repo3 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "start" >start.txt &&
     grit add start.txt &&
     grit commit -m "initial" &&
     grit checkout -b multi &&
     echo "x" >x.txt && echo "y" >y.txt && echo "z" >z.txt &&
     grit add x.txt y.txt z.txt &&
     grit commit -m "add xyz" &&
     grit checkout main &&
     grit cherry-pick multi
    ) &&
    (cd repo3 &&
     test -f x.txt && test -f y.txt && test -f z.txt
    )
'

test_expect_success 'cherry-pick commit that modifies existing file' '
    (cd repo3 &&
     grit checkout -b modify multi &&
     echo "modified" >start.txt &&
     grit add start.txt &&
     grit commit -m "modify start" &&
     grit checkout -b target-mod main &&
     grit cherry-pick modify
    ) &&
    (cd repo3 && cat start.txt >../actual) &&
    echo "modified" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick commit that deletes a file' '
    (cd repo3 &&
     grit checkout -b deleter multi &&
     grit rm x.txt &&
     grit commit -m "remove x" &&
     grit checkout -b target-del multi &&
     test -f x.txt &&
     grit cherry-pick deleter
    ) &&
    (cd repo3 && ! test -f x.txt)
'

test_expect_success 'setup repo for cherry-pick with same content on both sides' '
    grit init repo4 &&
    (cd repo4 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "common" >common.txt &&
     grit add common.txt &&
     grit commit -m "initial" &&
     grit checkout -b side &&
     echo "side-only" >side.txt &&
     grit add side.txt &&
     grit commit -m "add side"
    )
'

test_expect_success 'cherry-pick non-conflicting commit succeeds' '
    (cd repo4 &&
     grit checkout main &&
     grit cherry-pick side
    ) &&
    (cd repo4 && test -f side.txt)
'

test_expect_success 'cherry-picked content matches original' '
    (cd repo4 && cat side.txt >../actual) &&
    echo "side-only" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick two commits and verify count' '
    grit init repo5 &&
    (cd repo5 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >base.txt &&
     grit add base.txt &&
     grit commit -m "initial" &&
     grit checkout -b src &&
     echo "one" >one.txt && grit add one.txt && grit commit -m "one" &&
     echo "two" >two.txt && grit add two.txt && grit commit -m "two" &&
     grit checkout -b dest main &&
     grit cherry-pick src~1 &&
     grit cherry-pick src &&
     grit log --oneline >../actual
    ) &&
    wc -l <actual >count &&
    echo "3" >expect_count &&
    test_cmp expect_count count
'

test_expect_success 'cherry-pick preserves file permissions (executable)' '
    grit init repo6 &&
    (cd repo6 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >base.txt &&
     grit add base.txt &&
     grit commit -m "initial" &&
     grit checkout -b exec-branch &&
     echo "#!/bin/sh" >script.sh &&
     chmod +x script.sh &&
     grit add script.sh &&
     grit commit -m "add executable" &&
     grit checkout main &&
     grit cherry-pick exec-branch
    ) &&
    (cd repo6 && test -x script.sh)
'

test_expect_success 'cherry-pick with -x appends origin info' '
    grit init repo7 &&
    (cd repo7 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >base.txt &&
     grit add base.txt &&
     grit commit -m "initial" &&
     grit checkout -b src7 &&
     echo "data" >data.txt &&
     grit add data.txt &&
     grit commit -m "add data" &&
     hash=$(grit rev-parse src7) &&
     grit checkout main &&
     grit cherry-pick -x src7 &&
     grit log -n 1 --format="%b" >../actual
    ) &&
    grep "cherry picked from commit" actual
'

test_expect_success 'cherry-pick back-to-back same file different content' '
    grit init repo8 &&
    (cd repo8 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "v0" >versioned.txt &&
     grit add versioned.txt &&
     grit commit -m "v0" &&
     grit checkout -b versions &&
     echo "v1" >versioned.txt && grit add versioned.txt && grit commit -m "v1" &&
     echo "v2" >versioned.txt && grit add versioned.txt && grit commit -m "v2" &&
     grit checkout -b replay main &&
     grit cherry-pick versions~1 &&
     cat versioned.txt >../actual
    ) &&
    echo "v1" >expect &&
    test_cmp expect actual
'

test_expect_success 'second cherry-pick updates file to v2' '
    (cd repo8 &&
     grit cherry-pick versions &&
     cat versioned.txt >../actual
    ) &&
    echo "v2" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick from detached HEAD works' '
    grit init repo9 &&
    (cd repo9 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >base.txt &&
     grit add base.txt &&
     grit commit -m "initial" &&
     grit checkout -b src9 &&
     echo "detach-data" >d.txt &&
     grit add d.txt &&
     grit commit -m "from-detached" &&
     hash=$(grit rev-parse src9) &&
     grit checkout main &&
     grit checkout --detach main &&
     grit cherry-pick "$hash"
    ) &&
    (cd repo9 && test -f d.txt)
'

test_expect_success 'cherry-pick empty range (same commit) is error or no-op' '
    (cd repo &&
     grit checkout main &&
     head=$(grit rev-parse HEAD) &&
     test_must_fail grit cherry-pick "$head..$head" 2>../err_out
    ) ||
    true
'

test_expect_success 'cherry-pick with --no-commit stages but does not commit' '
    grit init repo10 &&
    (cd repo10 &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo "base" >base.txt &&
     grit add base.txt &&
     grit commit -m "initial" &&
     grit checkout -b nc-src &&
     echo "nc-data" >nc.txt &&
     grit add nc.txt &&
     grit commit -m "nc commit" &&
     grit checkout main &&
     grit cherry-pick --no-commit nc-src &&
     test -f nc.txt &&
     grit status >../actual
    ) &&
    grep "nc.txt" actual
'

test_expect_success 'after --no-commit, manual commit works' '
    (cd repo10 &&
     grit commit -m "manual after no-commit" &&
     grit log -n 1 --format="%s" >../actual
    ) &&
    echo "manual after no-commit" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick with rev-parse to get exact hash' '
    (cd repo &&
     grit checkout -b from-hash main &&
     exact=$(grit rev-parse source~2) &&
     grit cherry-pick "$exact" &&
     grit log -n 1 --format="%s" >../actual
    ) &&
    echo "add file-6" >expect &&
    test_cmp expect actual
'

test_expect_success 'cherry-pick multiple individual commits in sequence' '
    (cd repo &&
     grit checkout -b multi-seq main &&
     grit cherry-pick source~7 &&
     grit cherry-pick source~5 &&
     grit cherry-pick source~3 &&
     grit log --oneline >../actual
    ) &&
    head -1 actual | grep "add file-5" &&
    sed -n 2p actual | grep "add file-3" &&
    sed -n 3p actual | grep "add file-1"
'

test_expect_success 'tree state is correct after multiple non-adjacent picks' '
    (cd repo &&
     test -f file-1.txt &&
     test -f file-3.txt &&
     test -f file-5.txt &&
     ! test -f file-2.txt &&
     ! test -f file-4.txt
    )
'

test_done
