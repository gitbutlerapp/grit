# Harness helpers for TAP output compatible with git/t/test-lib.sh (t0000 self-tests).
# Sourced from test-lib.sh after TEST_DIRECTORY is set.

# Upstream `test-lib-functions.sh` helper; several sparse/mv tests rely on it.
test_path_exists () { test -e "$1"; }

# Minimal URI escaping for bundle-uri / git-svn style tests (lib-bundle-uri-protocol.sh).
test_uri_escape () {
	sed 's/ /%20/g'
}

# From upstream test-lib-functions.sh: compare two config files via `git config --list --file`.
test_cmp_config_output () {
	git config --list --file="$1" >config-expect &&
	git config --list --file="$2" >config-actual &&
	sort config-expect >sorted-expect &&
	sort config-actual >sorted-actual &&
	test_cmp sorted-expect sorted-actual
}

# Defaults (may be set by environment before parse)
GIT_SKIP_TESTS=${GIT_SKIP_TESTS:-}
run_list=
verbose=
immediate=
invert_exit_code=
trace=
verbose_only=
debug=
quiet=
help=
tee=
valgrind=
valgrind_only=
stress=
root=
unset store_arg_to opt_required_arg

mark_option_requires_arg() {
	if test -n "$opt_required_arg"
	then
		echo "error: options that require args cannot be bundled" \
			"together: '$opt_required_arg' and '$1'" >&2
		exit 1
	fi
	opt_required_arg=$1
	store_arg_to=$2
}

parse_option() {
	opt="$1"
	case "$opt" in
	-d|--d|--de|--deb|--debu|--debug)
		debug=t ;;
	-i|--i|--im|--imm|--imme|--immed|--immedi|--immedia|--immediat|--immediate)
		immediate=t ;;
	-l|--l|--lo|--lon|--long|--long-|--long-t|--long-te|--long-tes|--long-test|--long-tests)
		GIT_TEST_LONG=t
		export GIT_TEST_LONG ;;
	-r)
		mark_option_requires_arg "$opt" run_list
		;;
	--run=*)
		run_list=${opt#--*=} ;;
	-h|--h|--he|--hel|--help)
		help=t ;;
	-v|--v|--ve|--ver|--verb|--verbo|--verbos|--verbose)
		verbose=t ;;
	--verbose-only=*)
		verbose_only=${opt#--*=}
		;;
	-q|--q|--qu|--qui|--quie|--quiet)
		test -z "$HARNESS_ACTIVE" && quiet=t ;;
	--with-dashes|--no-bin-wrappers|--no-color) ;;
	--va|--val|--valg|--valgr|--valgri|--valgrin|--valgrind)
		valgrind=memcheck
		tee=t ;;
	--valgrind=*)
		valgrind=${opt#--*=}
		tee=t ;;
	--valgrind-only=*)
		valgrind_only=${opt#--*=}
		tee=t ;;
	--tee)
		tee=t ;;
	--root=*)
		root=${opt#--*=} ;;
	--chain-lint|--no-chain-lint) ;;
	-x)
		trace=t ;;
	-V|--verbose-log)
		verbose_log=t
		tee=t ;;
	--write-junit-xml|--github-workflow-markup) ;;
	--stress|--stress=*|--stress-jobs=*|--stress-limit=*) ;;
	--invert-exit-code)
		invert_exit_code=t ;;
	*)
		echo "error: unknown test option '$opt'" >&2
		exit 1 ;;
	esac
}

# shellcheck disable=SC3045
init_test_harness_options() {
	for opt in "$@"
	do
		if test -n "$store_arg_to"
		then
			eval "$store_arg_to=\$opt"
			store_arg_to=
			opt_required_arg=
			continue
		fi
		case "$opt" in
		--*|-?)
			parse_option "$opt" ;;
		-?*)
			opt=${opt#-}
			while test -n "$opt"
			do
				extra=${opt#?}
				this=${opt%"$extra"}
				opt=$extra
				parse_option "-$this"
			done
			;;
		*)
			echo "error: unknown test option '$opt'" >&2
			exit 1 ;;
		esac
	done
	if test -n "$store_arg_to"
	then
		echo "error: $opt_required_arg requires an argument" >&2
		exit 1
	fi
	if test -n "$valgrind_only" && test -z "$valgrind"
	then
		valgrind=memcheck
	fi
	if test -n "$valgrind" && test -z "$verbose_log"
	then
		verbose=t
	fi
}

