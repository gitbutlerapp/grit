#!/bin/sh
# Tests for grit rev-parse --git-dir, --show-toplevel, --is-inside-work-tree, etc.

test_description='grit rev-parse: --git-dir, --show-toplevel, --is-inside-work-tree, --is-bare-repository, --is-inside-git-dir, --show-prefix'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create normal repo with subdirs' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	mkdir -p src/lib docs/api &&
	echo "root" >root.txt &&
	echo "main" >src/main.rs &&
	echo "lib" >src/lib/mod.rs &&
	echo "api" >docs/api/index.md &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: --git-dir
###########################################################################

test_expect_success '--git-dir from repo root returns .git' '
	(
	cd repo &&
	git rev-parse --git-dir >output &&
	test "$(cat output)" = ".git"
	)
'

test_expect_success '--git-dir from repo root matches real git' '
	(
	cd repo &&
	git rev-parse --git-dir >grit_out &&
	"$REAL_GIT" rev-parse --git-dir >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success '--git-dir from subdir' '
	(
	cd repo/src &&
	git rev-parse --git-dir >output &&
	test -s output
	)
'

test_expect_success '--git-dir from deep subdir' '
	(
	cd repo/src/lib &&
	git rev-parse --git-dir >output &&
	test -s output
	)
'

test_expect_success '--git-dir points to valid git dir' '
	(
	cd repo &&
	gitdir=$(git rev-parse --git-dir) &&
	test -d "$gitdir" &&
	test -f "$gitdir/HEAD"
	)
'

###########################################################################
# Section 3: --show-toplevel
###########################################################################

test_expect_success '--show-toplevel from repo root' '
	(
	cd repo &&
	git rev-parse --show-toplevel >output &&
	test -s output
	)
'

test_expect_success '--show-toplevel from repo root matches real git' '
	(
	cd repo &&
	git rev-parse --show-toplevel >grit_out &&
	"$REAL_GIT" rev-parse --show-toplevel >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success '--show-toplevel from subdir returns repo root' '
	(
	cd repo/src &&
	git rev-parse --show-toplevel >output &&
	toplevel=$(cat output) &&
	test -f "$toplevel/root.txt"
	)
'

test_expect_success '--show-toplevel from deep subdir' '
	(
	cd repo/src/lib &&
	git rev-parse --show-toplevel >output &&
	toplevel=$(cat output) &&
	test -f "$toplevel/root.txt"
	)
'

test_expect_success '--show-toplevel from subdir matches real git' '
	(
	cd repo/src &&
	git rev-parse --show-toplevel >grit_out &&
	"$REAL_GIT" rev-parse --show-toplevel >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 4: --is-inside-work-tree
###########################################################################

test_expect_success '--is-inside-work-tree from work tree returns true' '
	(
	cd repo &&
	git rev-parse --is-inside-work-tree >output &&
	test "$(cat output)" = "true"
	)
'

test_expect_success '--is-inside-work-tree from subdir returns true' '
	(
	cd repo/src &&
	git rev-parse --is-inside-work-tree >output &&
	test "$(cat output)" = "true"
	)
'

test_expect_success '--is-inside-work-tree matches real git' '
	(
	cd repo &&
	git rev-parse --is-inside-work-tree >grit_out &&
	"$REAL_GIT" rev-parse --is-inside-work-tree >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 5: --is-bare-repository
###########################################################################

test_expect_success '--is-bare-repository on normal repo returns false' '
	(
	cd repo &&
	git rev-parse --is-bare-repository >output &&
	test "$(cat output)" = "false"
	)
'

test_expect_success '--is-bare-repository matches real git' '
	(
	cd repo &&
	git rev-parse --is-bare-repository >grit_out &&
	"$REAL_GIT" rev-parse --is-bare-repository >git_out &&
	test_cmp grit_out git_out
	)
'

test_expect_success 'setup: create bare repo' '
	"$REAL_GIT" init --bare bare.git
'

test_expect_success '--is-bare-repository on bare repo returns true' '
	(
	cd bare.git &&
	git rev-parse --is-bare-repository >output &&
	test "$(cat output)" = "true"
	)
'

test_expect_success '--is-bare-repository on bare repo matches real git' '
	(
	cd bare.git &&
	git rev-parse --is-bare-repository >grit_out &&
	"$REAL_GIT" rev-parse --is-bare-repository >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 6: --is-inside-git-dir
###########################################################################

test_expect_success '--is-inside-git-dir from work tree returns false' '
	(
	cd repo &&
	git rev-parse --is-inside-git-dir >output &&
	test "$(cat output)" = "false"
	)
'

test_expect_success '--is-inside-git-dir matches real git from work tree' '
	(
	cd repo &&
	git rev-parse --is-inside-git-dir >grit_out &&
	"$REAL_GIT" rev-parse --is-inside-git-dir >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 7: --show-prefix
###########################################################################

test_expect_success '--show-prefix from repo root is empty' '
	(
	cd repo &&
	result=$(git rev-parse --show-prefix) &&
	test -z "$result"
	)
'

test_expect_success '--show-prefix from src/ returns src/' '
	(
	cd repo/src &&
	git rev-parse --show-prefix >output &&
	test "$(cat output)" = "src/"
	)
'

test_expect_success '--show-prefix from deep subdir' '
	(
	cd repo/src/lib &&
	git rev-parse --show-prefix >output &&
	test "$(cat output)" = "src/lib/"
	)
'

test_expect_success '--show-prefix from docs/api/' '
	(
	cd repo/docs/api &&
	git rev-parse --show-prefix >output &&
	test "$(cat output)" = "docs/api/"
	)
'

test_expect_success '--show-prefix matches real git from subdir' '
	(
	cd repo/src &&
	git rev-parse --show-prefix >grit_out &&
	"$REAL_GIT" rev-parse --show-prefix >git_out &&
	test_cmp grit_out git_out
	)
'

###########################################################################
# Section 8: Combining rev-parse options with rev arguments
###########################################################################

test_expect_success 'rev-parse HEAD from subdir works' '
	(
	cd repo/src &&
	git rev-parse HEAD >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

test_expect_success 'rev-parse HEAD from subdir matches root' '
	(
	cd repo &&
	git rev-parse HEAD >root_hash &&
	cd src &&
	git rev-parse HEAD >sub_hash &&
	test_cmp ../root_hash sub_hash
	)
'

test_expect_success 'rev-parse main from subdir works' '
	(
	cd repo/src/lib &&
	git rev-parse main >output &&
	grep -qE "^[0-9a-f]{40}$" output
	)
'

###########################################################################
# Section 9: Bare repo specifics
###########################################################################

test_expect_success '--git-dir in bare repo' '
	(
	cd bare.git &&
	git rev-parse --git-dir >output &&
	test -s output
	)
'

test_expect_success '--is-inside-work-tree in bare repo returns false' '
	(
	cd bare.git &&
	git rev-parse --is-inside-work-tree >output &&
	test "$(cat output)" = "false"
	)
'

###########################################################################
# Section 10: Multiple options at once
###########################################################################

test_expect_success '--git-dir and HEAD can be queried separately' '
	(
	cd repo &&
	git rev-parse --git-dir >gitdir_out &&
	git rev-parse HEAD >head_out &&
	test -s gitdir_out &&
	test -s head_out
	)
'

test_expect_success '--show-toplevel and --git-dir give consistent paths' '
	(
	cd repo/src &&
	git rev-parse --show-toplevel >toplevel &&
	git rev-parse --git-dir >gitdir &&
	test -s toplevel &&
	test -s gitdir
	)
'

test_expect_success '--is-inside-work-tree and --is-bare-repository are consistent' '
	(
	cd repo &&
	git rev-parse --is-inside-work-tree >wt &&
	git rev-parse --is-bare-repository >bare &&
	test "$(cat wt)" = "true" &&
	test "$(cat bare)" = "false"
	)
'

test_expect_success 'bare repo: --is-inside-work-tree and --is-bare-repository are consistent' '
	(
	cd bare.git &&
	git rev-parse --is-inside-work-tree >wt &&
	git rev-parse --is-bare-repository >bare &&
	test "$(cat wt)" = "false" &&
	test "$(cat bare)" = "true"
	)
'

test_done
