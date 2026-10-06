#!/bin/sh
# Tests for restore --staged with various file states.

test_description='restore --staged — unstaging and index manipulation'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup repository' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo alpha >a.txt &&
	echo beta >b.txt &&
	echo gamma >c.txt &&
	mkdir -p sub &&
	echo delta >sub/d.txt &&
	git add . &&
	git commit -m "initial" &&
	echo second >second.txt &&
	git add second.txt &&
	git commit -m "second commit" &&
	git tag v1
	)
'

# ── Basic unstaging ─────────────────────────────────────────────────────────

test_expect_success 'restore --staged unstages a modified file' '
	(
	cd repo &&
	echo modified >a.txt &&
	git add a.txt &&
	git restore --staged a.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged
	)
'

test_expect_success 'restore --staged keeps worktree modification' '
	(
	cd repo &&
	echo modified-keep >a.txt &&
	git add a.txt &&
	git restore --staged a.txt &&
	test "$(cat a.txt)" = "modified-keep" &&
	git checkout -- a.txt
	)
'

test_expect_success 'restore --staged unstages a newly added file' '
	(
	cd repo &&
	echo new >new.txt &&
	git add new.txt &&
	git restore --staged new.txt &&
	git diff --cached --name-only >staged &&
	! grep "new.txt" staged &&
	rm -f new.txt
	)
'

test_expect_success 'restore --staged on deleted file restores index entry' '
	(
	cd repo &&
	git rm b.txt &&
	git restore --staged b.txt &&
	git ls-files --stage b.txt >ls_out &&
	grep "b.txt" ls_out &&
	git checkout -- b.txt
	)
'

test_expect_success 'restore --staged with dot unstages everything' '
	(
	cd repo &&
	echo mod-a >a.txt &&
	echo mod-c >c.txt &&
	git add a.txt c.txt &&
	git restore --staged . &&
	git diff --cached --name-only >staged &&
	test_must_be_empty staged &&
	git checkout -- a.txt c.txt
	)
'

# ── Multiple files ──────────────────────────────────────────────────────────

test_expect_success 'restore --staged with multiple pathspecs' '
	(
	cd repo &&
	echo x >a.txt &&
	echo y >b.txt &&
	echo z >c.txt &&
	git add a.txt b.txt c.txt &&
	git restore --staged a.txt c.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	! grep "c.txt" staged &&
	grep "b.txt" staged &&
	git restore --staged . &&
	git checkout -- a.txt b.txt c.txt
	)
'

test_expect_success 'restore --staged on file in subdirectory' '
	(
	cd repo &&
	echo modified-sub >sub/d.txt &&
	git add sub/d.txt &&
	git restore --staged sub/d.txt &&
	git diff --cached --name-only >staged &&
	! grep "sub/d.txt" staged &&
	git checkout -- sub/d.txt
	)
'

# ── --source with --staged ──────────────────────────────────────────────────

test_expect_success 'restore --staged --source=HEAD is default (no-op on clean)' '
	(
	cd repo &&
	echo mod >a.txt &&
	git add a.txt &&
	git restore --staged --source=HEAD a.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	git checkout -- a.txt
	)
'

test_expect_success 'restore --staged --source=SHA restores older version to index' '
	(
	cd repo &&
	PARENT=$(git rev-parse HEAD~1) &&
	INITIAL_BLOB=$(git rev-parse "$PARENT:a.txt") &&
	git restore --staged --source="$PARENT" a.txt &&
	INDEX_BLOB=$(git ls-files -s a.txt | awk "{print \$2}") &&
	test "$INDEX_BLOB" = "$INITIAL_BLOB" &&
	git restore --staged a.txt
	)
'

test_expect_success 'restore --staged --source=tag restores from tag' '
	(
	cd repo &&
	git restore --staged --source=v1 a.txt &&
	INDEX_BLOB=$(git ls-files -s a.txt | awk "{print \$2}") &&
	TAG_BLOB=$(git rev-parse v1:a.txt) &&
	test "$INDEX_BLOB" = "$TAG_BLOB" &&
	git restore --staged a.txt
	)
'

test_expect_success 'restore --staged --source does not change worktree' '
	(
	cd repo &&
	PARENT=$(git rev-parse HEAD~1) &&
	cp a.txt a.txt.bak &&
	git restore --staged --source="$PARENT" a.txt &&
	test_cmp a.txt.bak a.txt &&
	git restore --staged a.txt &&
	rm a.txt.bak
	)
'

# ── --staged --worktree combined ────────────────────────────────────────────

test_expect_success 'restore --staged --worktree restores both' '
	(
	cd repo &&
	echo dirty >a.txt &&
	git add a.txt &&
	git restore --staged --worktree a.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	HEAD_BLOB=$(git rev-parse HEAD:a.txt) &&
	WORKTREE_BLOB=$(git hash-object a.txt) &&
	test "$WORKTREE_BLOB" = "$HEAD_BLOB"
	)
'

test_expect_success 'restore --staged --worktree --source restores from ref' '
	(
	cd repo &&
	PARENT=$(git rev-parse HEAD~1) &&
	INITIAL_BLOB=$(git rev-parse "$PARENT:a.txt") &&
	echo dirty >a.txt &&
	git add a.txt &&
	git restore --staged --worktree --source="$PARENT" a.txt &&
	INDEX_BLOB=$(git ls-files -s a.txt | awk "{print \$2}") &&
	test "$INDEX_BLOB" = "$INITIAL_BLOB" &&
	WORKTREE_BLOB=$(git hash-object a.txt) &&
	test "$WORKTREE_BLOB" = "$INITIAL_BLOB" &&
	git restore --staged --worktree a.txt
	)
