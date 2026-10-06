#!/bin/sh
# Tests for update-index with different modes, --cacheinfo, and flags.

test_description='update-index modes and --cacheinfo'

. ./test-lib.sh

GIT_AUTHOR_NAME='A U Thor'
GIT_AUTHOR_EMAIL='author@example.com'
GIT_COMMITTER_NAME='C O Mmiter'
GIT_COMMITTER_EMAIL='committer@example.com'
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

test_expect_success 'setup: init repo' '
	(
	git init repo &&
	cd repo
	)
'

test_expect_success 'update-index --add adds a file' '
	(
	cd repo &&
	echo "hello" >hello.txt &&
	git update-index --add hello.txt &&
	git ls-files >actual &&
	grep -q "hello.txt" actual
	)
'

test_expect_success 'update-index --add a second file' '
	(
	cd repo &&
	echo "world" >world.txt &&
	git update-index --add world.txt &&
	git ls-files >actual &&
	grep -q "world.txt" actual
	)
'

test_expect_success 'both files are in the index' '
	(
	cd repo &&
	git ls-files >actual &&
	test_line_count = 2 actual
	)
'

test_expect_success 'update-index --remove removes a file from index' '
	(
	cd repo &&
	git update-index --remove world.txt &&
	git ls-files >actual &&
	! grep -q "world.txt" actual
	)
'

test_expect_success 'update-index --force-remove removes even if file exists' '
	(
	cd repo &&
	echo "temp" >temp.txt &&
	git update-index --add temp.txt &&
	git ls-files >before &&
	grep -q "temp.txt" before &&
	git update-index --force-remove temp.txt &&
	git ls-files >after &&
	! grep -q "temp.txt" after &&
	# File should still exist on disk
	test -f temp.txt
	)
'

test_expect_success '--cacheinfo adds entry directly' '
	(
	cd repo &&
	# Hash an object first
	blob=$(echo "cached content" | git hash-object -w --stdin) &&
	git update-index --cacheinfo "100644,$blob,cached-file.txt" &&
	git ls-files >actual &&
	grep -q "cached-file.txt" actual
	)
'

test_expect_success '--cacheinfo entry has correct blob' '
	(
	cd repo &&
	git ls-files --stage cached-file.txt >stage_out &&
	blob=$(echo "cached content" | git hash-object --stdin) &&
	grep -q "$blob" stage_out
	)
'

test_expect_success '--cacheinfo with 100755 mode for executable' '
	(
	cd repo &&
	blob=$(echo "exec content" | git hash-object -w --stdin) &&
	git update-index --cacheinfo "100755,$blob,exec-file.sh" &&
	git ls-files --stage exec-file.sh >stage_out &&
	grep -q "100755" stage_out
	)
'

test_expect_success '--cacheinfo with 120000 mode for symlink' '
	(
	cd repo &&
	blob=$(echo "target" | git hash-object -w --stdin) &&
	git update-index --cacheinfo "120000,$blob,link-file" &&
	git ls-files --stage link-file >stage_out &&
	grep -q "120000" stage_out
	)
'

test_expect_success 'ls-files --stage shows all entries with modes' '
	(
	cd repo &&
	git ls-files --stage >actual &&
	# Should have hello.txt, cached-file.txt, exec-file.sh, link-file
	test_line_count = 4 actual
	)
'

test_expect_success 'write-tree after --cacheinfo produces valid tree' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git cat-file -t "$tree" >type &&
	echo "tree" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tree contains cacheinfo entries' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >ls_out &&
	grep -q "cached-file.txt" ls_out &&
	grep -q "exec-file.sh" ls_out &&
	grep -q "link-file" ls_out
	)
'

test_expect_success '--assume-unchanged does not error' '
	(
	cd repo &&
	git update-index --assume-unchanged hello.txt
	)
'

test_expect_success '--no-assume-unchanged does not error' '
	(
	cd repo &&
	git update-index --no-assume-unchanged hello.txt
	)
'

test_expect_success '--skip-worktree does not error' '
	(
	cd repo &&
	git update-index --skip-worktree hello.txt
	)
'

test_expect_success '--no-skip-worktree does not error' '
	(
	cd repo &&
	git update-index --no-skip-worktree hello.txt
	)
'

