#!/bin/sh
# Tests for fetch/push edge cases:
#   1. fetch --depth N / --deepen N  (shallow fetch)
#   2. push --force-with-lease       (conditional force push)
#   3. push --atomic                 (atomic push)
#   4. push --push-option            (server push options)
#   5. fetch --refetch               (re-fetch all objects)
#   6. push --porcelain              (machine-readable push output)
#   7. fetch --output                (machine-readable fetch output)
#   8. Pre-push hook execution

test_description='fetch and push edge cases'

GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# Helper to create a fresh origin repo with 4 commits
make_origin () {
	git init -q "$1" &&
	(
		cd "$1" &&
		git config receive.denyCurrentBranch updateInstead &&
		echo "commit 1" >file &&
		git add file &&
		git commit -q -m "first" &&
		echo "commit 2" >file &&
		git add file &&
		git commit -q -m "second" &&
		echo "commit 3" >file &&
		git add file &&
		git commit -q -m "third" &&
		echo "commit 4" >file &&
		git add file &&
		git commit -q -m "fourth"
	)
}

# ---- 1. fetch --depth / --deepen ----
test_expect_success 'fetch --depth N writes shallow file' '
	make_origin depth-origin &&
	git init -q depth-test &&
	(
		cd depth-test &&
		git remote add origin ../depth-origin &&
		grit fetch --depth 2 origin &&
		test -f .git/shallow &&
		test -s .git/shallow
	)
'

test_expect_success 'fetch --deepen N writes shallow file' '
	make_origin deepen-origin &&
	git init -q deepen-test &&
	(
		cd deepen-test &&
		git remote add origin ../deepen-origin &&
		grit fetch --deepen 1 origin &&
		test -f .git/shallow &&
		test -s .git/shallow
	)
'

# ---- 2. push --force-with-lease ----
test_expect_success 'push --force-with-lease allows non-ff when ref unchanged' '
	make_origin fwl-origin &&
	git clone fwl-origin fwl-ok &&
	(
		cd fwl-ok &&
		git reset --hard HEAD~1 &&
		echo "diverged" >file &&
		git add file &&
		git commit -q -m "diverged" &&
		grit push --force-with-lease origin main
	)
'

test_expect_success 'push --force-with-lease rejects when ref changed underneath' '
	make_origin fwl2-origin &&
	git clone fwl2-origin fwl-alice &&
	git clone fwl2-origin fwl-bob &&
	# Alice pushes a new commit
	(
		cd fwl-alice &&
		echo "alice change" >afile &&
		git add afile &&
		git commit -q -m "alice" &&
		git push origin main
	) &&
	# Bob makes a change but does NOT fetch — his origin/main is stale
	(
		cd fwl-bob &&
		echo "bob change" >bfile &&
		git add bfile &&
		git commit -q -m "bob" &&
		test_must_fail grit push --force-with-lease origin main
	)
'

# ---- 3. push --atomic ----
test_expect_success 'push --atomic pushes multiple refs' '
	make_origin atomic-origin &&
	git clone atomic-origin atomic-test &&
	(
		cd atomic-test &&
		git branch feature &&
		grit push --atomic origin main feature
	) &&
	(cd atomic-origin && git rev-parse feature)
'

# ---- 4. push --push-option ----
test_expect_success 'push --push-option writes options to remote' '
	make_origin pushopt-origin &&
	git clone pushopt-origin pushopt-test &&
	(
		cd pushopt-test &&
		grit push --push-option "ci.skip" --push-option "topic=test" origin main
	) &&
	test -f pushopt-origin/.git/push_options &&
	grep "ci.skip" pushopt-origin/.git/push_options &&
	grep "topic=test" pushopt-origin/.git/push_options
'

# ---- 5. fetch --refetch ----
test_expect_success 'fetch --refetch re-fetches objects' '
	make_origin refetch-origin &&
	git clone refetch-origin refetch-test &&
	(
		cd refetch-test &&
		find .git/objects -type f | wc -l >../before_count &&
		obj=$(find .git/objects/[0-9a-f][0-9a-f] -type f | head -1) &&
		if test -n "$obj"; then
			rm "$obj"
		fi &&
		grit fetch --refetch origin &&
		find .git/objects -type f | wc -l >../after_count &&
		test "$(cat ../after_count)" -ge "$(cat ../before_count)"
	)
'

# ---- 6. push --porcelain ----
test_expect_success 'push --porcelain produces machine-readable output' '
	git init --bare porcelain-remote.git &&
	make_origin porcelain-origin &&
	git clone porcelain-origin porcelain-test &&
	(
		cd porcelain-test &&
		git remote add target ../porcelain-remote.git &&
		grit push --porcelain target main >../porcelain-output 2>&1
	) &&
	cat porcelain-output &&
	grep "	" porcelain-output &&
	grep "main" porcelain-output
'

# ---- 7. fetch --output ----
test_expect_success 'fetch --output writes machine-readable output' '
	make_origin output-origin &&
	git init -q output-test &&
	(
		cd output-test &&
		git remote add origin ../output-origin &&
		grit fetch --output ../fetch-output.txt origin &&
		test -f ../fetch-output.txt &&
		test -s ../fetch-output.txt &&
		grep "refs/remotes/origin/" ../fetch-output.txt
	)
'

# ---- 8. Pre-push hook execution ----
test_expect_success 'pre-push hook blocks push on exit 1' '
	git init --bare hook-remote.git &&
	make_origin hook-origin &&
	git clone hook-origin hook-test &&
	(
		cd hook-test &&
		git remote add target ../hook-remote.git &&
		mkdir -p .git/hooks &&
		cat >.git/hooks/pre-push <<-\HOOK &&
		#!/bin/sh
		exit 1
		HOOK
		chmod +x .git/hooks/pre-push &&
		test_must_fail grit push target main
	)
'

test_expect_success 'pre-push hook receives correct stdin' '
	git init --bare hook2-remote.git &&
	make_origin hook2-origin &&
	git clone hook2-origin hook2-test &&
	(
		cd hook2-test &&
		git remote add target ../hook2-remote.git &&
		mkdir -p .git/hooks &&
		cat >.git/hooks/pre-push <<-\HOOK &&
		#!/bin/sh
		cat >../hook-stdin.txt
		exit 0
		HOOK
		chmod +x .git/hooks/pre-push &&
		grit push target main &&
		test -f ../hook-stdin.txt &&
		grep "refs/heads/main" ../hook-stdin.txt
	)
'

test_expect_success 'pre-push hook allowing push succeeds' '
	git init --bare hook3-remote.git &&
	make_origin hook3-origin &&
	git clone hook3-origin hook3-test &&
	(
		cd hook3-test &&
		git remote add target ../hook3-remote.git &&
		mkdir -p .git/hooks &&
		cat >.git/hooks/pre-push <<-\HOOK &&
		#!/bin/sh
		exit 0
		HOOK
		chmod +x .git/hooks/pre-push &&
		grit push target main
	) &&
	git --git-dir=hook3-remote.git rev-parse main
'

test_done
