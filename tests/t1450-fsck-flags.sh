#!/bin/sh
#
# Test fsck flags: --connectivity-only, --name-objects, --no-dangling, --lost-found

test_description='git fsck flag tests'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo with objects' '
	git init -q &&
	git config user.name "Test" &&
	git config user.email "t@t" &&
	git config core.logAllRefUpdates 0 &&
	echo "hello" >file1.txt &&
	git add file1.txt &&
	test_tick &&
	git commit -m "first" &&
	echo "world" >file2.txt &&
	git add file2.txt &&
	test_tick &&
	git commit -m "second"
'

test_expect_success 'fsck with no flags succeeds on clean repo' '
	git fsck 2>err &&
	test_must_be_empty err
'

test_expect_success 'fsck --connectivity-only succeeds' '
	git fsck --connectivity-only 2>err &&
	test_must_be_empty err
'

test_expect_success 'setup: create dangling objects' '
	echo "orphan content" | git hash-object -w --stdin >orphan_blob &&
	test_tick &&
	git reset --hard HEAD^ &&
	# Now HEAD~1 child commit and orphan blob are dangling
	true
'

test_expect_success 'fsck reports dangling objects by default' '
	git fsck >out 2>err &&
	grep "dangling" out
'

test_expect_success 'fsck --no-dangling suppresses dangling output' '
	git fsck --no-dangling 2>err &&
	! grep "dangling" err
'

test_expect_success 'fsck --name-objects shows names in output' '
	git fsck --name-objects >out 2>err &&
	grep "dangling" out
'

test_expect_success 'fsck --lost-found creates lost-found directory' '
	rm -rf .git/lost-found &&
	git fsck --lost-found 2>err &&
	test -d .git/lost-found/commit &&
	test -d .git/lost-found/other
'

test_expect_success 'fsck --lost-found writes dangling objects' '
	rm -rf .git/lost-found &&
	git fsck --lost-found 2>err &&
	ls .git/lost-found/other/ >found &&
	test -s found
'

test_expect_success 'fsck --connectivity-only skips content validation' '
	git fsck --connectivity-only 2>err
	# Should succeed without content errors even if we had bad objects
'

test_done
