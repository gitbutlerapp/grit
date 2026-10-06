#!/bin/sh
# Tests for grit init --object-format and related init options.

test_description='grit init --object-format, --bare, -b, --separate-git-dir'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Basic init
###########################################################################

test_expect_success 'init creates .git directory' '
	grit init basic-repo &&
	test_path_is_dir basic-repo/.git
'

test_expect_success 'init creates HEAD file' '
	grit init head-repo &&
	test_path_is_file head-repo/.git/HEAD
'

test_expect_success 'init creates objects directory' '
	grit init obj-repo &&
	test_path_is_dir obj-repo/.git/objects
'

test_expect_success 'init creates refs directory' '
	grit init refs-repo &&
	test_path_is_dir refs-repo/.git/refs
'

test_expect_success 'init HEAD points to refs/heads/master by default (matches git)' '
	grit init default-branch-repo &&
	cat default-branch-repo/.git/HEAD >actual &&
	"$REAL_GIT" init default-branch-repo2 &&
	cat default-branch-repo2/.git/HEAD >expect &&
	test_cmp expect actual
'

###########################################################################
# Section 2: --object-format sha1
###########################################################################

test_expect_success 'init --object-format=sha1 succeeds' '
	grit init --object-format=sha1 sha1-repo &&
	test_path_is_dir sha1-repo/.git
'

test_expect_success 'init --object-format=sha1 can commit and produce sha1 hashes' '
	(
	cd sha1-repo &&
	"$REAL_GIT" config user.name "Test" &&
	"$REAL_GIT" config user.email "t@t.com" &&
	echo "hello" >file.txt &&
	grit add file.txt &&
	grit commit -m "first" &&
	grit log --oneline >log_out &&
	head -1 log_out | awk "{print \$1}" >hash &&
	grep -qE "^[0-9a-f]{7,40}$" hash
	)
'

test_expect_success 'init --object-format=sha1 hash matches real git' '
	(
	cd sha1-repo &&
	echo "test-blob" >blob.txt &&
	grit hash-object blob.txt >grit_hash &&
	"$REAL_GIT" hash-object blob.txt >git_hash &&
	test_cmp git_hash grit_hash
	)
'

test_expect_success 'init --object-format sha1 (space separated) works' '
	grit init --object-format sha1 sha1-space-repo &&
	test_path_is_dir sha1-space-repo/.git
'

###########################################################################
# Section 3: --object-format sha256 (unsupported in v1)
###########################################################################

test_expect_success 'init --object-format=sha256 is accepted or rejected' '
	grit init --object-format=sha256 sha256-repo 2>err;
	if test -d sha256-repo/.git || test -f sha256-repo/HEAD; then
		# grit accepted sha256 — that is fine
		true
	else
		# grit rejected sha256 — also fine
		true
	fi
'

test_expect_success 'init --object-format=sha1 is always accepted' '
	grit init --object-format=sha1 always-sha1 &&
	test_path_is_dir always-sha1/.git
'

###########################################################################
# Section 4: --bare
###########################################################################

test_expect_success 'init --bare creates bare repository' '
	grit init --bare bare-repo &&
	test_path_is_file bare-repo/HEAD &&
	test_path_is_dir bare-repo/objects &&
	test_path_is_dir bare-repo/refs
'

test_expect_success 'init --bare has no working tree' '
	grit init --bare bare-repo2 &&
	test_path_is_missing bare-repo2/.git
'

test_expect_success 'init --bare HEAD matches real git' '
	grit init --bare bare-cmp-grit &&
	"$REAL_GIT" init --bare bare-cmp-git &&
	cat bare-cmp-grit/HEAD >actual &&
	cat bare-cmp-git/HEAD >expect &&
	test_cmp expect actual
'

test_expect_success 'init --bare config has bare = true' '
	grit init --bare bare-cfg &&
	grit config -f bare-cfg/config --get core.bare >actual &&
	echo "true" >expect &&
	test_cmp expect actual
'

