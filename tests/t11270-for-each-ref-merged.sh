#!/bin/sh
# Tests for grit for-each-ref with --merged, --no-merged, --contains, --sort, --format.

test_description='grit for-each-ref: merged/no-merged filters, contains, sort, format options'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with multiple branches and tags' '
	(
	"$REAL_GIT" init -b main repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&

	echo "base" >file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "initial commit" &&
	"$REAL_GIT" tag v1.0 &&

	"$REAL_GIT" branch feature-a &&
	"$REAL_GIT" branch feature-b &&

	echo "main line 2" >>file.txt &&
	"$REAL_GIT" add file.txt &&
	"$REAL_GIT" commit -m "main commit 2" &&
	"$REAL_GIT" tag v2.0 &&

	"$REAL_GIT" checkout feature-a &&
	echo "feature a" >a.txt &&
	"$REAL_GIT" add a.txt &&
	"$REAL_GIT" commit -m "feature a commit" &&
	"$REAL_GIT" tag feature-a-done &&

	"$REAL_GIT" checkout feature-b &&
	echo "feature b" >b.txt &&
	"$REAL_GIT" add b.txt &&
	"$REAL_GIT" commit -m "feature b commit" &&

	"$REAL_GIT" checkout main &&
	"$REAL_GIT" merge feature-a -m "merge feature-a" &&
	"$REAL_GIT" tag v3.0 &&

	"$REAL_GIT" branch old-merged feature-a &&
	"$REAL_GIT" branch unmerged-work feature-b
	)
'

###########################################################################
# Section 2: Basic for-each-ref listing
###########################################################################

test_expect_success 'for-each-ref: lists all refs' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref >output.txt &&
	test -s output.txt
	)
'

test_expect_success 'for-each-ref: output includes branches' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt &&
	grep "refs/heads/feature-a" output.txt &&
	grep "refs/heads/feature-b" output.txt
	)
'

test_expect_success 'for-each-ref: output includes tags' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/tags/ >output.txt &&
	grep "refs/tags/v1.0" output.txt &&
	grep "refs/tags/v2.0" output.txt &&
	grep "refs/tags/v3.0" output.txt
	)
'

test_expect_success 'for-each-ref: filter by pattern refs/heads/' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads/ >output.txt &&
	! grep "refs/tags/" output.txt
	)
'

test_expect_success 'for-each-ref: filter by pattern refs/tags/' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/tags/ >output.txt &&
	! grep "refs/heads/" output.txt
	)
'

###########################################################################
# Section 3: --merged and --no-merged
###########################################################################

test_expect_success 'for-each-ref --merged: shows branches merged into HEAD' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --merged HEAD refs/heads/ >output.txt &&
	grep "refs/heads/feature-a" output.txt &&
	grep "refs/heads/main" output.txt
	)
'

test_expect_success 'for-each-ref --merged: does not show unmerged branches' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --merged HEAD refs/heads/ >output.txt &&
	! grep "refs/heads/feature-b" output.txt &&
	! grep "refs/heads/unmerged-work" output.txt
	)
'

test_expect_success 'for-each-ref --no-merged: shows unmerged branches' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --no-merged HEAD refs/heads/ >output.txt &&
	grep "refs/heads/feature-b" output.txt
	)
'

test_expect_success 'for-each-ref --no-merged: does not show merged branches' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --no-merged HEAD refs/heads/ >output.txt &&
	! grep "refs/heads/feature-a" output.txt
	)
'

test_expect_success 'for-each-ref --merged with tag ref' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --merged v3.0 refs/heads/ >output.txt &&
	grep "refs/heads/feature-a" output.txt
	)
'

test_expect_success 'for-each-ref --merged with SHA' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" for-each-ref --merged "$SHA" refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt
	)
'

###########################################################################
# Section 4: --contains
###########################################################################

test_expect_success 'for-each-ref --contains: finds refs containing a commit' '
	(
	cd repo &&
	INITIAL=$("$REAL_GIT" rev-parse v1.0) &&
	"$GUST_BIN" for-each-ref --contains "$INITIAL" refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt
	)
'

test_expect_success 'for-each-ref --contains: main contains initial commit' '
	(
	cd repo &&
	INITIAL=$("$REAL_GIT" rev-parse v1.0) &&
	"$GUST_BIN" for-each-ref --contains "$INITIAL" refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt
	)
'

test_expect_success 'for-each-ref --contains: only main contains merge commit' '
	(
	cd repo &&
	MERGE=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" for-each-ref --contains "$MERGE" refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt &&
	! grep "refs/heads/feature-b" output.txt
	)
'

###########################################################################
# Section 5: --format
###########################################################################

