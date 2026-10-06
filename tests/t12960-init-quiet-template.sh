#!/bin/sh

test_description='init: --quiet, --template, --bare, -b, reinit, directory handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'init creates .git directory' '
    grit init repo &&
    test -d repo/.git
'

test_expect_success 'init creates HEAD' '
    test -f repo/.git/HEAD
'

test_expect_success 'init HEAD points to refs/heads/main by default' '
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect repo/.git/HEAD
'

test_expect_success 'init creates objects directory' '
    test -d repo/.git/objects
'

test_expect_success 'init creates refs directory' '
    test -d repo/.git/refs
'

test_expect_success 'init creates refs/heads' '
    test -d repo/.git/refs/heads
'

test_expect_success 'init creates refs/tags' '
    test -d repo/.git/refs/tags
'

test_expect_success 'init creates config file' '
    test -f repo/.git/config
'

test_expect_success 'init creates description file' '
    test -f repo/.git/description
'

test_expect_success 'init prints message to stdout' '
    grit init msg_test >actual 2>&1 &&
    grep "Initialized" actual
'

test_expect_success 'init -q suppresses output' '
    grit init -q quiet_repo >actual 2>&1 &&
    test_must_be_empty actual
'

test_expect_success 'init -q creates valid repository' '
    test -d quiet_repo/.git &&
    test -f quiet_repo/.git/HEAD &&
    test -d quiet_repo/.git/objects &&
    test -d quiet_repo/.git/refs
'

test_expect_success 'init --quiet suppresses output' '
    grit init --quiet quiet2 >actual 2>&1 &&
    test_must_be_empty actual
'

test_expect_success 'init --quiet creates valid repository' '
    test -d quiet2/.git &&
    test -f quiet2/.git/HEAD
'

test_expect_success 'init -b sets custom initial branch' '
    grit init -b main branch_repo &&
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect branch_repo/.git/HEAD
'

test_expect_success 'init --initial-branch sets custom branch' '
    grit init --initial-branch=develop branch2 &&
    echo "ref: refs/heads/develop" >expect &&
    test_cmp expect branch2/.git/HEAD
'

test_expect_success 'init -b with unusual branch name' '
    grit init -b feature/test branch3 &&
    echo "ref: refs/heads/feature/test" >expect &&
    test_cmp expect branch3/.git/HEAD
'

test_expect_success 'init --bare creates bare repository' '
    grit init --bare bare_repo &&
    test -f bare_repo/HEAD &&
    test -d bare_repo/objects &&
    test -d bare_repo/refs &&
    ! test -d bare_repo/.git
'

test_expect_success 'init --bare config has bare=true' '
    (cd bare_repo && grit config get core.bare >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'init in current directory' '
    mkdir init_cwd &&
    (cd init_cwd && grit init) &&
    test -d init_cwd/.git
'

test_expect_success 'reinit existing repo succeeds' '
    grit init reinit_test &&
    grit init reinit_test &&
    test -d reinit_test/.git
'

test_expect_success 'reinit creates fresh config' '
    grit init reinit_test &&
    (cd reinit_test && grit config get core.repositoryformatversion >../actual) &&
    echo "0" >expect &&
    test_cmp expect actual
'

test_expect_success 'reinit preserves HEAD' '
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect reinit_test/.git/HEAD
'

test_expect_success 'reinit with -q is quiet' '
    grit init -q reinit_test >actual 2>&1 &&
    test_must_be_empty actual
'

test_expect_success 'template copies files into .git' '
    mkdir tpl1 &&
    echo "template content" >tpl1/custom_file &&
    grit init --template=tpl1 tpl_repo1 &&
    test -f tpl_repo1/.git/custom_file &&
    echo "template content" >expect &&
    test_cmp expect tpl_repo1/.git/custom_file
'

test_expect_success 'template copies multiple files' '
    mkdir tpl2 &&
    echo "file_a" >tpl2/file_a &&
    echo "file_b" >tpl2/file_b &&
    grit init --template=tpl2 tpl_repo2 &&
    test -f tpl_repo2/.git/file_a &&
    test -f tpl_repo2/.git/file_b
'

test_expect_success 'template does not override HEAD' '
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect tpl_repo2/.git/HEAD
'

test_expect_success 'template with empty directory' '
    mkdir tpl_empty &&
    grit init --template=tpl_empty tpl_repo3 &&
    test -d tpl_repo3/.git
'

test_expect_success 'init -q with --template still quiet' '
    mkdir tpl3 &&
    echo "data" >tpl3/info_file &&
    grit init -q --template=tpl3 tpl_quiet >actual 2>&1 &&
    test_must_be_empty actual &&
    test -f tpl_quiet/.git/info_file
'

test_expect_success 'init -b combined with --bare' '
    grit init --bare -b trunk bare_branch &&
    echo "ref: refs/heads/trunk" >expect &&
    test_cmp expect bare_branch/HEAD
'

test_expect_success 'init -q combined with --bare' '
    grit init -q --bare bare_quiet >actual 2>&1 &&
    test_must_be_empty actual &&
    test -f bare_quiet/HEAD
'

test_expect_success 'init -q combined with -b' '
    grit init -q -b custom quiet_branch >actual 2>&1 &&
    test_must_be_empty actual &&
    echo "ref: refs/heads/custom" >expect &&
    test_cmp expect quiet_branch/.git/HEAD
'

test_expect_success 'init creates hooks directory' '
    grit init hooks_test &&
    test -d hooks_test/.git/hooks
'

test_expect_success 'init creates info directory' '
    test -d hooks_test/.git/info
'

test_expect_success 'init with nested directory path' '
    grit init deep/nested/repo &&
    test -d deep/nested/repo/.git
'

test_expect_success 'init with absolute path' '
    abspath="$PWD/abs_repo" &&
    grit init "$abspath" &&
    test -d "$abspath/.git"
'

test_done
