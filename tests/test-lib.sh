#!/bin/sh
# Simplified test library for Gust tests.
# Modelled on git/t/test-lib.sh but stripped to what our tests need.
#
# Usage in test scripts:
#   . ./test-lib.sh
#   test_expect_success 'description' 'commands'
#   test_done

# Locate grit binary: env GUST_BIN wins; else repo layout under tests/ (run-tests.sh sets GUST_BIN).
# Note: `$(dirname "$(dirname "$0")")` breaks when $0 is a bare filename (dirname is `.`), so we
# derive the repo root from the directory containing the test script, not from double-dirname.
if test -z "$GUST_BIN"
then
	_tests_home="$(cd "$(dirname "$0")" && pwd)"
	_repo_root="$(cd "$_tests_home/.." && pwd)"
	for candidate in \
		"$_repo_root/target/debug/grit-git" \
		"$_repo_root/target/release/grit-git" \
		"$_tests_home/grit"
	do
		if test -x "$candidate"
		then
			GUST_BIN="$candidate"
			break
		fi
	done
	if test -z "$GUST_BIN"
	then
		for f in /var/folders/*/T/cursor-sandbox-cache/*/cargo-target/debug/grit-git \
		          /tmp/cargo-target/debug/grit-git
		do
			if test -x "$f"
			then
				GUST_BIN="$f"
				break
			fi
		done
	fi
fi

if test -z "$GUST_BIN"
then
	echo "FATAL: could not locate grit binary (set GUST_BIN)" >&2
	exit 1
fi

# Resolve GUST_BIN to an absolute path so wrapper scripts work regardless of cwd.
GUST_BIN="$(cd "$(dirname "$GUST_BIN")" && pwd)/$(basename "$GUST_BIN")"
export GUST_BIN

# Shell used for nested test scripts (lib-subtest.sh).
TEST_SHELL_PATH="${TEST_SHELL_PATH:-/bin/sh}"
export TEST_SHELL_PATH

# Original stdio for framework messages (matches git/t/test-lib.sh fd layout).
exec 5>&1
exec 6<&0
exec 7>&2

# Test environment (honour TEST_DIRECTORY when exported, e.g. subtests in a subdir)
if test -z "$TEST_DIRECTORY"
then
	TEST_DIRECTORY="$(cd "$(dirname "$0")" && pwd)"
fi
# Upstream Git manpage sources (t0450-txt-doc-vs-help, lib-gettext, etc.)
if test -z "$GIT_SOURCE_DIR"
then
	_repo_root="$(cd "$(dirname "$TEST_DIRECTORY")" && pwd)"
	GIT_SOURCE_DIR="$_repo_root/git"
	export GIT_SOURCE_DIR
fi
# Upstream tests and completion scripts expect a "build dir" pointing at the Git
# source tree (contrib/, Documentation/, t/helper/, etc.). In this workspace that
# is the same path as GIT_SOURCE_DIR.
if test -z "$GIT_BUILD_DIR"
then
	GIT_BUILD_DIR="$GIT_SOURCE_DIR"
	export GIT_BUILD_DIR
fi
# Use a per-test trash directory to avoid interference between tests.
# Derive from the test script name (e.g., t4050-diff.sh -> trash.t4050-diff)
_test_basename="$(basename "$0" .sh)"
# Nested tests (lib-subtest.sh) set TEST_OUTPUT_DIRECTORY_OVERRIDE to their cwd
# so trash paths and ../../ references match upstream git/t behavior.
if test -n "${TEST_OUTPUT_DIRECTORY_OVERRIDE:-}"
then
	_test_output_base="$TEST_OUTPUT_DIRECTORY_OVERRIDE"
else
	_test_output_base="$TEST_DIRECTORY"
fi
TRASH_DIRECTORY="${TRASH_DIRECTORY:-$_test_output_base/trash.$_test_basename}"
# Wrapper scripts must survive `git clean` and must not live under
# `$TEST_DIRECTORY` (some tests remove sibling paths). Use a per-host temp dir.
_tmpbase="${TMPDIR:-/tmp}"
BIN_DIRECTORY="${_tmpbase}/gust-test-bin.${_test_basename}.$$"
TEST_RESULTS_DIR="${TEST_DIRECTORY}/test-results"

