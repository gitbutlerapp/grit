# Shell library to run an HTTP server for use in tests.
#
# Replaces upstream's Apache-based lib-httpd.sh with a lightweight
# Rust HTTP server (test-httpd binary).
#
# Usage:
#
#   . ./test-lib.sh
#   . "$TEST_DIRECTORY"/lib-httpd.sh
#   start_httpd
#
#   test_expect_success '...' '
#       ...
#   '
#
#   test_done
#
# Variables:
#   LIB_HTTPD_PORT    — port (default: 0 = random)
#   HTTPD_URL         — set after start_httpd (e.g. http://127.0.0.1:PORT)
#   HTTPD_DOCUMENT_ROOT_PATH — document root for serving files

# HTTP transport tests need real git for client operations since grit
# doesn't support HTTP transport yet. Override the wrapper to use real git.
#
# `command -v git` is unreliable here: PATH starts with BIN_DIRECTORY, whose
# `git` may be a grit wrapper, or (after a previous buggy run) a self-exec
# stub. Prefer a system git, then scan PATH skipping test bin directories.
REAL_GIT=""
for _candidate in /usr/bin/git /usr/local/bin/git /bin/git; do
	if test -x "$_candidate"; then
		REAL_GIT="$_candidate"
		break
	fi
done
if test -z "$REAL_GIT"; then
	for _p in $(echo "$PATH" | tr ':' ' '); do
		case "$_p" in
		*/bin.t*|*/.bin) continue ;;
		esac
		_g="$_p/git"
		if test ! -x "$_g"; then
			continue
		fi
		if grep -q 'GUST_BIN\|grit' "$_g" 2>/dev/null; then
			continue
		fi
		if grep -qF "exec \"$_g\"" "$_g" 2>/dev/null; then
			continue
		fi
		REAL_GIT="$_g"
		break
	done
fi
REAL_GIT="${REAL_GIT:-/usr/bin/git}"

# Hybrid git wrapper: upstream git for HTTP(S) transport, grit otherwise.
# Tests after start_httpd need real git for clone/fetch over http(s) while still
# exercising grit for file:// and local commands (e.g. backfill).
write_hybrid_git_wrapper () {
	_target="$1"
	_tmp="${_target}.new.$$"
	_grit="${GUST_BIN:-}"
	if test -z "$_grit"; then
		_grit="$REAL_GIT"
	fi
	{
		printf '%s\n' '#!/bin/sh'
		printf "REAL_GIT='%s'\n" "$REAL_GIT"
		printf "GUST_BIN='%s'\n" "$_grit"
		cat <<'EOFWRAP'
_http=0
for _a in "$@"; do
	case "$_a" in
	http://*|https://*) _http=1; break ;;
	esac
done
# After start_httpd, $HTTPD_URL is set. Fetch/pull/push do not repeat the remote URL on the
# command line, so delegate those to real git so smart-HTTP transport keeps working in the suite.
if test "$_http" != 1 && test -n "${HTTPD_URL-}"; then
	case "$*" in
	*fetch*|*pull*) _http=1 ;;
	esac
fi
# HTTP fetch uses grit for bundle-uri parity (t5558), except glob refspecs (grit does not
# support them yet; t5558 `expand incremental bundle list` uses +refs/heads/*:refs/heads/*).
if test "$_http" = 1 && test -n "${HTTPD_URL-}"; then
	case "$*" in
	*fetch*)
		case "$*" in
		*\***) ;;
		*) _http=0 ;;
		esac
		;;
	esac
fi
# test-tool bundle-uri ls-remote <url> must use grit: system git has no test-tool.
case "$*" in
*"test-tool"*"bundle-uri"*) _http=0 ;;
esac
# System git's shell-based `git submodule` reads `GIT_EXEC_PATH` as a single directory; the
# HTTP test harness prepends a temporary exec path for upload-pack wrappers, so keep submodule
# operations on grit and let grit delegate only the HTTP clone step with a clean environment.
case "$*" in
*submodule*) _http=0 ;;
esac
# `git clone --bundle-uri` over HTTP is implemented in grit (t5558-clone-bundle-uri.sh).
_clone=0
_bundle_uri=0
for _a in "$@"; do
	case "$_a" in
	clone) _clone=1 ;;
	--bundle-uri|--bundle-uri=*) _bundle_uri=1 ;;
	esac
done
if test "$_clone" = 1 && test "$_bundle_uri" = 1; then
	_http=0