match_pattern_list() {
	arg="$1"
	shift
	test -z "$*" && return 1
	(
		set -f
		for pattern_ in $*
		do
			case "$arg" in
			$pattern_)
				exit 0 ;;
			esac
		done
		exit 1
	)
}

match_test_selector_list () {
	operation="$1"
	shift
	title="$1"
	shift
	arg="$1"
	shift
	test -z "$1" && return 0

	# Commas are accepted as separators.
	OLDIFS=$IFS
	IFS=','
	set -- $1
	IFS=$OLDIFS

	# If the first selector is negative we include by default.
	include=
	case "$1" in
		!*) include=t ;;
	esac

	for selector
	do
		orig_selector=$selector

		positive=t
		case "$selector" in
			!*)
				positive=
				selector=${selector##?}
				;;
		esac

		test -z "$selector" && continue

		case "$selector" in
			*-*)
				if expr "z${selector%%-*}" : "z[0-9]*[^0-9]" >/dev/null
				then
					echo "error: $operation: invalid non-numeric in range start: '$orig_selector'" >&2
					exit 1
				fi
				if expr "z${selector#*-}" : "z[0-9]*[^0-9]" >/dev/null
				then
					echo "error: $operation: invalid non-numeric in range end: '$orig_selector'" >&2
					exit 1
				fi
				;;
			*)
				if expr "z$selector" : "z[0-9]*[^0-9]" >/dev/null
				then
					case "$title" in *${selector}*)
						include=$positive
						;;
					esac
					continue
				fi
		esac

		# Short cut for "obvious" cases
		test -z "$include" && test -z "$positive" && continue
		test -n "$include" && test -n "$positive" && continue

		case "$selector" in
			-*)
				if test $arg -le ${selector#-}
				then
					include=$positive
				fi
				;;
			*-)
				if test $arg -ge ${selector%-}
				then
					include=$positive
				fi
				;;
			*-*)
				if test ${selector%%-*} -le $arg \
					&& test $arg -le ${selector#*-}
				then
					include=$positive
				fi
				;;
			*)
				if test $arg -eq $selector
				then
					include=$positive
				fi
				;;
		esac
	done

	test -n "$include"
}

# Compare filesystem paths like upstream test_cmp_fspath (test-lib-functions.sh).
# Used by t0001-init and other tests that compare gitdir: paths on case-insensitive volumes.
test_cmp_fspath () {
	if test "x$1" = "x$2"
	then
		return 0
	fi

	if test true != "$(git config --get --type=bool core.ignorecase)"
	then
		return 1
	fi

	test "x$(echo "$1" | tr A-Z a-z)" = "x$(echo "$2" | tr A-Z a-z)"
}

# File size in bytes (t1006, etc.) when `test-tool path-utils file-size` is unavailable.
test_file_size () {
	test "$#" -eq 1 || BUG "test_file_size needs 1 argument"
	wc -c <"$1" | tr -d ' '
}

# Path exists (file, dir, or symlink); upstream test-lib-functions.sh name.
test_path_exists () { test -e "$1"; }

# Trace2 JSON event stream: assert a child_start argv sequence appears (t6500, t7900, …).
#	test_subcommand [!] <command> <args>... < <trace>
test_subcommand () {
	local negate=
	if test "$1" = "!"
	then
		negate=t
		shift
	fi

	local expr="$(printf '"%s",' "$@")"
	expr="${expr%,}"

	if test -n "$negate"
	then
		! grep "\[$expr\]"
	else
		grep "\[$expr\]"
	fi
}

# Like test_subcommand but allows extra argv after the given prefix.
#	test_subcommand_flex [!] <command> <args>... < <trace>
test_subcommand_flex () {
	local negate=
	if test "$1" = "!"
	then
		negate=t
		shift
	fi

	local expr="$(printf '"%s".*' "$@")"

	if test -n "$negate"
	then
		! grep "\[$expr\]"
	else
		grep "\[$expr\]"
	fi
}
