#!/bin/sh
# Ported from git/t/t7101-reset-empty-subdirs.sh (upstream)
# git reset should cull empty subdirs

test_description='git reset should cull empty subdirs'

cd "$(dirname "$0")" || exit 1
. ./test-lib.sh

test_expect_success 'setup: init repo' '
	git init -q &&
	git config user.email "test@example.com" &&
	git config user.name "Test User"
'

test_expect_success 'creating initial files' '
	mkdir -p path0 &&
	echo "test content" >path0/COPYING &&
	git add path0/COPYING &&
	git commit -m add
'

test_expect_success 'creating second files' '
	mkdir -p path1/path2 &&
	echo "test content 2" >path1/path2/COPYING &&
	echo "test content 3" >path1/COPYING &&
	echo "test content 4" >COPYING &&
	echo "test content 5" >path0/COPYING-TOO &&
	git add path1/path2/COPYING path1/COPYING COPYING path0/COPYING-TOO &&
	git commit -m change
'

test_expect_success 'resetting tree HEAD^' '
	git reset --hard HEAD^
'

test_expect_success 'checking initial files exist after rewind' '
	test_path_is_dir path0 &&
	test_path_is_file path0/COPYING
'

test_expect_success 'checking lack of path1/path2/COPYING' '
	test_path_is_missing path1/path2/COPYING
'

test_expect_success 'checking lack of path1/COPYING' '
	test_path_is_missing path1/COPYING
'

test_expect_success 'checking lack of COPYING' '
	test_path_is_missing COPYING
'

test_expect_success 'checking lack of path0/COPYING-TOO' '
	test_path_is_missing path0/COPYING-TOO
'

test_expect_success 'checking lack of path1/path2' '
	test_path_is_missing path1/path2
'

test_expect_success 'checking lack of path1' '
	test_path_is_missing path1
'

test_done
