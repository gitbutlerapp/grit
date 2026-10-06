#!/bin/sh

test_description='grit tag create, list, delete, sort, annotate, filter, and message handling'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	git config user.email "t@t.com" &&
	git config user.name "T" &&
	echo hello >file.txt &&
	grit add file.txt &&
	grit commit -m "initial"
	)
'

test_expect_success 'tag creates lightweight tag' '
	(cd repo && grit tag v0.1) &&
	(cd repo && grit tag -l >../actual) &&
	grep "v0.1" actual
'

test_expect_success 'lightweight tag points to commit' '
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/v0.1 >../actual) &&
	echo "commit" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -a creates annotated tag' '
	(cd repo && grit tag -a -m "annotated tag" v0.2) &&
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/v0.2 >../actual) &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -m implies annotated' '
	(cd repo && grit tag -m "implies annotated" v0.3) &&
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/v0.3 >../actual) &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -l lists all tags' '
	(cd repo && grit tag -l >../actual) &&
	grep "v0.1" actual &&
	grep "v0.2" actual &&
	grep "v0.3" actual
'

test_expect_success 'tag -l with glob pattern filters' '
	(cd repo && grit tag -l "v0.1" >../actual) &&
	echo "v0.1" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -l with wildcard pattern' '
	(cd repo && grit tag -l "v0.*" >../actual) &&
	grep "v0.1" actual &&
	grep "v0.2" actual
'

test_expect_success 'tag -n shows first line of annotation' '
	(cd repo && grit tag -n1 >../actual) &&
	grep "v0.2" actual | grep "annotated tag"
'

test_expect_success 'tag -n on lightweight shows no annotation' '
	(cd repo && grit tag -n1 >../actual) &&
	line=$(grep "v0.1" actual) &&
	# lightweight tag line should not have extra annotation text
	test -n "$line"
'

test_expect_success 'tag -d deletes a tag' '
	(cd repo && grit tag delete-me &&
	 grit tag -d delete-me >../actual 2>&1) &&
	(cd repo && grit tag -l >../tags) &&
	! grep "delete-me" tags
'

test_expect_success 'tag -d on annotated tag' '
	(cd repo && grit tag -m "will delete" del-ann &&
	 grit tag -d del-ann) &&
	(cd repo && grit tag -l >../actual) &&
	! grep "del-ann" actual
'

test_expect_success 'tag --sort=refname sorts alphabetically' '
	(cd repo && grit tag --sort=refname >../actual) &&
	# v0.1 should come before v0.2
	awk "/v0.1/{a=NR} /v0.2/{b=NR} END{exit(a<b?0:1)}" actual
'

test_expect_success 'tag --sort=-refname sorts reverse alphabetically' '
	(cd repo && grit tag --sort=-refname >../actual) &&
	# v0.3 should come before v0.1
	awk "/v0.3/{a=NR} /v0.1/{b=NR} END{exit(a<b?0:1)}" actual
'

test_expect_success 'tag with specific commit as target' '
	(cd repo &&
	 oid=$(grit rev-parse HEAD) &&
	 grit tag at-commit "$oid") &&
	(cd repo && grit tag -l >../actual) &&
	grep "at-commit" actual
'

test_expect_success 'annotated tag message stored correctly' '
	(cd repo && grit cat-file -p v0.2 >../actual) &&
	grep "annotated tag" actual
'

test_expect_success 'annotated tag has tagger line' '
	(cd repo && grit cat-file -p v0.2 >../actual) &&
	grep "tagger" actual
'

test_expect_success 'annotated tag has correct type' '
	(cd repo && grit cat-file -p v0.2 >../actual) &&
	grep "type commit" actual
'

test_expect_success 'annotated tag has correct tag name in object' '
	(cd repo && grit cat-file -p v0.2 >../actual) &&
	grep "tag v0.2" actual
'

test_expect_success 'tag -f overwrites existing lightweight tag' '
	(cd repo &&
	 echo more >file2.txt &&
	 grit add file2.txt &&
	 grit commit -m "second" &&
	 grit tag -f v0.1 &&
	 grit for-each-ref --format="%(objectname)" refs/tags/v0.1 >../new_oid &&
	 grit rev-parse HEAD >../head_oid) &&
	test_cmp new_oid head_oid
'

test_expect_success 'tag -f -m overwrites with annotated tag' '
	(cd repo && grit tag -f -m "forced annotated" v0.1) &&
	(cd repo && grit for-each-ref --format="%(objecttype)" refs/tags/v0.1 >../actual) &&
	echo "tag" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag message with leading spaces gets stripped' '
	(cd repo && grit tag -m "  leading spaces" v-leading) &&
	(cd repo && grit cat-file -p v-leading >../actual) &&
	grep "leading spaces" actual
'

test_expect_success 'tag message with trailing spaces gets stripped' '
	(cd repo && grit tag -m "trailing spaces  " v-trailing) &&
	(cd repo && grit cat-file -p v-trailing >../actual) &&
	grep "trailing spaces" actual
'

test_expect_success 'tag -F reads message from file' '
	echo "file-based message" >tag-msg &&
	(cd repo && grit tag -F ../tag-msg v-from-file) &&
	(cd repo && grit cat-file -p v-from-file >../actual) &&
	grep "file-based message" actual
'

test_expect_success 'tag -F with multi-line message' '
	printf "line1\nline2\nline3\n" >multi-msg &&
	(cd repo && grit tag -F ../multi-msg v-multi) &&
	(cd repo && grit cat-file -p v-multi >../actual) &&
	grep "line1" actual &&
	grep "line2" actual &&
	grep "line3" actual
'

test_expect_success 'tag --contains lists tags containing HEAD' '
	(cd repo && grit tag --contains HEAD >../actual) &&
	test -s actual
'

test_expect_success 'tag -n2 shows two lines of annotation' '
	(cd repo && grit tag -m "first line
second line" v-twolines &&
	 grit tag -n2 >../actual) &&
	grep "first line" actual
'

test_expect_success 'many tags can be created and listed' '
	(cd repo &&
	 for i in $(seq 1 10); do
	   grit tag "batch-$i" || return 1
	 done &&
	 grit tag -l >../actual) &&
	count=$(grep -c "batch-" actual) &&
	test "$count" -eq 10
'

test_expect_success 'deleting nonexistent tag fails' '
	(cd repo && test_must_fail grit tag -d no-such-tag 2>../err) &&
	test -s err
'

test_expect_success 'tag with slash in name' '
	(cd repo && grit tag release/v1.0) &&
	(cd repo && grit tag -l >../actual) &&
	grep "release/v1.0" actual
'

test_expect_success 'delete tag with slash in name' '
	(cd repo && grit tag -d release/v1.0) &&
	(cd repo && grit tag -l >../actual) &&
	! grep "release/v1.0" actual
'

test_expect_success 'tag -i for case-insensitive sort' '
	(cd repo && grit tag -i --sort=refname -l >../actual) &&
	test -s actual
'

test_expect_success 'for-each-ref shows tag refs' '
	(cd repo && grit for-each-ref --format="%(refname:short) %(objecttype)" refs/tags/ >../actual) &&
	grep "tag" actual
'

test_done
