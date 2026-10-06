#!/usr/bin/env bash
# Run grit harness tests and update the per-test status TOMLs in data/tests/.
#
# Usage:
#   ./scripts/run-tests.sh                     # all in-scope test files
#   ./scripts/run-tests.sh t1                  # all tests/t1*.sh (glob prefix; t1xxx family)
#   ./scripts/run-tests.sh t3200-branch.sh     # single file
#   ./scripts/run-tests.sh t0500 t4064 t4051   # multiple prefixes, .sh paths, or mixes (order preserved, deduped)
#   ./scripts/run-tests.sh --parallel          # all families t0–t9 in parallel (disjoint TOMLs; no merge step)
#
# Options:
#   --timeout N    per-file timeout (default: 120)
#   --quiet        minimal output
#   --verbose, -v  print each test file as it starts ([i/N] name …) before the per-file result line
#   --from NAME    resume: skip tests before NAME (stem or .sh; first match in run order)
#   --parallel     run one process per Git test family (t0–t9); families write disjoint TOMLs
#   --family N     only tests whose group is tN (N is 0–9 or t0–t9)
#   --data-dir PATH  write status TOMLs under this directory instead of data/tests
#                    (isolated run: canonical data and dashboards untouched)
#                    Per-file harness logs are written under PATH/logs/ when set.
#   --list FILE    read test file names to run (one per line; # starts a comment)
#   --strict       exit 1 if any explicit/--list target does not resolve to a runnable file,
#                  if the resolved explicit list is empty, if any file has fail>0,
#                  status timeout/error, or 0 tests; print each failing stem and TAP
#                  "not ok" lines (full output in logs/)
#   --dashboard    regenerate docs/ dashboards after the run (off by default)
#   --no-catalog   skip generate-test-files-catalog.py (parent already refreshed the catalog)
#
# Skipped files (in_scope = "skip" in data/tests/<group>/<stem>.toml) are never run.
# After each test file finishes, its status TOML is updated. Dashboards are only
# regenerated when --dashboard is passed.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
TESTS_DIR="$REPO/tests"
DATA_DIR="$REPO/data"
DATA_TESTS="$DATA_DIR/tests"
CATALOG="$REPO/scripts/generate-test-files-catalog.py"
APPLY="$REPO/scripts/apply-test-run-results.py"
GEN_DASH="$REPO/scripts/generate-dashboard-from-test-files.py"
BIN="$REPO/target/release/grit-git"
TIMEOUT=120
QUIET=false
VERBOSE=false
TARGET=""
FROM=""
POS=()
PARALLEL=false
FAMILY=""
DATA_DIR_OVERRIDE=""
DASHBOARD=false
NO_CATALOG=false
STRICT=false
LIST_FILE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
  --timeout)
    TIMEOUT="$2"
    shift 2
    ;;
  --quiet)
    QUIET=true
    shift
    ;;
  --verbose|-v)
    VERBOSE=true
    shift
    ;;
  --parallel)
    PARALLEL=true
    shift
    ;;
  --family)
    if [[ $# -lt 2 ]]; then
      echo "ERROR: --family requires a digit 0-9 or t0-t9"
      exit 1
    fi
    FAMILY="$2"
    shift 2
    ;;
  --data-dir)
    if [[ $# -lt 2 ]]; then
      echo "ERROR: --data-dir requires a path"
      exit 1
    fi
    DATA_DIR_OVERRIDE="$2"
    shift 2
    ;;
  --dashboard)
    DASHBOARD=true
    shift
    ;;
  --no-catalog)
    NO_CATALOG=true
    shift
    ;;
  --strict)
    STRICT=true
    shift
    ;;
  --list)
    if [[ $# -lt 2 ]]; then
      echo "ERROR: --list requires a path"
      exit 1
    fi
    LIST_FILE="$2"
    shift 2
    ;;
  --from)
    if [[ $# -lt 2 ]]; then
      echo "ERROR: --from requires a test name (e.g. t1017-foo or t1017-foo.sh)"
      exit 1
    fi
    FROM="$2"
    shift 2
    ;;
  --)
    shift
    POS+=("$@")
    break
    ;;
  -*)
    echo "Unknown option: $1"
    exit 1
    ;;
  *)
    POS+=("$1")
    shift
    ;;
  esac
done