'

# ── Edge cases ──────────────────────────────────────────────────────────────

test_expect_success 'restore --staged on clean file is a no-op' '
	(
	cd repo &&
	git checkout -- a.txt &&
	BEFORE=$(git ls-files -s a.txt | awk "{print \$2}") &&
	git restore --staged a.txt &&
	AFTER=$(git ls-files -s a.txt | awk "{print \$2}") &&
	test "$BEFORE" = "$AFTER"
	)
'

test_expect_success 'restore --staged without pathspec fails' '
	(
	cd repo &&
	test_must_fail git restore --staged 2>stderr
	)
'

test_expect_success 'restore --staged on file not in HEAD (newly added) removes from index' '
	(
	cd repo &&
	echo brand-new >brand-new.txt &&
	git add brand-new.txt &&
	git restore --staged brand-new.txt &&
	git ls-files --stage brand-new.txt >ls_out &&
	test_must_be_empty ls_out &&
	rm -f brand-new.txt
	)
'

test_expect_success 'restore --staged after rename unstages rename' '
	(
	cd repo &&
	git mv a.txt a-renamed.txt &&
	git restore --staged a-renamed.txt a.txt &&
	git ls-files --stage a.txt >ls_out &&
	grep "a.txt" ls_out &&
	git ls-files --stage a-renamed.txt >ls_out2 &&
	test_must_be_empty ls_out2 &&
	git checkout -- a.txt &&
	rm -f a-renamed.txt
	)
'

test_expect_success 'restore --staged after staging executable file' '
	(
	cd repo &&
	chmod +x a.txt &&
	git add a.txt &&
	git restore --staged a.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	chmod -x a.txt
	)
'

test_expect_success 'restore --staged with explicit file list unstages all listed' '
	(
	cd repo &&
	echo mod-a >a.txt &&
	echo mod-b >b.txt &&
	echo mod-c >c.txt &&
	git add a.txt b.txt c.txt &&
	git restore --staged a.txt b.txt c.txt &&
	git diff --cached --name-only >staged &&
	test_must_be_empty staged &&
	git checkout -- a.txt b.txt c.txt
	)
'

test_expect_success 'restore --staged with explicit subdirectory file' '
	(
	cd repo &&
	echo modified-sub >sub/d.txt &&
	git add sub/d.txt &&
	git restore --staged sub/d.txt &&
	git diff --cached --name-only >staged &&
	! grep "sub/d.txt" staged &&
	git checkout -- sub/d.txt
	)
'

test_expect_success 'restore --staged twice is idempotent' '
	(
	cd repo &&
	echo mod >a.txt &&
	git add a.txt &&
	git restore --staged a.txt &&
	FIRST=$(git ls-files -s a.txt | awk "{print \$2}") &&
	git restore --staged a.txt &&
	SECOND=$(git ls-files -s a.txt | awk "{print \$2}") &&
	test "$FIRST" = "$SECOND" &&
	git checkout -- a.txt
	)
'

test_expect_success 'restore --staged works with full path from root' '
	(
	cd repo &&
	echo mod >sub/d.txt &&
	git add sub/d.txt &&
	git restore --staged sub/d.txt &&
	git diff --cached --name-only >staged &&
	! grep "d.txt" staged &&
	git checkout -- sub/d.txt
	)
'

test_expect_success 'restore --staged -q is quiet' '
	(
	cd repo &&
	echo mod >a.txt &&
	git add a.txt &&
	git restore --staged -q a.txt >out 2>&1 &&
	test_must_be_empty out &&
	git checkout -- a.txt
	)
'

test_expect_success 'restore --staged with multiple new files' '
	(
	cd repo &&
	echo one >new1.txt &&
	echo two >new2.txt &&
	echo three >new3.txt &&
	git add new1.txt new2.txt new3.txt &&
	git restore --staged new1.txt new3.txt &&
	git ls-files --stage new1.txt >ls1 &&
	git ls-files --stage new2.txt >ls2 &&
	git ls-files --stage new3.txt >ls3 &&
	test_must_be_empty ls1 &&
	grep "new2.txt" ls2 &&
	test_must_be_empty ls3 &&
	git restore --staged . &&
	rm -f new1.txt new2.txt new3.txt
	)
'

test_expect_success 'restore --staged on binary file' '
	(
	cd repo &&
	printf "\x00\x01\x02\x03" >binary.bin &&
	git add binary.bin &&
	git commit -m "add binary" &&
	printf "\x04\x05\x06\x07" >binary.bin &&
	git add binary.bin &&
	git restore --staged binary.bin &&
	git diff --cached --name-only >staged &&
	! grep "binary.bin" staged &&
	git checkout -- binary.bin
	)
'

test_expect_success 'restore --staged preserves other staged changes' '
	(
	cd repo &&
	echo change-a >a.txt &&
	echo change-b >b.txt &&
	git add a.txt b.txt &&
	git restore --staged a.txt &&
	git diff --cached --name-only >staged &&
	! grep "a.txt" staged &&
	grep "b.txt" staged &&
	git restore --staged . &&
	git checkout -- a.txt b.txt
	)
'

test_done
