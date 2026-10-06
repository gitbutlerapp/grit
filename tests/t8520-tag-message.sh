#!/bin/sh
# Tests for 'grit tag -m, -F, -a' with various message formats.

test_description='tag -m, -F, -a with various message formats'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=${REAL_GIT:-/usr/bin/git}

# ── Setup ────────────────────────────────────────────────────────────────────

test_expect_success 'setup: create repository with commits' '
	(
	git init repo &&
	cd repo &&
	git config user.name "Test User" &&
	git config user.email "test@example.com" &&
	echo "first" >file.txt &&
	git add file.txt &&
	git commit -m "first" &&
	git tag c1 &&
	echo "second" >file.txt &&
	git add file.txt &&
	git commit -m "second" &&
	git tag c2
	)
'

# ── tag -m: inline message ──────────────────────────────────────────────────

test_expect_success 'tag -m creates annotated tag with message' '
	(
	cd repo &&
	git tag -m "release one" v1.0 &&
	git cat-file -t v1.0 >type &&
	echo "tag" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tag -m message content is correct' '
	(
	cd repo &&
	git cat-file tag v1.0 >raw &&
	grep "release one" raw
	)
'

test_expect_success 'tag -m stores tagger name' '
	(
	cd repo &&
	git cat-file tag v1.0 >raw &&
	grep "tagger C O Mitter" raw
	)
'

test_expect_success 'tag -m stores tagger email' '
	(
	cd repo &&
	git cat-file tag v1.0 >raw &&
	grep "committer@example.com" raw
	)
'

test_expect_success 'tag -m with empty message is accepted' '
	(
	cd repo &&
	git tag -m "" v-empty &&
	git cat-file -t v-empty >type &&
	echo "tag" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tag -m with multiline message' '
	(
	cd repo &&
	git tag -m "line one
line two
line three" v-multi &&
	git cat-file tag v-multi >raw &&
	grep "line one" raw &&
	grep "line two" raw &&
	grep "line three" raw
	)
'

test_expect_success 'tag -m with special characters' '
	(
	cd repo &&
	git tag -m "special: !@#%&*()_+-=[]{}|;:,.<>?" v-special &&
	git cat-file tag v-special >raw &&
	grep "special:" raw
	)
'

test_expect_success 'tag -m with unicode characters' '
	(
	cd repo &&
	git tag -m "héllo wörld 日本語" v-unicode &&
	git cat-file tag v-unicode >raw &&
	grep "héllo" raw
	)
'

test_expect_success 'tag -m with long message' '
	(
	cd repo &&
	long_msg=$(printf "%0200d this is a very long tag message" 0) &&
	git tag -m "$long_msg" v-long &&
	git cat-file tag v-long >raw &&
	grep "very long tag message" raw
	)
'

# ── tag -a: annotated without inline message ─────────────────────────────────

test_expect_success 'tag -a -m creates annotated tag' '
	(
	cd repo &&
	git tag -a -m "annotated release" v2.0 &&
	git cat-file -t v2.0 >type &&
	echo "tag" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tag -a -m message content correct' '
	(
	cd repo &&
	git cat-file tag v2.0 >raw &&
	grep "annotated release" raw
	)
'

# ── tag -F: message from file ────────────────────────────────────────────────

test_expect_success 'tag -F creates tag from file message' '
	(
	cd repo &&
	echo "message from file" >msg.txt &&
	git tag -F msg.txt v-from-file &&
	git cat-file -t v-from-file >type &&
	echo "tag" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tag -F message content matches file' '
	(
	cd repo &&
	git cat-file tag v-from-file >raw &&
	grep "message from file" raw
	)
'

test_expect_success 'tag -F with multiline file' '
	(
	cd repo &&
	printf "line A\nline B\nline C\n" >multi-msg.txt &&
	git tag -F multi-msg.txt v-multifile &&
	git cat-file tag v-multifile >raw &&
	grep "line A" raw &&
	grep "line B" raw &&
	grep "line C" raw
	)
'

test_expect_success 'tag -F with empty file is accepted' '
	(
	cd repo &&
	>empty-msg.txt &&
	git tag -F empty-msg.txt v-emptyfile &&
	git cat-file -t v-emptyfile >type &&
	echo "tag" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'tag -F from /dev/stdin' '
	(
	cd repo &&
	echo "stdin message" | git tag -F /dev/stdin v-stdin &&
	git cat-file tag v-stdin >raw &&
	grep "stdin message" raw
	)
'

# ── tag on specific commits ─────────────────────────────────────────────────

test_expect_success 'tag -m on specific commit (not HEAD)' '
	(
	cd repo &&
	sha=$(git rev-parse c1) &&
	git tag -m "old commit" v-old "$sha" &&
	git rev-parse "v-old^{commit}" >actual &&
	echo "$sha" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'tag -m on HEAD explicitly' '
	(
	cd repo &&
	git tag -m "at head" v-head HEAD &&
	git rev-parse "v-head^{commit}" >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# ── tag listing with -n ──────────────────────────────────────────────────────

test_expect_success 'tag -n shows first line of annotation' '
	(
	cd repo &&
	git tag -n >out &&
	grep "v1.0" out &&
	grep "release one" out
	)
'

test_expect_success 'tag -n1 shows one line of annotation' '
	(
	cd repo &&
	git tag -n1 >out &&
	grep "v-multi" out &&
	grep "line one" out
	)
'

test_expect_success 'tag -l lists all tags' '
	(
	cd repo &&
	git tag -l >out &&
	grep "v1.0" out &&
	grep "v2.0" out &&
	grep "v-from-file" out
	)
'

test_expect_success 'tag -l with pattern filters tags' '
	(
	cd repo &&
	git tag -l "v1*" >out &&
	grep "v1.0" out &&
	! grep "v2.0" out
	)
'

# ── tag -f: force overwrite ─────────────────────────────────────────────────

test_expect_success 'tag without -f fails if tag exists' '
	(
	cd repo &&
	test_must_fail git tag -m "dup" v1.0 2>err
	)
'

test_expect_success 'tag -f overwrites existing tag' '
	(
	cd repo &&
	git rev-parse "v1.0^{commit}" >before &&
	sha=$(git rev-parse c1) &&
	git tag -f -m "updated v1.0" v1.0 "$sha" &&
	git rev-parse "v1.0^{commit}" >after &&
	echo "$sha" >expect &&
	test_cmp expect after
	)
'

test_expect_success 'tag -f updates message' '
	(
	cd repo &&
	git cat-file tag v1.0 >raw &&
	grep "updated v1.0" raw
	)
'

# ── lightweight vs annotated ─────────────────────────────────────────────────

test_expect_success 'tag without -m/-a creates lightweight tag' '
	(
	cd repo &&
	git tag v-light &&
	git cat-file -t v-light >type &&
	echo "commit" >expect &&
	test_cmp expect type
	)
'

test_expect_success 'lightweight tag points to commit directly' '
	(
	cd repo &&
	git rev-parse v-light >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'annotated tag dereferences to commit' '
	(
	cd repo &&
	git rev-parse "v2.0^{commit}" >actual &&
	git rev-parse HEAD >expect &&
	test_cmp expect actual
	)
'

# ── tag deletion ─────────────────────────────────────────────────────────────

test_expect_success 'tag -d deletes annotated tag' '
	(
	cd repo &&
	git tag -m "doomed" v-doomed &&
	git tag -d v-doomed &&
	test_must_fail git rev-parse v-doomed 2>err
	)
'

test_expect_success 'tag -d deletes lightweight tag' '
	(
	cd repo &&
	git tag v-light-doomed &&
	git tag -d v-light-doomed &&
	test_must_fail git rev-parse v-light-doomed 2>err
	)
'

test_done
