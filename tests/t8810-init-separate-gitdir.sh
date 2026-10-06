#!/bin/sh
# Tests for git init with --separate-git-dir and related init scenarios.

test_description='init with separate git dir and various options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- basic init ----------------------------------------------------------------

test_expect_success 'init creates .git directory' '
	git init basic-repo &&
	test -d basic-repo/.git
'

test_expect_success 'init creates HEAD file' '
	test -f basic-repo/.git/HEAD
'

test_expect_success 'init creates refs directory' '
	test -d basic-repo/.git/refs
'

test_expect_success 'init creates objects directory' '
	test -d basic-repo/.git/objects
'

test_expect_success 'init HEAD points to refs/heads/main or refs/heads/master' '
	head_ref=$(cat basic-repo/.git/HEAD) &&
	case "$head_ref" in
	"ref: refs/heads/main"|"ref: refs/heads/master") true ;;
	*) echo "unexpected HEAD: $head_ref" && false ;;
	esac
'

# -- init in existing directory -----------------------------------------------

test_expect_success 'init in existing empty directory' '
	mkdir existing-dir &&
	git init existing-dir &&
	test -d existing-dir/.git
'

test_expect_success 'init in existing directory with files preserves files' '
	mkdir has-files &&
	echo "hello" >has-files/file.txt &&
	git init has-files &&
	test -d has-files/.git &&
	test -f has-files/file.txt &&
	echo "hello" >expect &&
	test_cmp expect has-files/file.txt
'

test_expect_success 're-init existing repo is safe' '
	git init re-init-repo &&
	test -d re-init-repo/.git &&
	git init re-init-repo &&
	test -d re-init-repo/.git
'

# -- bare init -----------------------------------------------------------------

test_expect_success 'init --bare creates bare repo' '
	git init --bare bare-repo &&
	test -f bare-repo/HEAD &&
	test -d bare-repo/refs &&
	test -d bare-repo/objects
'

test_expect_success 'init --bare has no .git subdirectory' '
	! test -d bare-repo/.git
'

test_expect_success 'init --bare config shows bare = true' '
	(
	cd bare-repo &&
	git config core.bare >out &&
	echo true >expect &&
	test_cmp expect out
	)
'

test_expect_success 'normal init config shows bare = false' '
	(
	cd basic-repo &&
	git config core.bare >out &&
	echo false >expect &&
	test_cmp expect out
	)
'

# -- init with object-format ---------------------------------------------------

test_expect_success 'init --object-format=sha1 works' '
	git init --object-format=sha1 objfmt-repo &&
	test -d objfmt-repo/.git
'

# -- init multiple times -------------------------------------------------------

test_expect_success 're-init preserves existing objects' '
	(
	git init reinit-obj &&
	cd reinit-obj &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "data" >f.txt &&
	git add f.txt &&
	test_tick &&
	git commit -m "keep this" &&
	git log --oneline >before &&
	git init . &&
	git log --oneline >after &&
	test_cmp before after
	)
'

test_expect_success 're-init does not remove objects directory' '
	(
	cd reinit-obj &&
	test -d .git/objects
	)
'

test_expect_success 're-init preserves branches' '
	(
	cd reinit-obj &&
	git branch test-branch &&
	git init . &&
	git branch >out &&
	grep test-branch out
	)
'

test_expect_success 're-init preserves HEAD target' '
	(
	cd reinit-obj &&
	head_before=$(cat .git/HEAD) &&
	git init . &&
	head_after=$(cat .git/HEAD) &&
	test "$head_before" = "$head_after"
	)
'

# -- working in initialized repo -----------------------------------------------

test_expect_success 'can add and commit after init' '
	(
	git init work-repo &&
	cd work-repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo "hello" >file.txt &&
	git add file.txt &&
	test_tick &&
	git commit -m "initial" &&
	git log --oneline >out &&
	grep initial out
	)
'

test_expect_success 'log shows commit in new repo' '
	(
	cd work-repo &&
	git log --oneline >out &&
	test "$(wc -l <out)" -eq 1
	)
'

# -- init with template --------------------------------------------------------

test_expect_success 'init creates config file' '
	git init config-check &&
	test -f config-check/.git/config
'

# -- init with initial branch --------------------------------------------------

test_expect_success 'init with -b sets initial branch name' '
	git init -b trunk branch-repo &&
	head_ref=$(cat branch-repo/.git/HEAD) &&
	echo "ref: refs/heads/trunk" >expect &&
	test_cmp expect branch-repo/.git/HEAD
'

test_expect_success 'init with --initial-branch sets branch name' '
	git init --initial-branch=develop branch-repo2 &&
	head_ref=$(cat branch-repo2/.git/HEAD) &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect branch-repo2/.git/HEAD
'

# -- nested repos --------------------------------------------------------------

test_expect_success 'init inside another repo creates nested repo' '
	git init outer &&
	mkdir -p outer/inner &&
	git init outer/inner &&
	test -d outer/.git &&
	test -d outer/inner/.git
'

# -- permissions ---------------------------------------------------------------

test_expect_success 'init objects directory exists and is accessible' '
	git init perm-repo &&
	test -d perm-repo/.git/objects &&
	test -r perm-repo/.git/objects
'

test_expect_success 'init refs/heads directory exists' '
	test -d perm-repo/.git/refs/heads
'

test_expect_success 'init refs/tags directory exists' '
	test -d perm-repo/.git/refs/tags
'

# -- quiet mode ----------------------------------------------------------------

test_expect_success 'init --quiet suppresses output' '
	git init --quiet quiet-repo >out 2>&1 &&
	test -d quiet-repo/.git &&
	test_must_be_empty out
'

test_done
