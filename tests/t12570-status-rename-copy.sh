#!/bin/sh
# Tests for grit status with renames, copies, staged/unstaged mixes,
# various output formats (short, porcelain, -z, -b), untracked files,
# ignored files, and combinations.

test_description='grit status: renames, copies, and advanced scenarios'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

test_expect_success 'setup: initial repo with files' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	sane_unset GIT_AUTHOR_NAME &&
	sane_unset GIT_AUTHOR_EMAIL &&
	sane_unset GIT_COMMITTER_NAME &&
	sane_unset GIT_COMMITTER_EMAIL &&
	echo "alpha content" >a.txt &&
	echo "bravo content" >b.txt &&
	echo "charlie content" >c.txt &&
	mkdir -p dir &&
	echo "deep content" >dir/d.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ---- clean state ----

test_expect_success 'status -s: clean repo is empty' '
	(cd repo && grit status -s >../actual) &&
	test_must_be_empty actual
'

test_expect_success 'status --porcelain: clean repo is empty (matches git)' '
	(cd repo && grit status --porcelain >../actual) &&
	test_must_be_empty actual
'

# ---- staged rename (delete + add) ----

test_expect_success 'setup: rename a file via git mv' '
	(cd repo && "$REAL_GIT" mv a.txt renamed-a.txt)
'

test_expect_success 'status -s: rename shows R' '
	(cd repo && grit status -s >../actual) &&
	grep -E "R.*renamed-a.txt" actual
'

test_expect_success 'status --porcelain: rename shows R' '
	(cd repo && grit status --porcelain >../actual) &&
	grep -E "R.*renamed-a.txt" actual
'

test_expect_success 'setup: commit rename' '
	(cd repo && grit commit -m "rename a to renamed-a")
'

# ---- staged modification + untracked ----

test_expect_success 'setup: modify file and create untracked' '
	(cd repo &&
	 echo "bravo2" >>b.txt &&
	 grit add b.txt &&
	 echo "untracked" >unt.txt)
'

test_expect_success 'status -s: staged mod shows M in first col' '
	(cd repo && grit status -s >../actual) &&
	grep "^M" actual | grep "b.txt"
'

test_expect_success 'status -s: untracked shows ??' '
	(cd repo && grit status -s >../actual) &&
	grep "^??" actual | grep "unt.txt"
'

test_expect_success 'setup: commit staged mod' '
	(cd repo && grit commit -m "mod b")
'

# ---- worktree modification only ----

test_expect_success 'setup: modify file without staging' '
	(cd repo && echo "charlie2" >>c.txt)
'

test_expect_success 'status -s: worktree mod shows M in second col' '
	(cd repo && grit status -s >../actual) &&
	grep "^ M c.txt" actual
'

# ---- staged and worktree modification (same file) ----

test_expect_success 'setup: stage c.txt then modify again' '
	(cd repo &&
	 grit add c.txt &&
	 echo "charlie3" >>c.txt)
'

test_expect_success 'status -s: both staged and worktree mod shown' '
	(cd repo && grit status -s >../actual) &&
	grep "^MM c.txt" actual
'

test_expect_success 'status --porcelain: MM status code' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "^MM c.txt" actual
'

test_expect_success 'setup: reset for next tests' '
	(cd repo && "$REAL_GIT" reset HEAD -- c.txt && "$REAL_GIT" checkout -- c.txt)
'

# ---- staged deletion ----

test_expect_success 'setup: stage deletion of c.txt' '
	(cd repo && "$REAL_GIT" rm -f c.txt)
'

test_expect_success 'status -s: staged deletion shows D in first col' '
	(cd repo && grit status -s >../actual) &&
	grep "^D  c.txt" actual
'

test_expect_success 'setup: commit deletion' '
	(cd repo && grit commit -m "del c")
'

# ---- new file staged ----

test_expect_success 'setup: create and stage new file' '
	(cd repo && echo "new content" >new.txt && grit add new.txt)
'

test_expect_success 'status -s: new staged file shows A' '
	(cd repo && grit status -s >../actual) &&
	grep "^A  new.txt" actual
'

test_expect_success 'setup: commit new file' '
	(cd repo && grit commit -m "add new")
'

# ---- -u flag (untracked files) ----

test_expect_success 'setup: create untracked files' '
	(cd repo &&
	 echo "ut1" >ut1.txt &&
	 mkdir -p utdir &&
	 echo "ut2" >utdir/ut2.txt)
'

test_expect_success 'status -s: untracked files shown by default' '
	(cd repo && grit status -s >../actual) &&
	grep "^??" actual
'

test_expect_success 'status -s -u no: no untracked files shown' '
	(cd repo && grit status -s -u no >../actual) &&
	! grep "^??" actual
'

# ---- --ignored flag ----

test_expect_success 'setup: create .gitignore and ignored file' '
	(cd repo &&
	 echo "*.log" >.gitignore &&
	 echo "log data" >test.log &&
	 grit add .gitignore)
'

test_expect_success 'status -s: ignored file not shown by default' '
	(cd repo && grit status -s >../actual) &&
	! grep "!! test.log" actual
'

test_expect_success 'status --porcelain --ignored: ignored file shown' '
	(cd repo && grit status --porcelain --ignored >../actual) &&
	grep "test.log" actual
'

test_expect_success 'setup: commit gitignore' '
	(cd repo && grit commit -m "add gitignore")
'

# ---- -z (NUL termination) ----

test_expect_success 'setup: create change for -z test' '
	(cd repo && echo "mod" >>b.txt && grit add b.txt)
'

test_expect_success 'status --porcelain -z: NUL-terminated output' '
	(cd repo && grit status --porcelain -z >../actual) &&
	# NUL bytes should be present
	tr "\0" "\n" <actual >actual_lines &&
	grep "b.txt" actual_lines
'

test_expect_success 'setup: commit for -z test' '
	(cd repo && grit commit -m "mod b again")
'

# ---- -b (branch header) ----

test_expect_success 'status --porcelain -b: shows branch header' '
	(cd repo && grit status --porcelain -b >../actual) &&
	grep "^## main" actual
'

# ---- multiple changes at once ----

test_expect_success 'setup: multiple simultaneous changes' '
	(cd repo &&
	 echo "mod b" >>b.txt &&
	 grit add b.txt &&
	 echo "new2" >new2.txt &&
	 grit add new2.txt &&
	 echo "worktree only" >>dir/d.txt &&
	 echo "ut" >ut_new.txt)
'

test_expect_success 'status -s: all change types shown' '
	(cd repo && grit status -s >../actual) &&
	grep "^M" actual | grep "b.txt" &&
	grep "^A" actual | grep "new2.txt" &&
	grep "^ M" actual | grep "dir/d.txt" &&
	grep "^??" actual | grep "ut_new.txt"
'

test_expect_success 'status --porcelain: all change types shown' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "^M" actual &&
	grep "^A" actual &&
	grep "^ M" actual &&
	grep "^??" actual
'

test_expect_success 'status: long format shows sections' '
	(cd repo && grit status >../actual) &&
	grep "Changes to be committed" actual &&
	grep "Changes not staged" actual &&
	grep "Untracked files" actual
'

test_done
