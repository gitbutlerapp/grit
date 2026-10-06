#!/bin/sh

test_description='tag: -f force replace, annotated tags, listing, deletion, sorting'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup repo with commits' '
    grit init repo &&
    (cd repo &&
     grit config user.email "t@t.com" &&
     grit config user.name "T" &&
     echo hello >file.txt &&
     grit add file.txt &&
     grit commit -m "first" &&
     echo world >>file.txt &&
     grit add file.txt &&
     grit commit -m "second" &&
     echo extra >>file.txt &&
     grit add file.txt &&
     grit commit -m "third")
'

test_expect_success 'create lightweight tag' '
    (cd repo && grit tag v1.0)
'

test_expect_success 'tag points to HEAD' '
    (cd repo && grit rev-parse v1.0 >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'tag list shows created tag' '
    (cd repo && grit tag -l >../actual) &&
    grep "v1.0" actual
'

test_expect_success 'create tag at specific commit' '
    (cd repo && first=$(grit rev-parse HEAD~2) &&
     grit tag v0.1 "$first") &&
    (cd repo && grit rev-parse v0.1 >../actual) &&
    (cd repo && grit rev-parse HEAD~2 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'tag without -f on existing tag fails' '
    (cd repo && test_must_fail grit tag v1.0 2>../err) &&
    grep -i "already exists\|fatal" err
'

test_expect_success 'tag -f replaces existing tag' '
    (cd repo &&
     old=$(grit rev-parse v0.1) &&
     grit tag -f v0.1 HEAD &&
     new=$(grit rev-parse v0.1) &&
     test "$old" != "$new")
'

test_expect_success 'tag -f points to new target' '
    (cd repo && grit rev-parse v0.1 >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'create annotated tag with -m' '
    (cd repo && grit tag -m "release 2.0" v2.0) &&
    (cd repo && grit tag -l >../actual) &&
    grep "v2.0" actual
'

test_expect_success 'annotated tag dereferences to commit' '
    (cd repo && grit rev-parse "v2.0^{commit}" >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'create annotated tag with -a -m' '
    (cd repo && grit tag -a -m "annotated release" v2.1) &&
    (cd repo && grit tag -l >../actual) &&
    grep "v2.1" actual
'

test_expect_success 'tag -f replaces annotated tag with lightweight' '
    (cd repo && grit tag -f v2.1 HEAD~1) &&
    (cd repo && grit rev-parse v2.1 >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_expect_success 'tag -f replaces lightweight with annotated' '
    (cd repo && grit tag -f -m "now annotated" v0.1) &&
    (cd repo && grit rev-parse "v0.1^{commit}" >../actual) &&
    (cd repo && grit rev-parse HEAD >../expect) &&
    test_cmp expect actual
'

test_expect_success 'tag -d deletes tag' '
    (cd repo && grit tag delme &&
     grit tag -d delme) &&
    (cd repo && grit tag -l >../actual) &&
    ! grep "delme" actual
'

test_expect_success 'tag -d on nonexistent tag fails' '
    (cd repo && test_must_fail grit tag -d nonexistent 2>../err)
'

test_expect_success 'tag -l lists all tags' '
    (cd repo && grit tag -l >../actual) &&
    grep "v0.1" actual &&
    grep "v1.0" actual &&
    grep "v2.0" actual
'

test_expect_success 'tag -l with pattern filters' '
    (cd repo && grit tag -l "v2*" >../actual) &&
    grep "v2.0" actual &&
    grep "v2.1" actual &&
    ! grep "v1.0" actual &&
    ! grep "v0.1" actual
'

test_expect_success 'tag -l with pattern matches single tag' '
    (cd repo && grit tag -l "v1*" >../actual) &&
    echo "v1.0" >expect &&
    test_cmp expect actual
'

test_expect_success 'tag -l with no match returns empty' '
    (cd repo && grit tag -l "zzz*" >../actual) &&
    test_must_be_empty actual
'

test_expect_success 'create multiple tags for listing' '
    (cd repo &&
     grit tag alpha &&
     grit tag beta &&
     grit tag gamma)
'

test_expect_success 'tag list includes new tags' '
    (cd repo && grit tag -l >../actual) &&
    grep "alpha" actual &&
    grep "beta" actual &&
    grep "gamma" actual
'

test_expect_success 'tag -n shows annotation lines' '
    (cd repo && grit tag -n >../actual) &&
    grep "v2.0" actual | grep "release 2.0"
'

test_expect_success 'tag -f force replace annotated with new annotation' '
    (cd repo && grit tag -f -m "updated release" v2.0) &&
    (cd repo && grit tag -n >../actual) &&
    grep "v2.0" actual | grep "updated release"
'

test_expect_success 'tag -f on same commit changes tag object' '
    (cd repo &&
     grit tag -m "first annotation" same_commit_tag &&
     old=$(grit rev-parse same_commit_tag) &&
     grit tag -f -m "second annotation" same_commit_tag &&
     new=$(grit rev-parse same_commit_tag) &&
     test "$old" != "$new")
'

test_expect_success 'tag --contains lists tags containing HEAD' '
    (cd repo && grit tag --contains HEAD >../actual) &&
    grep "v1.0" actual &&
    grep "v2.0" actual
'

test_expect_success 'tag --contains with older commit lists more tags' '
    (cd repo && grit tag --contains HEAD~2 >../actual) &&
    grep "v0.1" actual
'

test_expect_success 'tag -d multiple tags (one at a time)' '
    (cd repo &&
     grit tag -d alpha &&
     grit tag -d beta &&
     grit tag -d gamma) &&
    (cd repo && grit tag -l >../actual) &&
    ! grep "alpha" actual &&
    ! grep "beta" actual &&
    ! grep "gamma" actual
'

test_expect_success 'tag with message from file' '
    echo "tag from file content" >msg_file &&
    (cd repo && grit tag -F ../msg_file v3.0) &&
    (cd repo && grit tag -n >../actual) &&
    grep "v3.0" actual | grep "tag from file"
'

test_expect_success 'tag -l pattern with question mark' '
    (cd repo && grit tag -l "v?.0" >../actual) &&
    grep "v1.0" actual &&
    grep "v2.0" actual &&
    grep "v3.0" actual
'

test_expect_success 'tag -f replaces tag from file with inline message' '
    (cd repo && grit tag -f -m "inline msg" v3.0) &&
    (cd repo && grit tag -n >../actual) &&
    grep "v3.0" actual | grep "inline msg"
'

test_expect_success 'tag count matches expected' '
    (cd repo && grit tag -l >../actual) &&
    count=$(wc -l <actual) &&
    test "$count" -gt 3
'

test_expect_success 'create tag and verify with show-ref' '
    (cd repo && grit tag showref_tag) &&
    (cd repo && grit show-ref --tags >../actual) &&
    grep "showref_tag" actual
'

test_expect_success 'tag -d then recreate same tag' '
    (cd repo &&
     grit tag -d showref_tag &&
     grit tag showref_tag HEAD~1) &&
    (cd repo && grit rev-parse showref_tag >../actual) &&
    (cd repo && grit rev-parse HEAD~1 >../expect) &&
    test_cmp expect actual
'

test_done
