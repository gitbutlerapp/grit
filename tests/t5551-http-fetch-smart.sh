#!/bin/sh
: ${HTTP_PROTO:=HTTP/1.1}
test_description="test smart fetching over http via http-backend ($HTTP_PROTO)"
GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main
export GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME
cd "$(dirname "$0")" || exit 1
. ./test-lib.sh
. "$TEST_DIRECTORY"/lib-httpd.sh
start_httpd

test_expect_success 'setup repository' '
	git config push.default matching &&
	echo content >file && git add file && git commit -m one
'
test_expect_success 'create http-accessible bare repository' '
	mkdir "$HTTPD_DOCUMENT_ROOT_PATH/repo.git" &&
	(cd "$HTTPD_DOCUMENT_ROOT_PATH/repo.git" && git --bare init) &&
	git remote add public "$HTTPD_DOCUMENT_ROOT_PATH/repo.git" &&
	git push public main:main
'
setup_askpass_helper

test_expect_success 'clone http repository' '
	git clone --quiet $HTTPD_URL/smart/repo.git clone 2>err &&
	test_cmp file clone/file
'
test_expect_success 'fetch changes via http' '
	echo content >>file && git commit -a -m two && git push public &&
	(cd clone && git pull) && test_cmp file clone/file
'
test_expect_success 'used upload-pack service' '
	strip_access_log >log &&
	grep "GET  /smart/repo.git/info/refs?service=git-upload-pack HTTP/[0-9.]* 200" log &&
	grep "POST /smart/repo.git/git-upload-pack HTTP/[0-9.]* 200" log
'
test_expect_failure 'follow redirects (301) — needs redirect support' '
	git clone $HTTPD_URL/smart-redir-perm/repo.git --quiet repo-p
'
test_expect_failure 'follow redirects (302) — needs redirect support' '
	git clone $HTTPD_URL/smart-redir-temp/repo.git --quiet repo-t
'
test_expect_failure 'redirects re-root further requests' '
	git clone $HTTPD_URL/smart-redir-limited/repo.git repo-redir-limited
'
test_expect_success 'clone from password-protected repository' '
	echo two >expect && set_askpass user@host pass@host &&
	git clone --bare "$HTTPD_URL/auth/smart/repo.git" smart-auth &&
	expect_askpass both user%40host &&
	git --git-dir=smart-auth log -1 --format=%s >actual && test_cmp expect actual
'
test_expect_success 'clone from auth-only-for-push repository' '
	echo two >expect && set_askpass wrong &&
	git clone --bare "$HTTPD_URL/auth-push/smart/repo.git" smart-noauth &&
	expect_askpass none &&
	git --git-dir=smart-noauth log -1 --format=%s >actual && test_cmp expect actual
'
test_expect_success 'clone from auth-only-for-objects repository' '
	echo two >expect && set_askpass user@host pass@host &&
	git clone --bare "$HTTPD_URL/auth-fetch/smart/repo.git" half-auth &&
	expect_askpass both user%40host &&
	git --git-dir=half-auth log -1 --format=%s >actual && test_cmp expect actual
'
test_expect_success 'no-op half-auth fetch does not require a password' '
	set_askpass wrong &&
	GIT_TEST_PROTOCOL_VERSION=0 git --git-dir=half-auth fetch &&
	expect_askpass none
'
test_expect_success 'redirects send auth to new location' '
	set_askpass user@host pass@host &&
	git -c credential.useHttpPath=true \
	  clone $HTTPD_URL/smart-redir-auth/repo.git repo-redir-auth &&
	test_cmp file repo-redir-auth/file
'
test_expect_success 'GIT_TRACE_CURL redacts auth details' '
	rm -rf redact-auth trace && set_askpass user@host pass@host &&
	GIT_TRACE_CURL="$(pwd)/trace" git clone --bare "$HTTPD_URL/auth/smart/repo.git" redact-auth &&
	expect_askpass both user%40host &&
	! grep -i "Authorization: Basic [0-9a-zA-Z+/]" trace &&
	grep -i "Authorization: Basic <redacted>" trace
'
test_expect_success 'GIT_CURL_VERBOSE redacts auth details' '
	rm -rf redact-auth trace && set_askpass user@host pass@host &&
	GIT_CURL_VERBOSE=1 git clone --bare "$HTTPD_URL/auth/smart/repo.git" redact-auth 2>trace &&
	expect_askpass both user%40host &&
	! grep -i "Authorization: Basic [0-9a-zA-Z+/]" trace &&
	grep -i "Authorization: Basic <redacted>" trace
