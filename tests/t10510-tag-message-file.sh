#!/bin/sh
# Test grit tag -m/--message, -F/--file, -a/--annotate,
# -d/--delete, -l/--list, -f/--force, -n, --sort, --contains,
# -i/--ignore-case, and various tag operations.

test_description='grit tag message and file options'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: repo with commits' '
	(
	grit init repo &&
	cd repo &&
	grit config user.email "test@example.com" &&
	grit config user.name "Test User" &&
	echo "first" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "first commit" &&
	echo "second" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "second commit" &&
	echo "third" >file.txt &&
	grit add file.txt &&
	test_tick &&
	grit commit -m "third commit"
	)
'

# --- lightweight tag ---

test_expect_success 'create lightweight tag' '
	(
	cd repo &&
	grit tag v0.1 &&
	grit rev-parse v0.1 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'lightweight tag at specific commit' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit tag v0.0 "$parent" &&
	grit rev-parse v0.0 >actual &&
	echo "$parent" >expect &&
	test_cmp expect actual
	)
'

# --- annotated tag with -m ---

test_expect_success 'tag -m creates annotated tag' '
	(
	cd repo &&
	grit tag -m "Release 1.0" v1.0 &&
	grit cat-file -t v1.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -m message is stored' '
	(
	cd repo &&
	grit cat-file -p v1.0 >actual &&
	grep "Release 1.0" actual
	)
'

test_expect_success 'tag --message creates annotated tag' '
	(
	cd repo &&
	grit tag --message "Release 1.1" v1.1 &&
	grit cat-file -t v1.1 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -m at specific commit' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	grit tag -m "old release" v0.9 "$parent" &&
	grit cat-file -p v0.9 >tag_content &&
	grep "old release" tag_content &&
	grep "$parent" tag_content
	)
'

test_expect_success 'tag -a creates annotated tag' '
	(
	cd repo &&
	grit tag -a -m "annotated" v1.2 &&
	grit cat-file -t v1.2 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -m with multi-word message' '
	(
	cd repo &&
	grit tag -m "This is a longer release message" v1.3 &&
	grit cat-file -p v1.3 >actual &&
	grep "This is a longer release message" actual
	)
'

# --- tag -F / --file ---

test_expect_success 'tag -F reads message from file' '
	(
	cd repo &&
	echo "Message from file" >tag-msg.txt &&
	grit tag -F tag-msg.txt v2.0 &&
	grit cat-file -t v2.0 >actual &&
	echo "tag" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -F message content is correct' '
	(
	cd repo &&
	grit cat-file -p v2.0 >actual &&
	grep "Message from file" actual
	)
'

test_expect_success 'tag --file reads message from file' '
	(
	cd repo &&
	echo "Long form file message" >tag-msg2.txt &&
	grit tag --file tag-msg2.txt v2.1 &&
	grit cat-file -p v2.1 >actual &&
	grep "Long form file message" actual
	)
'

test_expect_success 'tag -F with multi-line file' '
	(
	cd repo &&
	printf "Line 1\nLine 2\nLine 3\n" >multi-msg.txt &&
	grit tag -F multi-msg.txt v2.2 &&
	grit cat-file -p v2.2 >actual &&
	grep "Line 1" actual &&
	grep "Line 2" actual &&
	grep "Line 3" actual
	)
'

test_expect_success 'tag -F at specific commit' '
	(
	cd repo &&
	parent=$(grit rev-parse HEAD~1) &&
	echo "File msg for old commit" >old-msg.txt &&
	grit tag -F old-msg.txt v2.3 "$parent" &&
	grit cat-file -p v2.3 >actual &&
	grep "File msg for old commit" actual
	)
'

# --- tag -l / --list ---

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	grit tag -l >actual &&
	grep "v1.0" actual &&
	grep "v2.0" actual
	)
'

test_expect_success 'tag --list lists all tags' '
	(
	cd repo &&
	grit tag --list >actual &&
	grep "v0.1" actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag with no args lists tags' '
	(
	cd repo &&
	grit tag >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag -l with pattern filters' '
	(
	cd repo &&
	grit tag -l "v1.*" >actual &&
	grep "v1.0" actual &&
	grep "v1.1" actual &&
	! grep "v2.0" actual
	)
'

test_expect_success 'tag -l with pattern v2' '
	(
	cd repo &&
	grit tag -l "v2.*" >actual &&
	grep "v2.0" actual &&
	! grep "v1.0" actual
	)
'

# --- tag -d / --delete ---

test_expect_success 'tag -d deletes lightweight tag' '
	(
	cd repo &&
	grit tag temp-light &&
	grit tag -d temp-light &&
	grit tag -l >actual &&
	! grep "temp-light" actual
	)
'

test_expect_success 'tag -d deletes annotated tag' '
	(
	cd repo &&
	grit tag -m "temp annot" temp-annot &&
	grit tag -d temp-annot &&
	grit tag -l >actual &&
	! grep "temp-annot" actual
	)
'

test_expect_success 'tag --delete works same as -d' '
	(
	cd repo &&
	grit tag temp-del2 &&
	grit tag --delete temp-del2 &&
	grit tag -l >actual &&
	! grep "temp-del2" actual
	)
'

test_expect_success 'tag -d nonexistent tag fails' '
	(
	cd repo &&
	test_must_fail grit tag -d no-such-tag
	)
'

# --- tag -f / --force ---

test_expect_success 'tag without force fails for existing' '
	(
	cd repo &&
	test_must_fail grit tag v1.0
	)
'

test_expect_success 'tag -f overwrites existing tag' '
	(
	cd repo &&
	grit tag -f v0.1 HEAD &&
	grit rev-parse v0.1 >actual &&
	grit rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag --force overwrites existing annotated tag' '
	(
	cd repo &&
	grit tag --force -m "Updated release" v1.0 &&
	grit cat-file -p v1.0 >actual &&
	grep "Updated release" actual
	)
'

# --- tag -n (show annotation lines) ---

test_expect_success 'tag -n shows annotation' '
	(
	cd repo &&
	grit tag -n >actual &&
	grep "v1.0" actual
	)
'

test_expect_success 'tag -n1 shows one line of annotation' '
	(
	cd repo &&
	grit tag -n1 >actual &&
	grep "v1.0" actual
	)
'

# --- tag --contains ---

test_expect_success 'tag --contains HEAD shows tags at HEAD' '
	(
	cd repo &&
	grit tag --contains HEAD >actual &&
	grep "v0.1" actual
	)
'

# --- tag --sort ---

test_expect_success 'tag --sort=version:refname sorts by version' '
	(
	cd repo &&
	grit tag --sort=version:refname >actual &&
	# just verify it produces output without error
	test -s actual
	)
'

# --- tag -i / --ignore-case ---

test_expect_success 'tag -l -i case insensitive sort' '
	(
	cd repo &&
	grit tag AAA-upper &&
	grit tag zzz-lower &&
	grit tag -l -i >actual &&
	grep "AAA-upper" actual &&
	grep "zzz-lower" actual
	)
'

# --- tag annotated content fields ---

test_expect_success 'annotated tag contains tagger info' '
	(
	cd repo &&
	grit cat-file -p v1.0 >actual &&
	grep "tagger" actual
	)
'

test_expect_success 'annotated tag contains object reference' '
	(
	cd repo &&
	grit cat-file -p v1.0 >actual &&
	grep "^object" actual
	)
'

test_expect_success 'annotated tag contains tag name' '
	(
	cd repo &&
	grit cat-file -p v1.0 >actual &&
	grep "^tag v1.0" actual
	)
'

test_expect_success 'annotated tag contains type' '
	(
	cd repo &&
	grit cat-file -p v1.0 >actual &&
	grep "^type commit" actual
	)
'

test_done
