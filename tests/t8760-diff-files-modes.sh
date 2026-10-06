#!/bin/sh
# Tests for diff-files comparing working tree against the index.

test_description='diff-files modes and worktree comparisons'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

GIT_COMMITTER_EMAIL=test@test.com
GIT_COMMITTER_NAME='Test User'
GIT_AUTHOR_NAME='Test Author'
GIT_AUTHOR_EMAIL=author@test.com
export GIT_COMMITTER_EMAIL GIT_COMMITTER_NAME GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL

# -- setup ---------------------------------------------------------------------

test_expect_success 'setup repository with several files' '
	(
	git init repo &&
	cd repo &&
	echo "alpha" >a.txt &&
	echo "beta" >b.txt &&
	echo "gamma" >c.txt &&
	mkdir dir &&
	echo "delta" >dir/d.txt &&
	echo "epsilon" >dir/e.txt &&
	git add . &&
	git commit -m "initial"
	)
'

# -- clean worktree: no output -------------------------------------------------

test_expect_success 'diff-files on clean worktree shows nothing' '
	(
	cd repo &&
	git diff-files >out &&
	test_line_count = 0 out
	)
'

test_expect_success 'diff-files --exit-code on clean worktree returns 0' '
	(
	cd repo &&
	git diff-files --exit-code
	)
'

# -- single file modification --------------------------------------------------

test_expect_success 'diff-files detects single modified file' '
	(
	cd repo &&
	echo "alpha modified" >a.txt &&
	git diff-files >out &&
	grep "a.txt" out &&
	! grep "b.txt" out
	)
'

test_expect_success 'diff-files shows M status for modified file' '
	(
	cd repo &&
	git diff-files >out &&
	grep "M" out | grep "a.txt"
	)
'

test_expect_success 'diff-files raw output starts with colon' '
	(
	cd repo &&
	git diff-files >out &&
	grep "^:" out
	)
'

# -- multiple file modifications -----------------------------------------------

test_expect_success 'diff-files detects multiple modified files' '
	(
	cd repo &&
	echo "beta modified" >b.txt &&
	echo "gamma modified" >c.txt &&
	git diff-files >out &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out
	)
'

test_expect_success 'diff-files does not show unmodified nested file' '
	(
	cd repo &&
	git diff-files >out &&
	! grep "dir/d.txt" out &&
	! grep "dir/e.txt" out
	)
'

# -- nested directory modifications --------------------------------------------

test_expect_success 'diff-files detects nested file modifications' '
	(
	cd repo &&
	echo "delta modified" >dir/d.txt &&
	git diff-files >out &&
	grep "dir/d.txt" out
	)
'

test_expect_success 'diff-files shows all modified files including nested' '
	(
	cd repo &&
	git diff-files >out &&
	grep "a.txt" out &&
	grep "b.txt" out &&
	grep "c.txt" out &&
	grep "dir/d.txt" out
	)
'

# -- path filter ---------------------------------------------------------------

test_expect_success 'diff-files with path filter shows only matching files' '
	(
	cd repo &&
	git diff-files -- a.txt >out &&
	grep "a.txt" out &&
	! grep "b.txt" out &&
	! grep "c.txt" out
	)
'

test_expect_success 'diff-files with directory path filter' '
	(
	cd repo &&
	git diff-files -- dir/ >out &&
	grep "dir/d.txt" out &&
	! grep "a.txt" out
	)
'

test_expect_success 'diff-files with multiple path filters' '
	(
	cd repo &&
	git diff-files -- a.txt c.txt >out &&
	grep "a.txt" out &&
	grep "c.txt" out &&
	! grep "b.txt" out
	)
'

# -- stage changes and verify diff-files updates -------------------------------

test_expect_success 'diff-files stops showing file after git add' '
	(
	cd repo &&
	git add a.txt &&
	git diff-files >../stage-out &&
	! grep "a.txt" ../stage-out &&
	grep "b.txt" ../stage-out
	)
'

test_expect_success 'diff-files shows nothing after staging all changes' '
	(
	cd repo &&
	git add . &&
	git diff-files >../stage-all-out &&
	test_line_count = 0 ../stage-all-out
	)
'

test_expect_success 'cleanup: commit all changes' '
	(
	cd repo &&
	git commit -m "modifications round 1"
	)
'

# -- deleted files (worktree delete, not git rm) --------------------------------

test_expect_success 'diff-files detects deleted worktree file' '
	(
	cd repo &&
	rm a.txt &&
	git diff-files >out &&
	grep "a.txt" out
	)
'

test_expect_success 'diff-files shows D status for deleted file' '
	(
	cd repo &&
	git diff-files >out &&
	grep "D" out | grep "a.txt"
	)
'

test_expect_success 'diff-files still shows only deleted file' '
	(
	cd repo &&
	git diff-files >out &&
	! grep "b.txt" out
	)
'

test_expect_success 'restore deleted file clears diff-files output' '
	(
	cd repo &&
	git reset --hard HEAD &&
	git diff-files >../restore-out &&
	test_line_count = 0 ../restore-out
	)
'

# -- exit-code flag ------------------------------------------------------------

test_expect_success 'diff-files --exit-code returns 1 on changes' '
	(
	cd repo &&
	echo "exit test" >a.txt &&
	test_must_fail git diff-files --exit-code
	)
'

test_expect_success 'diff-files --exit-code returns 0 after staging' '
	(
	cd repo &&
	git add . &&
	git diff-files --exit-code
	)
'

test_expect_success 'cleanup: commit exit-code test' '
	(
	cd repo &&
	git commit -m "exit code test"
	)
'

# -- mode information in raw output --------------------------------------------

test_expect_success 'diff-files raw output contains mode 100644' '
	(
	cd repo &&
	echo "mode test" >b.txt &&
	git diff-files >out &&
	grep "100644" out
	)
'

test_expect_success 'diff-files raw output contains null OID for worktree side' '
	(
	cd repo &&
	git diff-files >out &&
	grep "0000000000000000000000000000000000000000" out
	)
'

test_expect_success 'cleanup mode test' '
	(
	cd repo &&
	git add b.txt &&
	git commit -m "mode test"
	)
'

# -- new repo: file with executable bit ----------------------------------------

test_expect_success 'setup executable file repo' '
	(
	git init exec-repo &&
	cd exec-repo &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	echo "normal" >normal.txt &&
	git add . &&
	git commit -m "initial with executable"
	)
'

test_expect_success 'diff-files detects content change in executable file' '
	(
	cd exec-repo &&
	echo "#!/bin/sh\necho hello" >script.sh &&
	git diff-files >out &&
	grep "script.sh" out
	)
'

test_expect_success 'diff-files shows 100755 mode for executable files' '
	(
	cd exec-repo &&
	git diff-files >out &&
	grep "100755" out | grep "script.sh"
	)
'

test_expect_success 'diff-files shows 100644 mode for normal files' '
	(
	cd exec-repo &&
	echo "changed" >normal.txt &&
	git diff-files -- normal.txt >out &&
	grep "100644" out | grep "normal.txt"
	)
'

test_expect_success 'cleanup exec-repo' '
	(
	cd exec-repo &&
	git add . &&
	git commit -m "modified"
	)
'

# -- quiet flag ----------------------------------------------------------------

test_expect_success 'diff-files --quiet on clean worktree exits 0' '
	(
	cd exec-repo &&
	git diff-files --quiet
	)
'

test_expect_success 'diff-files --quiet with changes exits non-zero' '
	(
	cd exec-repo &&
	echo "quiet test" >normal.txt &&
	test_must_fail git diff-files --quiet
	)
'

test_done
