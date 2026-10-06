#!/bin/sh
# Tests for grit ls-tree --format with various format atoms.

test_description='grit ls-tree --format atoms and combinations'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=$(command -v git)

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repository with blobs, trees, executable' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Test User" &&
	"$REAL_GIT" config user.email "test@example.com" &&
	echo "hello" >file.txt &&
	echo "world" >other.txt &&
	echo "#!/bin/sh" >script.sh &&
	chmod +x script.sh &&
	mkdir -p sub/deep &&
	echo "nested" >sub/nested.txt &&
	echo "deeper" >sub/deep/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial commit"
	)
'

###########################################################################
# Section 2: %(objectname) atom
###########################################################################

test_expect_success 'format %(objectname) lists OIDs' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >actual &&
	while read oid; do
		echo "$oid" | grep -qE "^[0-9a-f]{40}$" ||
			{ echo "bad oid: $oid"; return 1; }
	done <actual
	)
'

test_expect_success 'format %(objectname) matches ls-tree column 3' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >actual &&
	grit ls-tree HEAD | awk "{print \$3}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objectname) matches real git' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectname)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 3: %(objecttype) atom
###########################################################################

test_expect_success 'format %(objecttype) lists types' '
	(
	cd repo &&
	grit ls-tree --format="%(objecttype)" HEAD >actual &&
	while read type; do
		case "$type" in
			blob|tree) ;;
			*) echo "unexpected type: $type"; return 1 ;;
		esac
	done <actual
	)
'

test_expect_success 'format %(objecttype) matches ls-tree column 2' '
	(
	cd repo &&
	grit ls-tree --format="%(objecttype)" HEAD >actual &&
	grit ls-tree HEAD | awk "{print \$2}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objecttype) matches real git' '
	(
	cd repo &&
	grit ls-tree --format="%(objecttype)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objecttype)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 4: %(objectmode) atom
###########################################################################

test_expect_success 'format %(objectmode) lists octal modes' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode)" HEAD >actual &&
	while read mode; do
		echo "$mode" | grep -qE "^[0-9]{6}$" ||
			{ echo "bad mode: $mode"; return 1; }
	done <actual
	)
'

test_expect_success 'format %(objectmode) matches ls-tree column 1' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode)" HEAD >actual &&
	grit ls-tree HEAD | awk "{print \$1}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objectmode) matches real git' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectmode)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(objectmode) includes 100755 for executable' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode) %(path)" HEAD >actual &&
	grep "100755 script.sh" actual
	)
'

test_expect_success 'format %(objectmode) includes 040000 for tree' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode) %(path)" HEAD >actual &&
	grep "040000 sub" actual
	)
'

###########################################################################
# Section 5: %(path) atom
###########################################################################

test_expect_success 'format %(path) lists names' '
	(
	cd repo &&
	grit ls-tree --format="%(path)" HEAD >actual &&
	grit ls-tree --name-only HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format %(path) matches real git' '
	(
	cd repo &&
	grit ls-tree --format="%(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Combined format strings
###########################################################################

test_expect_success 'format with objectname and path' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname) %(path)" HEAD >actual &&
	grit ls-tree HEAD | awk "{print \$3, \$4}" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with objectname and path matches real git' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname) %(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectname) %(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with mode type name path' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode) %(objecttype) %(objectname) %(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectmode) %(objecttype) %(objectname) %(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with tab between objectname and path' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode) %(objecttype) %(objectname)\t%(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectmode) %(objecttype) %(objectname)\t%(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 7: Literal text in format
###########################################################################

test_expect_success 'format with literal prefix' '
	(
	cd repo &&
	grit ls-tree --format="OBJ=%(objectname)" HEAD >actual &&
	while read line; do
		echo "$line" | grep -q "^OBJ=" ||
			{ echo "missing prefix: $line"; return 1; }
	done <actual
	)
'

test_expect_success 'format with literal prefix matches real git' '
	(
	cd repo &&
	grit ls-tree --format="OBJ=%(objectname)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="OBJ=%(objectname)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with literal surrounding text' '
	(
	cd repo &&
	grit ls-tree --format="[%(objecttype)] %(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="[%(objecttype)] %(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with separator between atoms' '
	(
	cd repo &&
	grit ls-tree --format="%(objectmode)|%(objecttype)|%(objectname)|%(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree --format="%(objectmode)|%(objecttype)|%(objectname)|%(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 8: Format with recursive flag
###########################################################################

test_expect_success 'format %(path) with -r shows full paths' '
	(
	cd repo &&
	grit ls-tree -r --format="%(path)" HEAD >actual &&
	grep "sub/nested.txt" actual &&
	grep "sub/deep/file.txt" actual
	)
'

test_expect_success 'format with -r matches real git' '
	(
	cd repo &&
	grit ls-tree -r --format="%(objectname) %(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree -r --format="%(objectname) %(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'format with -r shows only blobs (no trees)' '
	(
	cd repo &&
	grit ls-tree -r --format="%(objecttype)" HEAD >actual &&
	! grep "tree" actual
	)
'

###########################################################################
# Section 9: Format with -t flag (show trees when recursive)
###########################################################################

test_expect_success 'format with -r -t shows trees and blobs' '
	(
	cd repo &&
	grit ls-tree -r -t --format="%(objecttype) %(path)" HEAD >actual &&
	grep "^tree " actual &&
	grep "^blob " actual
	)
'

test_expect_success 'format with -r -t matches real git' '
	(
	cd repo &&
	grit ls-tree -r -t --format="%(objecttype) %(path)" HEAD >actual &&
	"$REAL_GIT" ls-tree -r -t --format="%(objecttype) %(path)" HEAD >expect &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 10: Edge cases
###########################################################################

test_expect_success 'format with only literal text (no atoms)' '
	(
	cd repo &&
	grit ls-tree --format="static" HEAD >actual &&
	line_count=$(wc -l <actual | tr -d " ") &&
	entry_count=$(grit ls-tree HEAD | wc -l | tr -d " ") &&
	test "$line_count" -eq "$entry_count"
	)
'

test_expect_success 'format empty string produces empty lines' '
	(
	cd repo &&
	grit ls-tree --format="" HEAD >actual &&
	entry_count=$(grit ls-tree HEAD | wc -l | tr -d " ") &&
	line_count=$(wc -l <actual | tr -d " ") &&
	test "$line_count" -eq "$entry_count"
	)
'

test_expect_success 'format output line count matches entry count' '
	(
	cd repo &&
	grit ls-tree --format="%(objectname)" HEAD >actual &&
	grit ls-tree HEAD >default_out &&
	test $(wc -l <actual) -eq $(wc -l <default_out)
	)
'

test_expect_success 'format on tree with single entry' '
	(
	cd repo &&
	sub_tree=$("$REAL_GIT" rev-parse HEAD:sub/deep) &&
	grit ls-tree --format="%(objecttype) %(path)" "$sub_tree" >actual &&
	echo "blob file.txt" >expect &&
	test_cmp expect actual
	)
'

test_done