. "$TEST_DIRECTORY"/test-lib-harness.sh
# Upstream git/t/test-lib.sh sources test-lib-functions.sh for helpers such as
# `test_set_magic_mtime` / `test_is_magic_mtime` (t2108, t7508, …).
. "$TEST_DIRECTORY"/test-lib-functions.sh
init_test_harness_options "$@"
TEST_NAME="$_test_basename"
TEST_NUMBER="${TEST_NAME%%-*}"
TEST_NUMBER="${TEST_NUMBER#t}"
this_test=${0##*/}
this_test=${this_test%%-*}
if test "$verbose" = t
then
	exec 4>&2 3>&2
else
	exec 4>/dev/null 3>/dev/null
fi
test_success=0
test_failure=0
test_fixed=0
test_broken=0
skip_all=

# Counters
test_count=0
test_pass=0
test_fail=0
test_skip=0
test_failures=""

# Colour
if test -t 1 && command -v tput >/dev/null 2>&1
then
	RED="$(tput setaf 1)" GREEN="$(tput setaf 2)" YELLOW="$(tput setaf 3)" RESET="$(tput sgr0)"
else
	RED='' GREEN='' YELLOW='' RESET=''
fi

# Used by lib-git-p4 (git-p4 wrapper) and other tests; must exist before `setup_trash`.
PYTHON_PATH=${PYTHON_PATH:-$(command -v python3 2>/dev/null || command -v python 2>/dev/null || echo /usr/bin/python3)}
export PYTHON_PATH

# Set up a fresh trash directory for this test script.
setup_trash () {
	if test -d "$TRASH_DIRECTORY"; then
		chmod -R u+rwx "$TRASH_DIRECTORY" 2>/dev/null
		rm -rf "$TRASH_DIRECTORY" 2>/dev/null
		# If rm -rf failed (e.g. locked files), try harder
		if test -d "$TRASH_DIRECTORY"; then
			find "$TRASH_DIRECTORY" -type f -exec chmod u+w {} + 2>/dev/null
			find "$TRASH_DIRECTORY" -type d -exec chmod u+rwx {} + 2>/dev/null
			rm -rf "$TRASH_DIRECTORY"
		fi
	fi
	mkdir -p "$TRASH_DIRECTORY"
	# BIN_DIRECTORY is outside the working tree so git clean -x cannot remove it
	mkdir -p "$BIN_DIRECTORY"
	# Remove stale wrappers before rewrite: avoids Linux ETXTBSY when replacing a
	# running executable and restores wrappers if a prior run left BIN_DIRECTORY empty.
	rm -f "$BIN_DIRECTORY/git" "$BIN_DIRECTORY/grit" "$BIN_DIRECTORY/test-tool" "$BIN_DIRECTORY/scalar" 2>/dev/null || true
	# Write a 'git' wrapper script that calls grit (GUST_BIN is absolute path)
	cat >"$BIN_DIRECTORY/git" <<EOF
#!/bin/sh
exec "$GUST_BIN" "\$@"
EOF
	chmod +x "$BIN_DIRECTORY/git"
	# Also write a 'grit' wrapper (same binary; some tests invoke `grit` by name)
	cat >"$BIN_DIRECTORY/grit" <<EOF
#!/bin/sh
exec "$GUST_BIN" "\$@"
EOF
	chmod +x "$BIN_DIRECTORY/grit"
	# Write a 'test-tool' wrapper for shell tests invoking it directly
	cat >"$BIN_DIRECTORY/test-tool" <<EOF
#!/bin/sh
exec "$TEST_DIRECTORY/test-tool" "\$@"
EOF
	chmod +x "$BIN_DIRECTORY/test-tool"
	# Write a 'scalar' wrapper
	cat >"$BIN_DIRECTORY/scalar" <<EOF
#!/bin/sh
exec "$GUST_BIN" scalar "\$@"
EOF
	chmod +x "$BIN_DIRECTORY/scalar"
	# GIT_EXEC_PATH: upstream git-p4 lives beside the git binary; grit delegates to
	# git-p4.py from the vendored Git tree so `git p4` and `$(git --exec-path)/git-p4` work.
	_GIT_EXEC_HELPER_DIR="$BIN_DIRECTORY/git-exec"
	mkdir -p "$_GIT_EXEC_HELPER_DIR"
	if test -f "$GIT_SOURCE_DIR/git-p4.py"
	then
		cat >"$_GIT_EXEC_HELPER_DIR/git-p4" <<EOF
#!/bin/sh
exec "$PYTHON_PATH" "$GIT_SOURCE_DIR/git-p4.py" "\$@"
EOF
		chmod +x "$_GIT_EXEC_HELPER_DIR/git-p4"
	fi
	GIT_EXEC_PATH="$_GIT_EXEC_HELPER_DIR"
	export GIT_EXEC_PATH
	# Save PATH before grit/git wrappers so test_done can restore the caller's environment.
	TEST_LIB_ORIG_PATH=$PATH
	export TEST_LIB_ORIG_PATH
	# Prepend BIN_DIRECTORY to PATH so every subshell sees 'git' → grit
PATH="$TEST_DIRECTORY:$PATH"
	export PATH="$BIN_DIRECTORY:$PATH"
	# Avoid a stale bash(1) command hash for `git` from before PATH was rewritten.
	hash -r 2>/dev/null || true
	# cd into trash so each test starts with a clean cwd
	cd "$TRASH_DIRECTORY" || exit 1

	# Initialize a git repository in the trash directory (like upstream)
	if test -z "$TEST_NO_CREATE_REPO"
	then
		"$GUST_BIN" init >/dev/null 2>&1 ||
			echo "warning: could not git init trash directory" >&2

	fi
}

# Restore PATH to the value before `setup_trash` added grit/git wrappers.
test_lib_restore_path () {
	if test -n "${TEST_LIB_ORIG_PATH-}"
	then
		PATH=$TEST_LIB_ORIG_PATH
		export PATH
	fi
}

setup_trash

# Persist test_tick across subshell boundaries via a state file.
# Prefer `.git/.test_tick` so `git status` never lists it (t7508 compares full
# ignored/untracked output). Fall back to the resolved git-dir for gitfile
# worktrees, then trash root when there is no repository yet.
_TICK_FILE="$TRASH_DIRECTORY/.git/.test_tick"
TEST_OID_CACHE_FILE="$TRASH_DIRECTORY/.git/.test_oid_cache"

test_tick () {
	local _tick_file="$_TICK_FILE"
	if ! test -d "$(dirname "$_tick_file")"
	then
		local _gitdir
		_gitdir="$(git rev-parse --git-dir 2>/dev/null || true)"
		if test -n "$_gitdir"
		then
			_tick_file="${_gitdir%/}/.test_tick"
		else
			_tick_file="$TRASH_DIRECTORY/.test_tick"
		fi
	fi
	if test -z "${test_tick+set}"
	then
		# Try to load from file (survives subshell boundaries)
		if test -f "$_tick_file"
		then
			test_tick=$(cat "$_tick_file")
			test_tick=$(($test_tick + 60))
		else
			test_tick=1112911993
		fi
	else
		test_tick=$(($test_tick + 60))
	fi
	echo "$test_tick" >"$_tick_file"
	GIT_COMMITTER_DATE="$test_tick -0700"
	GIT_AUTHOR_DATE="$test_tick -0700"
	export GIT_COMMITTER_DATE GIT_AUTHOR_DATE
}

# Stub for git test infrastructure function (no-op unless TEST_DEBUG is set)
test_debug () {
	test -n "$TEST_DEBUG" && eval "$@" || true
}

# Default diff program
DIFF="${DIFF:-diff}"

# Allow tests to use $HOME — isolate from real user config
HOME="$TRASH_DIRECTORY"
XDG_CONFIG_HOME="$TRASH_DIRECTORY/.config"
export HOME XDG_CONFIG_HOME
LC_ALL=C
LANG=C
export LC_ALL LANG
EDITOR=:
export EDITOR

# Prevent tests from discovering enclosing repositories
GIT_CEILING_DIRECTORIES="$(dirname "$TRASH_DIRECTORY")"
export GIT_CEILING_DIRECTORIES

# Set default author/committer identity for all tests
GIT_AUTHOR_NAME="A U Thor"
GIT_AUTHOR_EMAIL="author@example.com"
GIT_COMMITTER_NAME="C O Mitter"
GIT_COMMITTER_EMAIL="committer@example.com"
GIT_AUTHOR_DATE="1112354055 +0200"
GIT_COMMITTER_DATE="1112354055 +0200"
TEST_AUTHOR_LOCALNAME=author
TEST_AUTHOR_DOMAIN=example.com
TEST_COMMITTER_LOCALNAME=committer
TEST_COMMITTER_DOMAIN=example.com
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL
export GIT_AUTHOR_DATE GIT_COMMITTER_DATE

# Tests using GIT_TRACE grep lines starting with `trace:` (no timestamp prefix).
GIT_TRACE_BARE=1
export GIT_TRACE_BARE

# Quiet git/grit unless TEST_VERBOSE is set
if test -z "$TEST_VERBOSE"
then
	GIT_QUIET=-q
else
	GIT_QUIET=
fi

# ── constants ────────────────────────────────────────────────────────────────

ZERO_OID=0000000000000000000000000000000000000000
SQ="'"
LF='
'
export ZERO_OID SQ LF

if test -n "$GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME"
then
	git config --global init.defaultBranch "$GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME"
fi

# ── helpers used by test bodies ──────────────────────────────────────────────

test_path_is_file () { test -f "$1"; }
test_path_is_dir  () { test -d "$1"; }
test_path_is_missing () { ! test -e "$1"; }
test_path_is_executable () {
	if test $# -ne 1
	then
		echo "test_path_is_executable: expected 1 argument" >&2
		return 1
	fi
	if ! test -x "$1"
	then
		echo "$1 is not executable" >&2
		return 1
	fi
}

test_stdout_line_count () {
	if test $# -le 3
	then
		echo "test_stdout_line_count: expected at least 4 arguments" >&2
		return 1
	fi
	local op="$1" count="$2"
	shift 2
	local trashdir
	trashdir="$(git rev-parse --git-dir 2>/dev/null)/trash" || {
		echo "test_stdout_line_count: must run inside a repository" >&2
		return 1
	}
	mkdir -p "$trashdir" &&
	"$@" >"$trashdir/output" &&
	test_line_count "$op" "$count" "$trashdir/output"
}

test_match_signal () {
	if test "$2" = "$((128 + $1))"
	then
		return 0
	elif test "$2" = "$((256 + $1))"
	then
		return 0
	fi
	return 1
}

# Used by lib-terminal.sh `test_terminal` (upstream test-lib-functions.sh).
test_declared_prereq () {
	case ",${test_prereq-}," in
	*,$1,*)
		return 0
		;;
	esac
	return 1
}

