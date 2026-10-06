#!/bin/sh
test_description='test smart pushing over http via http-backend'
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh
ROOT_PATH="$PWD"
. "$TEST_DIRECTORY"/lib-httpd.sh
start_httpd

test_expect_success 'setup remote repository' '
	cd "$ROOT_PATH" && mkdir test_repo && cd test_repo && git init &&
	: >path1 && git add path1 && test_tick && git commit -m initial && cd - &&
	git clone --bare test_repo test_repo.git &&
	cd test_repo.git && git config http.receivepack true &&
	git config core.logallrefupdates true &&
	ORIG_HEAD=$(git rev-parse --verify HEAD) && cd - &&
	mv test_repo.git "$HTTPD_DOCUMENT_ROOT_PATH"
'
setup_askpass_helper

test_expect_success 'clone remote repository' '
	rm -rf test_repo_clone &&
	git clone $HTTPD_URL/smart/test_repo.git test_repo_clone &&
	(cd test_repo_clone && git config push.default matching)
'
test_expect_success 'push to remote repository (standard)' '
	>"$HTTPD_ROOT_PATH"/access.log &&
	cd "$ROOT_PATH"/test_repo_clone &&
	: >path2 && git add path2 && test_tick && git commit -m path2 &&
	HEAD=$(git rev-parse --verify HEAD) &&
	GIT_TRACE_CURL=true git push -v -v 2>err &&
	! grep "Expect: 100-continue" err &&
	grep "POST git-receive-pack ([0-9]* bytes)" err &&
	(cd "$HTTPD_DOCUMENT_ROOT_PATH"/test_repo.git && test $HEAD = $(git rev-parse --verify HEAD))
'
test_expect_success 'used receive-pack service' '
	strip_access_log >log &&
	grep "GET  /smart/test_repo.git/info/refs?service=git-receive-pack HTTP/[0-9.]* 200" log &&
	grep "POST /smart/test_repo.git/git-receive-pack HTTP/[0-9.]* 200" log
'
test_expect_success 'push already up-to-date' '
	cd "$ROOT_PATH"/test_repo_clone && git push
'
test_expect_success 'create and delete remote branch' '
	cd "$ROOT_PATH"/test_repo_clone &&
	git checkout -b dev && : >path3 && git add path3 && test_tick && git commit -m dev &&
	git push origin dev && git push origin :dev &&
	test_must_fail git show-ref --verify refs/remotes/origin/dev
'
test_expect_success 'rejected update hook prints status' '
	test_hook --setup -C "$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git" update <<-\EOF &&
	exit 1
	EOF
	cd "$ROOT_PATH"/test_repo_clone && git checkout -b dev2 &&
	: >path4 && git add path4 && test_tick && git commit -m dev2 &&
	test_must_fail git push origin dev2 2>act &&
	grep "hook declined" act && grep "failed to push" act
'
rm -f "$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git/hooks/update"

test_expect_success 'push (chunked)' '
	cd "$ROOT_PATH"/test_repo_clone && git checkout main &&
	test_commit commit path3 && HEAD=$(git rev-parse --verify HEAD) &&
	test_config http.postbuffer 4 &&
	git push -v -v origin $BRANCH 2>err &&
	grep "POST git-receive-pack (chunked)" err &&
	(cd "$HTTPD_DOCUMENT_ROOT_PATH"/test_repo.git && test $HEAD = $(git rev-parse --verify HEAD))
'
test_expect_success 'push --atomic prevents branch creation, reports collateral' '
	d=$HTTPD_DOCUMENT_ROOT_PATH/atomic-branches.git &&
	git init --bare "$d" && git --git-dir="$d" config http.receivepack true &&
	up="$HTTPD_URL"/smart/atomic-branches.git &&
	cd "$ROOT_PATH"/test_repo_clone &&
	test_commit atomic1 && test_commit atomic2 &&
	git branch collateral && git branch other &&
	git push "$up" atomic1 main collateral other && git tag -d atomic1 &&
	git checkout collateral && test_commit collateral1 &&
	git checkout main && git reset --hard HEAD^ && git branch atomic &&
	test_must_fail git push --atomic "$up" main atomic collateral 2>output &&
	test_must_fail git -C "$d" show-ref --verify refs/heads/atomic &&
	git rev-parse atomic2 >expected &&
	git -C "$d" rev-parse refs/heads/main >actual && test_cmp expected actual &&
	git -C "$d" rev-parse refs/heads/collateral >actual && test_cmp expected actual &&
	grep "^ ! .*rejected.* main -> main" output &&
	grep "^ ! .*rejected.* atomic -> atomic .*atomic push failed" output &&
	grep "^ ! .*rejected.* collateral -> collateral .*atomic push failed" output