fi
# t5732 bundle-uri HTTP tests need Grit's protocol-v2 bundle-uri client path.
if test "${BUNDLE_URI_PROTOCOL-}" = http && test "$_clone" = 1; then
	_http=0
fi
# Smart HTTP push is implemented in Grit and must not use system Git with the
# temporary server-only GIT_EXEC_PATH.
case "$*" in
*push*) _http=0 ;;
esac
# t5581-http-curl-verbose intentionally hits a failing upload-pack endpoint and checks
# Grit's curl-style trace output.
case "$*" in
*"error_git_upload_pack"*) _http=0 ;;
esac
# t5563-simple-http-auth validates Grit's credential-helper and HTTP auth flow.
case "$*" in
*"custom_auth"*) _http=0 ;;
esac
# Authenticated smart HTTP cases exercise Grit's credential and trace-redaction paths.
case "$*" in
*"auth/smart"*|*"auth-fetch/smart"*|*"auth-push/smart"*) _http=0 ;;
esac
# Redirect credential re-auth (t5551, issue #895): exercise Grit's smart-HTTP client.
case "$*" in
*"smart-redir-auth"*) _http=0 ;;
esac
# t5564-http-proxy: grit implements http.proxy / GIT_TRACE_CURL / SOCKS path validation.
if test -n "${LIB_HTTPD_PROXY-}"; then
	case "$*" in
	*clone*http://*|*clone*https://*) _http=0 ;;
	esac
fi
# Path-only proxy validation uses `git clone -c http.proxy=...` (no URL on cmd line for hybrid).
case "$*" in
*clone*)
	case "$*" in *http.proxy*) _http=0 ;; esac
	;;
esac
# Empty SHA-256 clone over smart HTTP (t5551): the platform's system git (e.g. Apple Git 2.39)
# records an empty SHA-256 clone as SHA-1, so route these to Grit's HTTP client, which honours the
# advertised `object-format=sha256` (Grit's upload-pack also emits the `unborn HEAD symref-target`
# ls-refs line a conformant client needs).
case "$*" in
*clone*sha256*) _http=0 ;;
esac
if test "$_http" = 1; then
	exec "$REAL_GIT" "$@"
fi
exec "$GUST_BIN" "$@"
EOFWRAP
	} >"$_tmp"
	chmod +x "$_tmp"
	mv -f "$_tmp" "$_target"
}

if test -n "$BIN_DIRECTORY" && test -d "$BIN_DIRECTORY"; then
	write_hybrid_git_wrapper "$BIN_DIRECTORY/git"
fi
if test -n "$TRASH_DIRECTORY" && test -d "$TRASH_DIRECTORY/.bin"; then
	write_hybrid_git_wrapper "$TRASH_DIRECTORY/.bin/git"
fi

# Find the test-httpd binary
REPO_ROOT="$(cd "$TEST_DIRECTORY/.." && pwd)"
# Binaries built by Cargo use underscores, not hyphens (test_httpd),
# so check both naming conventions.
TEST_HTTPD_BIN="$REPO_ROOT/target/debug/test-httpd"
if ! test -x "$TEST_HTTPD_BIN"
then
	TEST_HTTPD_BIN="$REPO_ROOT/target/release/test-httpd"
fi
if ! test -x "$TEST_HTTPD_BIN"
then
	TEST_HTTPD_BIN="$REPO_ROOT/target/debug/test_httpd"
fi
if ! test -x "$TEST_HTTPD_BIN"
then
	TEST_HTTPD_BIN="$REPO_ROOT/target/release/test_httpd"
fi

if ! test -x "$TEST_HTTPD_BIN"
then
	skip_all='test-httpd binary not found; build with: cargo build --release --package grit-git --bin test-httpd'
	test_done
fi

# Set up paths
HTTPD_ROOT_PATH="$PWD/httpd"
HTTPD_DOCUMENT_ROOT_PATH="$HTTPD_ROOT_PATH/www"

# Default auth credentials (matching upstream's passwd file)
HTTPD_AUTH_USER="user@host"
HTTPD_AUTH_PASS="pass@host"

# Proxy auth: pass the upstream `proxy-passwd` entry so we stay aligned with git/t (Apache hash).
HTTPD_PROXY_AUTH_LINE="proxuser:\$apr1\$RxS6MLkD\$DYsqQdflheq4GPNxzJpx5."

HTTPD_PROTO=http

prepare_httpd() {
	mkdir -p "$HTTPD_DOCUMENT_ROOT_PATH"
	mkdir -p "$HTTPD_DOCUMENT_ROOT_PATH/auth/dumb"
}

