#!/bin/sh

test_description='init directory structure, permissions, --bare, --template, -b, -q, reinit'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'init creates .git directory' '
    grit init repo &&
    test -d repo/.git
'

test_expect_success 'init creates HEAD' '
    test -f repo/.git/HEAD
'

test_expect_success 'HEAD points to refs/heads/main by default' '
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect repo/.git/HEAD
'

test_expect_success 'init creates config file' '
    test -f repo/.git/config
'

test_expect_success 'config has repositoryformatversion=0' '
    (cd repo && grit config --get core.repositoryformatversion >../actual) &&
    echo "0" >expect &&
    test_cmp expect actual
'

test_expect_success 'config has bare=false for non-bare repo' '
    (cd repo && grit config --get core.bare >../actual) &&
    echo "false" >expect &&
    test_cmp expect actual
'

test_expect_success 'config has filemode=true' '
    (cd repo && grit config --get core.filemode >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'config has logallrefupdates=true' '
    (cd repo && grit config --get core.logallrefupdates >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'init creates objects directory' '
    test -d repo/.git/objects
'

test_expect_success 'init creates objects/pack directory' '
    test -d repo/.git/objects/pack
'

test_expect_success 'init creates objects/info directory' '
    test -d repo/.git/objects/info
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

test_expect_success 'init creates description file' '
    test -f repo/.git/description
'

test_expect_success 'init creates hooks directory' '
    test -d repo/.git/hooks
'

test_expect_success 'init creates info directory' '
    test -d repo/.git/info
'

test_expect_success 'objects directory is group-accessible' '
    test "$(stat -c "%a" repo/.git/objects)" = "775"
'

test_expect_success 'refs directory is group-accessible' '
    test "$(stat -c "%a" repo/.git/refs)" = "775"
'

test_expect_success 'HEAD file is 664' '
    test "$(stat -c "%a" repo/.git/HEAD)" = "664"
'

test_expect_success 'init --bare creates bare repo' '
    grit init --bare bare-repo &&
    test -f bare-repo/HEAD &&
    test -f bare-repo/config &&
    test -d bare-repo/objects &&
    test -d bare-repo/refs
'

test_expect_success 'bare repo has no .git subdirectory' '
    ! test -d bare-repo/.git
'

test_expect_success 'bare repo config has bare=true' '
    (cd bare-repo && grit config --get core.bare >../actual) &&
    echo "true" >expect &&
    test_cmp expect actual
'

test_expect_success 'bare repo does not have logallrefupdates' '
    (cd bare-repo && ! grit config --get core.logallrefupdates)
'

test_expect_success 'init -b sets initial branch name' '
    grit init -b develop branch-repo &&
    echo "ref: refs/heads/develop" >expect &&
    test_cmp expect branch-repo/.git/HEAD
'

test_expect_success 'init -b main works' '
    grit init -b main main-repo &&
    echo "ref: refs/heads/main" >expect &&
    test_cmp expect main-repo/.git/HEAD
'

test_expect_success 'init -b with slash in branch name' '
    grit init -b feature/init slash-repo &&
    echo "ref: refs/heads/feature/init" >expect &&
    test_cmp expect slash-repo/.git/HEAD
'

test_expect_success 'init -q produces no output' '
    grit init -q quiet-repo >actual 2>&1 &&
    test_must_be_empty actual
'

test_expect_success 'quiet init still creates valid repo' '
    test -f quiet-repo/.git/HEAD &&
    test -d quiet-repo/.git/objects
'

test_expect_success 'reinit existing repo succeeds' '
    grit init repo 2>err &&
    test -f repo/.git/HEAD
'

test_expect_success 'reinit preserves HEAD' '
    echo "ref: refs/heads/main" >expect &&
    grit init repo &&
    test_cmp expect repo/.git/HEAD
'

test_expect_success 'init with template copies hooks' '
    mkdir -p custom-template/hooks &&
    echo "#!/bin/sh" >custom-template/hooks/pre-commit &&
    chmod +x custom-template/hooks/pre-commit &&
    grit init --template custom-template tpl-repo &&
    test -f tpl-repo/.git/hooks/pre-commit
'

test_expect_success 'template hook content is preserved' '
    echo "#!/bin/sh" >expect &&
    test_cmp expect tpl-repo/.git/hooks/pre-commit
'

test_expect_success 'init in current directory' '
    mkdir curdir-repo && (cd curdir-repo && grit init .) &&
    test -f curdir-repo/.git/HEAD
'

test_expect_success 'init in nested directory creates parents' '
    grit init a/b/c/nested-repo &&
    test -f a/b/c/nested-repo/.git/HEAD
'

test_expect_success 'bare init with -b sets HEAD' '
    grit init --bare -b custom bare-branch &&
    echo "ref: refs/heads/custom" >expect &&
    test_cmp expect bare-branch/HEAD
'

test_expect_success 'init creates working repo that can commit' '
    grit init working &&
    (cd working &&
     git config user.email "t@t.com" &&
     git config user.name "T" &&
     echo data >f.txt &&
     grit add f.txt &&
     grit commit -m "test commit"
    )
'

test_done