'
test_expect_success 'push --atomic fails on server-side errors' '
	d=$HTTPD_DOCUMENT_ROOT_PATH/atomic-branches.git &&
	git --git-dir="$d" config http.receivepack true &&
	up="$HTTPD_URL"/smart/atomic-branches.git &&
	cd "$ROOT_PATH"/test_repo_clone &&
	git -C "$d" update-ref -d refs/heads/other &&
	git -C "$d" update-ref refs/heads/other/conflict HEAD &&
	git branch -f other collateral &&
	test_must_fail git push --atomic "$up" atomic other 2>output &&
	test_must_fail git -C "$d" show-ref --verify refs/heads/atomic &&
	test_must_fail git -C "$d" show-ref --verify refs/heads/other &&
	grep "^ ! .*rejected.* other -> other .*atomic transaction failed" output &&
	grep "^ ! .*rejected.* atomic -> atomic .*atomic transaction failed" output
'
test_expect_success 'push --all can push to empty repo' '
	cd "$ROOT_PATH"/test_repo_clone &&
	d=$HTTPD_DOCUMENT_ROOT_PATH/empty-all.git &&
	git init --bare "$d" && git --git-dir="$d" config http.receivepack true &&
	git push --all "$HTTPD_URL"/smart/empty-all.git
'
test_expect_success 'push --mirror can push to empty repo' '
	cd "$ROOT_PATH"/test_repo_clone &&
	d=$HTTPD_DOCUMENT_ROOT_PATH/empty-mirror.git &&
	git init --bare "$d" && git --git-dir="$d" config http.receivepack true &&
	git push --mirror "$HTTPD_URL"/smart/empty-mirror.git
'
test_expect_success 'push --all to repo with alternates' '
	cd "$ROOT_PATH"/test_repo_clone &&
	s=$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git && d=$HTTPD_DOCUMENT_ROOT_PATH/alternates-all.git &&
	git clone --bare --shared "$s" "$d" && git --git-dir="$d" config http.receivepack true &&
	git --git-dir="$d" repack -adl && git push --all "$HTTPD_URL"/smart/alternates-all.git
'
test_expect_success 'push --mirror to repo with alternates' '
	cd "$ROOT_PATH"/test_repo_clone &&
	s=$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git && d=$HTTPD_DOCUMENT_ROOT_PATH/alternates-mirror.git &&
	git clone --bare --shared "$s" "$d" && git --git-dir="$d" config http.receivepack true &&
	git --git-dir="$d" repack -adl && git push --mirror "$HTTPD_URL"/smart/alternates-mirror.git
'
test_expect_success 'push --progress shows progress to non-tty' '
	cd "$ROOT_PATH"/test_repo_clone && test_commit progress &&
	git push --progress >output 2>&1 &&
	test_grep "^To http" output && test_grep "^Writing objects" output
'
test_expect_success 'http push updates reflog' '
	cd "$ROOT_PATH"/test_repo_clone && test_commit reflog-test &&
	git push "$HTTPD_URL"/smart/test_repo.git &&
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git" log -g -1 --format="%gn" >actual &&
	test -s actual
'
test_expect_success 'push over smart http with auth' '
	cd "$ROOT_PATH/test_repo_clone" && echo push-auth-test >expect &&
	test_commit push-auth-test && set_askpass user@host pass@host &&
	git push "$HTTPD_URL"/auth/smart/test_repo.git &&
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git" log -1 --format=%s >actual &&
	expect_askpass both user%40host && test_cmp expect actual
'
test_expect_success 'push to auth-only-for-push repo' '
	cd "$ROOT_PATH/test_repo_clone" && echo push-half-auth >expect &&
	test_commit push-half-auth && set_askpass user@host pass@host &&
	git push "$HTTPD_URL"/auth-push/smart/test_repo.git &&
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/test_repo.git" log -1 --format=%s >actual &&
	expect_askpass both user%40host && test_cmp expect actual
'
test_expect_success 'push status output scrubs password' '
	cd "$ROOT_PATH/test_repo_clone" &&
	git push --porcelain "$HTTPD_URL_USER_PASS/smart/test_repo.git" +HEAD:scrub >status &&
	grep "^To $HTTPD_URL/smart/test_repo.git" status
'
test_expect_success 'clone/fetch scrubs password from reflogs' '
	cd "$ROOT_PATH" &&
	git clone "$HTTPD_URL_USER_PASS/smart/test_repo.git" reflog-test && cd reflog-test &&
	test_commit prepare-for-force-fetch && git switch -c away &&
	git fetch "$HTTPD_URL_USER_PASS/smart/test_repo.git" +main:main &&
	git log -g main >reflog &&
	grep "$HTTPD_URL" reflog && ! grep "$HTTPD_URL_USER_PASS" reflog
'
test_done