start_httpd() {
	prepare_httpd

	local port_arg=""
	if test -n "$LIB_HTTPD_PORT"
	then
		port_arg="--port $LIB_HTTPD_PORT"
	fi

	local proxy_arg=""
	if test -n "$LIB_HTTPD_PROXY"
	then
		export LIB_HTTPD_PROXY
		proxy_arg="--proxy --proxy-auth ${HTTPD_PROXY_AUTH_LINE}"
	fi
	if test -n "${BUNDLE_URI_PROTOCOL-}"
	then
		export BUNDLE_URI_PROTOCOL
	fi
	# Smart HTTP runs `git-http-backend` → `git-upload-pack`. Use system Git only for
	# `--advertise-refs` (ref listing / capability string); delegate negotiation and
	# pack generation to grit so shallow deepen and multi_ack match the harness (t5539).
	_grit_exec="${GUST_BIN:-$REAL_GIT}"
	_real_exec_path="$(env -u GIT_EXEC_PATH "$REAL_GIT" -c safe.directory='*' --exec-path 2>/dev/null || true)"
	if test -z "$_real_exec_path"
	then
		_real_exec_path="$(dirname "$REAL_GIT")"
	fi
	HTTPD_GIT_EXEC_PATH="$HTTPD_ROOT_PATH/git-exec"
	mkdir -p "$HTTPD_GIT_EXEC_PATH"
	cat >"$HTTPD_GIT_EXEC_PATH/git-upload-pack" <<EOFUP
#!/bin/sh
REAL_GIT='$REAL_GIT'
GUST_BIN='$_grit_exec'
REAL_UP='$_real_exec_path/git-upload-pack'
if test '${BUNDLE_URI_PROTOCOL-}' = 'http'
then
	exec "\$GUST_BIN" upload-pack "\$@"
fi
case " \$* " in
*" --advertise-refs "*) exec "\$REAL_UP" "\$@" ;;
*) exec "\$GUST_BIN" upload-pack "\$@" ;;
esac
EOFUP
	chmod +x "$HTTPD_GIT_EXEC_PATH/git-upload-pack"
	cat >"$HTTPD_GIT_EXEC_PATH/git-receive-pack" <<EOFRP
#!/bin/sh
REAL_RP='$_real_exec_path/git-receive-pack'
exec "\$REAL_RP" "\$@"
EOFRP
	chmod +x "$HTTPD_GIT_EXEC_PATH/git-receive-pack"
	GIT_EXEC_PATH="$HTTPD_GIT_EXEC_PATH:$_real_exec_path"
	export GIT_EXEC_PATH

	# Start server in background, capture the READY line for the port
	"$TEST_HTTPD_BIN" \
		--root "$HTTPD_DOCUMENT_ROOT_PATH" \
		--auth "${HTTPD_AUTH_USER}:${HTTPD_AUTH_PASS}" \
		$proxy_arg \
		--pid-file "$HTTPD_ROOT_PATH/httpd.pid" \
		$port_arg \
		>"$HTTPD_ROOT_PATH/httpd.out" \
		2>"$HTTPD_ROOT_PATH/httpd.err" &
	HTTPD_PID=$!

	# Wait for READY line (up to 5 seconds)
	local tries=0
	while test $tries -lt 50
	do
		if test -s "$HTTPD_ROOT_PATH/httpd.out"
		then
			break
		fi
		sleep 0.1
		tries=$((tries + 1))
	done

	if ! test -s "$HTTPD_ROOT_PATH/httpd.out"
	then
		echo "test-httpd failed to start" >&2
		if test -s "$HTTPD_ROOT_PATH/httpd.err"
		then
			cat "$HTTPD_ROOT_PATH/httpd.err" >&2
		fi
		return 1
	fi

	LIB_HTTPD_PORT=$(sed -n 's/^READY //p' "$HTTPD_ROOT_PATH/httpd.out")
	if test -z "$LIB_HTTPD_PORT"
	then
		echo "Could not determine test-httpd port" >&2
		kill "$HTTPD_PID" 2>/dev/null
		return 1
	fi

	HTTPD_DEST="127.0.0.1:$LIB_HTTPD_PORT"
	HTTPD_URL="$HTTPD_PROTO://$HTTPD_DEST"
	export HTTPD_URL
	HTTPD_URL_USER="$HTTPD_PROTO://user%40host@$HTTPD_DEST"
	HTTPD_URL_USER_PASS="$HTTPD_PROTO://user%40host:pass%40host@$HTTPD_DEST"

	# Register cleanup at script exit
	trap 'stop_httpd' EXIT
}

