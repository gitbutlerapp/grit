#!/bin/sh
# Test grit branch --force/-f creation, overwrite behavior,
# delete, move/rename, copy, --contains, --merged,
# --show-current, verbose, quiet, and various edge cases.

test_description='grit branch --force create and management'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: repo with multiple commits' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first commit" &&
	echo "second" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "second commit" &&
	echo "third" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "third commit"
	)
'

# --- basic branch creation ---

test_expect_success 'branch creates new branch at HEAD' '
	(
	cd repo &&
	grit branch feature1 &&
	grit rev-parse feature1 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch creates branch at specific ref' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit branch feature2 "$parent" &&
	grit rev-parse feature2 >actual &&
	echo "$parent" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch list shows new branches' '
	(
	cd repo &&
	grit branch --list >actual &&
	grep "feature1" actual &&
	grep "feature2" actual
	)
'

test_expect_success 'branch creation without force fails for existing' '
	(
	cd repo &&
	test_must_fail grit branch feature1
	)
'

# --- branch --force / -f ---

test_expect_success 'branch --force overwrites existing branch' '
	(
	cd repo &&
	grit rev-parse feature2 >old_sha &&
	grit branch --force feature2 HEAD &&
	grit rev-parse feature2 >new_sha &&
	grit rev-parse HEAD >expect &&
	test_cmp expect new_sha &&
	! test_cmp old_sha new_sha
	)
'

test_expect_success 'branch -f short flag works' '
	(
	cd repo &&
	root=$(grit rev-parse HEAD~2) &&
	grit branch -f feature1 "$root" &&
	grit rev-parse feature1 >actual &&
	echo "$root" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'branch --force to same commit is idempotent' '
	(
	cd repo &&
	grit rev-parse feature1 >before &&
	sha=$(cat before) &&
	grit branch -f feature1 "$sha" &&
	grit rev-parse feature1 >after &&
	test_cmp before after
	)
'

test_expect_success 'branch --force does not affect current branch' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

# --- branch delete ---

test_expect_success 'branch -d deletes branch' '
	(
	cd repo &&
	grit branch temp-del &&
	grit branch -d temp-del &&
	grit branch --list >actual &&
	! grep "temp-del" actual
	)
'

test_expect_success 'branch -D force deletes branch' '
	(
	cd repo &&
	root=$(grit rev-parse HEAD~2) &&
	grit branch unmerged "$root" &&
	grit branch -D unmerged &&
	grit branch --list >actual &&
	! grep "unmerged" actual
	)
'

test_expect_success 'branch -d cannot delete current branch' '
	(
	cd repo &&
	test_must_fail grit branch -d main
	)
'

test_expect_success 'branch --delete works same as -d' '
	(
	cd repo &&
	grit branch del-long &&
	grit branch --delete del-long &&
	grit branch --list >actual &&
	! grep "del-long" actual
	)
'

test_expect_success 'branch -d multiple branches' '
	(
	cd repo &&
	grit branch multi1 &&
	grit branch multi2 &&
	grit branch -d multi1 &&
	grit branch -d multi2 &&
	grit branch --list >actual &&
	! grep "multi1" actual &&
	! grep "multi2" actual
	)
'

# --- branch move/rename ---

test_expect_success 'branch -m renames branch' '
	(
	cd repo &&
	grit branch rename-me &&
	grit branch -m rename-me renamed &&
	grit branch --list >actual &&
	grep "renamed" actual &&
	! grep "rename-me" actual
	)
'

test_expect_success 'branch --move renames branch' '
	(
	cd repo &&
	grit branch long-rename &&
	grit branch --move long-rename long-renamed &&
	grit branch --list >actual &&
	grep "long-renamed" actual
	)
'

test_expect_success 'branch -M force renames over existing' '
	(
	cd repo &&
	grit branch target-name &&
	grit branch source-name &&
	grit branch -M source-name target-name &&
	grit branch --list >actual &&
	! grep "source-name" actual
	)
'

test_expect_success 'branch rename preserves commit' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit branch pres "$parent" &&
	grit rev-parse pres >before &&
	grit branch -m pres pres-renamed &&
	grit rev-parse pres-renamed >after &&
	test_cmp before after
	)
'

# --- branch copy ---

test_expect_success 'branch -c copies branch' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit branch copy-src "$parent" &&
	grit branch -c copy-src copy-dst &&
	grit rev-parse copy-src >src_sha &&
	grit rev-parse copy-dst >dst_sha &&
	test_cmp src_sha dst_sha
	)
'

test_expect_success 'branch --copy copies branch' '
	(
	cd repo &&
	grit branch --copy copy-src copy-dst2 &&
	grit rev-parse copy-dst2 >actual &&
	grit rev-parse copy-src >expect &&
	test_cmp expect actual
	)
'

# --- branch --show-current ---

test_expect_success 'branch --show-current shows main' '
	(
	cd repo &&
	grit branch --show-current >actual &&
	echo "main" >expect &&
	test_cmp expect actual
	)
'

# --- branch --verbose ---

test_expect_success 'branch -v shows commit info' '
	(
	cd repo &&
	grit branch -v >actual &&
	grep "third commit" actual
	)
'

test_expect_success 'branch --verbose shows commit info' '
	(
	cd repo &&
	grit branch --verbose >actual &&
	grep "third commit" actual
	)
'

# --- branch --quiet ---

test_expect_success 'branch -q suppresses output on create' '
	(
	cd repo &&
	grit branch -q quiet-branch >actual 2>&1 &&
	test_line_count = 0 actual
	)
'

# --- branch --contains ---

test_expect_success 'branch --contains HEAD lists branches at HEAD' '
	(
	cd repo &&
	grit branch --contains HEAD >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch --contains specific commit' '
	(
	cd repo &&
	grit branch -f feature1 HEAD &&
	root=$(grit rev-parse HEAD~2) &&
	grit branch --contains "$root" >actual &&
	grep "main" actual &&
	grep "feature1" actual
	)
'

# --- branch --merged ---

test_expect_success 'branch --merged lists merged branches' '
	(
	cd repo &&
	grit branch --merged HEAD >actual &&
	grep "main" actual
	)
'

# --- branch at tag ---

test_expect_success 'branch at tag start point' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit tag v1.0 "$parent" &&
	grit branch from-tag v1.0 &&
	grit rev-parse from-tag >actual &&
	grit rev-parse v1.0 >expect &&
	test_cmp expect actual
	)
'

# --- branch --force with SHA ---

test_expect_success 'branch --force at specific SHA' '
	(
	cd repo &&
	sha=$(grit rev-parse HEAD~1) &&
	grit branch -f feature2 "$sha" &&
	grit rev-parse feature2 >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

# --- multiple branches ---

test_expect_success 'create many branches' '
	(
	cd repo &&
	grit branch b1 &&
	grit branch b2 &&
	grit branch b3 &&
	grit branch b4 &&
	grit branch b5 &&
	grit branch --list >actual &&
	grep "b1" actual &&
	grep "b5" actual
	)
'

test_expect_success 'branch -a shows all branches' '
	(
	cd repo &&
	grit branch -a >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch -l lists branches' '
	(
	cd repo &&
	grit branch -l >actual &&
	grep "main" actual
	)
'

test_expect_success 'branch with no args lists branches' '
	(
	cd repo &&
	grit branch >actual &&
	grep "main" actual
	)
'

test_done
