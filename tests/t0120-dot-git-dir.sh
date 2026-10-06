#!/bin/sh
# Tests for .git directory structure: HEAD, config, refs/, objects/

test_description='.git directory structure'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------
test_expect_success 'setup: init a repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com"
	)
'

# ---------------------------------------------------------------------------
# .git directory basics
# ---------------------------------------------------------------------------
test_expect_success '.git is a directory' '
	test_path_is_dir repo/.git
'

test_expect_success '.git/HEAD exists and is a file' '
	test_path_is_file repo/.git/HEAD
'

test_expect_success '.git/HEAD is a symref to refs/heads/master or main' '
	head_content=$(cat repo/.git/HEAD) &&
	case "$head_content" in
	"ref: refs/heads/"*) : ok ;;
	*) echo "unexpected HEAD: $head_content"; false ;;
	esac
'

test_expect_success '.git/config exists' '
	test_path_is_file repo/.git/config
'

test_expect_success '.git/config contains core section' '
	grep -q "\\[core\\]" repo/.git/config
'

test_expect_success '.git/refs directory exists' '
	test_path_is_dir repo/.git/refs
'

test_expect_success '.git/refs/heads directory exists' '
	test_path_is_dir repo/.git/refs/heads
'

test_expect_success '.git/refs/tags directory exists' '
	test_path_is_dir repo/.git/refs/tags
'

test_expect_success '.git/objects directory exists' '
	test_path_is_dir repo/.git/objects
'

# ---------------------------------------------------------------------------
# After a commit, refs and objects populate
# ---------------------------------------------------------------------------
test_expect_success 'create a commit' '
	(
	cd repo &&
	echo "hello" >file.txt &&
	git add file.txt &&
	git commit -m "first commit"
	)
'

test_expect_success 'HEAD resolves to a valid commit after commit' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	test $(echo "$head_sha" | wc -c) -ge 40
	)
'

test_expect_success 'objects directory has loose objects or packs' '
	(
	cd repo &&
	obj_count=$(find .git/objects -type f | grep -v info | grep -v pack | wc -l) &&
	pack_count=$(find .git/objects/pack -name "*.pack" 2>/dev/null | wc -l) &&
	total=$(($obj_count + $pack_count)) &&
	test "$total" -gt 0
	)
'

test_expect_success 'branch ref file exists after commit' '
	(
	cd repo &&
	branch=$(git symbolic-ref HEAD | sed "s|refs/heads/||") &&
	test_path_is_file ".git/refs/heads/$branch"
	)
'

test_expect_success 'branch ref contains the HEAD sha' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	branch=$(git symbolic-ref HEAD | sed "s|refs/heads/||") &&
	ref_sha=$(cat ".git/refs/heads/$branch") &&
	test "$head_sha" = "$ref_sha"
	)
'

# ---------------------------------------------------------------------------
# Bare repo structure
# ---------------------------------------------------------------------------
test_expect_success 'bare repo: init --bare' '
	git init --bare bare.git
'

test_expect_success 'bare repo: HEAD exists' '
	test_path_is_file bare.git/HEAD
'

test_expect_success 'bare repo: config exists' '
	test_path_is_file bare.git/config
'

test_expect_success 'bare repo: bare = true in config' '
	grep -q "bare = true" bare.git/config
'

test_expect_success 'bare repo: refs/heads exists' '
	test_path_is_dir bare.git/refs/heads
'

test_expect_success 'bare repo: refs/tags exists' '
	test_path_is_dir bare.git/refs/tags
'

test_expect_success 'bare repo: objects exists' '
	test_path_is_dir bare.git/objects
'

# ---------------------------------------------------------------------------
# HEAD updates
# ---------------------------------------------------------------------------
test_expect_success 'HEAD updates when switching branches' '
	(
	cd repo &&
	git checkout -b feature &&
	head_content=$(cat .git/HEAD) &&
	echo "$head_content" | grep -q "refs/heads/feature"
	)
'

test_expect_success 'detached HEAD contains a raw sha' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	git checkout "$head_sha" 2>/dev/null &&
	head_content=$(cat .git/HEAD) &&
	test "$head_content" = "$head_sha"
	)
'

test_expect_success 'reattach HEAD to branch' '
	(
	cd repo &&
	git checkout feature &&
	head_content=$(cat .git/HEAD) &&
	echo "$head_content" | grep -q "refs/heads/feature"
	)
'

# ---------------------------------------------------------------------------
# Multiple commits - objects grow
# ---------------------------------------------------------------------------
test_expect_success 'second commit creates more objects' '
	(
	cd repo &&
	before=$(find .git/objects -type f | grep -v info | grep -v pack | wc -l) &&
	echo "world" >file2.txt &&
	git add file2.txt &&
	git commit -m "second commit" &&
	after=$(find .git/objects -type f | grep -v info | grep -v pack | wc -l) &&
	test "$after" -gt "$before"
	)
'

test_expect_success 'cat-file on HEAD commit works' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	git cat-file -t "$head_sha" >type_out &&
	echo "commit" >expected &&
	test_cmp expected type_out
	)
'

test_expect_success 'cat-file on tree works' '
	(
	cd repo &&
	tree_sha=$(git rev-parse HEAD^{tree}) &&
	git cat-file -t "$tree_sha" >type_out &&
	echo "tree" >expected &&
	test_cmp expected type_out
	)
'

# ---------------------------------------------------------------------------
# Tag creates ref in refs/tags
# ---------------------------------------------------------------------------
test_expect_success 'lightweight tag creates ref in refs/tags' '
	(
	cd repo &&
	git tag v1.0 &&
	test_path_is_file .git/refs/tags/v1.0
	)
'

test_expect_success 'tag ref points to HEAD' '
	(
	cd repo &&
	head_sha=$(git rev-parse HEAD) &&
	tag_sha=$(cat .git/refs/tags/v1.0) &&
	test "$head_sha" = "$tag_sha"
	)
'

test_expect_success 'config user.name is readable' '
	(
	cd repo &&
	git config user.name >out &&
	echo "Test User" >expected &&
	test_cmp expected out
	)
'

test_expect_success 'config user.email is readable' '
	(
	cd repo &&
	git config user.email >out &&
	echo "test@example.com" >expected &&
	test_cmp expected out
	)
'

test_done