# GIT_TRACE2_EVENT helpers (upstream test-lib-functions.sh); use `command grep` so PATH cannot
# shadow with a directory named `grep` (t6500-gc).
test_subcommand () {
	negate=
	if test "$1" = "!"
	then
		negate=t
		shift
	fi
	expr="$(printf '"%s",' "$@")"
	expr="${expr%,}"
	if test -n "$negate"
	then
		! command grep "\[$expr\]"
	else
		command grep "\[$expr\]"
	fi
}

test_subcommand_flex () {
	negate=
	if test "$1" = "!"
	then
		negate=t
		shift
	fi
	expr="$(printf '"%s".*' "$@")"
	if test -n "$negate"
	then
		! command grep "\[$expr\]"
	else
		command grep "\[$expr\]"
	fi
}

test_create_repo () {
	local repo="$1"
	mkdir -p "$repo" &&
	(
		cd "$repo" &&
		git init &&
		git config user.name "Test User" &&
		git config user.email "test@example.com"
	)
}

test_write_lines () {
	while test $# -gt 0; do
		printf '%s\n' "$1"
		shift
	done
}

test_set_editor () {
	FAKE_EDITOR="$1"
	export FAKE_EDITOR
	EDITOR='"$FAKE_EDITOR"'
	export EDITOR
}

test_set_sequence_editor () {
	FAKE_SEQUENCE_EDITOR="$1"
	export FAKE_SEQUENCE_EDITOR
	GIT_SEQUENCE_EDITOR='"$FAKE_SEQUENCE_EDITOR"'
	export GIT_SEQUENCE_EDITOR
}

test_config () {
	config_dir=
	if test "$1" = -C
	then
		shift
		config_dir=$1
		shift
	fi

	is_worktree=
	if test "$1" = --worktree
	then
		is_worktree=1
		shift
	fi

	test_when_finished "test_unconfig ${config_dir:+-C '$config_dir'} ${is_worktree:+--worktree} '$1'" &&
	git ${config_dir:+-C "$config_dir"} config ${is_worktree:+--worktree} "$@"
}

test_config_global () {
	local key="$1" val="$2"
	git config --global "$key" "$val" &&
	test_when_finished "git config --global --unset '$key'"
}

# Trace2 JSON helper (GIT_TRACE2_EVENT); matches git/t test_trace2_data.
test_trace2_data () {
	grep -e '"category":"'"$1"'","key":"'"$2"'","value":"'"$3"'"'
}

test_file_not_empty () {
	if ! test -s "$1"
	then
		echo >&2 "test_file_not_empty: '$1' is empty"
		return 1
	fi
}

test_might_fail () {
	"$@"
	return 0
}

sane_unset () {
	while test $# -gt 0; do
		# If unsetting test_tick, also remove the persistence file
		if test "$1" = "test_tick" && test -n "${_TICK_FILE:-}"
		then
			rm -f "$_TICK_FILE"
		fi
		unset "$1" 2>/dev/null
		shift
	done
}

test_cmp_bin () {
	cmp "$@"
}