if [[ -n "$LIST_FILE" ]]; then
  if [[ ! -f "$LIST_FILE" ]]; then
    echo "ERROR: --list file not found: $LIST_FILE"
    exit 1
  fi
  while IFS= read -r _list_line || [[ -n "$_list_line" ]]; do
    _list_line="${_list_line%%#*}"
    _list_line="${_list_line#"${_list_line%%[![:space:]]*}"}"
    _list_line="${_list_line%"${_list_line##*[![:space:]]}"}"
    [[ -z "$_list_line" ]] && continue
    POS+=("$_list_line")
  done <"$LIST_FILE"
fi

if [[ "$VERBOSE" == true && "$QUIET" == true ]]; then
  echo "ERROR: --verbose and --quiet are mutually exclusive"
  exit 1
fi

if [[ "$PARALLEL" == true ]]; then
  PY_ARGS=(--timeout "$TIMEOUT")
  [[ "$QUIET" == true ]] && PY_ARGS+=(--quiet)
  [[ "$VERBOSE" == true ]] && PY_ARGS+=(--verbose)
  [[ "$DASHBOARD" == true ]] && PY_ARGS+=(--dashboard)
  [[ -n "$FROM" ]] && PY_ARGS+=(--from "$FROM")
  exec python3 "$REPO/scripts/run-tests-parallel.py" "${PY_ARGS[@]}" -- "${POS[@]}"
fi

if [[ -n "$FAMILY" ]]; then
  case "$FAMILY" in
  t[0-9]|[0-9]) ;;
  *)
    echo "ERROR: --family must be a single digit 0-9 or t0-t9 (got: $FAMILY)"
    exit 1
    ;;
  esac
fi