test_expect_success 'for-each-ref --format: refname only' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname)" refs/heads/ >output.txt &&
	grep "refs/heads/main" output.txt &&
	! grep "commit" output.txt
	)
'

test_expect_success 'for-each-ref --format: objectname (SHA)' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objectname)" refs/heads/main >output.txt &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/main) &&
	grep "$SHA" output.txt
	)
'

test_expect_success 'for-each-ref --format: refname:short' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(refname:short)" refs/heads/ >output.txt &&
	grep "^main$" output.txt
	)
'

test_expect_success 'for-each-ref --format: objecttype' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/heads/main >output.txt &&
	grep "commit" output.txt
	)
'

test_expect_success 'for-each-ref --format: compound format string' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objectname) %(refname)" refs/heads/main >output.txt &&
	test -s output.txt &&
	grep "main" output.txt
	)
'

test_expect_success 'for-each-ref --format: objectname full SHA' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objectname)" refs/heads/main >output.txt &&
	SHA=$("$REAL_GIT" rev-parse refs/heads/main) &&
	test "$(cat output.txt)" = "$SHA"
	)
'

test_expect_success 'for-each-ref --format: subject' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(subject)" refs/heads/main >output.txt &&
	grep "merge feature-a" output.txt
	)
'

###########################################################################
# Section 6: --sort
###########################################################################

test_expect_success 'for-each-ref --sort=refname: alphabetical order' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=refname --format="%(refname:short)" refs/heads/ >output.txt &&
	head -1 output.txt >first.txt &&
	tail -1 output.txt >last.txt &&
	# feature-a should come before unmerged-work alphabetically
	grep "feature-a" first.txt
	)
'

test_expect_success 'for-each-ref --sort=-refname: reverse alphabetical' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=-refname --format="%(refname:short)" refs/heads/ >output.txt &&
	head -1 output.txt >first.txt &&
	grep "unmerged-work" first.txt
	)
'

test_expect_success 'for-each-ref --sort=refname: tags in order' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=refname --format="%(refname:short)" refs/tags/ >output.txt &&
	head -1 output.txt >first.txt &&
	grep "feature-a-done" first.txt
	)
'

test_expect_success 'for-each-ref --sort=-refname: tags reverse order' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --sort=-refname --format="%(refname:short)" refs/tags/ >output.txt &&
	head -1 output.txt >first.txt &&
	grep "v3.0" first.txt
	)
'

###########################################################################
# Section 7: --count
###########################################################################

test_expect_success 'for-each-ref --count: limits output' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=1 refs/heads/ >output.txt &&
	test_line_count = 1 output.txt
	)
'

test_expect_success 'for-each-ref --count=2: two results' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --count=2 refs/heads/ >output.txt &&
	test_line_count = 2 output.txt
	)
'

###########################################################################
# Section 8: Multiple patterns
###########################################################################

test_expect_success 'for-each-ref: multiple patterns' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads/main refs/tags/v1.0 >output.txt &&
	grep "refs/heads/main" output.txt &&
	grep "refs/tags/v1.0" output.txt
	)
'

test_expect_success 'for-each-ref: non-matching pattern returns empty' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref refs/heads/nonexistent >output.txt &&
	test_must_be_empty output.txt
	)
'

###########################################################################
# Section 9: --points-at
###########################################################################

test_expect_success 'for-each-ref --points-at: finds refs pointing at commit' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse v1.0) &&
	"$GUST_BIN" for-each-ref --points-at="$SHA" refs/tags/ >output.txt &&
	grep "refs/tags/v1.0" output.txt
	)
'

test_expect_success 'for-each-ref --points-at: HEAD commit' '
	(
	cd repo &&
	SHA=$("$REAL_GIT" rev-parse HEAD) &&
	"$GUST_BIN" for-each-ref --points-at="$SHA" >output.txt &&
	grep "refs/heads/main" output.txt &&
	grep "refs/tags/v3.0" output.txt
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'for-each-ref: empty repo has no refs' '
	(
	"$REAL_GIT" init -b main empty-repo &&
	cd empty-repo &&
	"$GUST_BIN" for-each-ref >output.txt &&
	test_must_be_empty output.txt
	)
'

test_expect_success 'for-each-ref: works with annotated tags' '
	(
	cd repo &&
	"$REAL_GIT" tag -a v4.0 -m "annotated tag" HEAD &&
	"$GUST_BIN" for-each-ref --format="%(refname) %(objecttype)" refs/tags/v4.0 >output.txt &&
	grep "refs/tags/v4.0" output.txt
	)
'

test_expect_success 'for-each-ref: annotated tag shows tag objecttype' '
	(
	cd repo &&
	"$GUST_BIN" for-each-ref --format="%(objecttype)" refs/tags/v4.0 >output.txt &&
	grep "tag" output.txt
	)
'

test_done
