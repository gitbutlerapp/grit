# Shell library to run git-daemon in tests.  Ends the test early if
# GIT_TEST_GIT_DAEMON is not set.
#
# Usage:
#
#	. ./test-lib.sh
#	. "$TEST_DIRECTORY"/lib-git-daemon.sh
#	start_git_daemon
#
#	test_expect_success '...' '
#		...
#	'
#
#	test_expect_success ...
#
#	test_done

if ! test_bool_env GIT_TEST_GIT_DAEMON true
then
	skip_all="git-daemon testing disabled (unset GIT_TEST_GIT_DAEMON to enable)"
	test_done
fi

if test_have_prereq !PIPE
then
	test_skip_or_die GIT_TEST_GIT_DAEMON "file system does not support FIFOs"
fi

# Harness uses a `git` wrapper that runs grit; grit does not implement `daemon`
# yet, so delegate the subprocess to the system git binary.
: "${LIB_GIT_DAEMON_COMMAND:=/usr/bin/git daemon}"
export LIB_GIT_DAEMON_COMMAND

test_set_port LIB_GIT_DAEMON_PORT

GIT_DAEMON_PID=
GIT_DAEMON_PIDFILE="$PWD"/daemon.pid
GIT_DAEMON_DOCUMENT_ROOT_PATH="$PWD"/repo
export GIT_DAEMON_DOCUMENT_ROOT_PATH
GIT_DAEMON_HOST_PORT=127.0.0.1:$LIB_GIT_DAEMON_PORT
GIT_DAEMON_URL=git://$GIT_DAEMON_HOST_PORT
# Match the `host=` header Git sends so grit's git:// client matches upstream packet traces.
GIT_OVERRIDE_VIRTUAL_HOST=$GIT_DAEMON_HOST_PORT
export GIT_OVERRIDE_VIRTUAL_HOST

registered_stop_git_daemon_atexit_handler=
start_git_daemon() {
	if test -n "$GIT_DAEMON_PID"
	then
		error "start_git_daemon already called"
	fi

	mkdir -p "$GIT_DAEMON_DOCUMENT_ROOT_PATH"

	# Ensure the daemon is torn down when the test script exits (no test_atexit in harness).
	if test -z "$registered_stop_git_daemon_atexit_handler"
	then
		trap 'stop_git_daemon 2>/dev/null; true' EXIT
		registered_stop_git_daemon_atexit_handler=AlreadyDone
	fi

	echo "Starting git daemon ..." >&2
	rm -f git_daemon_output
	${LIB_GIT_DAEMON_COMMAND:-git daemon} \
		--listen=127.0.0.1 --port="$LIB_GIT_DAEMON_PORT" \
		--reuseaddr --verbose --pid-file="$GIT_DAEMON_PIDFILE" \
		--base-path="$GIT_DAEMON_DOCUMENT_ROOT_PATH" \
		"$@" "$GIT_DAEMON_DOCUMENT_ROOT_PATH" \
		>/dev/null 2>git_daemon_output &
	GIT_DAEMON_PID=$!

	# Poll log for readiness (avoid FIFO + background cat: that blocks shells that
	# wait for all jobs, e.g. command substitution in the test harness).
	_tries=0
	while test "$_tries" -lt 50
	do
		if test -f git_daemon_output && grep -q "Ready to rumble" git_daemon_output 2>/dev/null
		then
			break
		fi
		sleep 0.1
		_tries=$((_tries + 1))
	done

	line=$(grep "Ready to rumble" git_daemon_output 2>/dev/null | head -1)
	if test -n "$line"
	then
		printf "%s\n" "$line" >&2
	fi
	if ! grep -q "Ready to rumble" git_daemon_output 2>/dev/null
	then
		kill "$GIT_DAEMON_PID" 2>/dev/null
		wait "$GIT_DAEMON_PID" 2>/dev/null
		unset GIT_DAEMON_PID
		test_skip_or_die GIT_TEST_GIT_DAEMON \
			"git daemon failed to start"
	fi
}

stop_git_daemon() {
	_dpid=
	if test -f "$GIT_DAEMON_PIDFILE"
	then
		_dpid=$(cat "$GIT_DAEMON_PIDFILE" 2>/dev/null)
	fi

	echo "Stopping git daemon ..." >&2
	# The listening process is usually the PID in the daemon pid-file; the
	# background shell may still be blocked on the FIFO until it dies.
	if test -n "$_dpid"
	then
		kill "$_dpid" 2>/dev/null
	fi
	if test -n "$GIT_DAEMON_PID"
	then
		kill "$GIT_DAEMON_PID" 2>/dev/null
	fi
	# Reap or force-kill so the FIFO read side unblocks and ports are freed.
	if test -n "$_dpid"
	then
		_i=0
		while kill -0 "$_dpid" 2>/dev/null && test "$_i" -lt 20
		do
			sleep 0.1
			_i=$((_i + 1))
		done
		kill -9 "$_dpid" 2>/dev/null
		wait "$_dpid" 2>/dev/null
	fi
	if test -n "$GIT_DAEMON_PID"
	then
		wait "$GIT_DAEMON_PID" 2>/dev/null
	fi
	rm -f "$GIT_DAEMON_PIDFILE"
	GIT_DAEMON_PID=
	rm -f git_daemon_output
}

# A stripped-down version of a netcat client, that connects to a "host:port"
# given in $1, sends its stdin followed by EOF, then dumps the response (until
# EOF) to stdout.
fake_nc() {
	if ! test_have_prereq FAKENC
	then
		echo >&4 "fake_nc: need to declare FAKENC prerequisite"
		return 127
	fi
	perl -Mstrict -MIO::Socket::INET -e '
		my $s = IO::Socket::INET->new(shift)
			or die "unable to open socket: $!";
		print $s <STDIN>;
		$s->shutdown(1);
		print <$s>;
	' "$@"
}

test_lazy_prereq FAKENC '
	perl -MIO::Socket::INET -e "exit 0"
'
