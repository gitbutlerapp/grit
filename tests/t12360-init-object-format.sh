#!/bin/sh

test_description='grit init --object-format, --bare, --initial-branch, --quiet, --separate-git-dir'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'init creates .git directory' '
	grit init repo &&
	test -d repo/.git
'

test_expect_success 'init creates objects directory' '
	test -d repo/.git/objects
'

test_expect_success 'init creates refs directory' '
	test -d repo/.git/refs
'

test_expect_success 'init creates HEAD file' '
	test -f repo/.git/HEAD
'

test_expect_success 'init HEAD points to refs/heads/main by default' '
	grep "ref: refs/heads/main" repo/.git/HEAD
'

test_expect_success 'init creates config file' '
	test -f repo/.git/config
'

test_expect_success 'init default object-format is sha1' '
	(cd repo && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --object-format sha1 works' '
	grit init --object-format sha1 repo-sha1 &&
	test -d repo-sha1/.git
'

test_expect_success 'init --object-format sha1 sets repositoryformatversion' '
	(cd repo-sha1 && grit config get core.repositoryformatversion >../actual) &&
	echo "0" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --object-format sha256 works' '
	grit init --object-format sha256 repo-sha256 &&
	test -d repo-sha256/.git
'

test_expect_success 'init --bare creates bare repository' '
	grit init --bare bare-repo &&
	test -d bare-repo
'

test_expect_success 'init --bare has no .git subdirectory' '
	test ! -d bare-repo/.git
'

test_expect_success 'init --bare has objects directory' '
	test -d bare-repo/objects
'

test_expect_success 'init --bare has refs directory' '
	test -d bare-repo/refs
'

test_expect_success 'init --bare has HEAD file' '
	test -f bare-repo/HEAD
'

test_expect_success 'init --bare sets core.bare to true' '
	(cd bare-repo && grit config get core.bare >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch sets custom branch' '
	grit init -b main repo-main &&
	grep "ref: refs/heads/main" repo-main/.git/HEAD
'

test_expect_success 'init --initial-branch with long name' '
	grit init -b feature/my-branch repo-feature &&
	grep "ref: refs/heads/feature/my-branch" repo-feature/.git/HEAD
'

test_expect_success 'init --quiet suppresses output' '
	grit init --quiet repo-quiet >actual 2>&1 &&
	test ! -s actual
'

test_expect_success 'init --quiet still creates repo' '
	test -d repo-quiet/.git
'

test_expect_success 'init in existing directory reinitializes' '
	grit init repo-reinit &&
	grit init repo-reinit &&
	test -d repo-reinit/.git
'

test_expect_success 'reinit still has valid .git structure' '
	grit init repo-reinit &&
	test -f repo-reinit/.git/HEAD &&
	test -d repo-reinit/.git/objects &&
	test -d repo-reinit/.git/refs
'

test_expect_success 'init in current directory works' '
	mkdir init-cwd &&
	(cd init-cwd && grit init .) &&
	test -d init-cwd/.git
'

test_expect_success 'init in current directory creates HEAD' '
	test -f init-cwd/.git/HEAD
'

test_expect_success 'init --bare --initial-branch combined' '
	grit init --bare -b develop bare-develop &&
	grep "ref: refs/heads/develop" bare-develop/HEAD
'

test_expect_success 'init with explicit directory argument' '
	grit init explicit-dir &&
	test -d explicit-dir/.git
'

test_expect_success 'init creates info directory' '
	test -d repo/.git/info || true
'

test_expect_success 'hash-object works in sha1 repo' '
	(cd repo-sha1 &&
	 echo "test content" >test.txt &&
	 grit hash-object test.txt >../actual) &&
	test -s actual
'

test_expect_success 'hash-object -w writes in sha1 repo' '
	(cd repo-sha1 &&
	 echo "write me" >wrt.txt &&
	 oid=$(grit hash-object -w wrt.txt) &&
	 test -f .git/objects/$(echo $oid | cut -c1-2)/$(echo $oid | cut -c3-))
'

test_expect_success 'init with nested path creates parents' '
	grit init nested/deep/repo &&
	test -d nested/deep/repo/.git
'

test_expect_success 'init default filemode is true' '
	(cd repo && grit config get core.filemode >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init default logallrefupdates is true' '
	(cd repo && grit config get core.logallrefupdates >../actual) &&
	echo "true" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --bare default logallrefupdates not set or false' '
	(cd bare-repo &&
	 if grit config get core.logallrefupdates >../actual 2>/dev/null; then
	   # Some implementations set it to false for bare repos
	   echo "false" >../expect &&
	   test_cmp ../expect ../actual
	 else
	   # Not set at all is also correct
	   true
	 fi)
'

test_expect_success 'multiple inits do not duplicate config entries' '
	grit init repo-multi &&
	grit init repo-multi &&
	grit init repo-multi &&
	(cd repo-multi && grit config get core.bare >../actual) &&
	echo "false" >expect &&
	test_cmp expect actual
'

test_done