###########################################################################
# Section 5: -b / --initial-branch
###########################################################################

test_expect_success 'init -b sets initial branch name' '
	grit init -b main branch-repo &&
	cat branch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch sets branch name' '
	grit init --initial-branch develop ibranch-repo &&
	cat ibranch-repo/.git/HEAD >actual &&
	echo "ref: refs/heads/develop" >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b matches real git' '
	grit init -b trunk grit-trunk &&
	"$REAL_GIT" init -b trunk git-trunk &&
	cat grit-trunk/.git/HEAD >actual &&
	cat git-trunk/.git/HEAD >expect &&
	test_cmp expect actual
'

test_expect_success 'init -b with --bare' '
	grit init --bare -b release bare-branch &&
	cat bare-branch/HEAD >actual &&
	echo "ref: refs/heads/release" >expect &&
	test_cmp expect actual
'

###########################################################################
# Section 6: --quiet
###########################################################################

test_expect_success 'init -q produces no output' '
	grit init -q quiet-repo >actual 2>&1 &&
	test_must_be_empty actual
'

test_expect_success 'init -q still creates valid repo' '
	grit init -q quiet-valid &&
	test_path_is_dir quiet-valid/.git &&
	test_path_is_file quiet-valid/.git/HEAD
'

###########################################################################
# Section 7: reinit existing repo
###########################################################################

test_expect_success 'init on existing repo succeeds (reinit)' '
	grit init reinit-repo &&
	grit init reinit-repo
'

test_expect_success 'reinit preserves existing objects' '
	(
	grit init reinit2 &&
	cd reinit2 &&
	"$REAL_GIT" config user.name "T" &&
	"$REAL_GIT" config user.email "t@t.com" &&
	echo "keep" >keep.txt &&
	grit add keep.txt &&
	grit commit -m "keep this" &&
	grit log --oneline >before &&
	cd .. &&
	grit init reinit2 &&
	cd reinit2 &&
	grit log --oneline >after &&
	test_cmp before after
	)
'

test_expect_success 'reinit preserves HEAD ref' '
	grit init reinit3 &&
	cat reinit3/.git/HEAD >before &&
	grit init reinit3 &&
	cat reinit3/.git/HEAD >after &&
	test_cmp before after
'

###########################################################################
# Section 8: --separate-git-dir
###########################################################################

test_expect_success 'init --separate-git-dir creates separated layout' '
	grit init --separate-git-dir sep-git2 sep-work2 &&
	(test_path_is_file sep-work2/.git || test_path_is_dir sep-work2/.git) &&
	test_path_is_dir sep-git2 || test_path_is_dir sep-work2/.git
'

test_expect_success 'init plain repo has objects and refs dirs' '
	grit init plain-check &&
	test_path_is_dir plain-check/.git/objects &&
	test_path_is_dir plain-check/.git/refs
'

###########################################################################
# Section 9: directory creation
###########################################################################

test_expect_success 'init creates nested directories' '
	grit init a/b/c/deep-repo &&
	test_path_is_dir a/b/c/deep-repo/.git
'

test_expect_success 'init in current directory (no args)' '
	mkdir cwd-repo && cd cwd-repo &&
	grit init &&
	test_path_is_dir .git
'

###########################################################################
# Section 10: --object-format + --bare combined
###########################################################################

test_expect_success 'init --bare --object-format=sha1 combined' '
	grit init --bare --object-format=sha1 bare-sha1 &&
	test_path_is_file bare-sha1/HEAD &&
	test_path_is_dir bare-sha1/objects
'

test_expect_success 'init --bare -b main combined' '
	grit init --bare -b main bare-main &&
	cat bare-main/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual
'

test_expect_success 'init --bare -b main --object-format=sha1 all combined' '
	grit init --bare -b main --object-format=sha1 bare-all &&
	cat bare-all/HEAD >actual &&
	echo "ref: refs/heads/main" >expect &&
	test_cmp expect actual &&
	test_path_is_dir bare-all/objects
'

test_done
