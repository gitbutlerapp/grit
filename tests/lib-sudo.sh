# Helpers for running git commands under sudo.

# Runs a scriplet passed through stdin under sudo.
run_with_sudo () {
	local ret
	local RUN="$TEST_DIRECTORY/$$.sh"
	local shell="${TEST_SHELL_PATH:-/bin/sh}"
	write_script "$RUN" "$shell"
	# avoid calling "$RUN" directly so sudo doesn't get a chance to
	# override the shell, add additional restrictions or even reject
	# running the script because its security policy deem it unsafe
	sudo env PATH="$PATH" "$shell" -c "\"$RUN\""
	ret=$?
	rm -f "$RUN"
	return $ret
}