test_expect_success '--refresh reports missing cacheinfo worktree files' '
	(
	cd repo &&
	test_must_fail git update-index --refresh
	)
'

test_expect_success 'update-index --add replaces existing entry' '
	(
	cd repo &&
	echo "updated hello" >hello.txt &&
	git update-index --add hello.txt &&
	blob=$(git ls-files --stage hello.txt | awk "{print \$2}") &&
	content=$(git cat-file -p "$blob") &&
	test "$content" = "updated hello"
	)
'

test_expect_success '--cacheinfo overwrites existing entry' '
	(
	cd repo &&
	blob=$(echo "new cached" | git hash-object -w --stdin) &&
	git update-index --cacheinfo "100644,$blob,cached-file.txt" &&
	actual_blob=$(git ls-files --stage cached-file.txt | awk "{print \$2}") &&
	test "$blob" = "$actual_blob"
	)
'

test_expect_success 'setup: fresh repo for --info-only tests' '
	(
	git init info-repo &&
	cd info-repo
	)
'

test_expect_success '--info-only records without checking worktree' '
	(
	cd info-repo &&
	blob=$(echo "phantom" | git hash-object -w --stdin) &&
	git update-index --add --info-only --cacheinfo "100644,$blob,phantom.txt" &&
	git ls-files >actual &&
	grep -q "phantom.txt" actual &&
	# File should not exist on disk
	test ! -f phantom.txt
	)
'

test_expect_success 'multiple --cacheinfo entries in sequence' '
	(
	cd info-repo &&
	b1=$(echo "one" | git hash-object -w --stdin) &&
	b2=$(echo "two" | git hash-object -w --stdin) &&
	b3=$(echo "three" | git hash-object -w --stdin) &&
	git update-index --cacheinfo "100644,$b1,one.txt" &&
	git update-index --cacheinfo "100644,$b2,two.txt" &&
	git update-index --cacheinfo "100644,$b3,three.txt" &&
	git ls-files >actual &&
	grep -q "one.txt" actual &&
	grep -q "two.txt" actual &&
	grep -q "three.txt" actual
	)
'

test_expect_success 'write-tree after multiple cacheinfo' '
	(
	cd info-repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >ls_out &&
	test_line_count = 4 ls_out
	)
'

test_expect_success '--remove non-existent file is not an error with --ignore-missing' '
	(
	cd info-repo &&
	git update-index --remove --ignore-missing nonexistent.txt
	)
'

test_expect_success 'setup: repo for index-info tests' '
	(
	git init index-info-repo &&
	cd index-info-repo
	)
'

test_expect_success '--index-info reads from stdin' '
	(
	cd index-info-repo &&
	blob=$(echo "stdin content" | git hash-object -w --stdin) &&
	printf "%s %s\t%s\n" "100644" "$blob" "stdin-file.txt" |
	git update-index --index-info &&
	git ls-files >actual &&
	grep -q "stdin-file.txt" actual
	)
'

test_expect_success '--index-info with multiple entries' '
	(
	cd index-info-repo &&
	b1=$(echo "alpha" | git hash-object -w --stdin) &&
	b2=$(echo "beta" | git hash-object -w --stdin) &&
	printf "%s %s\t%s\n%s %s\t%s\n" \
		"100644" "$b1" "alpha.txt" \
		"100644" "$b2" "beta.txt" |
	git update-index --index-info &&
	git ls-files >actual &&
	grep -q "alpha.txt" actual &&
	grep -q "beta.txt" actual
	)
'

test_expect_success 'tree from --index-info entries is valid' '
	(
	cd index-info-repo &&
	tree=$(git write-tree) &&
	git ls-tree "$tree" >ls_out &&
	test_line_count = 3 ls_out
	)
'

test_expect_success 'update-index --add with subdirectory' '
	(
	cd repo &&
	mkdir -p sub/dir &&
	echo "nested" >sub/dir/nested.txt &&
	git update-index --add sub/dir/nested.txt &&
	git ls-files >actual &&
	grep -q "sub/dir/nested.txt" actual
	)
'

test_expect_success 'write-tree includes subdirectory entries' '
	(
	cd repo &&
	tree=$(git write-tree) &&
	git ls-tree -r "$tree" >ls_out &&
	grep -q "sub/dir/nested.txt" ls_out
	)
'

test_done