# Decode ANSI SGR sequences to tagged text (matches git/t/test-lib-functions.sh).
# A sed-only decoder cannot represent combined codes like ESC[1;31m as <BOLD;RED>.
test_decode_color () {
	awk '
		function name(n) {
			if (n == 0) return "RESET";
			if (n == 1) return "BOLD";
			if (n == 2) return "FAINT";
			if (n == 3) return "ITALIC";
			if (n == 7) return "REVERSE";
			if (n == 30) return "BLACK";
			if (n == 31) return "RED";
			if (n == 32) return "GREEN";
			if (n == 33) return "YELLOW";
			if (n == 34) return "BLUE";
			if (n == 35) return "MAGENTA";
			if (n == 36) return "CYAN";
			if (n == 37) return "WHITE";
			if (n == 40) return "BLACK";
			if (n == 41) return "BRED";
			if (n == 42) return "BGREEN";
			if (n == 43) return "BYELLOW";
			if (n == 44) return "BBLUE";
			if (n == 45) return "BMAGENTA";
			if (n == 46) return "BCYAN";
			if (n == 47) return "BWHITE";
		}
		{
			while (match($0, /\033\[[0-9;]*m/) != 0) {
				printf "%s<", substr($0, 1, RSTART-1);
				codes = substr($0, RSTART+2, RLENGTH-3);
				if (length(codes) == 0)
					printf "%s", name(0)
				else {
					n = split(codes, ary, ";");
					sep = "";
					for (i = 1; i <= n; i++) {
						printf "%s%s", sep, name(ary[i]);
						sep = ";"
					}
				}
				printf ">";
				$0 = substr($0, RSTART + RLENGTH, length($0) - RSTART - RLENGTH + 1);
			}
			print
		}
	'
}

_x05='[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'
_x35="$_x05$_x05$_x05$_x05$_x05$_x05$_x05"
_x40="$_x35$_x05"
OID_REGEX="$_x40"
# Loose-object path regex (e.g. ab/cdef…): the object id with a "/" after the
# first two hex digits and every hex nibble turned into a character class.
# Mirrors upstream test-lib.sh OIDPATH_REGEX (test_oid_to_path $ZERO_OID | sed …).
OIDPATH_REGEX=$(echo "$ZERO_OID" | sed -e 's,^\(..\),\1/,' -e 's/0/[0-9a-f]/g')
# Canonical SHA-1 empty tree (matches `git hash-object -t tree --stdin </dev/null`).
EMPTY_TREE=4b825dc642cb6eb9a060e54bf8d69288fbee4904
EMPTY_BLOB=e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
u200c=$(printf '\342\200\214')
export OID_REGEX OIDPATH_REGEX _x05 _x35 _x40 ZERO_OID EMPTY_TREE EMPTY_BLOB u200c

# Hash algorithm for test_oid (sha1 / sha256); overridden by test_set_hash / test_detect_hash.
test_hash_algo=
test_set_hash () {
	test_hash_algo="$1"
}
test_detect_hash () {
	test_hash_algo=$(git rev-parse --show-object-format 2>/dev/null) || test_hash_algo=
	case "$test_hash_algo" in
	sha256) ;;
	*) test_hash_algo=sha1 ;;
	esac
}

# test_oid — match git/t test_oid expectations for t0000 (SHA-1 and SHA-256).
test_oid () {
	local hash_algo=
	while test "${1#--hash=}" != "$1"
	do
		hash_algo="${1#--hash=}"
		shift
		test "${hash_algo:-}" = "builtin" && hash_algo=sha1
	done

	local effective=${hash_algo:-${test_hash_algo:-sha1}}
	case "$effective" in
	sha1|builtin) effective=sha1 ;;
	sha256) ;;
	*) echo "unknown-oid"; return ;;
	esac

	case "$1" in
	numeric)
		if test "$effective" = sha256
		then
			echo "1234567890123456789012345678901234567890123456789012345678901234"
		else
			echo "1234567890123456789012345678901234567890"
		fi
		;;
	oid_version) echo "1" ;;
	rawsz)
		if test "$effective" = sha256
		then
			echo 32
		else
			echo 20
		fi
		;;
	hexsz)
		if test "$effective" = sha256
		then
			echo 64
		else
			echo 40
		fi
		;;
	algo) echo "$effective" ;;
	zero)
		if test "$effective" = sha256
		then
			printf '%064d\n' 0
		else
			echo "$ZERO_OID"
		fi
		;;
	*)
		if test -n "$TEST_OID_CACHE_FILE" && test -f "$TEST_OID_CACHE_FILE"
		then
			oid=$(awk -v key="$1" -v algo="$effective" '$1 == key && $2 == algo { print $3; exit }' "$TEST_OID_CACHE_FILE")
			if test -n "$oid"
			then
				echo "$oid"
				return
			fi
		fi
		# Upstream git/t/oid-info (ff_1, deadbeef, …) when the per-test cache was not primed.
		if test -n "${GIT_SOURCE_DIR-}" && test -f "$GIT_SOURCE_DIR/t/oid-info/oid"
		then
			oid=$(awk -v k="$1" -v a="$effective" '
				$1 == k {
					for (i = 2; i <= NF; i++) {
						if ($i ~ "^" a ":") {
							print substr($i, length(a) + 2)
							exit
						}
					}
				}
			' FS='[[:space:]]+' "$GIT_SOURCE_DIR/t/oid-info/oid")
			if test -n "$oid"
			then
				echo "$oid"
				return
			fi
		fi
		case "$1" in
		empty_blob)
			if test "$effective" = sha256
			then
				echo "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813"
			else
				echo "$EMPTY_BLOB"
			fi
			return
			;;
		empty_tree)
			if test "$effective" = sha256
			then
				echo "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321"
			else
				echo "$EMPTY_TREE"
			fi
			return
			;;
		''|*[!0-9]*) ;;
		*)
			if test "$effective" = sha256
			then
				printf '%064d\n' "$1"
			else
				printf '%040d\n' "$1"
			fi
			return
			;;
		esac
		echo "unknown-oid"
		;;
	esac
}

test_oid_cache () {
	{ test -n "$test_hash_algo" || test_detect_hash; } || true
	# Append: some upstream tests (e.g. t7422) call this in a loop; truncating would keep only
	# the last entry and break $(test_oid A) / $(test_oid B) in later cases.
	while read -r name value
	do
		test -z "$name" && continue
		case "$value" in
		sha1:*)
			oid="${value#sha1:}"
			echo "$name sha1 $oid" >>"$TEST_OID_CACHE_FILE"
			eval "test_oid_sha1_$name=\$oid"
			;;
		sha256:*)
			oid="${value#sha256:}"
			echo "$name sha256 $oid" >>"$TEST_OID_CACHE_FILE"
			eval "test_oid_sha256_$name=\$oid"
			;;
		esac
	done
}

# CR/LF helpers
q_to_nul () {
	tr 'Q' '\000'
}

q_to_cr () {
	tr Q '\015'
}

q_to_tab () {
	tr Q '\011'
}

append_cr () {
	sed -e 's/$/Q/' | tr Q '\015'
}

remove_cr () {
	tr '\015' Q | sed -e 's/Q$//'
}

