#!/bin/sh
# Tests for grit diff --stat and --numstat summary line formatting:
# correct file counts, insertion/deletion counts, singular/plural,
# various file operations (add, delete, modify), and edge cases.

test_description='grit diff --stat and --numstat summary formatting'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

test_expect_success 'setup: initial repo with files' '
	(
	grit init repo &&
	cd repo &&
	"$REAL_GIT" config user.email "t@t.com" &&
	"$REAL_GIT" config user.name "T" &&
	echo "line1" >f1.txt &&
	echo "line1" >f2.txt &&
	echo "line1" >f3.txt &&
	echo "line1" >f4.txt &&
	echo "line1" >f5.txt &&
	grit add . &&
	grit commit -m "initial"
	)
'

# ---- single file modification ----

test_expect_success 'setup: modify one file' '
	(cd repo && echo "line2" >>f1.txt && grit add . && grit commit -m "mod f1")
'

test_expect_success 'stat: 1 file changed shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "1 file changed" actual
'

test_expect_success 'stat: insertion count shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "1 insertion" actual
'

test_expect_success 'stat: file name appears in stat' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "f1.txt" actual
'

test_expect_success 'stat: plus sign shown for insertions' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "+" actual
'

test_expect_success 'numstat: single file matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- multiple file modifications ----

test_expect_success 'setup: modify three files' '
	(cd repo &&
	 echo "line2" >>f2.txt &&
	 echo "line2" >>f3.txt &&
	 echo "line2" >>f4.txt &&
	 grit add . && grit commit -m "mod f2 f3 f4")
'

test_expect_success 'stat: 3 files changed shown (plural)' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "3 files changed" actual
'

test_expect_success 'stat: 3 insertions shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "3 insertion" actual
'

test_expect_success 'stat: all three filenames appear' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "f2.txt" actual &&
	grep "f3.txt" actual &&
	grep "f4.txt" actual
'

test_expect_success 'numstat: multiple files matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- file deletion ----

test_expect_success 'setup: delete a file' '
	(cd repo && "$REAL_GIT" rm f5.txt && grit commit -m "del f5")
'

test_expect_success 'stat: deletion shows minus sign' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "-" actual
'

test_expect_success 'stat: 1 deletion shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "1 deletion" actual
'

test_expect_success 'stat: deleted file name appears' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "f5.txt" actual
'

test_expect_success 'numstat: deletion matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- file addition ----

test_expect_success 'setup: add a new file' '
	(cd repo && echo "brand new" >newfile.txt && grit add newfile.txt && grit commit -m "add new")
'

test_expect_success 'stat: addition shows file name' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "newfile.txt" actual
'

test_expect_success 'stat: 1 file changed 1 insertion for new file' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "1 file changed" actual &&
	grep "1 insertion" actual
'

test_expect_success 'numstat: addition matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- mixed add + delete + modify ----

test_expect_success 'setup: mixed changes' '
	(cd repo &&
	 echo "modified" >f1.txt &&
	 "$REAL_GIT" rm f2.txt &&
	 echo "another new" >another.txt &&
	 grit add another.txt f1.txt &&
	 grit commit -m "mixed changes")
'

test_expect_success 'stat: mixed shows files changed' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "3 files changed" actual
'

test_expect_success 'stat: mixed shows insertions and deletions' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "insertion" actual &&
	grep "deletion" actual
'

test_expect_success 'numstat: mixed matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- large number of insertions ----

test_expect_success 'setup: many-line file' '
	(cd repo &&
	 for i in $(seq 1 100); do echo "line $i"; done >big.txt &&
	 grit add big.txt && grit commit -m "add big")
'

test_expect_success 'stat: large insertion count shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "100 insertion" actual
'

test_expect_success 'stat: bar graph uses + for insertions' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "+++++" actual
'

test_expect_success 'numstat: large file matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- large number of deletions ----

test_expect_success 'setup: delete big file' '
	(cd repo && "$REAL_GIT" rm big.txt && grit commit -m "del big")
'

test_expect_success 'stat: large deletion count shown' '
	(cd repo && grit diff --stat HEAD~1 HEAD >../actual) &&
	grep "100 deletion" actual
'

test_expect_success 'numstat: large deletion matches git' '
	(cd repo && grit diff --numstat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff --numstat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff --stat on working tree changes ----

test_expect_success 'stat: working tree modification' '
	(cd repo &&
	 echo "wt change" >>f1.txt &&
	 grit diff --stat >../actual) &&
	grep "f1.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'stat: working tree matches git' '
	(cd repo &&
	 grit diff --stat >../grit_out &&
	 "$REAL_GIT" diff --stat >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff --stat on --cached ----

test_expect_success 'stat: cached modification' '
	(cd repo &&
	 grit add f1.txt &&
	 grit diff --cached --stat >../actual) &&
	grep "f1.txt" actual &&
	grep "1 file changed" actual
'

test_expect_success 'stat: cached matches git' '
	(cd repo &&
	 grit diff --cached --stat >../grit_out &&
	 "$REAL_GIT" diff --cached --stat >../git_out) &&
	test_cmp git_out grit_out
'

# ---- diff-tree --stat ----

test_expect_success 'setup: commit for diff-tree stat test' '
	(cd repo && grit commit -m "commit wt change")
'

test_expect_success 'diff-tree --stat: summary line present' '
	(cd repo && grit diff-tree --stat HEAD~1 HEAD >../actual) &&
	grep "1 file changed" actual
'

test_expect_success 'diff-tree --stat: matches git' '
	(cd repo && grit diff-tree --stat HEAD~1 HEAD >../grit_out &&
	 "$REAL_GIT" diff-tree --stat HEAD~1 HEAD >../git_out) &&
	test_cmp git_out grit_out
'

test_done