'
test_expect_success 'GIT_TRACE_CURL does not redact if GIT_TRACE_REDACT=0' '
	rm -rf redact-auth trace && set_askpass user@host pass@host &&
	GIT_TRACE_REDACT=0 GIT_TRACE_CURL="$(pwd)/trace" \
		git clone --bare "$HTTPD_URL/auth/smart/repo.git" redact-auth &&
	expect_askpass both user%40host &&
	grep -i "Authorization: Basic [0-9a-zA-Z+/]" trace
'
test_expect_success 'disable dumb http on server' '
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" config http.getanyfile false
'
test_expect_success 'GIT_SMART_HTTP can disable smart http' '
	(GIT_SMART_HTTP=0 && export GIT_SMART_HTTP && cd clone && test_must_fail git fetch)
'
test_expect_failure 'invalid Content-Type rejected — needs broken_smart route' '
	test_must_fail git clone $HTTPD_URL/broken_smart/repo.git 2>actual &&
	test_grep "not valid:" actual
'
test_expect_success 'create namespaced refs' '
	test_commit namespaced &&
	git push public HEAD:refs/namespaces/ns/refs/heads/main &&
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" \
		symbolic-ref refs/namespaces/ns/HEAD refs/namespaces/ns/refs/heads/main
'
test_expect_failure 'smart clone respects namespace — needs namespace route' '
	git clone "$HTTPD_URL/smart_namespace/repo.git" ns-smart
'
test_expect_success 'transfer.hiderefs works over smart-http' '
	test_commit hidden && test_commit visible &&
	git push public HEAD^:refs/heads/a HEAD:refs/heads/b &&
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" config transfer.hiderefs refs/heads/a &&
	git clone --bare "$HTTPD_URL/smart/repo.git" hidden.git &&
	test_must_fail git -C hidden.git rev-parse --verify a &&
	git -C hidden.git rev-parse --verify b
'
test_expect_success 'test allowreachablesha1inwant' '
	test_when_finished "rm -rf test_reachable.git" &&
	server="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" &&
	main_sha=$(git -C "$server" rev-parse refs/heads/main) &&
	git -C "$server" config uploadpack.allowreachablesha1inwant 1 &&
	git init --bare test_reachable.git &&
	git -C test_reachable.git remote add origin "$HTTPD_URL/smart/repo.git" &&
	git -C test_reachable.git fetch origin "$main_sha"
'
test_expect_success 'test allowreachablesha1inwant with unreachable' '
	server="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" &&
	git -C "$server" config uploadpack.allowreachablesha1inwant 1 &&
	test_commit unreachable1 &&
	unreachable_sha=$(git rev-parse HEAD) &&
	git push public HEAD:refs/heads/doomed &&
	git push public :refs/heads/doomed &&
	git reset --hard HEAD^ &&
	git init --bare test_reachable2.git &&
	git -C test_reachable2.git remote add origin "$HTTPD_URL/smart/repo.git" &&
	test_must_fail env GIT_TEST_PROTOCOL_VERSION=0 \
		git -C test_reachable2.git fetch origin "$unreachable_sha"
'
test_expect_success 'test allowanysha1inwant with unreachable' '
	server="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" &&
	git -C "$server" config uploadpack.allowreachablesha1inwant 1 &&
	test_commit unreachable2 &&
	unreachable_sha=$(git rev-parse HEAD) &&
	git push public HEAD:refs/heads/doomed2 &&
	git push public :refs/heads/doomed2 &&
	git reset --hard HEAD^ &&
	git init --bare test_reachable3.git &&
	git -C test_reachable3.git remote add origin "$HTTPD_URL/smart/repo.git" &&
	test_must_fail env GIT_TEST_PROTOCOL_VERSION=0 \
		git -C test_reachable3.git fetch origin "$unreachable_sha" &&
	git -C "$server" config uploadpack.allowanysha1inwant 1 &&
	git -C test_reachable3.git fetch origin "$unreachable_sha"
'
test_expect_success 'using fetch command in remote-curl updates refs' '
	SERVER="$HTTPD_DOCUMENT_ROOT_PATH/twobranch" && rm -rf "$SERVER" client &&
	git init "$SERVER" && test_commit -C "$SERVER" foo &&
	git -C "$SERVER" update-ref refs/heads/anotherbranch foo &&
	git clone $HTTPD_URL/smart/twobranch client &&
	test_commit -C "$SERVER" bar &&
	git -C client -c protocol.version=0 fetch &&
	git -C "$SERVER" rev-parse main >expect &&
	git -C client rev-parse origin/main >actual && test_cmp expect actual
