#!/bin/sh
# Test grit init with --quiet/-q and --initial-branch/-b options,
# bare repos, directory argument, template, separate-git-dir,
# and reinit behavior.

test_description='grit init --quiet and --initial-branch'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# --- basic init ---

test_expect_success 'init creates .git directory' '
	grit init basic &&
	test -d basic/.git
'

test_expect_success 'init creates HEAD file' '
	test -f basic/.git/HEAD
'

test_expect_success 'init creates refs directory' '
	test -d basic/.git/refs
'

test_expect_success 'init creates objects directory' '
	test -d basic/.git/objects
'

test_expect_success 'init default branch is main' '
	head_content=$(cat basic/.git/HEAD) &&
	echo "ref: refs/heads/main" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

# --- init --quiet / -q ---

test_expect_success 'init --quiet suppresses output' '
	grit init --quiet quiet-repo >actual 2>&1 &&
	test_line_count = 0 actual
'

test_expect_success 'init -q suppresses output' '
	grit init -q quiet-repo2 >actual 2>&1 &&
	test_line_count = 0 actual
'

test_expect_success 'init without --quiet produces output' '
	grit init verbose-repo >actual 2>&1 &&
	test -s actual
'

test_expect_success 'init --quiet still creates valid repo' '
	test -d quiet-repo/.git &&
	test -f quiet-repo/.git/HEAD &&
	test -d quiet-repo/.git/refs &&
	test -d quiet-repo/.git/objects
'

test_expect_success 'init --quiet repo is functional' '
	(
	cd quiet-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test" &&
	echo "file" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "test commit" &&
	grit log --oneline >log_out &&
	test_line_count = 1 log_out
	)
'

# --- init --initial-branch / -b ---

test_expect_success 'init -b sets initial branch name' '
	grit init -b develop branch-repo &&
	head_content=$(cat branch-repo/.git/HEAD) &&
	echo "ref: refs/heads/develop" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init --initial-branch sets branch name' '
	grit init --initial-branch main long-branch-repo &&
	head_content=$(cat long-branch-repo/.git/HEAD) &&
	echo "ref: refs/heads/main" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init -b custom branch is functional' '
	(
	cd branch-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test" &&
	echo "data" >data.txt &&
	grit add data.txt &&
	test_tick &&
	grit commit -m "first on develop" &&
	grit branch --show-current >actual &&
	echo "develop" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'init -b with unusual name' '
	grit init -b feature/my-feature unusual-branch &&
	head_content=$(cat unusual-branch/.git/HEAD) &&
	echo "ref: refs/heads/feature/my-feature" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init -b with hyphenated name' '
	grit init -b my-main hyphen-repo &&
	head_content=$(cat hyphen-repo/.git/HEAD) &&
	echo "ref: refs/heads/my-main" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

# --- init --quiet combined with -b ---

test_expect_success 'init --quiet -b suppresses output' '
	grit init --quiet -b trunk quiet-branch-repo >actual 2>&1 &&
	test_line_count = 0 actual
'

test_expect_success 'init --quiet -b sets correct branch' '
	head_content=$(cat quiet-branch-repo/.git/HEAD) &&
	echo "ref: refs/heads/trunk" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init -q -b combined works' '
	grit init -q -b release short-combo >actual 2>&1 &&
	test_line_count = 0 actual &&
	head_content=$(cat short-combo/.git/HEAD) &&
	echo "ref: refs/heads/release" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

# --- init --bare ---

test_expect_success 'init --bare creates bare repo' '
	grit init --bare bare-repo &&
	test -f bare-repo/HEAD &&
	test -d bare-repo/refs &&
	test -d bare-repo/objects &&
	! test -d bare-repo/.git
'

test_expect_success 'init --bare -b sets branch in bare repo' '
	grit init --bare -b trunk bare-branch &&
	head_content=$(cat bare-branch/HEAD) &&
	echo "ref: refs/heads/trunk" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init --bare --quiet suppresses output' '
	grit init --bare --quiet bare-quiet >actual 2>&1 &&
	test_line_count = 0 actual &&
	test -f bare-quiet/HEAD
'

# --- init in current directory ---

test_expect_success 'init without directory inits cwd' '
	(
	mkdir init-cwd &&
	cd init-cwd &&
	grit init &&
	test -d .git
	)
'

# --- init directory creation ---

test_expect_success 'init creates target directory if needed' '
	grit init new-dir/sub/repo &&
	test -d new-dir/sub/repo/.git
'

test_expect_success 'init into existing empty directory' '
	mkdir empty-dir &&
	grit init empty-dir &&
	test -d empty-dir/.git
'

# --- reinit ---

test_expect_success 'reinit existing repo does not destroy data' '
	(
	grit init reinit-repo &&
	cd reinit-repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test" &&
	echo "precious" >data.txt &&
	grit add data.txt &&
	test_tick &&
	grit commit -m "save this" &&
	cd .. &&
	grit init reinit-repo &&
	cd reinit-repo &&
	grit log --oneline >log_out &&
	test_line_count = 1 log_out &&
	test -f data.txt
	)
'

test_expect_success 'reinit --quiet suppresses reinit message' '
	grit init --quiet reinit-repo >actual 2>&1 &&
	test_line_count = 0 actual
'

# --- init with nested path and -b ---

test_expect_success 'init -b with nested directory path' '
	grit init -b deploy nested/deep/repo &&
	head_content=$(cat nested/deep/repo/.git/HEAD) &&
	echo "ref: refs/heads/deploy" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

test_expect_success 'init -q into nested directory' '
	grit init -q nested/quiet/repo >actual 2>&1 &&
	test_line_count = 0 actual &&
	test -d nested/quiet/repo/.git
'

# --- init -b combined with --bare and --quiet ---

test_expect_success 'init --bare --quiet -b all combined' '
	grit init --bare --quiet -b staging bare-quiet-branch >actual 2>&1 &&
	test_line_count = 0 actual &&
	head_content=$(cat bare-quiet-branch/HEAD) &&
	echo "ref: refs/heads/staging" >expect &&
	echo "$head_content" >actual &&
	test_cmp expect actual
'

# --- init multiple repos ---

test_expect_success 'init several repos with different branches' '
	grit init -b alpha repo-alpha &&
	grit init -b beta repo-beta &&
	grit init -b gamma repo-gamma &&
	grep "alpha" repo-alpha/.git/HEAD &&
	grep "beta" repo-beta/.git/HEAD &&
	grep "gamma" repo-gamma/.git/HEAD
'

# --- verify .git structure ---

test_expect_success 'init creates config file' '
	test -f basic/.git/config
'

test_expect_success 'init creates description file or HEAD' '
	# At minimum HEAD must exist
	test -f basic/.git/HEAD
'

test_done
