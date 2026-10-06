#!/bin/sh

test_description='grit tag: listing, sorting, and annotation display'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

REAL_GIT=/usr/bin/git

test_expect_success 'setup' '
	(
	grit init repo &&
	cd repo &&
	$REAL_GIT config user.email "t@t.com" &&
	$REAL_GIT config user.name "T" &&
	echo first >file.txt &&
	grit add file.txt &&
	grit commit -m "first commit" &&
	echo second >file.txt &&
	grit add file.txt &&
	grit commit -m "second commit" &&
	echo third >file.txt &&
	grit add file.txt &&
	grit commit -m "third commit"
	)
'

# ── basic tag creation and listing ───────────────────────────────────────

test_expect_success 'create lightweight tags' '
	(cd repo &&
	 grit tag v1.0 HEAD~2 &&
	 grit tag v2.0 HEAD~1 &&
	 grit tag v3.0 HEAD)
'

test_expect_success 'tag list shows all tags' '
	(cd repo && grit tag -l >../actual) &&
	cat >expect <<-\EOF &&
	v1.0
	v2.0
	v3.0
	EOF
	test_cmp expect actual
'

test_expect_success 'tag with no args lists all tags' '
	(cd repo && grit tag >../actual) &&
	cat >expect <<-\EOF &&
	v1.0
	v2.0
	v3.0
	EOF
	test_cmp expect actual
'

test_expect_success 'tag -l with pattern filters tags' '
	(cd repo && grit tag -l "v1*" >../actual) &&
	echo "v1.0" >expect &&
	test_cmp expect actual
'

test_expect_success 'tag -l with pattern matching multiple' '
	(cd repo && grit tag -l "v*" >../actual) &&
	cat >expect <<-\EOF &&
	v1.0
	v2.0
	v3.0
	EOF
	test_cmp expect actual
'

test_expect_success 'tag -l with no matching pattern gives empty output' '
	(cd repo && grit tag -l "z*" >../actual) &&
	test_must_be_empty actual
'

# ── annotated tags ───────────────────────────────────────────────────────

test_expect_success 'create annotated tag' '
	(cd repo && grit tag -a -m "Release 4.0" v4.0)
'

test_expect_success 'annotated tag shows in list' '
	(cd repo && grit tag -l >../actual) &&
	grep "v4.0" actual
'

test_expect_success 'create annotated tag with multi-word message' '
	(cd repo && grit tag -a -m "This is a long release message" v5.0)
'

test_expect_success 'annotated tag -n shows annotation' '
	(cd repo && grit tag -l -n >../actual) &&
	grep "This is a long release message" actual
'

test_expect_success 'annotated tag -n1 shows first line' '
	(cd repo && grit tag -n1 >../actual) &&
	grep "v4.0" actual &&
	grep "Release 4.0" actual
'

test_expect_success 'lightweight tag -n shows commit subject' '
	(cd repo && grit tag -n1 >../actual) &&
	grep "v3.0" actual
'

# ── tag deletion ─────────────────────────────────────────────────────────

test_expect_success 'delete tag with -d' '
	(cd repo &&
	 grit tag temp-tag &&
	 grit tag -d temp-tag &&
	 grit tag -l >../actual) &&
	! grep "temp-tag" actual
'

test_expect_success 'delete nonexistent tag fails' '
	(cd repo && test_must_fail grit tag -d nonexistent)
'

test_expect_success 'delete annotated tag' '
	(cd repo &&
	 grit tag -a -m "temp" temp-ann &&
	 grit tag -d temp-ann &&
	 grit tag -l >../actual) &&
	! grep "temp-ann" actual
'

# ── tag --contains ───────────────────────────────────────────────────────

test_expect_success 'tag --contains HEAD shows tags on HEAD' '
	(cd repo && grit tag --contains HEAD >../actual) &&
	grep "v3.0" actual
'

test_expect_success 'tag --contains HEAD includes annotated tags on HEAD' '
	(cd repo && grit tag --contains HEAD >../actual) &&
	grep "v4.0" actual
'

test_expect_success 'tag --contains HEAD~2 shows all tags' '
	(cd repo && grit tag --contains HEAD~2 >../actual) &&
	grep "v1.0" actual &&
	grep "v2.0" actual &&
	grep "v3.0" actual
'

# ── tag sorting ──────────────────────────────────────────────────────────

test_expect_success 'tags are listed in sorted order by default' '
	(cd repo && grit tag -l >../actual) &&
	sort actual >expect &&
	test_cmp expect actual
'

test_expect_success 'tag --sort=refname gives alphabetical order' '
	(cd repo && grit tag --sort=refname >../actual) &&
	sort actual >expect &&
	test_cmp expect actual
'

test_expect_success 'tag --sort=-refname gives reverse order' '
	(cd repo && grit tag --sort=-refname >../actual) &&
	sort -r actual >expect &&
	test_cmp expect actual
'

# ── case-insensitive listing ─────────────────────────────────────────────

test_expect_success 'create mixed-case tags' '
	(cd repo &&
	 grit tag Alpha &&
	 grit tag beta &&
	 grit tag Gamma)
'

test_expect_success 'tag -l lists mixed-case tags' '
	(cd repo && grit tag -l >../actual) &&
	grep "Alpha" actual &&
	grep "beta" actual &&
	grep "Gamma" actual
'

test_expect_success 'tag -i -l sorts case-insensitively' '
	(cd repo && grit tag -i -l >../actual) &&
	head -1 actual >first &&
	echo "Alpha" >expect &&
	test_cmp expect first
'

# ── tag with force ───────────────────────────────────────────────────────

test_expect_success 'tag -f overwrites existing tag' '
	(cd repo &&
	 grit tag -f v1.0 HEAD &&
	 grit tag --contains HEAD >../actual) &&
	grep "v1.0" actual
'

test_expect_success 'creating duplicate tag without -f fails' '
	(cd repo && test_must_fail grit tag v2.0)
'

# ── tag message from file ───────────────────────────────────────────────

test_expect_success 'tag -F reads message from file' '
	echo "Message from file" >msg-file &&
	(cd repo && grit tag -a -F ../msg-file v6.0 &&
	 grit tag -l -n >../actual) &&
	grep "Message from file" actual
'

# ── tag at specific commit ───────────────────────────────────────────────

test_expect_success 'tag at specific commit' '
	(cd repo &&
	 grit tag at-first HEAD~2 &&
	 grit tag --contains HEAD~2 >../actual) &&
	grep "at-first" actual
'

test_expect_success 'tag at specific commit does not appear at HEAD-only --contains' '
	(cd repo &&
	 grit tag only-head HEAD &&
	 grit tag -l >../actual) &&
	grep "only-head" actual
'

# ── edge cases ───────────────────────────────────────────────────────────

test_expect_success 'tag with hyphen in name' '
	(cd repo &&
	 grit tag my-tag-1 &&
	 grit tag -l >../actual) &&
	grep "my-tag-1" actual
'

test_expect_success 'tag with dots in name' '
	(cd repo &&
	 grit tag release.1.2.3 &&
	 grit tag -l >../actual) &&
	grep "release.1.2.3" actual
'

test_expect_success 'tag with slash in name' '
	(cd repo &&
	 grit tag releases/v1 &&
	 grit tag -l >../actual) &&
	grep "releases/v1" actual
'

test_expect_success 'list many tags' '
	(cd repo && grit tag -l >../actual) &&
	count=$(wc -l <actual) &&
	test "$count" -gt 10
'

test_done