if [[ -n "$DATA_DIR_OVERRIDE" && "$DATA_DIR_OVERRIDE" != /* ]]; then
  DATA_DIR_OVERRIDE="$REPO/$DATA_DIR_OVERRIDE"
fi
APPLY_DATA="${DATA_DIR_OVERRIDE:-$DATA_TESTS}"
# Per-file harness output for strict diagnostics and CI artifacts.
LOGS_DIR=""
STRICT_LOGS_TMP=""
if [[ -n "$DATA_DIR_OVERRIDE" ]]; then
  LOGS_DIR="$APPLY_DATA/logs"
  mkdir -p "$LOGS_DIR"
elif [[ "$STRICT" == true ]]; then
  STRICT_LOGS_TMP="$(mktemp -d "${TMPDIR:-/tmp}/grit-run-tests-logs.XXXXXX")"
  LOGS_DIR="$STRICT_LOGS_TMP"
fi

# GNU coreutils `timeout` is not installed by default on macOS; `gtimeout` may be.
# Built after parsing `--timeout` so the wrapper uses the final TIMEOUT value.
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_PREFIX=(timeout "$TIMEOUT")
elif command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT_PREFIX=(gtimeout "$TIMEOUT")
else
  TIMEOUT_PREFIX=()
fi

if [[ ! -x "$BIN" ]]; then
  echo "ERROR: grit binary not found at $BIN"
  echo "Run: cargo build --release"
  exit 1
fi

rm -f "$TESTS_DIR/grit"
cp "$BIN" "$TESTS_DIR/grit"
chmod +x "$TESTS_DIR/grit"

mkdir -p "$DATA_DIR"
if [[ "$NO_CATALOG" != true ]]; then
  python3 "$CATALOG"
fi

if [[ ! -d "$DATA_TESTS" ]]; then
  echo "ERROR: $DATA_TESTS was not created"
  exit 1
fi

if [[ -n "$FAMILY" ]]; then
  export GRIT_FAMILY_FILTER="$FAMILY"
else
  unset GRIT_FAMILY_FILTER 2>/dev/null || true
fi

GRIT_RUN_TESTS_STRICT=0
[[ "$STRICT" == true ]] && GRIT_RUN_TESTS_STRICT=1
export GRIT_RUN_TESTS_STRICT

# Explicit list mode: --list was passed and/or argv named test files. Distinct from
# whether any lines survived comment/blank stripping (empty smoke list must not
# fall through to the full in-scope suite).
GRIT_RUN_TESTS_EXPLICIT_LIST=0
if [[ -n "$LIST_FILE" || ${#POS[@]} -gt 0 ]]; then
  GRIT_RUN_TESTS_EXPLICIT_LIST=1
fi
export GRIT_RUN_TESTS_EXPLICIT_LIST

# Build list of files to run: skip in_scope=skip. Use a read loop instead of
# Bash 4 `mapfile` so the runner works with macOS' default Bash 3.
# Scope is always read from the canonical data/tests tree; --data-dir only
# redirects where results are written.
FILES=()
FILE_LIST_TMP="$(mktemp)"
if ! python3 - "$DATA_TESTS" "$TESTS_DIR" "$FROM" "${POS[@]}" >"$FILE_LIST_TMP" <<'PY'
import os, sys, glob, tomllib

data_dir, tests_dir, from_stem = sys.argv[1], sys.argv[2], sys.argv[3]
targets = sys.argv[4:]
strict = os.environ.get("GRIT_RUN_TESTS_STRICT") == "1"
explicit_list = os.environ.get("GRIT_RUN_TESTS_EXPLICIT_LIST") == "1"
if from_stem.endswith(".sh"):
    from_stem = from_stem[:-3]

# stem -> {"in_scope": ..., "group": <parent dir name>}
rows = {}
for path in sorted(glob.glob(os.path.join(data_dir, "*", "*.toml"))):
    stem = os.path.splitext(os.path.basename(path))[0]
    try:
        with open(path, "rb") as f:
            fields = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError):
        continue
    rows[stem] = {
        "in_scope": str(fields.get("in_scope", "yes")),
        "group": os.path.basename(os.path.dirname(path)),
    }

def want_file(base: str) -> bool:
    row = rows.get(base)
    if row is None:
        return True
    return row["in_scope"].strip().lower() != "skip"


def normalize_target(raw: str) -> str:
    s = raw.strip()
    if not s:
        return s
    for prefix in ("tests/", "./tests/"):
        if s.startswith(prefix):
            s = s[len(prefix) :]
            break
    return os.path.basename(s)


def expand_one(target):
    out = []
    if target.endswith(".sh"):
        base = target[:-3]
        if want_file(base):
            p = os.path.join(tests_dir, target)
            if os.path.isfile(p):
                out.append(target)
        return out
    if target:
        for p in sorted(glob.glob(os.path.join(tests_dir, target + "*.sh"))):
            base = os.path.basename(p)[:-3]
            if want_file(base):
                out.append(os.path.basename(p))
        return out
    return out


candidates = []
if explicit_list and not targets:
    print(
        "ERROR: %sexplicit test list has no runnable entries (empty or comment-only --list file)."
        % ("strict mode: " if strict else ""),
        file=sys.stderr,
    )
    sys.exit(1)

if targets:
    seen = set()
    unmatched = []
    for raw in targets:
        t = normalize_target(raw)
        if not t:
            continue
        got = expand_one(t)
        if not got:
            if strict and explicit_list:
                unmatched.append(raw)
            else:
                print(
                    "WARNING: no test files matched %r (skipped, typo, or missing under %s)"
                    % (raw, tests_dir),
                    file=sys.stderr,
                )
            continue
        for fn in got:
            if fn not in seen:
                seen.add(fn)
                candidates.append(fn)
    if strict and explicit_list and unmatched:
        for raw in unmatched:
            print(
                "ERROR: strict mode: no test files matched %r (typo, missing, or in_scope=skip under %s)"
                % (raw, tests_dir),
                file=sys.stderr,
            )
        sys.exit(1)
else:
    for base in sorted(rows):
        if rows[base]["in_scope"].strip().lower() == "skip":
            continue
        fn = base + ".sh"
        p = os.path.join(tests_dir, fn)
        if os.path.isfile(p):
            candidates.append(fn)

if from_stem:
    idx = None
    for i, c in enumerate(candidates):
        base = os.path.basename(c)
        stem = base[:-3] if base.endswith(".sh") else base
        if stem == from_stem:
            idx = i
            break
    if idx is None:
        print(
            "ERROR: --from %r: that test is not in this run list (wrong name, skipped, or no match)."
            % (from_stem,),
            file=sys.stderr,
        )
        sys.exit(1)
    candidates = candidates[idx:]

ff = os.environ.get("GRIT_FAMILY_FILTER", "").strip()
def canon_family(s):
    if not s:
        return ""
    if s.startswith("t") and len(s) >= 2 and s[1].isdigit():
        return "t" + s[1]
    if s.isdigit():
        return "t" + s[0]
    return ""

want = canon_family(ff)
if want:
    candidates = [
        c
        for c in candidates
        if rows.get(c[:-3] if c.endswith(".sh") else c, {}).get("group", "") == want
    ]

if strict and explicit_list and not candidates:
    print(
        "ERROR: strict mode: explicit test list resolved to zero runnable files.",
        file=sys.stderr,
    )
    sys.exit(1)

for c in candidates:
    print(c)
PY
then
  rm -f "$FILE_LIST_TMP"
  exit 1
fi
while IFS= read -r file; do
  [[ -n "$file" ]] && FILES+=("$file")
done <"$FILE_LIST_TMP"
rm -f "$FILE_LIST_TMP"

if [[ ${#FILES[@]} -eq 0 ]]; then
  echo "No test files to run (all skipped or no match)." >&2
  if [[ "$STRICT" == true ]]; then
    echo "ERROR: strict mode requires at least one test file to run." >&2
    exit 1
  fi
  if [[ "$DASHBOARD" == true && -z "$DATA_DIR_OVERRIDE" ]]; then
    python3 "$GEN_DASH"
  fi
  exit 0
fi

RUN_NOTE=""
for _f in "${FILES[@]}"; do
  if [[ "$_f" == "t0410-partial-clone.sh" ]]; then
    RUN_NOTE=" (t0410-partial-clone.sh: no per-file timeout — long promisor/fetch suite)"
    break
  fi
done
[[ "$QUIET" != true ]] && echo "Running ${#FILES[@]} test file(s) (timeout: ${TIMEOUT}s)${RUN_NOTE}..."

LINE_TMP="$(mktemp)"
trap 'rm -f "$LINE_TMP"; [[ -n "$STRICT_LOGS_TMP" ]] && rm -rf "$STRICT_LOGS_TMP"' EXIT

run_one() {
  # Bash 3 treats empty array expansions as unbound under `set -u`.
  set +u
  local f="$1"
  local base="${f%.sh}"
  local output summary total pass fail status ef
  local git_test_allow_sudo=
  local utf8_nfd_to_nfc=
  local timeout_prefix=("${TIMEOUT_PREFIX[@]}")
  # `command -v perl` can print a bare "perl"; shebangs require an absolute path (t5532-fetch-proxy).
  local perl_abs
  perl_abs="$(type -P perl 2>/dev/null || true)"
  [[ -z "$perl_abs" ]] && perl_abs=/usr/bin/perl
  if [[ "$f" == "t0034-root-safe-directory.sh" ]]; then
    git_test_allow_sudo=YES
  fi
  # macOS NFC/NFD filesystem tests: force prereq on normal Linux CI (Git uses the same for portability).
  if [[ "$f" == "t3910-mac-os-precompose.sh" ]]; then
    utf8_nfd_to_nfc=1
  fi
  # t0410 can exceed any reasonable wall-clock cap on slow hosts; omit `timeout` so we still get # Tests: / TAP summary.
  if [[ "$f" == "t0410-partial-clone.sh" ]]; then
    timeout_prefix=()
  fi
  output=$(
    cd "$TESTS_DIR" &&
      # Cursor/agent shells often export `git () { ./grit "$@"; }`, which overrides the
      # harness `git` wrapper and breaks once a test `cd`s into trash (./grit missing).
      unset -f git grit 2>/dev/null || true &&
      env -u GIT_INDEX_FILE -u GIT_DIR -u GIT_WORK_TREE -u GIT_SEQUENCE_EDITOR \
        -u GIT_EDITOR \
        -u GIT_AUTHOR_DATE -u GIT_COMMITTER_DATE -u test_tick \
        -u GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME \
        -u TRASH_DIRECTORY -u BIN_DIRECTORY -u TEST_OUTPUT_DIRECTORY_OVERRIDE \
        EDITOR=: VISUAL=: LC_ALL=C LANG=C TZ=UTC _prereq_DEFAULT_REPO_FORMAT=set \
        GRIT_TEST_LIB_SUMMARY=1 \
        ${utf8_nfd_to_nfc:+GIT_TEST_UTF8_NFD_TO_NFC=$utf8_nfd_to_nfc} \
        ${git_test_allow_sudo:+GIT_TEST_ALLOW_SUDO=$git_test_allow_sudo} \
        GUST_BIN="$BIN" \
        PERL_PATH="$perl_abs" \
        SHELL_PATH="${SHELL_PATH:-/bin/sh}" \
        GIT_TEST_BUILTIN_HASH=sha1 \
        GIT_TEST_MAINT_AUTO_DETACH=false \
        GIT_DEFAULT_REF_FORMAT="${GIT_TEST_DEFAULT_REF_FORMAT:-files}" \
        GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME=main \
        GIT_SOURCE_DIR="$REPO/git" \
        GIT_CONFIG_NOSYSTEM=1 \
        GIT_CONFIG_PARAMETERS= \
        env -u GIT_CONFIG_GLOBAL -u GIT_CONFIG_SYSTEM \
        "${timeout_prefix[@]}" bash "$f" 2>&1
  ) || true
  set -u
  summary=$(echo "$output" | grep "^# Tests:" | tail -1) || true
  total=0 pass=0 fail=0 status="error"
  if [[ -n "$summary" ]]; then
    total=$(echo "$summary" | sed 's/.*Tests: \([0-9]*\).*/\1/')
    pass=$(echo "$summary" | sed 's/.*Pass: \([0-9]*\).*/\1/')
    fail=$(echo "$summary" | sed 's/.*Fail: \([0-9]*\).*/\1/')
    status="ok"
  else
    status="timeout"
  fi
  ef=$(grep -c 'test_expect_failure' "$TESTS_DIR/$f" 2>/dev/null || true)
  ef=${ef:-0}
  if [[ -n "$LOGS_DIR" ]]; then
    printf '%s\n' "$output" >"$LOGS_DIR/${base}.log"
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$base" "$total" "$pass" "$fail" "$status" "$ef"
}

STRICT_FAIL_STEMS=()

for i in "${!FILES[@]}"; do
  f="${FILES[$i]}"
  if [[ "$VERBOSE" == true && "$QUIET" != true ]]; then
    printf '  [%d/%d] %s ...\n' "$((i + 1))" "${#FILES[@]}" "$f" >&2
  fi
  line=$(run_one "$f")
  base="${f%.sh}"
  printf '%s\n' "$line" >"$LINE_TMP"
  python3 "$APPLY" "$LINE_TMP" --data-dir "$APPLY_DATA"
  if [[ "$QUIET" != true ]]; then
    pass=$(echo "$line" | cut -f3)
    fail=$(echo "$line" | cut -f4)
    total=$(echo "$line" | cut -f2)
    if [[ "$fail" == "0" && "$total" != "0" ]]; then
      mark="✓"
    elif [[ "$total" == "0" ]]; then
      mark="⚠"
    else
      mark="✗"
    fi
    printf "  %s %s (%s/%s)\n" "$mark" "$base" "$pass" "$total"
  fi
  if [[ "$STRICT" == true ]]; then
    _st=$(echo "$line" | cut -f5)
    _fl=$(echo "$line" | cut -f4)
    _tl=$(echo "$line" | cut -f2)
    if [[ "$_fl" != "0" || "$_st" != "ok" || "$_tl" == "0" ]]; then
      STRICT_FAIL_STEMS+=("${line%%$'\t'*}")
    fi
  fi
done

if [[ "$STRICT" == true && ${#STRICT_FAIL_STEMS[@]} -gt 0 ]]; then
  echo "Strict mode: ${#STRICT_FAIL_STEMS[@]} test file(s) did not pass cleanly." >&2
  for _stem in "${STRICT_FAIL_STEMS[@]}"; do
    echo "" >&2
    echo "=== $_stem ===" >&2
    if [[ -f "$LOGS_DIR/${_stem}.log" ]]; then
      grep 'not ok' "$LOGS_DIR/${_stem}.log" >&2 || true
    fi
  done
  if [[ -n "$DATA_DIR_OVERRIDE" && -n "$LOGS_DIR" ]]; then
    echo "" >&2
    echo "Full per-file logs: $LOGS_DIR" >&2
  fi
  exit 1
fi

if [[ "$DASHBOARD" == true && -z "$DATA_DIR_OVERRIDE" ]]; then
  python3 "$GEN_DASH"
fi

if [[ "$QUIET" != true ]]; then
  if [[ -n "$DATA_DIR_OVERRIDE" ]]; then
    echo "Updated status TOMLs under $APPLY_DATA (isolated; canonical data/tests untouched)."
  elif [[ "$DASHBOARD" == true ]]; then
    echo "Updated $DATA_TESTS and dashboards."
  else
    echo "Updated $DATA_TESTS (dashboards not regenerated; pass --dashboard or run scripts/generate-dashboard-from-test-files.py)."
  fi
fi