# test_dir_is_empty DIR
test_dir_is_empty () {
	test_path_is_dir "$1" &&
	if test -n "$(ls -a1 "$1" | grep -E -v '^\.\.$|^\.$')"
	then
		echo "Directory '$1' is not empty, it contains:"
		ls -la "$1"
		return 1
	fi
}

# test_bool_env VAR DEFAULT — match git/t (errors to fd 7; empty env = invalid).
test_bool_env () {
	if test $# -ne 2
	then
		echo >&2 "BUG: test_bool_env requires two parameters"
		return 1
	fi
	local _d="$2"
	case "$_d" in
	true|yes|1|false|no|0) ;;
	*)
		echo >&7 "error: test_bool_env requires bool values both for \$$1 and for the default fallback"
		return 1
		;;
	esac
	local val
	if eval "test \"\${$1+set}\" = set"
	then
		eval "val=\"\$$1\""
		if test -z "$val"
		then
			echo >&7 "error: test_bool_env requires bool values both for \$$1 and for the default fallback"
			return 1
		fi
	else
		val="$_d"
	fi
	case "$val" in
	true|yes|1) return 0 ;;
	false|no|0) return 1 ;;
	*)
		echo >&7 "error: test_bool_env requires bool values both for \$$1 and for the default fallback"
		return 1
		;;
	esac
}

# skip_all — set by tests that want to skip everything
skip_all=""

# test_ln_s_add TARGET LINK — create symlink and git add
test_ln_s_add () {
	ln -s "$1" "$2" &&
	git add "$2"
}

# test_cmp_rev [!] REV1 REV2
test_cmp_rev () {
	local _negate=""
	if test "$1" = "!"
	then
		_negate=1
		shift
	fi
	local r1 r2
	r1=$(git rev-parse --verify "$1") &&
	r2=$(git rev-parse --verify "$2") &&
	if test -n "$_negate"
	then
		if test "$r1" != "$r2"
		then
			return 0
		else
			echo >&2 "test_cmp_rev: $1 ($r1) == $2 ($r2) (expected different)"
			return 1
		fi
	else
		if test "$r1" = "$r2"
		then
			return 0
		else
			echo >&2 "test_cmp_rev: $1 ($r1) != $2 ($r2)"
			return 1
		fi
	fi
}

# test_unconfig [-C <dir>] [--worktree] KEY...
test_unconfig () {
	config_dir=
	if test "$1" = -C
	then
		shift
		config_dir=$1
		shift
	fi

	is_worktree=
	if test "$1" = --worktree
	then
		is_worktree=1
		shift
	fi

	git ${config_dir:+-C "$config_dir"} config ${is_worktree:+--worktree} --unset-all "$@" 2>/dev/null
	config_status=$?
	case "$config_status" in
	5)
		config_status=0
		;;
	esac
	return $config_status
}

nongit () {
	test -d non-repo ||
	mkdir non-repo ||
	return 1

	(
		GIT_CEILING_DIRECTORIES=$(pwd) &&
		export GIT_CEILING_DIRECTORIES &&
		cd non-repo &&
		"$@" 2>&7
	)
} 7>&2 2>&4

test_i18ngrep () {
	test_grep "$@"
}

# test_line_count OP N FILE — assert wc -l $FILE $OP $N (e.g., = 1)
test_line_count () {
	local op="$1" count="$2" file="$3"
	local actual
	actual=$(wc -l <"$file")
	actual=$(echo "$actual" | tr -d ' ')
	if test "$actual" "$op" "$count"
	then
		return 0
	else
		echo >&2 "test_line_count: expected $count lines ($op), got $actual in '$file'"
		return 1
	fi
}

# test_must_be_empty FILE — assert FILE has zero bytes
test_must_be_empty () { test ! -s "$1"; }

test_have_prereq () {
	local _p="$1"
	case "$_p" in
	*,*)
		local _saveIFS=$IFS
		IFS=','
		for _p in $_p
		do
			IFS=$_saveIFS
			if ! test_have_prereq "$_p"
			then
				return 1
			fi
			IFS=','
		done
		IFS=$_saveIFS
		return 0
		;;
	esac
	# Handle negation: !PREREQ means "PREREQ is NOT set"
	if test "${_p#!}" != "$_p"; then
		local _neg="${_p#!}"
		if test_have_prereq "$_neg"
		then
			missing_prereq=$_p
			return 1
		fi
		return 0
	fi

	case " $lazily_tested_prereq " in
	*" $_p "*)
		;;
	*)
		case " $lazily_testable_prereq " in
		*" $_p "*)
			eval "script=\$test_prereq_lazily_${_p}"
			if test_run_lazy_prereq "$_p" "$script"
			then
				test_set_prereq "$_p"
			fi
			lazily_tested_prereq="$lazily_tested_prereq$_p "
			;;
		esac
		;;
	esac

	case "$_p" in
	POSIXPERM) return 0 ;;
	SYMLINKS)  return 0 ;;
	PIPE)      command -v mkfifo >/dev/null 2>&1 && return 0 ; missing_prereq=$_p; return 1 ;;
	SANITY)    return 0 ;;
	FUNNYNAMES) return 0 ;;
	FILEMODE)  return 0 ;;
	COLON_DIR) return 0 ;;
	BSLASHPSPEC) return 0 ;;
	MINGW)     missing_prereq=$_p; return 1 ;;  # Not on Windows
	CYGWIN)    missing_prereq=$_p; return 1 ;;  # Not on Cygwin
	PERL)      command -v perl >/dev/null 2>&1 && return 0 ; missing_prereq=$_p; return 1 ;;
	PERL_TEST_HELPERS) command -v perl >/dev/null 2>&1 && return 0 ; missing_prereq=$_p; return 1 ;;
	GZIP)      command -v gzip >/dev/null 2>&1 && return 0 ; missing_prereq=$_p; return 1 ;;
	FAKENC)    perl -MIO::Socket::INET -e 'exit 0' 2>/dev/null && return 0 ; missing_prereq=$_p; return 1 ;;
	CURL)      command -v curl >/dev/null 2>&1 && return 0 ; missing_prereq=$_p; return 1 ;;
	CGIPASSAUTH) eval "test \"\${_prereq_${_p}:-}\" = set" && return 0; missing_prereq=$_p; return 1 ;;
	*)
		# Check dynamic prereqs set by test_set_prereq
		local _var="_prereq_${_p}"
		if eval "test \"\${${_var}:-}\" = set"
		then
			return 0
		fi
		missing_prereq=$_p
		return 1
		;;
	esac
}

