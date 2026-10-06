#!/bin/sh
# Tests for grit commit-tree with GIT_AUTHOR_* and GIT_COMMITTER_* env vars.

test_description='grit commit-tree respects author/committer environment variables'

REAL_GIT=$(command -v git)

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

###########################################################################
# Section 1: Setup
###########################################################################

test_expect_success 'setup: create repo with a tree' '
	(
	"$REAL_GIT" init repo &&
	cd repo &&
	"$REAL_GIT" config user.name "Default User" &&
	"$REAL_GIT" config user.email "default@example.com" &&
	echo "content" >file.txt &&
	mkdir -p sub &&
	echo "nested" >sub/file.txt &&
	"$REAL_GIT" add . &&
	"$REAL_GIT" commit -m "initial"
	)
'

###########################################################################
# Section 2: Basic commit-tree
###########################################################################

test_expect_success 'commit-tree creates a commit from tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(echo "test message" | grit commit-tree "$tree") &&
	echo "$commit" | grep -qE "^[0-9a-f]{40}$"
	)
'

test_expect_success 'commit-tree with -m flag' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(grit commit-tree "$tree" -m "message via flag") &&
	grit cat-file -p "$commit" >actual &&
	grep "message via flag" actual
	)
'

test_expect_success 'commit-tree creates valid commit object' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(grit commit-tree "$tree" -m "valid commit") &&
	grit cat-file -t "$commit" >actual &&
	echo "commit" >expect &&
	test_cmp expect actual
	)
'

test_expect_success 'commit-tree commit references correct tree' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(grit commit-tree "$tree" -m "tree ref test") &&
	grit cat-file -p "$commit" >actual &&
	grep "^tree $tree" actual
	)
'

###########################################################################
# Section 3: GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL
###########################################################################

test_expect_success 'commit-tree respects GIT_AUTHOR_NAME' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="Custom Author" \
		GIT_AUTHOR_EMAIL="custom@author.com" \
		GIT_AUTHOR_DATE="1234567890 +0000" \
		grit commit-tree "$tree" -m "custom author") &&
	grit cat-file -p "$commit" >actual &&
	grep "author Custom Author <custom@author.com>" actual
	)
'

test_expect_success 'commit-tree respects GIT_AUTHOR_EMAIL' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="Test" \
		GIT_AUTHOR_EMAIL="special@email.org" \
		GIT_AUTHOR_DATE="1234567890 +0000" \
		grit commit-tree "$tree" -m "custom email") &&
	grit cat-file -p "$commit" >actual &&
	grep "special@email.org" actual
	)
'

test_expect_success 'commit-tree respects GIT_AUTHOR_DATE' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="Test" \
		GIT_AUTHOR_EMAIL="t@t.com" \
		GIT_AUTHOR_DATE="1000000000 +0000" \
		grit commit-tree "$tree" -m "custom date") &&
	grit cat-file -p "$commit" >actual &&
	grep "author Test <t@t.com> 1000000000 +0000" actual
	)
'

test_expect_success 'commit-tree author env matches real git' '
	(
	cd repo &&
	tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	git_commit=$(GIT_AUTHOR_NAME="EnvAuth" \
		GIT_AUTHOR_EMAIL="env@auth.com" \
		GIT_AUTHOR_DATE="1111111111 +0000" \
		GIT_COMMITTER_NAME="EnvComm" \
		GIT_COMMITTER_EMAIL="env@comm.com" \
		GIT_COMMITTER_DATE="1111111111 +0000" \
		"$REAL_GIT" commit-tree "$tree" -m "env test") &&
	grit_commit=$(GIT_AUTHOR_NAME="EnvAuth" \
		GIT_AUTHOR_EMAIL="env@auth.com" \
		GIT_AUTHOR_DATE="1111111111 +0000" \
		GIT_COMMITTER_NAME="EnvComm" \
		GIT_COMMITTER_EMAIL="env@comm.com" \
		GIT_COMMITTER_DATE="1111111111 +0000" \
		grit commit-tree "$tree" -m "env test") &&
	"$REAL_GIT" cat-file -p "$git_commit" >expect &&
	grit cat-file -p "$grit_commit" >actual &&
	grep "^author " expect >expect_author &&
	grep "^author " actual >actual_author &&
	test_cmp expect_author actual_author
	)
'

###########################################################################
# Section 4: GIT_COMMITTER_NAME / GIT_COMMITTER_EMAIL
###########################################################################

