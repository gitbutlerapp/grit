#!/bin/sh
# Tests for format-patch extended options:
#   --base, --signoff/-s, --in-reply-to, --cc, --to, --attach, --inline, -k

test_description='format-patch extended options (base, signoff, threading, MIME, keep-subject)'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup' '
	(
	git init repo &&
	cd repo &&
	echo "line 1" >file &&
	git add file &&
	test_tick &&
	git commit -m "Initial commit" &&
	echo "line 2" >>file &&
	git add file &&
	test_tick &&
	git commit -m "Second commit" &&
	echo "line 3" >>file &&
	git add file &&
	test_tick &&
	git commit -m "Third commit"
	)
'

test_expect_success 'format-patch --base=<commit> adds base-commit info' '
	(
	cd repo &&
	git format-patch --base=HEAD~2 --stdout -- -1 >patch &&
	grep "^base-commit:" patch
	)
'

test_expect_success 'format-patch --base with multi-patch puts base-commit on last patch only' '
	(
	cd repo &&
	git format-patch --base=HEAD~2 --stdout HEAD~2..HEAD >patches &&
	# base-commit should appear exactly once
	test $(grep -c "^base-commit:" patches) = 1
	)
'

test_expect_success 'format-patch -s adds Signed-off-by' '
	(
	cd repo &&
	git format-patch -s --stdout -- -1 >patch &&
	grep "^Signed-off-by:" patch
	)
'

test_expect_success 'format-patch --signoff adds Signed-off-by' '
	(
	cd repo &&
	git format-patch --signoff --stdout -- -1 >patch &&
	grep "^Signed-off-by:" patch
	)
'

test_expect_success 'format-patch --in-reply-to adds In-Reply-To and References' '
	(
	cd repo &&
	git format-patch --in-reply-to="<abc123@example.com>" --stdout -- -1 >patch &&
	grep "^In-Reply-To: <abc123@example.com>" patch &&
	grep "^References: <abc123@example.com>" patch
	)
'

test_expect_success 'format-patch --cc adds Cc header' '
	(
	cd repo &&
	git format-patch --cc="alice@example.com" --stdout -- -1 >patch &&
	grep "^Cc: alice@example.com" patch
	)
'

test_expect_success 'format-patch --to adds To header' '
	(
	cd repo &&
	git format-patch --to="bob@example.com" --stdout -- -1 >patch &&
	grep "^To: bob@example.com" patch
	)
'

test_expect_success 'format-patch --cc and --to can be combined' '
	(
	cd repo &&
	git format-patch --cc="alice@example.com" --to="bob@example.com" \
		--stdout -- -1 >patch &&
	grep "^Cc: alice@example.com" patch &&
	grep "^To: bob@example.com" patch
	)
'

test_expect_success 'format-patch --attach produces MIME attachment' '
	(
	cd repo &&
	git format-patch --attach --stdout -- -1 >patch &&
	grep "^MIME-Version: 1.0" patch &&
	grep "Content-Type: multipart/mixed" patch &&
	grep "Content-Disposition: attachment" patch
	)
'

test_expect_success 'format-patch --inline produces MIME inline' '
	(
	cd repo &&
	git format-patch --inline --stdout -- -1 >patch &&
	grep "^MIME-Version: 1.0" patch &&
	grep "Content-Type: multipart/mixed" patch &&
	grep "Content-Disposition: inline" patch
	)
'

test_expect_success 'format-patch -k keeps subject without [PATCH] prefix' '
	(
	cd repo &&
	git format-patch -k --stdout -- -1 >patch &&
	grep "^Subject: Third commit" patch &&
	! grep "\\[PATCH\\]" patch
	)
'

test_done