test_set_prereq () {
	eval "_prereq_$1=set"
}

# Grit implements the traditional loose-ref + packed-refs layout (not reftable).
test_set_prereq REFFILES

# Python prerequisite (matches upstream git/t test-lib NO_PYTHON).
if test -z "${NO_PYTHON-}"
then
	test_set_prereq PYTHON
fi

# Lazy prerequisites (git/t0000 nested-lazy): script runs in a temp dir under trash.
lazily_testable_prereq=
lazily_tested_prereq=

test_lazy_prereq () {
	lazily_testable_prereq="$lazily_testable_prereq$1 "
	eval "test_prereq_lazily_$1=\"\$2\""
}

test_run_lazy_prereq () {
	_name="$1"
	_script="$2"
	_pd="$TRASH_DIRECTORY/prereq-test-dir-$_name"
	rm -rf "$_pd"
	mkdir -p "$_pd" &&
	(
		cd "$_pd" && eval "$_script"
	)
	_ret=$?
	rm -rf "$_pd"
	return "$_ret"
}

# TAR for tests that need it
TAR=${TAR:-tar}
export TAR

PERL_PATH=${PERL_PATH:-$(command -v perl 2>/dev/null || echo /usr/bin/perl)}
export PERL_PATH

# test_set_port VAR — assign a random port (or use existing value)
test_set_port () {
	local _varname="$1"
	eval "local _existing=\${${_varname}:-}"
	if test -z "$_existing"
	then
		# Pick a random port in the ephemeral range
		eval "${_varname}=$((10000 + (RANDOM % 50000)))"
	fi
}

# test_skip_or_die VAR MSG — skip test or die based on env var
test_skip_or_die () {
	if test_bool_env "$1" false
	then
		error "$2"
	fi
	skip_all="$2"
	test_done
}

# error MSG — print an error and exit (fd 7 = original stderr, matches git test-lib)
error () {
	echo "error: $*" >&7
	_error_exit
}

# test_env — run command with additional env vars
# Usage: test_env VAR=VALUE ... command args
# Works with both binaries and shell functions
test_env () {
	local _te_vars=""
	local _te_ret
	while test $# -gt 0; do
		case "$1" in
		*=*)
			export "$1"
			_te_vars="$_te_vars $1"
			shift
			;;
		*)
			break
			;;
		esac
	done
	"$@"
	_te_ret=$?
	# Unset the exported variables
	for _te_v in $_te_vars; do
		unset "${_te_v%%=*}"
	done
	return $_te_ret
}

test_lazy_prereq ICONV '
	test -z "$NO_ICONV" &&
	iconv -f utf8 -t utf8 </dev/null
'

# Match git/t/test-lib.sh: far-future dates and 64-bit commit-graph generation overflow.
test_lazy_prereq TIME_IS_64BIT 'test-tool date is64bit'
test_lazy_prereq TIME_T_IS_64BIT 'test-tool date time_t-is64bit'

# Filesystem aliases NFC and NFD UTF-8 path spellings (macOS / HFS+). Matches git/t/test-lib.sh.
# Set GIT_TEST_UTF8_NFD_TO_NFC=true to force on CI where the FS is not normalization-sensitive.
test_lazy_prereq UTF8_NFD_TO_NFC '
	test "$GIT_TEST_UTF8_NFD_TO_NFC" = true ||
	test "$GIT_TEST_UTF8_NFD_TO_NFC" = 1 ||
	(
		auml=$(printf "\303\244") &&
		aumlcdiar=$(printf "\141\314\210") &&
		>"$auml" &&
		test -f "$aumlcdiar"
	)
'

# write_script FILE [INTERPRETER] — write a script from stdin
write_script () {
	{
		echo "#!${2-/bin/sh}" &&
		cat
	} >"$1" &&
	chmod +x "$1"
}

# test_hook [options] HOOKNAME — write or manipulate a hook (matches git/t/test-lib-functions.sh).
#   -C <dir>  run git rev-parse from that directory (bare: no .git; non-bare: has .git)
#   --setup / --clobber  as upstream
#   --disable  chmod -x an existing hook (must exist)
#   --remove   rm -f an existing hook (must exist)
test_hook () {
	local setup= clobber= disable= remove= indir=
	while test $# != 0
	do
		case "$1" in
		-C)
			indir="$2"
			shift 2
			;;
		--setup)
			setup=t
			shift
			;;
		--clobber)
			clobber=t
			setup=t
			shift
			;;
		--disable)
			disable=t
			shift
			;;
		--remove)
			remove=t
			shift
			;;
		-*)
			echo >&2 "BUG: test_hook: invalid argument: $1"
			exit 99
			;;
		*)
			break
			;;
		esac
	done
	local git_dir hook_dir hook_file
	git_dir=$(git -C "$indir" rev-parse --absolute-git-dir) &&
	hook_dir="$git_dir/hooks" &&
	hook_file="$hook_dir/$1" &&
	if test -n "$disable$remove"
	then
		test_path_is_file "$hook_file" &&
		if test -n "$disable"
		then
			chmod -x "$hook_file"
		elif test -n "$remove"
		then
			rm -f "$hook_file"
		fi &&
		return 0
	fi &&
	if test -z "$clobber"
	then
		test_path_is_missing "$hook_file"
	fi &&
	if test -z "$setup$clobber"
	then
		test_when_finished "rm -f \"$hook_file\""
	fi &&
	mkdir -p "$hook_dir" &&
	write_script "$hook_file"
}

# Look for trace2 region enter/leave in a trace file (from GIT_TRACE2_EVENT).
#	test_region [!] <category> <label> <tracefile>
#
# If the first parameter is !, the region must not appear.
test_region () {
	local expect_exit=0
	if test "$1" = "!"
	then
		expect_exit=1
		shift
	fi

	grep -e	'"region_enter".*"category":"'"$1"'","label":"'"$2"\" "$3"
	exitcode=$?

	if test $exitcode != $expect_exit
	then
		return 1
	fi

	grep -e	'"region_leave".*"category":"'"$1"'","label":"'"$2"\" "$3"
	exitcode=$?

	if test $exitcode != $expect_exit
	then
		return 1
	fi

	return 0
}

