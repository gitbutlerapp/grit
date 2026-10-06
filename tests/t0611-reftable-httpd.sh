#!/bin/sh

test_description='reftable HTTPD tests'

. ./test-lib.sh
. "$TEST_DIRECTORY"/lib-httpd.sh

start_httpd

REPO="$HTTPD_DOCUMENT_ROOT_PATH/repo.git"
SRC_REPO="$TRASH_DIRECTORY/src-repo"

test_expect_success 'serving ls-remote' '
	mkdir -p "$SRC_REPO" &&
	git init --ref-format=reftable -b main "$SRC_REPO" &&
	cd "$SRC_REPO" &&
	test_commit m1 &&
	cd "$TRASH_DIRECTORY" &&
	git clone --bare "$SRC_REPO" "$REPO" &&
	git ls-remote "http://127.0.0.1:$LIB_HTTPD_PORT/smart/repo.git" | cut -f 2-2 -d "$(printf "\t")" >actual &&
	cat >expect <<-EOF &&
	HEAD
	refs/heads/main
	refs/tags/m1
	EOF
	test_cmp actual expect
'

test_done