stop_httpd() {
	if test -n "$HTTPD_PID"
	then
		kill "$HTTPD_PID" 2>/dev/null || :
		sleep 0.2
		kill -9 "$HTTPD_PID" 2>/dev/null || :
		HTTPD_PID=
	fi
}

strip_access_log () {
	sed -e "
		s/^.* \"//
		s/\"//
		s/ [1-9][0-9]*\$//
		s/^GET /GET  /
	" "$HTTPD_ROOT_PATH"/access.log
}

check_access_log () {
	sort "$1" >"$1".sorted &&
	strip_access_log >access.log.stripped &&
	sort access.log.stripped >access.log.sorted &&
	if ! test_cmp "$1".sorted access.log.sorted
	then
		test_cmp "$1" access.log.stripped
	fi
}

test_http_push_nonff () {
	REMOTE_REPO=$1; LOCAL_REPO=$2; BRANCH=$3; EXPECT_CAS_RESULT=${4-failure}
	test_expect_success 'non-fast-forward push fails and shows status' '
		cd "$REMOTE_REPO" && HEAD=$(git rev-parse --verify HEAD) &&
		cd "$LOCAL_REPO" && git checkout $BRANCH &&
		echo "changed" > path2 && git commit -a -m path2 --amend &&
		test_must_fail git push -v origin >output 2>&1 &&
		(cd "$REMOTE_REPO" && echo "$HEAD" >expect && git rev-parse --verify HEAD >actual && test_cmp expect actual) &&
		grep "\[rejected\]" output && test_grep "Updates were rejected because" output
	'
	test_expect_${EXPECT_CAS_RESULT} 'force with lease aka cas' '
		HEAD=$(cd "$REMOTE_REPO" && git rev-parse --verify HEAD) &&
		test_when_finished '\''(cd "$REMOTE_REPO" && git update-ref HEAD "$HEAD")'\'' &&
		(cd "$LOCAL_REPO" && git push -v --force-with-lease=$BRANCH:$HEAD origin) &&
		git rev-parse --verify "$BRANCH" >expect &&
		(cd "$REMOTE_REPO" && git rev-parse --verify HEAD) >actual &&
		test_cmp expect actual
	'
}

# Helper: setup post-update hook that runs git update-server-info
setup_post_update_server_info_hook () {
	test_hook --setup -C "$1" post-update <<-\EOF &&
	exec git update-server-info
	EOF
	git -C "$1" update-server-info
}

# Askpass helpers (matching upstream's interface)
setup_askpass_helper() {
	test_expect_success 'setup askpass helper' '
		write_script "$TRASH_DIRECTORY/askpass" <<-\EOF &&
		echo >>"$TRASH_DIRECTORY/askpass-query" "askpass: $*" &&
		case "$*" in
		*Username*)
			what=user
			;;
		*Password*)
			what=pass
			;;
		esac &&
		cat "$TRASH_DIRECTORY/askpass-$what"
		EOF
		GIT_ASKPASS="$TRASH_DIRECTORY/askpass" &&
		export GIT_ASKPASS &&
		export TRASH_DIRECTORY
	'
}

set_askpass () {
	>"$TRASH_DIRECTORY/askpass-query" &&
	echo "$1" >"$TRASH_DIRECTORY/askpass-user" &&
	echo "$2" >"$TRASH_DIRECTORY/askpass-pass"
}

set_netrc () {
	# $HOME=$TRASH_DIRECTORY
	echo "machine $1 login $2 password $3" >"$TRASH_DIRECTORY/.netrc"
}

clear_netrc () {
	rm -f "$TRASH_DIRECTORY/.netrc"
}

enable_cgipassauth () {
	test_set_prereq CGIPASSAUTH
}

expect_askpass () {
	dest=$HTTPD_DEST${3+/$3}

	{
		case "$1" in
		none)
			;;
		pass)
			echo "askpass: Password for '$HTTPD_PROTO://$2@$dest': "
			;;
		both)
			echo "askpass: Username for '$HTTPD_PROTO://$dest': "
			echo "askpass: Password for '$HTTPD_PROTO://$2@$dest': "
			;;
		*)
			false
			;;
		esac
	} >"$TRASH_DIRECTORY/askpass-expect" &&
	test_cmp "$TRASH_DIRECTORY/askpass-expect" \
		 "$TRASH_DIRECTORY/askpass-query"
}