# Check that the given config key has the expected value.
#
#    test_cmp_config [-C <dir>] <expected-value>
#                    [<git-config-options>...] <config-key>
test_cmp_config () {
	local GD &&
	if test "$1" = "-C"
	then
		shift &&
		GD="-C $1" &&
		shift
	fi &&
	printf "%s\n" "$1" >expect.config &&
	shift &&
	git $GD config "$@" >actual.config &&
	test_cmp expect.config actual.config
}

test_commit () {
	local notick= signoff= indir= tag=light message= file= contents= author=
	local echo=echo append=
	while test $# != 0
	do
		case "$1" in
		--notick) notick=yes; shift ;;
		--signoff) signoff="$1"; shift ;;
		--no-tag) tag=none; shift ;;
		--annotate) tag=annotate; shift ;;
		--author) author="$2"; shift 2 ;;
		--date)
			notick=yes
			GIT_COMMITTER_DATE="$2"
			GIT_AUTHOR_DATE="$2"
			shift 2
			;;
		-C) indir="$2"; shift 2 ;;
		--append) append=yes; shift ;;
		--printf) echo=printf; shift ;;
		*) break ;;
		esac
	done
	message="${1:?test_commit}" && shift
	file="${1:-$message.t}" && { test $# -gt 0 && shift || true; }
	contents="${1:-$message}" && { test $# -gt 0 && shift || true; }
	if test -z "$notick"; then
		test_tick
	fi
	_target="$file"
	test -n "$indir" && _target="$indir/$file"
	if test -n "$append"
	then
		$echo "$contents" >>"$_target" || return 1
	else
		$echo "$contents" >"$_target" || return 1
	fi
	if test -n "$indir"
	then
		git -C "$indir" add "$file" || return 1
		git -C "$indir" commit -q ${signoff:+$signoff} ${author:+--author "$author"} -m "$message" ||
			return 1
		case "$tag" in
		none) ;;
		light) git -C "$indir" tag "${1:-$message}" || return 1 ;;
		annotate)
			if test -z "$notick"; then
				test_tick
			fi
			git -C "$indir" tag -a -m "$message" "${1:-$message}" || return 1
			;;
		esac
	else
		git add "$file" || return 1
		git commit -q ${signoff:+$signoff} ${author:+--author "$author"} -m "$message" ||
			return 1
		case "$tag" in
		none) ;;
		light) git tag "${1:-$message}" || return 1 ;;
		annotate)
			if test -z "$notick"; then
				test_tick
			fi
			git tag -a -m "$message" "${1:-$message}" || return 1
			;;
		esac
	fi
}

test_merge () {
	local message="${1:?test_merge}" && shift
	test_tick &&
	git merge -m "$message" "$@" &&
	git tag "$message"
}

test_commit_bulk () {
	local indir= ref=HEAD n=
	while test $# != 0
	do
		case "$1" in
		-C) indir="$2"; shift 2 ;;
		--ref) ref="$2"; shift 2 ;;
		*) n="$1"; shift; break ;;
		esac
	done
	(
		test -n "$indir" && cd "$indir"
		local i=1
		while test "$i" -le "$n"
		do
			local message="commit $i"
			test_tick &&
			echo "$message" >"bulk-$i.t" &&
			git add "bulk-$i.t" &&
			git commit -m "$message" || return 1
			i=$((i + 1))
		done &&
		# Match `git fast-import` with unpacklimit=0: objects land in a pack. Grit stores loose
		# objects from fast-import-style bulk paths unless we repack (t5332 verbatim reuse).
		git repack -a -d -q
	)
}

# Match git/t/test-lib-functions.sh: chmod on disk and record in index for every path argument.
test_chmod () {
	chmod "$@" &&
	git update-index --add "--chmod=$@"
}

debug () {
	"$@"
}

# Evaluate $2 and check $1 == stdout.
test_cmp () {
	diff -u "$1" "$2"
}

# Assert `git show` body matches a file or stdin (upstream test-lib-functions.sh).
# Usage: test_commit_message <rev> [-m <msg> | <file>]
test_commit_message () {
	msg_file=expect.msg
	case $# in
	3)
		if test "$2" = "-m"
		then
			printf '%s\n' "$3" >"$msg_file"
		else
			echo >&2 "test_commit_message: expected -m as second argument"
			exit 99
		fi
		;;
	2)
		msg_file="$2"
		;;
	1)
		cat >"$msg_file"
		;;
	*)
		echo >&2 "test_commit_message: bad usage"
		exit 99
		;;
	esac
	git show --no-patch --pretty=format:%B "$1" -- >actual.msg &&
	test_cmp "$msg_file" actual.msg
}

# Persist shell variables across test subshells.  Writes name=value pairs
# to a file that later subshells source on startup.  Usage:
#   test_export newf oldf f5id
test_export () {
	local _ef="$TRASH_DIRECTORY/.test-exports"
	for _var in "$@"; do
		local _val
		eval "_val=\"\$$_var\""
		# Remove any previous definition of this variable.
		if test -f "$_ef"; then
			sed -i "/^${_var}=/d" "$_ef"
		fi
		# Quote the value with single quotes (escape existing ones).
		local _escaped
		_escaped=$(printf '%s' "$_val" | sed "s/'/'\\\\''/g")
		printf "%s='%s'\n" "$_var" "$_escaped" >>"$_ef"
	done
}

test_cleanup=:

test_when_finished () {
	test_cleanup="{ $*
		} && (exit \"\$eval_ret\"); eval_ret=\$?; $test_cleanup"
}

test_atexit_cleanup=:

test_atexit () {
	test_atexit_cleanup="{ $*
		} && (exit \"\$eval_ret\"); eval_ret=\$?; $test_atexit_cleanup"
}

test_atexit_handler () {
	test : != "$test_atexit_cleanup" || return 0
	test_eval_ "$test_atexit_cleanup"
	test_atexit_cleanup=:
}

test_eval_inner_ () {
	local _eval_inner_ret
	# Nested scripts from lib-subtest.sh set TEST_OUTPUT_DIRECTORY_OVERRIDE; for those we
	# reset cwd around each test body (subtests have no trash-root setup_trash cd).
	# Top-level tests do not set it; cwd must persist across test_expect_success blocks
	# (matches upstream git/t; e.g. t5406-remote-rejects).
	if test -n "${TEST_OUTPUT_DIRECTORY_OVERRIDE:-}" &&
		test -z "${TEST_LIB_INHERIT_CWD-}"
	then
		cd "$TRASH_DIRECTORY" || exit 1
	fi
	eval "$1"
	_eval_inner_ret=$?
	if test -n "${TEST_OUTPUT_DIRECTORY_OVERRIDE:-}" &&
		test -z "${TEST_LIB_INHERIT_CWD-}"
	then
		cd "$TRASH_DIRECTORY" || exit 1
	fi
	return "$_eval_inner_ret"
}

