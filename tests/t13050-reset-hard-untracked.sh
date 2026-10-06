#!/bin/sh

test_description='grit reset with --soft, --mixed, --hard, --quiet, paths, and edge cases'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	grit init repo && cd repo &&
	git config user.email "t@t.com" && git config user.name "T" &&
	echo hello >file.txt && grit add file.txt && grit commit -m "first" &&
	echo world >file2.txt && grit add file2.txt && grit commit -m "second" &&
	echo third >file3.txt && grit add file3.txt && grit commit -m "third"
	)
'

test_expect_success 'reset --soft moves HEAD but keeps index and working tree' '
	(cd repo &&
	 before=$(git rev-parse HEAD~1) &&
	 grit reset --soft HEAD~1 &&
	 git rev-parse HEAD >../actual &&
	 echo "$before" >../expect) &&
	test_cmp expect actual
'

test_expect_success 'reset --soft keeps staged changes' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "file3.txt" actual
'

test_expect_success 'reset --soft keeps working tree intact' '
	test -f repo/file3.txt &&
	test "$(cat repo/file3.txt)" = "third"
'

test_expect_success 'commit after soft reset re-commits' '
	(cd repo && grit commit -m "recommit third" &&
	 grit ls-files --cached >../actual) &&
	grep "file3.txt" actual
'

test_expect_success 'reset --mixed (default) resets index but keeps working tree' '
	(cd repo &&
	 echo changed >file.txt && grit add file.txt &&
	 grit reset HEAD &&
	 grit status --porcelain >../actual) &&
	grep "^ M file.txt" actual || grep "M  file.txt" actual
'

test_expect_success 'reset --mixed keeps working tree file' '
	(cd repo && cat file.txt >../actual) &&
	echo "changed" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard resets index and working tree' '
	(cd repo &&
	 echo dirty >file.txt &&
	 grit reset --hard HEAD &&
	 cat file.txt >../actual) &&
	echo "hello" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard removes staged changes' '
	(cd repo && grit status --porcelain >../actual) &&
	! grep "file.txt" actual
'

test_expect_success 'reset --hard to previous commit' '
	(cd repo &&
	 grit reset --hard HEAD~1 &&
	 grit ls-files --cached >../actual) &&
	! grep "file3.txt" actual &&
	grep "file2.txt" actual
'

test_expect_success 'reset --hard does not remove untracked files' '
	(cd repo &&
	 echo untracked >untracked.txt &&
	 grit reset --hard HEAD) &&
	test -f repo/untracked.txt
'

test_expect_success 'reset --hard removes modified tracked files changes' '
	(cd repo &&
	 echo dirty >file.txt && echo dirty >file2.txt &&
	 grit reset --hard HEAD &&
	 cat file.txt >../actual1 && cat file2.txt >../actual2) &&
	echo "hello" >expect1 &&
	echo "world" >expect2 &&
	test_cmp expect1 actual1 &&
	test_cmp expect2 actual2
'

test_expect_success 'reset --quiet suppresses output' '
	(cd repo &&
	 echo changed >file.txt && grit add file.txt &&
	 grit reset --quiet HEAD >../actual 2>&1) &&
	! test -s actual
'

test_expect_success 'reset --quiet still resets' '
	(cd repo && grit status --porcelain >../actual) &&
	grep "^ M file.txt" actual || grep "M  file.txt" actual
'

test_expect_success 'reset with path unstages specific file' '
	(cd repo &&
	 echo a >a.txt && echo b >b.txt &&
	 grit add a.txt b.txt &&
	 grit reset -- a.txt &&
	 grit ls-files --cached >../actual) &&
	! grep "^a.txt$" actual &&
	grep "b.txt" actual
'

test_expect_success 'reset path does not modify working tree' '
	test -f repo/a.txt &&
	test "$(cat repo/a.txt)" = "a"
'

test_expect_success 'reset to specific commit hash' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 echo v2 >file.txt && grit add file.txt && grit commit -m "v2" &&
	 echo v3 >file.txt && grit add file.txt && grit commit -m "v3" &&
	 target=$(git rev-parse HEAD~1) &&
	 grit reset --hard "$target" &&
	 cat file.txt >../actual) &&
	echo "v2" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --soft preserves untracked files' '
	(cd repo &&
	 echo ut >ut.txt &&
	 grit reset --soft HEAD~1) &&
	test -f repo/ut.txt
'

test_expect_success 'reset --mixed preserves untracked files' '
	(cd repo &&
	 grit reset HEAD) &&
	test -f repo/ut.txt
'

test_expect_success 'reset --hard after adding new file' '
	(cd repo &&
	 echo new >newfile.txt && grit add newfile.txt &&
	 grit reset --hard HEAD &&
	 grit ls-files --cached >../actual) &&
	! grep "newfile.txt" actual
'

test_expect_success 'reset --hard removes newly added file from working tree' '
	! test -f repo/newfile.txt
'

test_expect_success 'reset --hard with deleted file restores it' '
	(cd repo &&
	 rm file2.txt &&
	 grit reset --hard HEAD) &&
	test -f repo/file2.txt &&
	test "$(cat repo/file2.txt)" = "world"
'

test_expect_success 'reset multiple paths' '
	(cd repo &&
	 echo m1 >m1.txt && echo m2 >m2.txt && echo m3 >m3.txt &&
	 grit add m1.txt m2.txt m3.txt &&
	 grit reset -- m1.txt m3.txt &&
	 grit status --porcelain >../actual) &&
	grep "m2.txt" actual
'

test_expect_success 'reset --soft then amend-style recommit' '
	(cd repo &&
	 grit reset --hard HEAD && grit add . && grit commit -m "clean" &&
	 echo amended >file.txt && grit add file.txt && grit commit -m "to amend" &&
	 grit reset --soft HEAD~1 &&
	 grit commit -m "amended version" &&
	 cat file.txt >../actual) &&
	echo "amended" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --hard to first commit' '
	(cd repo &&
	 first=$(git rev-list --reverse HEAD | head -1) &&
	 grit reset --hard "$first" &&
	 grit ls-files --cached >../actual) &&
	grep "file.txt" actual
'

test_expect_success 'reset with subdirectory path' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 mkdir -p sub && echo sf >sub/sf.txt &&
	 grit add sub/sf.txt && grit commit -m "subdir" &&
	 echo dirty >sub/sf.txt && grit add sub/sf.txt &&
	 grit reset -- sub/sf.txt &&
	 grit status --porcelain >../actual) &&
	grep "sub/sf.txt" actual
'

test_expect_success 'reset --hard cleans modified subdirectory files' '
	(cd repo &&
	 echo dirty >sub/sf.txt &&
	 grit reset --hard HEAD &&
	 cat sub/sf.txt >../actual) &&
	echo "sf" >expect &&
	test_cmp expect actual
'

test_expect_success 'reset --quiet --hard combined' '
	(cd repo &&
	 echo dirty >file.txt &&
	 grit reset --quiet --hard HEAD >../actual 2>&1) &&
	! test -s actual &&
	test "$(cat repo/file.txt)" = "hello"
'

test_expect_success 'reset to HEAD is no-op for clean working tree' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 grit diff --name-only >../actual) &&
	! test -s actual
'

test_expect_success 'reset --hard preserves git config' '
	(cd repo &&
	 grit reset --hard HEAD &&
	 git config user.email >../actual) &&
	echo "t@t.com" >expect &&
	test_cmp expect actual
'

test_done