test_expect_success 'commit-tree respects GIT_COMMITTER_NAME' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_COMMITTER_NAME="Custom Committer" \
		GIT_COMMITTER_EMAIL="custom@committer.com" \
		GIT_COMMITTER_DATE="1234567890 +0000" \
		grit commit-tree "$tree" -m "custom committer") &&
	grit cat-file -p "$commit" >actual &&
	grep "committer Custom Committer <custom@committer.com>" actual
	)
'

test_expect_success 'commit-tree respects GIT_COMMITTER_DATE' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_COMMITTER_NAME="Test" \
		GIT_COMMITTER_EMAIL="t@t.com" \
		GIT_COMMITTER_DATE="1500000000 +0000" \
		grit commit-tree "$tree" -m "committer date") &&
	grit cat-file -p "$commit" >actual &&
	grep "committer Test <t@t.com> 1500000000 +0000" actual
	)
'

test_expect_success 'commit-tree committer env matches real git' '
	(
	cd repo &&
	tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	git_commit=$(GIT_AUTHOR_NAME="A" \
		GIT_AUTHOR_EMAIL="a@a.com" \
		GIT_AUTHOR_DATE="1111111111 +0000" \
		GIT_COMMITTER_NAME="CommTest" \
		GIT_COMMITTER_EMAIL="comm@test.com" \
		GIT_COMMITTER_DATE="1111111111 +0000" \
		"$REAL_GIT" commit-tree "$tree" -m "comm test") &&
	grit_commit=$(GIT_AUTHOR_NAME="A" \
		GIT_AUTHOR_EMAIL="a@a.com" \
		GIT_AUTHOR_DATE="1111111111 +0000" \
		GIT_COMMITTER_NAME="CommTest" \
		GIT_COMMITTER_EMAIL="comm@test.com" \
		GIT_COMMITTER_DATE="1111111111 +0000" \
		grit commit-tree "$tree" -m "comm test") &&
	"$REAL_GIT" cat-file -p "$git_commit" >expect &&
	grit cat-file -p "$grit_commit" >actual &&
	grep "^committer " expect >expect_comm &&
	grep "^committer " actual >actual_comm &&
	test_cmp expect_comm actual_comm
	)
'

###########################################################################
# Section 5: Author != Committer
###########################################################################

test_expect_success 'commit-tree can have different author and committer' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="Alice Author" \
		GIT_AUTHOR_EMAIL="alice@example.com" \
		GIT_AUTHOR_DATE="1200000000 +0000" \
		GIT_COMMITTER_NAME="Bob Committer" \
		GIT_COMMITTER_EMAIL="bob@example.com" \
		GIT_COMMITTER_DATE="1200000000 +0000" \
		grit commit-tree "$tree" -m "split identity") &&
	grit cat-file -p "$commit" >actual &&
	grep "author Alice Author <alice@example.com>" actual &&
	grep "committer Bob Committer <bob@example.com>" actual
	)
'

test_expect_success 'commit-tree different author/committer matches real git' '
	(
	cd repo &&
	tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	git_c=$(GIT_AUTHOR_NAME="Auth" GIT_AUTHOR_EMAIL="auth@x.com" \
		GIT_AUTHOR_DATE="1300000000 +0000" \
		GIT_COMMITTER_NAME="Comm" GIT_COMMITTER_EMAIL="comm@x.com" \
		GIT_COMMITTER_DATE="1300000000 +0000" \
		"$REAL_GIT" commit-tree "$tree" -m "split") &&
	grit_c=$(GIT_AUTHOR_NAME="Auth" GIT_AUTHOR_EMAIL="auth@x.com" \
		GIT_AUTHOR_DATE="1300000000 +0000" \
		GIT_COMMITTER_NAME="Comm" GIT_COMMITTER_EMAIL="comm@x.com" \
		GIT_COMMITTER_DATE="1300000000 +0000" \
		grit commit-tree "$tree" -m "split") &&
	"$REAL_GIT" cat-file -p "$git_c" | grep -E "^(author|committer) " >expect &&
	grit cat-file -p "$grit_c" | grep -E "^(author|committer) " >actual &&
	test_cmp expect actual
	)
'

###########################################################################
# Section 6: Parent commits
###########################################################################

test_expect_success 'commit-tree with -p parent' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent=$(grit rev-parse HEAD) &&
	commit=$(grit commit-tree "$tree" -p "$parent" -m "with parent") &&
	grit cat-file -p "$commit" >actual &&
	grep "^parent $parent" actual
	)
'

test_expect_success 'commit-tree with multiple parents' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	parent1=$(grit rev-parse HEAD) &&
	parent2=$(grit commit-tree "$tree" -m "other parent") &&
	merge=$(grit commit-tree "$tree" -p "$parent1" -p "$parent2" -m "merge") &&
	grit cat-file -p "$merge" >actual &&
	grep "^parent $parent1" actual &&
	grep "^parent $parent2" actual
	)