# Run test body with stdin / stdout / stderr wired like git's test-lib (fd 3/4).
test_eval_ () {
	test_eval_inner_ </dev/null >&3 2>&4 "$1"
	return $?
}

test_run_ () {
	test_cleanup=:
	expecting_failure=$2
	# Do not use command substitution to prepend `cd "$TRASH_DIRECTORY"`:
	# `var=$(printf ... "$1")` parses $1 in the subshell and executes backticks
	# inside it, corrupting bodies that embed backticks in heredocs (t0040).
	# test_expect_success already cds to TRASH_DIRECTORY before calling us.
	test_eval_ "$1"
	eval_ret=$?
	if test -z "$immediate" || test "$eval_ret" -eq 0 ||
		{ test -n "$expecting_failure" && test "$test_cleanup" != ":"; }
	then
		test_eval_ "$test_cleanup"
	fi
	return "$eval_ret"
}

# Normal exits set GIT_EXIT_OK so the EXIT trap does not print FATAL (git test-lib).
GIT_EXIT_OK=

die () {
	code=$?
	test_atexit_handler || code=$?
	test_lib_restore_path
	if test -n "$GIT_EXIT_OK"
	then
		exit "$code"
	else
		echo >&5 "FATAL: Unexpected exit with code $code"
		exit 1
	fi
}

trap 'die' EXIT

_error_exit () {
	GIT_EXIT_OK=t
	exit 1
}

# Process-wide cleanup chain (Git `test_atexit`), run from `test_done` before
# trash teardown so daemons can stop while sockets/paths still exist.
test_atexit_cleanup=:
test_atexit () {
	test "${BASH_SUBSHELL-0}" = 0 ||
		(echo >&2 "BUG: test_atexit does nothing in a subshell"; exit 99)
	test_atexit_cleanup="{ $*
		} && (exit \"\$eval_ret\"); eval_ret=\$?; $test_atexit_cleanup"
}

test_atexit_handler () {
	test : != "$test_atexit_cleanup" || return 0
	eval "$test_atexit_cleanup"
	test_atexit_cleanup=:
}

. "$TEST_DIRECTORY"/test-lib-tap.sh
if match_pattern_list "$this_test" "$GIT_SKIP_TESTS"
then
	skip_all="skip all tests in $this_test"
	test_done
fi

test_must_be_empty () {
	if test -s "$1"
	then
		echo "file '$1' is not empty"
		cat "$1"
		return 1
	fi
}

test_ref_exists () {
	git rev-parse --verify -q "$1" >/dev/null 2>&1
}

test_ref_missing () {
	! test_ref_exists "$1"
}

test_path_is_file_not_symlink () {
	test -f "$1" && ! test -L "$1"
}

test_path_is_dir_not_symlink () {
	test -d "$1" && ! test -L "$1"
}

test_expect_code () {
	local expected_code="$1"
	local actual_code
	shift
	"$@"
	actual_code=$?
	if test "$actual_code" = "$expected_code"
	then
		return 0
	else
		echo >&2 "test_expect_code: expected exit code $expected_code, got $actual_code from: $*"
		return 1
	fi
}

test_match_signal () {
	local sig="$1"
	local code="$2"
	local expected=$((128 + sig))
	test "$code" = "$expected"
}

test_must_be_empty () {
	if test -s "$1"
	then
		echo >&2 "test_must_be_empty: file '$1' is not empty"
		return 1
	fi
	return 0
}

test_line_count () {
	local op="$1"
	local count="$2"
	local file="$3"
	local actual
	actual=$(wc -l <"$file")
	# trim whitespace
	actual=$(echo "$actual" | tr -d ' ')
	if test "$actual" "$op" "$count"
	then
		return 0
	else
		echo >&2 "test_line_count: expected $count lines ($op), got $actual in '$file'"
		return 1
	fi
}

# test_stdout_line_count OP N CMD...
# Run CMD and assert wc -l on its stdout.
test_stdout_line_count () {
	local op="$1"
	local count="$2"
	shift 2
	local tmp="${TRASH_DIRECTORY}/.stdout.$$"
	local rc
	"$@" >"$tmp" &&
	test_line_count "$op" "$count" "$tmp"
	rc=$?
	rm -f "$tmp"
	return $rc
}

test_match_signal () {
	if test "$2" = "$((128 + $1))"
	then
		return 0
	elif test "$2" = "$((256 + $1))"
	then
		return 0
	fi
	return 1
}

# Read up to "$1" bytes (or to EOF) from stdin and write them to stdout.
test_copy_bytes () {
	dd ibs=1 count="$1" 2>/dev/null
}

# Pkt-line helpers
packetize () {
	if test $# -gt 0
	then
		packet="$*"
		printf '%04x%s' "$((4 + ${#packet}))" "$packet"
	else
		test-tool pkt-line pack
	fi
}

packetize_raw () {
	test-tool pkt-line pack-raw-stdin
}

depacketize () {
	test-tool pkt-line unpack
}

# Build option stub — return reasonable defaults
build_option () {
	case "$1" in
	sizeof-size_t) echo 8 ;;
	*) echo "" ;;
	esac
}

# Extract remote HTTPS URLs from GIT_TRACE2_EVENT output
test_remote_https_urls() {
	grep -e '"event":"child_start".*"argv":\["git-remote-https",".*"\]' |
		sed -e 's/{"event":"child_start".*"argv":\["git-remote-https","//g' \
		    -e 's/"\]}//g'
}

# Convert Q to tab, Z to space.
qz_to_tab_space () {
	tr QZ '\011\040'
}

# Convert LF to NUL.
lf_to_nul () {
	tr '\012' '\000'
}

# Convert NUL to LF.
nul_to_q () {
	tr '\000' Q
}

# Append CR to each line.
append_cr () {
	sed -e 's/$/Q/' | tr Q '\015'
}

# Remove CR from each line.
remove_cr () {
	tr -d '\015'
}