'
test_expect_success 'fetch by SHA-1 without tag following' '
	SERVER="$HTTPD_DOCUMENT_ROOT_PATH/server" && rm -rf "$SERVER" client &&
	git init "$SERVER" && test_commit -C "$SERVER" foo &&
	git clone $HTTPD_URL/smart/server client &&
	test_commit -C "$SERVER" bar &&
	git -C "$SERVER" rev-parse bar >bar_hash &&
	git -C client -c protocol.version=0 fetch --no-tags origin $(cat bar_hash)
'
test_expect_success 'cookies are redacted by default' '
	rm -rf clone2 &&
	echo "Set-Cookie: Foo=1" >cookies && echo "Set-Cookie: Bar=2" >>cookies &&
	GIT_TRACE_CURL=true git -c "http.cookieFile=$(pwd)/cookies" clone \
		$HTTPD_URL/smart/repo.git clone2 2>err &&
	grep -i "Cookie:.*Foo=<redacted>" err && grep -i "Cookie:.*Bar=<redacted>" err &&
	! grep -i "Cookie:.*Foo=1" err && ! grep -i "Cookie:.*Bar=2" err
'
test_expect_success 'GIT_TRACE_REDACT=0 disables cookie redaction' '
	rm -rf clone4 &&
	echo "Set-Cookie: Foo=1" >cookies && echo "Set-Cookie: Bar=2" >>cookies &&
	GIT_TRACE_REDACT=0 GIT_TRACE_CURL=true git -c "http.cookieFile=$(pwd)/cookies" clone \
		$HTTPD_URL/smart/repo.git clone4 2>err &&
	grep -i "Cookie:.*Foo=1" err && grep -i "Cookie:.*Bar=2" err
'
test_expect_success 'GIT_TRACE_CURL_NO_DATA prevents data from being traced' '
	rm -rf clone5 &&
	GIT_TRACE_CURL=true git clone $HTTPD_URL/smart/repo.git clone5 2>err &&
	grep "=> Send data" err &&
	rm -rf clone6 &&
	GIT_TRACE_CURL=true GIT_TRACE_CURL_NO_DATA=1 git clone $HTTPD_URL/smart/repo.git clone6 2>err &&
	! grep "=> Send data" err
'
test_expect_success 'http auth remembers successful credentials' '
	rm -f .git-credentials && test_config credential.helper store &&
	set_askpass user@host pass@host &&
	git ls-remote "$HTTPD_URL/auth/smart/repo.git" >/dev/null &&
	expect_askpass both user%40host &&
	set_askpass bogus-user bogus-pass &&
	git ls-remote "$HTTPD_URL/auth/smart/repo.git" >/dev/null &&
	expect_askpass none
'
test_expect_success 'http auth forgets bogus credentials' '
	rm -f .git-credentials && test_config credential.helper store &&
	{ echo "url=$HTTPD_URL" && echo "username=bogus" && echo "password=bogus"; } | git credential approve &&
	set_askpass user@host pass@host &&
	test_must_fail git ls-remote "$HTTPD_URL/auth/smart/repo.git" >/dev/null &&
	expect_askpass none &&
	set_askpass user@host pass@host &&
	git ls-remote "$HTTPD_URL/auth/smart/repo.git" >/dev/null &&
	expect_askpass both user%40host
'
test_expect_success 'v2 to v0 fallback' '
	git --git-dir="$HTTPD_DOCUMENT_ROOT_PATH/repo.git" config http.getanyfile true &&
	GIT_TEST_PROTOCOL_VERSION=2 git clone $HTTPD_URL/smart/repo.git repo-v0-fallback && true
'
test_expect_success 'create empty SHA-256 repository' '
	mkdir "$HTTPD_DOCUMENT_ROOT_PATH/sha256.git" &&
	(cd "$HTTPD_DOCUMENT_ROOT_PATH/sha256.git" && git --bare init --object-format=sha256)
'
test_expect_success 'clone empty SHA-256 repository with protocol v2' '
	rm -fr sha256 && echo sha256 >expected &&
	git -c protocol.version=2 clone "$HTTPD_URL/smart/sha256.git" &&
	git -C sha256 rev-parse --show-object-format >actual && test_cmp actual expected &&
	git ls-remote "$HTTPD_URL/smart/sha256.git" >actual && test_must_be_empty actual
'
test_expect_success 'clone empty SHA-256 repository with protocol v0' '
	rm -fr sha256 && echo sha256 >expected &&
	GIT_TRACE=1 GIT_TRACE_PACKET=1 git -c protocol.version=0 clone "$HTTPD_URL/smart/sha256.git" &&
	git -C sha256 rev-parse --show-object-format >actual && test_cmp actual expected &&
	git ls-remote "$HTTPD_URL/smart/sha256.git" >actual && test_must_be_empty actual
'
test_done