'

test_expect_success 'commit-tree no parent has no parent line' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(grit commit-tree "$tree" -m "orphan") &&
	grit cat-file -p "$commit" >actual &&
	! grep "^parent " actual
	)
'

###########################################################################
# Section 7: Message from stdin
###########################################################################

test_expect_success 'commit-tree reads message from stdin' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(echo "stdin message" | grit commit-tree "$tree") &&
	grit cat-file -p "$commit" >actual &&
	grep "stdin message" actual
	)
'

test_expect_success 'commit-tree multiline stdin message' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(printf "line one\nline two\nline three" | grit commit-tree "$tree") &&
	grit cat-file -p "$commit" >actual &&
	grep "line one" actual &&
	grep "line two" actual &&
	grep "line three" actual
	)
'

###########################################################################
# Section 8: Message from file
###########################################################################

test_expect_success 'commit-tree -F reads message from file' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	echo "file message" >msg.txt &&
	commit=$(grit commit-tree "$tree" -F msg.txt) &&
	grit cat-file -p "$commit" >actual &&
	grep "file message" actual
	)
'

test_expect_success 'commit-tree -F with multiline file' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	printf "subject\n\nbody paragraph" >multi-msg.txt &&
	commit=$(grit commit-tree "$tree" -F multi-msg.txt) &&
	grit cat-file -p "$commit" >actual &&
	grep "subject" actual &&
	grep "body paragraph" actual
	)
'

###########################################################################
# Section 9: Timezone in dates
###########################################################################

test_expect_success 'commit-tree preserves positive timezone offset' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="TZ" GIT_AUTHOR_EMAIL="tz@test.com" \
		GIT_AUTHOR_DATE="1234567890 +0530" \
		grit commit-tree "$tree" -m "tz test") &&
	grit cat-file -p "$commit" >actual &&
	grep "+0530" actual
	)
'

test_expect_success 'commit-tree preserves negative timezone offset' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	commit=$(GIT_AUTHOR_NAME="TZ" GIT_AUTHOR_EMAIL="tz@test.com" \
		GIT_AUTHOR_DATE="1234567890 -0800" \
		grit commit-tree "$tree" -m "neg tz") &&
	grit cat-file -p "$commit" >actual &&
	grep "\-0800" actual
	)
'

###########################################################################
# Section 10: Hash stability
###########################################################################

test_expect_success 'commit-tree same input produces same hash' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	c1=$(GIT_AUTHOR_NAME="Same" GIT_AUTHOR_EMAIL="same@same.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="Same" GIT_COMMITTER_EMAIL="same@same.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		grit commit-tree "$tree" -m "deterministic") &&
	c2=$(GIT_AUTHOR_NAME="Same" GIT_AUTHOR_EMAIL="same@same.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="Same" GIT_COMMITTER_EMAIL="same@same.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		grit commit-tree "$tree" -m "deterministic") &&
	test "$c1" = "$c2"
	)
'

test_expect_success 'commit-tree same input matches real git hash' '
	(
	cd repo &&
	tree=$("$REAL_GIT" rev-parse HEAD^{tree}) &&
	c_grit=$(GIT_AUTHOR_NAME="HashTest" GIT_AUTHOR_EMAIL="hash@test.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="HashTest" GIT_COMMITTER_EMAIL="hash@test.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		grit commit-tree "$tree" -m "hash check") &&
	c_git=$(GIT_AUTHOR_NAME="HashTest" GIT_AUTHOR_EMAIL="hash@test.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="HashTest" GIT_COMMITTER_EMAIL="hash@test.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		"$REAL_GIT" commit-tree "$tree" -m "hash check") &&
	test "$c_grit" = "$c_git"
	)
'

test_expect_success 'commit-tree different message produces different hash' '
	(
	cd repo &&
	tree=$(grit rev-parse HEAD^{tree}) &&
	c1=$(GIT_AUTHOR_NAME="D" GIT_AUTHOR_EMAIL="d@d.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="D" GIT_COMMITTER_EMAIL="d@d.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		grit commit-tree "$tree" -m "message A") &&
	c2=$(GIT_AUTHOR_NAME="D" GIT_AUTHOR_EMAIL="d@d.com" \
		GIT_AUTHOR_DATE="1400000000 +0000" \
		GIT_COMMITTER_NAME="D" GIT_COMMITTER_EMAIL="d@d.com" \
		GIT_COMMITTER_DATE="1400000000 +0000" \
		grit commit-tree "$tree" -m "message B") &&
	test "$c1" != "$c2"
	)
'

test_done
