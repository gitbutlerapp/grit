//! `test-tool parse-options` — provides the behavior the upstream tests expect.

use std::collections::HashMap;

use super::git_number::{git_parse_signed, git_parse_unsigned};

const PARSE_OPTIONS_HELP: &str = include_str!("parse_options_help.txt");

/// Exit status from `cmd__parse_options` (0 ok, 1 expect mismatch).
pub type ParseOptionsStatus = i32;

#[derive(Debug)]
pub enum ParseOptionsToolError {
    /// `-h` / `--help`: help already printed to stdout, exit 129, empty stderr.
    Help,
    /// Callback returned an error without printing (Git `parse_options` exit 1, empty stderr).
    Silent,
    Fatal(String),
    Bug(String),
}

impl std::fmt::Display for ParseOptionsToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseOptionsToolError::Help => f.write_str("(help)"),
            ParseOptionsToolError::Silent => f.write_str("(silent)"),
            ParseOptionsToolError::Fatal(s) | ParseOptionsToolError::Bug(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ParseOptionsToolError {}

fn env_disallow_abbrev() -> bool {
    std::env::var("GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS")
        .ok()
        .as_deref()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn int_bounds_32() -> (i128, i128) {
    (i32::MIN as i128, i32::MAX as i128)
}

struct CmdElem {
    value: i32,
    opt: Option<(OptMeta, Option<String>, bool)>,
}

#[derive(Clone, Copy)]
struct OptMeta {
    name: &'static str,
    is_cmdmode: bool,
}

struct PoState {
    boolean: i32,
    integer: i32,
    unsigned_integer: u64,
    timestamp: i64,
    i16: i16,
    u16: u16,
    abbrev: i32,
    verbose: i32,
    dry_run: i32,
    quiet: i32,
    string: Option<String>,
    file: Option<String>,
    ambiguous: i32,
    list: Vec<String>,
    length_cb_called: bool,
    length_cb_arg: Option<String>,
    length_cb_unset: bool,
    expect: HashMap<String, String>,
    cmd: CmdElem,
}

impl Default for PoState {
    fn default() -> Self {
        Self {
            boolean: 0,
            integer: 0,
            unsigned_integer: 0,
            timestamp: 0,
            i16: 0,
            u16: 0,
            abbrev: 7,
            verbose: -1,
            dry_run: 0,
            quiet: 0,
            string: None,
            file: None,
            ambiguous: 0,
            list: Vec::new(),
            length_cb_called: false,
            length_cb_arg: None,
            length_cb_unset: false,
            expect: HashMap::new(),
            cmd: CmdElem {
                value: 0,
                opt: None,
            },
        }
    }
}

impl PoState {
    fn touch_integer(
        &mut self,
        new: i32,
        meta: OptMeta,
        arg: Option<&str>,
        unset: bool,
    ) -> Result<(), ParseOptionsToolError> {
        self.integer = new;
        let new_val = self.integer;
        let e = &mut self.cmd;
        if new_val == e.value {
            return Ok(());
        }
        if let Some((prev, prev_arg, prev_unset)) = &e.opt {
            if prev.is_cmdmode || meta.is_cmdmode {
                let o1 = format_opt_display(meta.name, arg, unset);
                let o2 = format_opt_display(prev.name, prev_arg.as_deref(), *prev_unset);
                return Err(ParseOptionsToolError::Fatal(format!(
                    "error: options '{o1}' and '{o2}' cannot be used together\n"
                )));
            }
        }
        e.opt = Some((meta, arg.map(|s| s.to_string()), unset));
        e.value = new_val;
        Ok(())
    }

    fn show_line(&self, expect: &HashMap<String, String>, line: &str, bad: &mut bool) {
        if expect.is_empty() {
            println!("{line}");
            return;
        }
        let Some(colon) = line.find(':') else {
            println!("{line}");
            return;
        };
        let key = &line[..colon];
        let Some(expected_full) = expect.get(key) else {
            println!("{line}");
            return;
        };
        if expected_full != line {
            println!("-{expected_full}");
            println!("+{line}");
            *bad = true;
        }
    }

    fn dump(
        &self,
        expect: &HashMap<String, String>,
        rest: &[String],
    ) -> Result<ParseOptionsStatus, ParseOptionsToolError> {
        let mut bad = false;
        if self.length_cb_called {
            let arg = self.length_cb_arg.as_deref().unwrap_or("not set");
            let u = if self.length_cb_unset { 1 } else { 0 };
            let line = format!("Callback: \"{arg}\", {u}");
            self.show_line(expect, &line, &mut bad);
        }
        self.show_line(expect, &format!("boolean: {}", self.boolean), &mut bad);
        self.show_line(expect, &format!("integer: {}", self.integer), &mut bad);
        self.show_line(expect, &format!("i16: {}", self.i16), &mut bad);
        self.show_line(
            expect,
            &format!("unsigned: {}", self.unsigned_integer),
            &mut bad,
        );
        self.show_line(expect, &format!("u16: {}", self.u16), &mut bad);
        self.show_line(expect, &format!("timestamp: {}", self.timestamp), &mut bad);
        let s = self.string.as_deref().unwrap_or("(not set)");
        self.show_line(expect, &format!("string: {s}"), &mut bad);
        self.show_line(expect, &format!("abbrev: {}", self.abbrev), &mut bad);
        self.show_line(expect, &format!("verbose: {}", self.verbose), &mut bad);
        self.show_line(expect, &format!("quiet: {}", self.quiet), &mut bad);
        self.show_line(
            expect,
            &format!("dry run: {}", if self.dry_run != 0 { "yes" } else { "no" }),
            &mut bad,
        );
        let f = self.file.as_deref().unwrap_or("(not set)");
        self.show_line(expect, &format!("file: {f}"), &mut bad);
        for item in &self.list {
            self.show_line(expect, &format!("list: {item}"), &mut bad);
        }
        for (i, a) in rest.iter().enumerate() {
            self.show_line(expect, &format!("arg {i:02}: {a}"), &mut bad);
        }
        Ok(if bad { 1 } else { 0 })
    }
}

fn format_opt_display(name: &'static str, arg: Option<&str>, unset: bool) -> String {
    if name == "mode34" && !unset {
        if let Some(a) = arg {
            return format!("--mode34={a}");
        }
    }
    format!("--{name}")
}

fn usage_append() -> String {
    PARSE_OPTIONS_HELP.to_string()
}

/// Git's `usage_with_options` is attached only for some parse errors; t0040 expects a bare line
/// for missing values, superfluous `=`, range errors, etc., but appends help for unknown/ambiguous
/// options (see `check_unknown_i18n` and ambiguous-abbrev tests).
fn append_usage_if_unknown(msg: &str) -> String {
    let with_usage = msg.to_string() + &usage_append();
    if msg.starts_with("error: unknown option `")
        || msg.starts_with("error: unknown switch `")
        || msg.starts_with("ambiguous option:")
    {
        with_usage
    } else {
        msg.to_string()
    }
}

fn map_parse_fatal(e: ParseOptionsToolError) -> ParseOptionsToolError {
    match e {
        ParseOptionsToolError::Fatal(m) => {
            ParseOptionsToolError::Fatal(append_usage_if_unknown(&m))
        }
        ParseOptionsToolError::Silent => ParseOptionsToolError::Silent,
        o => o,
    }
}

fn collect_expect(
    map: &mut HashMap<String, String>,
    arg: &str,
) -> Result<(), ParseOptionsToolError> {
    let Some(colon) = arg.find(':') else {
        return Err(ParseOptionsToolError::Fatal(
            "malformed --expect option\n".to_string() + &usage_append(),
        ));
    };
    let key = arg[..colon].to_string();
    if map.insert(key, arg.to_string()).is_some() {
        return Err(ParseOptionsToolError::Fatal(format!(
            "malformed --expect option, duplicate {}\n",
            &arg[..colon]
        )));
    }
    Ok(())
}

/// `test-tool parse-options` — mirrors `cmd__parse_options`.
pub fn run_parse_options(args: &[String]) -> Result<ParseOptionsStatus, ParseOptionsToolError> {
    let disallow_abbrev = env_disallow_abbrev();
    let mut st = PoState::default();
    let argv = args;
    if argv.is_empty() {
        return Err(ParseOptionsToolError::Fatal(
            "usage: test-tool parse-options <options>\n".to_string() + &usage_append(),
        ));
    }
    let mut i = 1usize;
    let prefix = "prefix/";
    let mut rest: Vec<String> = Vec::new();

    while i < argv.len() {
        let arg = &argv[i];
        if arg == "-h" || arg == "--help" {
            print!("{PARSE_OPTIONS_HELP}");
            return Err(ParseOptionsToolError::Help);
        }
        if arg == "--help-all" {
            print!("{PARSE_OPTIONS_HELP}");
            return Err(ParseOptionsToolError::Help);
        }
        if arg == "--" {
            i += 1;
            rest.extend(argv[i..].iter().cloned());
            return st.dump(&st.expect.clone(), &rest);
        }
        if let Some(rest_arg) = arg.strip_prefix("--") {
            if rest_arg == "end-of-options" {
                i += 1;
                rest.extend(argv[i..].iter().cloned());
                return st.dump(&st.expect.clone(), &rest);
            }
            let (name, eq_val) = if let Some(p) = rest_arg.find('=') {
                (&rest_arg[..p], Some(rest_arg[p + 1..].to_string()))
            } else {
                (rest_arg, None)
            };
            if name == "expect" {
                let v = eq_val.ok_or_else(|| {
                    ParseOptionsToolError::Fatal(
                        "error: option `expect' requires a value\n".to_string(),
                    )
                })?;
                collect_expect(&mut st.expect, &v)?;
                i += 1;
                continue;
            }
            match parse_long(&mut st, name, eq_val, argv, &mut i, prefix, disallow_abbrev) {
                Ok(()) => {}
                Err(e) => return Err(map_parse_fatal(e)),
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            if arg == "-" {
                i += 1;
                rest.extend(argv[i..].iter().cloned());
                return st.dump(&st.expect.clone(), &rest);
            }
            i = match parse_short(&mut st, argv, i, prefix, disallow_abbrev) {
                Ok(n) => n,
                Err(e) => return Err(map_parse_fatal(e)),
            };
            continue;
        }
        // Dashless argv (Git `parse_options_step`): only `+` is a NODASH option here.
        if arg == "+" {
            st.boolean = st.boolean.saturating_add(1);
            i += 1;
            continue;
        }
        rest.push(arg.clone());
        i += 1;
    }

    st.dump(&st.expect.clone(), &rest)
}

fn parse_long(
    st: &mut PoState,
    name: &str,
    eq_val: Option<String>,
    argv: &[String],
    i: &mut usize,
    prefix: &str,
    disallow_abbrev: bool,
) -> Result<(), ParseOptionsToolError> {
    let arg_end = name.find('=').unwrap_or(name.len());
    let original_key = name;
    let mut flags_unset = false;
    let mut arg_starts_with_no_no = false;
    let mut s = name;
    if let Some(x) = s.strip_prefix("no-") {
        if let Some(x2) = x.strip_prefix("no-") {
            arg_starts_with_no_no = true;
            s = x2;
        } else {
            flags_unset = true;
            s = x;
        }
    }

    let _ = arg_starts_with_no_no;

    let matched = long_exact(st, s, flags_unset, eq_val.clone(), argv, i, prefix)?;
    if matched {
        return Ok(());
    }

    if !disallow_abbrev {
        let m = long_abbrev(st, s, arg_end, flags_unset, eq_val.clone(), argv, i, prefix)?;
        if m {
            return Ok(());
        }
    }

    Err(unknown_long(original_key))
}

fn unknown_long(name: &str) -> ParseOptionsToolError {
    ParseOptionsToolError::Fatal(format!("error: unknown option `{name}'\n"))
}

fn long_exact(
    st: &mut PoState,
    full: &str,
    flags_unset: bool,
    eq_val: Option<String>,
    argv: &[String],
    i: &mut usize,
    prefix: &str,
) -> Result<bool, ParseOptionsToolError> {
    let u = flags_unset;
    let mut hit = false;
    match full {
        "yes" => {
            let err_name = if u { "no-yes" } else { "yes" };
            no_eq(eq_val.as_deref(), err_name, u)?;
            let unset = u ^ false;
            st.boolean = if unset { 0 } else { 1 };
            hit = true;
        }
        "doubt" => {
            let err_name = if u { "no-doubt" } else { "doubt" };
            no_eq(eq_val.as_deref(), err_name, u)?;
            let unset = u ^ true;
            st.boolean = if unset { 0 } else { 1 };
            hit = true;
        }
        "no-fear" => {
            no_eq(eq_val.as_deref(), "no-fear", u)?;
            st.boolean = 1;
            hit = true;
        }
        "boolean" => {
            no_eq(eq_val.as_deref(), "boolean", u)?;
            if u {
                st.boolean = 0;
            } else {
                st.boolean = st.boolean.saturating_add(1);
            }
            hit = true;
        }
        "or4" => {
            no_eq(eq_val.as_deref(), "or4", u)?;
            if u {
                st.boolean &= !4;
            } else {
                st.boolean |= 4;
            }
            hit = true;
        }
        "neg-or4" => {
            no_eq(eq_val.as_deref(), "neg-or4", u)?;
            if u {
                st.boolean |= 4;
            } else {
                st.boolean &= !4;
            }
            hit = true;
        }
        "integer" => {
            let v = take_val(eq_val, argv, i, "integer")?;
            set_int(st, &v, "integer")?;
            hit = true;
        }
        "i16" => {
            let v = take_val(eq_val, argv, i, "i16")?;
            set_i16(st, &v)?;
            hit = true;
        }
        "unsigned" => {
            let v = take_val(eq_val, argv, i, "unsigned")?;
            set_unsigned(st, &v)?;
            hit = true;
        }
        "u16" => {
            let v = take_val(eq_val, argv, i, "u16")?;
            set_u16(st, &v)?;
            hit = true;
        }
        "set23" => {
            no_eq(eq_val.as_deref(), "set23", u)?;
            st.touch_integer(if u { 0 } else { 23 }, opt("set23", false), None, u)?;
            hit = true;
        }
        "mode1" => {
            no_eq(eq_val.as_deref(), "mode1", u)?;
            st.touch_integer(if u { 0 } else { 1 }, opt("mode1", true), None, u)?;
            hit = true;
        }
        "mode2" => {
            no_eq(eq_val.as_deref(), "mode2", u)?;
            st.touch_integer(if u { 0 } else { 2 }, opt("mode2", true), None, u)?;
            hit = true;
        }
        "mode34" => {
            let v = take_val(eq_val, argv, i, "mode34")?;
            if u {
                st.touch_integer(0, opt("mode34", true), Some("0"), true)?;
            } else if v == "3" {
                st.touch_integer(3, opt("mode34", true), Some("3"), false)?;
            } else if v == "4" {
                st.touch_integer(4, opt("mode34", true), Some("4"), false)?;
            } else {
                return Err(ParseOptionsToolError::Fatal(format!(
                    "error: invalid value for '--mode34': '{v}'\n"
                )));
            }
            hit = true;
        }
        "length" => {
            if u {
                no_eq(eq_val.as_deref(), "no-length", u)?;
                return Err(ParseOptionsToolError::Silent);
            }
            let v = take_val(eq_val, argv, i, "length")?;
            st.length_cb_called = true;
            st.length_cb_arg = Some(v.clone());
            st.length_cb_unset = false;
            st.touch_integer(v.len() as i32, opt("length", false), None, false)?;
            hit = true;
        }
        "file" => {
            let v = take_val(eq_val, argv, i, "file")?;
            if u {
                st.file = None;
            } else {
                st.file = Some(format!("{prefix}{v}"));
            }
            hit = true;
        }
        "string" | "string2" | "st" => {
            let v = take_val(eq_val, argv, i, "string")?;
            if u {
                st.string = None;
            } else {
                st.string = Some(v);
            }
            hit = true;
        }
        "obsolete" => {
            no_eq(eq_val.as_deref(), "obsolete", false)?;
            hit = true;
        }
        "longhelp" => {
            no_eq(eq_val.as_deref(), "longhelp", u)?;
            st.touch_integer(0, opt("longhelp", false), None, u)?;
            hit = true;
        }
        "list" => {
            if u {
                no_eq(eq_val.as_deref(), "list", u)?;
                st.list.clear();
            } else {
                let v = take_val(eq_val, argv, i, "list")?;
                st.list.push(v);
            }
            hit = true;
        }
        "ambiguous" => {
            no_eq(eq_val.as_deref(), "ambiguous", false)?;
            st.ambiguous = st.ambiguous.saturating_add(1);
            hit = true;
        }
        "no-ambiguous" => {
            no_eq(eq_val.as_deref(), "no-ambiguous", false)?;
            st.ambiguous = 0;
            hit = true;
        }
        "abbrev" => {
            if u {
                st.abbrev = 0;
            } else if let Some(ev) = eq_val {
                parse_abbrev(&ev, &mut st.abbrev)?;
            } else if *i + 1 < argv.len() {
                let v = take_val(None, argv, i, "abbrev")?;
                parse_abbrev(&v, &mut st.abbrev)?;
            } else {
                st.abbrev = 7;
            }
            hit = true;
        }
        "verbose" => {
            no_eq(eq_val.as_deref(), "verbose", u)?;
            if u {
                st.verbose = 0;
            } else {
                st.verbose = if st.verbose < 0 { 1 } else { st.verbose + 1 };
            }
            hit = true;
        }
        "quiet" => {
            no_eq(eq_val.as_deref(), "quiet", u)?;
            if u {
                st.quiet = 0;
            } else {
                st.quiet = st.quiet.saturating_add(1);
            }
            hit = true;
        }
        "dry-run" => {
            no_eq(eq_val.as_deref(), "dry-run", u)?;
            st.dry_run = if u { 0 } else { 1 };
            hit = true;
        }
        "alias-source" | "alias-target" => {
            let v = take_val(eq_val, argv, i, "alias-source")?;
            if u {
                st.string = None;
            } else {
                st.string = Some(v);
            }
            hit = true;
        }
        _ => {}
    }
    if hit {
        *i += 1;
    }
    Ok(hit)
}

fn opt(name: &'static str, is_cmdmode: bool) -> OptMeta {
    OptMeta { name, is_cmdmode }
}

/// Long option names in `parse_long_opt` table order (for abbreviation + ambiguity).
const LONG_NAMES: &[&str] = &[
    "yes",
    "no-doubt",
    "doubt",
    "no-fear",
    "boolean",
    "or4",
    "neg-or4",
    "integer",
    "i16",
    "unsigned",
    "u16",
    "set23",
    "mode1",
    "mode2",
    "mode34",
    "length",
    "file",
    "string",
    "string2",
    "st",
    "obsolete",
    "longhelp",
    "list",
    "ambiguous",
    "no-ambiguous",
    "abbrev",
    "verbose",
    "quiet",
    "dry-run",
    "expect",
    "alias-source",
    "alias-target",
];

fn is_alias_pair(a: &str, b: &str) -> bool {
    (a == "alias-source" && b == "alias-target") || (a == "alias-target" && b == "alias-source")
}

#[allow(clippy::too_many_arguments)]
fn long_abbrev(
    st: &mut PoState,
    s: &str,
    _arg_end: usize,
    flags_unset: bool,
    eq_val: Option<String>,
    argv: &[String],
    i: &mut usize,
    prefix: &str,
) -> Result<bool, ParseOptionsToolError> {
    let user_len = s.len();
    if user_len == 0 {
        return Ok(false);
    }
    let mut matches: Vec<&'static str> = Vec::new();
    for &ln in LONG_NAMES {
        let mut long_name = ln;
        let mut opt_unset = false;
        if let Some(x) = long_name.strip_prefix("no-") {
            long_name = x;
            opt_unset = true;
        }
        let allow_unset = !matches!(
            ln,
            "no-fear" | "obsolete" | "longhelp" | "ambiguous" | "no-ambiguous"
        );
        if (flags_unset ^ opt_unset) && !allow_unset {
            continue;
        }
        if long_name.len() >= user_len && long_name.as_bytes().get(..user_len) == Some(s.as_bytes())
        {
            matches.push(ln);
        }
    }
    matches.sort_unstable();
    matches.dedup();
    if matches.is_empty() {
        return Ok(false);
    }
    let mut abbrev: Option<&str> = None;
    let mut ambiguous: Option<(&str, &str)> = None;
    for m in &matches {
        match abbrev {
            None => abbrev = Some(m),
            Some(a) => {
                if !is_alias_pair(a, m) {
                    ambiguous = Some((a, m));
                    break;
                }
                abbrev = Some(m);
            }
        }
    }
    if let Some((a, b)) = ambiguous {
        return Err(ParseOptionsToolError::Fatal(format!(
            "ambiguous option: {s} (could be --{a} or --{b})\n"
        )));
    }
    let Some(only) = abbrev else {
        return Ok(false);
    };
    let key = match only {
        "no-doubt" => "doubt",
        o => o,
    };
    long_exact(st, key, flags_unset, eq_val, argv, i, prefix)?;
    Ok(true)
}

fn no_eq(eq: Option<&str>, name: &str, unset: bool) -> Result<(), ParseOptionsToolError> {
    if let Some(x) = eq {
        if !x.is_empty() || unset {
            return Err(ParseOptionsToolError::Fatal(format!(
                "error: option `{name}' takes no value\n"
            )));
        }
    }
    Ok(())
}

fn take_val(
    eq_val: Option<String>,
    argv: &[String],
    i: &mut usize,
    optname: &str,
) -> Result<String, ParseOptionsToolError> {
    if let Some(v) = eq_val {
        return Ok(v);
    }
    if *i + 1 >= argv.len() {
        return Err(ParseOptionsToolError::Fatal(format!(
            "error: option `{optname}' requires a value\n"
        )));
    }
    *i += 1;
    Ok(argv[*i].clone())
}

fn parse_abbrev(s: &str, out: &mut i32) -> Result<(), ParseOptionsToolError> {
    if s.is_empty() {
        return Err(ParseOptionsToolError::Fatal(
            "error: option `abbrev' expects a numerical value\n".to_string(),
        ));
    }
    let v: i32 = s.parse().map_err(|_| {
        ParseOptionsToolError::Fatal(
            "error: option `abbrev' expects a numerical value\n".to_string(),
        )
    })?;
    *out = if v != 0 && v < 4 { 4 } else { v };
    Ok(())
}

fn set_int(st: &mut PoState, raw: &str, optname: &str) -> Result<(), ParseOptionsToolError> {
    let (lo, hi) = int_bounds_32();
    let opt_meta = match optname {
        "integer" | "j" => opt("integer", false),
        _ => {
            return Err(ParseOptionsToolError::Fatal(format!(
                "internal error: unknown option name for set_int: {optname}\n"
            )));
        }
    };
    match git_parse_signed(raw, hi) {
        Ok(v) if v >= lo && v <= hi => {
            st.touch_integer(v as i32, opt_meta, None, false)?;
            Ok(())
        }
        Err(std::io::ErrorKind::InvalidData) => Err(ParseOptionsToolError::Fatal(format!(
            "error: value {raw} for option `{optname}' not in range [{lo},{hi}]\n"
        ))),
        _ => Err(ParseOptionsToolError::Fatal(format!(
            "error: option `{optname}' expects an integer value with an optional k/m/g suffix\n"
        ))),
    }
}

fn set_i16(st: &mut PoState, raw: &str) -> Result<(), ParseOptionsToolError> {
    match git_parse_signed(raw, i16::MAX as i128) {
        Ok(v) if v >= i16::MIN as i128 && v <= i16::MAX as i128 => {
            st.i16 = v as i16;
            Ok(())
        }
        Err(std::io::ErrorKind::InvalidData) | Ok(_) => Err(ParseOptionsToolError::Fatal(format!(
            "error: value {raw} for option `i16' not in range [-32768,32767]\n"
        ))),
        _ => Err(ParseOptionsToolError::Fatal(
            "error: option `i16' expects an integer value with an optional k/m/g suffix\n"
                .to_string(),
        )),
    }
}

fn set_unsigned(st: &mut PoState, raw: &str) -> Result<(), ParseOptionsToolError> {
    match git_parse_unsigned(raw, u64::MAX as u128) {
        Ok(v) => {
            st.unsigned_integer = v as u64;
            Ok(())
        }
        Err(std::io::ErrorKind::InvalidData) => Err(ParseOptionsToolError::Fatal(format!(
            "error: value {raw} for option `unsigned' not in range [0,{}]\n",
            u64::MAX
        ))),
        _ => Err(ParseOptionsToolError::Fatal(
            "error: option `unsigned' expects a non-negative integer value with an optional k/m/g suffix\n"
                .to_string(),
        )),
    }
}

fn set_u16(st: &mut PoState, raw: &str) -> Result<(), ParseOptionsToolError> {
    match git_parse_unsigned(raw, u16::MAX as u128) {
        Ok(v) if v <= u16::MAX as u128 => {
            st.u16 = v as u16;
            Ok(())
        }
        _ => Err(ParseOptionsToolError::Fatal(format!(
            "error: value {raw} for option `u16' not in range [0,65535]\n"
        ))),
    }
}

/// Git `starts_with(long_name, arg)` for typo detection: byte prefix of `long_name`.
fn long_name_starts_with_user_typos(user: &str, long_name: &str) -> bool {
    long_name
        .as_bytes()
        .get(..user.len())
        .is_some_and(|pfx| pfx == user.as_bytes())
}

/// Git `check_typos(arg + 1, …)` for a short-option cluster: `cluster` is the full argv suffix
/// after `-` (e.g. `boolean` for `-boolean`), not the unconsumed tail.
fn check_typos_short_cluster(cluster: &str) -> Result<(), ParseOptionsToolError> {
    if cluster.len() < 3 {
        return Ok(());
    }
    if cluster.starts_with("no-") {
        return Err(ParseOptionsToolError::Fatal(format!(
            "error: did you mean `--{cluster}` (with two dashes)?\n"
        )));
    }
    for ln in LONG_NAMES {
        if long_name_starts_with_user_typos(cluster, ln) {
            return Err(ParseOptionsToolError::Fatal(format!(
                "error: did you mean `--{cluster}` (with two dashes)?\n"
            )));
        }
    }
    Ok(())
}

/// One step of Git `parse_short_opt`: explicit short options win; otherwise a digit run is
/// `OPTION_NUMBER`.
fn parse_short_opt_step(
    st: &mut PoState,
    full_suffix: &str,
    o: &mut usize,
    local_i: &mut usize,
    argv: &[String],
    prefix: &str,
) -> Result<(), ParseOptionsToolError> {
    let Some(first) = full_suffix[*o..].chars().next() else {
        return Ok(());
    };
    let flen = first.len_utf8();

    match first {
        'h' => {
            print!("{PARSE_OPTIONS_HELP}");
            return Err(ParseOptionsToolError::Help);
        }
        's' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `s' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.string = Some(v);
        }
        'b' => {
            st.boolean = st.boolean.saturating_add(1);
            *o += flen;
        }
        'i' => {
            let tail = &full_suffix[*o + flen..];
            let v = if !tail.is_empty() {
                tail.to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `i' requires a value\n".to_string(),
                ));
            };
            set_int(st, &v, "integer")?;
            *o = full_suffix.len();
        }
        'j' => {
            let tail = &full_suffix[*o + flen..];
            let v = if !tail.is_empty() {
                tail.to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `j' requires a value\n".to_string(),
                ));
            };
            set_int(st, &v, "j")?;
            *o = full_suffix.len();
        }
        'u' => {
            let tail = &full_suffix[*o + flen..];
            let v = if !tail.is_empty() {
                tail.to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `u' requires a value\n".to_string(),
                ));
            };
            set_unsigned(st, &v)?;
            *o = full_suffix.len();
        }
        'v' => {
            st.verbose = if st.verbose < 0 { 1 } else { st.verbose + 1 };
            *o += flen;
        }
        'n' => {
            st.dry_run = 1;
            *o += flen;
        }
        'q' => {
            st.quiet = st.quiet.saturating_add(1);
            *o += flen;
        }
        'D' => {
            st.boolean = 1;
            *o += flen;
        }
        'B' => {
            st.boolean = 1;
            *o += flen;
        }
        '4' => {
            st.boolean |= 4;
            *o += flen;
        }
        'L' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `L' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.length_cb_called = true;
            st.length_cb_arg = Some(v.clone());
            st.length_cb_unset = false;
            st.touch_integer(v.len() as i32, opt("length", false), None, false)?;
        }
        'F' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `F' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.file = Some(format!("{prefix}{v}"));
        }
        'A' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `A' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.string = Some(v);
        }
        'Z' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `Z' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.string = Some(v);
        }
        'o' => {
            let v = if *o + flen < full_suffix.len() {
                full_suffix[*o + flen..].to_string()
            } else if *local_i + 1 < argv.len() {
                *local_i += 1;
                argv[*local_i].clone()
            } else {
                return Err(ParseOptionsToolError::Fatal(
                    "error: switch `o' requires a value\n".to_string(),
                ));
            };
            *o = full_suffix.len();
            st.string = Some(v);
        }
        '+' => {
            st.boolean = st.boolean.saturating_add(1);
            *o += flen;
        }
        c if c.is_ascii_digit() => {
            let start = *o;
            let mut end = start;
            while end < full_suffix.len() && full_suffix.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            let digits = &full_suffix[start..end];
            let n: i32 = digits
                .parse()
                .map_err(|_| ParseOptionsToolError::Fatal("error: invalid number\n".to_string()))?;
            st.touch_integer(
                n,
                OptMeta {
                    name: "NUM",
                    is_cmdmode: false,
                },
                None,
                false,
            )?;
            *o = end;
        }
        _ => {
            if *o == 0 {
                check_typos_short_cluster(full_suffix)?;
            }
            return Err(ParseOptionsToolError::Fatal(format!(
                "error: unknown switch `{first}'\n"
            )));
        }
    }
    Ok(())
}

fn parse_short(
    st: &mut PoState,
    argv: &[String],
    i: usize,
    prefix: &str,
    _disallow_abbrev: bool,
) -> Result<usize, ParseOptionsToolError> {
    let arg = &argv[i];
    let full_suffix = &arg[1..];
    if full_suffix.is_empty() {
        return Err(ParseOptionsToolError::Fatal(
            "error: unknown switch\n".to_string(),
        ));
    }

    let mut o = 0usize;
    let mut local_i = i;

    parse_short_opt_step(st, full_suffix, &mut o, &mut local_i, argv, prefix)?;
    if o < full_suffix.len() {
        check_typos_short_cluster(full_suffix)?;
    }
    while o < full_suffix.len() {
        parse_short_opt_step(st, full_suffix, &mut o, &mut local_i, argv, prefix)?;
        if o < full_suffix.len() {
            check_typos_short_cluster(full_suffix)?;
        }
    }

    Ok(local_i + 1)
}
