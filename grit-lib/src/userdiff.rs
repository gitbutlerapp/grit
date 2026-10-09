//! User-defined and built-in diff function-name matching.
//!
//! This module implements the subset of Git's `userdiff` behavior needed for
//! hunk-header function context extraction.
//!
//! POSIX extended patterns from Git config are translated to Rust [`regex`] syntax
//! (including bracket classes like `[[:alpha:]]`). Matchers for built-in drivers are
//! compiled once and cached. Rust's regex engine uses leftmost-first alternation; GNU
//! `grep -E` uses leftmost-longest. Git's built-in funcname patterns are written so
//! this difference does not change hunk headers for the shipped drivers.

use crate::attributes::{collect_attrs_for_path, AttrValue, MacroTable};
use crate::config::ConfigSet;
use crate::crlf::{get_file_attrs, AttrRule, DiffAttr};
use regex::{Regex, RegexBuilder};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

/// Built-in diff driver funcname patterns (same strings as Git's userdiff builtin drivers).
const BUILTIN_PATTERN_DEFS: &[(&str, &str, bool)] = &[
    (
        "ada",
        r"!^(.*[ 	])?(is[ 	]+new|renames|is[ 	]+separate)([ 	].*)?$
!^[ 	]*with[ 	].*$
^[ 	]*((procedure|function)[ 	]+.*)$
^[ 	]*((package|protected|task)[ 	]+.*)$",
        true,
    ),
    (
        "bash",
        r"^[ 	]*((([a-zA-Z_][a-zA-Z0-9_]*[ 	]*\([ 	]*\))|(function[ 	]+[a-zA-Z_][a-zA-Z0-9_]*(([ 	]*\([ 	]*\))|([ 	]+)))).*$)",
        false,
    ),
    (
        "bibtex",
        r#"(@[a-zA-Z]{1,}[ 	]*\{{0,1}[ 	]*[^ 	"@',\#}{~%]*).*$"#,
        false,
    ),
    (
        "cpp",
        r"!^[ 	]*[A-Za-z_][A-Za-z_0-9]*:[[:space:]]*($|/[/*])
^((::[[:space:]]*)?[A-Za-z_].*)$",
        false,
    ),
    (
        "csharp",
        r"!(^|[ 	]+)(do|while|for|foreach|if|else|new|default|return|switch|case|throw|catch|using|lock|fixed)([ 	(]+|$)
^[ 	]*(([][[:alnum:]@_.](<[][[:alnum:]@_, 	<>]+>)?)+([ 	]+([][[:alnum:]@_.](<[][[:alnum:]@_, 	<>]+>)?)+)+[ 	]*\([^;]*)$
^[ 	]*(([][[:alnum:]@_.](<[][[:alnum:]@_, 	<>]+>)?)+([ 	]+([][[:alnum:]@_.](<[][[:alnum:]@_, 	<>]+>)?)+)+[^;=:,()]*)$
^[ 	]*(((static|public|internal|private|protected|new|unsafe|sealed|abstract|partial)[ 	]+)*(class|enum|interface|struct|record)[ 	]+.*)$
^[ 	]*(namespace[ 	]+.*)$",
        false,
    ),
    (
        "css",
        r"![:;][[:space:]]*$
^[:[@.#]?[_a-z0-9].*$",
        true,
    ),
    (
        "dts",
        r"!;
!=
^[ 	]*((/[ 	]*\{|&?[a-zA-Z_]).*)",
        false,
    ),
    (
        "elixir",
        r"^[ 	]*((def(macro|module|impl|protocol|p)?|test)[ 	].*)$",
        false,
    ),
    (
        "fortran",
        r#"!^([C*]|[ 	]*!)
!^[ 	]*MODULE[ 	]+PROCEDURE[ 	]
^[ 	]*((END[ 	]+)?(PROGRAM|MODULE|BLOCK[ 	]+DATA|([^!'" 	]+[ 	]+)*(SUBROUTINE|FUNCTION))[ 	]+[A-Z].*)$"#,
        true,
    ),
    (
        "fountain",
        r"^((\.[^.]|(int|ext|est|int\.?/ext|i/e)[. ]).*)$",
        true,
    ),
    (
        "golang",
        r"^[ 	]*(func[ 	]*.*(\{[ 	]*)?)
^[ 	]*(type[ 	].*(struct|interface)[ 	]*(\{[ 	]*)?)",
        false,
    ),
    ("html", r"^[ 	]*(<[Hh][1-6]([ 	].*)?>.*)$", false),
    ("ini", r"^[ 	]*\[[^]]+\]", false),
    (
        "java",
        r"!^[ 	]*(catch|do|for|if|instanceof|new|return|switch|throw|while)
^[ 	]*(([a-z-]+[ 	]+)*(class|enum|interface|record)[ 	]+.*)$
^[ 	]*(([A-Za-z_<>&][][?&<>.,A-Za-z_0-9]*[ 	]+)+[A-Za-z_][A-Za-z_0-9]*[ 	]*\([^;]*)$",
        false,
    ),
    (
        "kotlin",
        r"^[ 	]*(([a-z]+[ 	]+)*(fun|class|interface)[ 	]+.*)$",
        false,
    ),
    ("markdown", r"^ {0,3}#{1,6}[ 	].*", false),
    (
        "matlab",
        r"^[[:space:]]*((classdef|function)[[:space:]].*)$|^(%%%?|##)[[:space:]].*$",
        false,
    ),
    (
        "objc",
        r"!^[ 	]*(do|for|if|else|return|switch|while)
^[ 	]*([-+][ 	]*\([ 	]*[A-Za-z_][A-Za-z_0-9* 	]*\)[ 	]*[A-Za-z_].*)$
^[ 	]*(([A-Za-z_][A-Za-z_0-9]*[ 	]+)+[A-Za-z_][A-Za-z_0-9]*[ 	]*\([^;]*)$
^(@(implementation|interface|protocol)[ 	].*)$",
        false,
    ),
    (
        "pascal",
        r"^(((class[ 	]+)?(procedure|function)|constructor|destructor|interface|implementation|initialization|finalization)[ 	]*.*)$
^(.*=[ 	]*(class|record).*)$",
        false,
    ),
    (
        "perl",
        r"^package .*
^sub [[:alnum:]_':]+[ 	]*(\([^)]*\)[ 	]*)?(:[^;#]*)?(\{[ 	]*)?(#.*)?$
^(BEGIN|END|INIT|CHECK|UNITCHECK|AUTOLOAD|DESTROY)[ 	]*(\{[ 	]*)?(#.*)?$
^=head[0-9] .*",
        false,
    ),
    (
        "php",
        r"^[	 ]*(((public|protected|private|static|abstract|final)[	 ]+)*function.*)$
^[	 ]*((((final|abstract)[	 ]+)?class|enum|interface|trait).*)$",
        false,
    ),
    ("python", r"^[ 	]*((class|(async[ 	]+)?def)[ 	].*)$", false),
    (
        "r",
        r"^[ 	]*([a-zA-z][a-zA-Z0-9_.]*[ 	]*(<-|=)[ 	]*function.*)$",
        false,
    ),
    ("ruby", r"^[ 	]*((class|module|def)[ 	].*)$", false),
    (
        "rust",
        r#"^[	 ]*((pub(\([^\)]+\))?[	 ]+)?((async|const|unsafe|extern([	 ]+"[^"]+"))[	 ]+)?(struct|enum|union|mod|trait|fn|impl|macro_rules!)[< 	]+[^;]*)$"#,
        false,
    ),
    (
        "scheme",
        r"^[	 ]*(\(((define|def(struct|syntax|class|method|rules|record|proto|alias)?)[-*/ 	]|(library|module|struct|class)[*+ 	]).*)$",
        false,
    ),
    (
        "tex",
        r"^(\\((sub)*section|chapter|part)\*{0,1}\{.*)$",
        false,
    ),
];

/// POSIX extended word-regex fragments from Git's `userdiff.c` `PATTERNS`/`IPATTERN` macros
/// (driver-specific part before the `|[^[:space:]]` suffix Git appends at runtime).
///
/// When `ignore_case` is true, the regex should be compiled with `case_insensitive(true)` like
/// Git's `REG_ICASE` for `IPATTERN` drivers.
const BUILTIN_WORD_REGEX: &[(&str, &str, bool)] = &[
    (
        "ada",
        "[a-zA-Z][a-zA-Z0-9_]*\
         |[-+]?[0-9][0-9#_.aAbBcCdDeEfF]*([eE][+-]?[0-9_]+)?\
         |=>|\\.\\.|\\*\\*|:=|/=|>=|<=|<<|>>|<>",
        true,
    ),
    (
        "bash",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |\\$[a-zA-Z0-9_]+|\\$\\{\
         |\\|\\||&&|<<|>>\
         |==|!=|<=|>=|[-+*/%&|^]=\
         |:=|:-|:\\+|:\\?|##|%%|\\^\\^|,,\
         |[-a-zA-Z0-9_]+\
         |\\(|\\)|\\{|\\}|\\[|\\]",
        false,
    ),
    (
        "bibtex",
        "[={}\"]|[^={}\" \t]+",
        false,
    ),
    (
        "cpp",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[0-9][0-9.]*([Ee][-+]?[0-9]+)?[fFlLuU]*\
         |0[xXbB][0-9a-fA-F]+[lLuU]*\
         |\\.[0-9][0-9]*([Ee][-+]?[0-9]+)?[fFlL]?\
         |[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->\\*?|\\.\\*|<=>",
        false,
    ),
    (
        "csharp",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?\
         |[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->",
        false,
    ),
    (
        "css",
        "-?[_a-zA-Z][-_a-zA-Z0-9]*\
         |-?[0-9]+|\\#[0-9a-fA-F]+",
        true,
    ),
    (
        "dts",
        "[a-zA-Z0-9,._+?#-]+\
         |[-+*/%&^|!~]|>>|<<|&&|\\|\\|",
        false,
    ),
    (
        "elixir",
        "[@:]?[a-zA-Z0-9@_?!]+\
         |[-+]?0[xob][0-9a-fA-F]+\
         |[-+]?[0-9][0-9_.]*([eE][-+]?[0-9_]+)?\
         |:?(\\+\\+|--|\\.\\.|~~~|<>|\\^\\^\\^|<?\\|>|<<<?|>?>>|<<?~|~>?>|<~>|<=|>=|===?|!==?|=~|&&&?|\\|\\|\\|?|=>|<-|\\\\\\\\|->)\
         |:?%[A-Za-z0-9_.]\\{\\}?",
        false,
    ),
    (
        "fortran",
        "[a-zA-Z][a-zA-Z0-9_]*\
         |\\.([Ee][Qq]|[Nn][Ee]|[Gg][TtEe]|[Ll][TtEe]|[Tt][Rr][Uu][Ee]|[Ff][Aa][Ll][Ss][Ee]|[Aa][Nn][Dd]|[Oo][Rr]|[Nn]?[Ee][Qq][Vv]|[Nn][Oo][Tt])\\.\
         |[-+]?[0-9.]+([AaIiDdEeFfLlTtXx][Ss]?[-+]?[0-9.]*)?(_[a-zA-Z0-9][a-zA-Z0-9_]*)?\
         |//|\\*\\*|::|[/<>=]=",
        true,
    ),
    ("fountain", "[^ \t-]+", true),
    (
        "golang",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.eE]+i?|0[xX]?[0-9a-fA-F]+i?\
         |[-+*/<>%&^|=!:]=|--|\\+\\+|<<=?|>>=?|&\\^=?|&&|\\|\\||<-|\\.{3}",
        false,
    ),
    ("html", "[^<>= \t]+", false),
    ("ini", "[^ \t]+", false),
    (
        "java",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?\
         |[-+*/<>%&^|=!]=\
         |--|\\+\\+|<<=?|>>>?=?|&&|\\|\\|",
        false,
    ),
    (
        "kotlin",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |0[xXbB][0-9a-fA-F_]+[lLuU]*\
         |[0-9][0-9_]*([.][0-9_]*)?([Ee][-+]?[0-9]+)?[fFlLuU]*\
         |[.][0-9][0-9_]*([Ee][-+]?[0-9]+)?[fFlLuU]?\
         |[-+*/<>%&^|=!]==?|--|\\+\\+|<<=|>>=|&&|\\|\\||->|\\.\\*|!!|[?:.][.:]",
        false,
    ),
    ("markdown", "[^<>= \t]+", false),
    (
        "matlab",
        "[a-zA-Z_][a-zA-Z0-9_]*|[-+0-9.e]+|[=~<>]=|\\.[*/\\^']|\\|\\||&&",
        false,
    ),
    (
        "objc",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+[fFlL]?|0[xXbB]?[0-9a-fA-F]+[lL]?\
         |[-+*/<>%&^|=!]=|--|\\+\\+|<<=?|>>=?|&&|\\|\\||::|->",
        false,
    ),
    (
        "pascal",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+\
         |<>|<=|>=|:=|\\.\\.",
        false,
    ),
    (
        "perl",
        "[[:alpha:]_'][[:alnum:]_']*\
         |0[xb]?[0-9a-fA-F_]*\
         |[0-9a-fA-F_]+(\\.[0-9a-fA-F_]+)?([eE][-+]?[0-9_]+)?\
         |=>|-[rwxoRWXOezsfdlpSugkbctTBMAC>]|~~|::\
         |&&=|\\|\\|=|//=|\\*\\*=\
         |&&|\\|\\||//|\\+\\+|--|\\*\\*|\\.\\.\\.?\
         |[-+*/%.^&<>=!|]=\
         |=~|!~\
         |<<|<>|<=>|>>",
        false,
    ),
    (
        "php",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+\
         |[-+*/<>%&^|=!.]=|--|\\+\\+|<<=?|>>=?|===|&&|\\|\\||::|->",
        false,
    ),
    (
        "python",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+[jJlL]?|0[xX]?[0-9a-fA-F]+[lL]?\
         |[-+*/<>%&^|=!]=|//=?|<<=?|>>=?|\\*\\*=?",
        false,
    ),
    ("r", "[^ \t]+", false),
    (
        "ruby",
        "(@|@@|\\$)?[a-zA-Z_][a-zA-Z0-9_]*\
         |[-+0-9.e]+|0[xXbB]?[0-9a-fA-F]+|\\?(\\\\C-)?(\\\\M-)?.\
         |//=?|[-+*/<>%&^|=!]=|<<=?|>>=?|===|\\.{1,3}|::|[!=]~",
        false,
    ),
    (
        "rust",
        "[a-zA-Z_][a-zA-Z0-9_]*\
         |[0-9][0-9_a-fA-Fiosuxz]*(\\.([0-9]*[eE][+-]?)?[0-9_fF]*)?\
         |[-+*\\/<>%&^|=!:]=|<<=?|>>=?|&&|\\|\\||->|=>|\\.{2}=|\\.{3}|::",
        false,
    ),
    (
        "scheme",
        "\\|([^\\\\]*)\\||([^][)(}{[ \t])+",
        false,
    ),
    (
        "tex",
        "\\\\[a-zA-Z@]+|\\\\.|([a-zA-Z0-9]|[^\\x01-\\x7f])+",
        false,
    ),
];

/// Default word-regex suffix Git appends for built-in drivers (`userdiff.c` `PATTERNS` macro).
pub const GIT_WORD_REGEX_DEFAULT_SUFFIX: &str = "|[^[:space:]]|[\\xc0-\\xff][\\x80-\\xbf]+";

/// Built-in extended word regex when no driver matches (Git `userdiff` "default" driver).
pub const GIT_WORD_REGEX_FALLBACK: &str = "[^[:space:]]|[\\xc0-\\xff][\\x80-\\xbf]+";

/// Returns the driver-specific word-regex pattern fragment plus whether it uses `REG_ICASE`.
#[must_use]
pub fn builtin_word_regex(driver: &str) -> Option<(&'static str, bool)> {
    BUILTIN_WORD_REGEX
        .iter()
        .find(|(name, _, _)| *name == driver)
        .map(|(_, pat, ic)| (*pat, *ic))
}

#[derive(Debug, Clone)]
struct FuncRule {
    matcher: RuleMatcher,
    negate: bool,
}

#[derive(Debug, Clone)]
enum RuleMatcher {
    Rust(Regex),
}

#[derive(Debug, Clone)]
struct BuiltinPattern {
    pattern: String,
    ignore_case: bool,
}

// hygiene: immutable built-in diff funcname matchers, initialized once
static BUILTIN_FUNCNAME_MATCHERS: OnceLock<BTreeMap<String, Arc<FuncnameMatcher>>> =
    OnceLock::new();

/// Compiled function-name matcher used for diff hunk headers.
#[derive(Debug, Clone)]
pub struct FuncnameMatcher {
    rules: Vec<FuncRule>,
}

impl FuncnameMatcher {
    /// Match a source line against configured funcname rules.
    ///
    /// Returns the text to show after the hunk header when matched.
    #[must_use]
    pub fn match_line(&self, line: &str) -> Option<String> {
        let mut text = line;
        if let Some(stripped) = text.strip_suffix('\n') {
            text = stripped;
            if let Some(stripped_cr) = text.strip_suffix('\r') {
                text = stripped_cr;
            }
        }

        for rule in &self.rules {
            let matched_text = match &rule.matcher {
                RuleMatcher::Rust(regex) => {
                    if let Some(caps) = regex.captures(text) {
                        caps.get(1)
                            .or_else(|| caps.get(0))
                            .map(|m| m.as_str())
                            .unwrap_or_default()
                            .trim_end_matches(char::is_whitespace)
                            .to_owned()
                    } else if regex.is_match(text) {
                        text.trim_end_matches(char::is_whitespace).to_owned()
                    } else {
                        continue;
                    }
                }
            };
            if rule.negate {
                return None;
            }
            return Some(matched_text);
        }
        None
    }
}

/// Resolve a function-name matcher for `rel_path` from attributes + config.
///
/// Returns `Ok(None)` when no diff driver is configured for the path.
pub fn matcher_for_path(
    config: &ConfigSet,
    rules: &[AttrRule],
    rel_path: &str,
) -> Result<Option<FuncnameMatcher>, String> {
    let attrs = get_file_attrs(rules, rel_path, false, config);
    let DiffAttr::Driver(ref driver) = attrs.diff_attr else {
        return Ok(None);
    };
    matcher_for_driver(config, driver)
}

/// Like [`matcher_for_path`] but uses parsed `.gitattributes` rules from [`crate::attributes`].
pub fn matcher_for_path_parsed(
    config: &ConfigSet,
    rules: &[crate::attributes::AttrRule],
    macros: &MacroTable,
    rel_path: &str,
    ignore_case: bool,
) -> Result<Option<FuncnameMatcher>, String> {
    let map = collect_attrs_for_path(rules, macros, rel_path, ignore_case);
    let Some(AttrValue::Value(driver)) = map.get("diff") else {
        return Ok(None);
    };
    matcher_for_driver(config, driver.as_str())
}

/// Effective word-diff regular expression for `rel_path` (Git `diff.wordRegex` + driver + builtins).
///
/// Returns `None` when Git would use no `word_regex` (`regcomp`): tokenization is maximal runs of
/// non-whitespace (`find_word_boundaries` fallback in `diff.c`).
///
/// Otherwise returns the full extended-regex pattern and whether Git compiles it with `REG_ICASE`.
#[must_use]
pub fn word_regex_pattern_for_path_parsed(
    config: &ConfigSet,
    rules: &[crate::attributes::AttrRule],
    macros: &MacroTable,
    rel_path: &str,
    ignore_case_attrs: bool,
) -> Option<(String, bool)> {
    let map = collect_attrs_for_path(rules, macros, rel_path, ignore_case_attrs);
    let driver = match map.get("diff") {
        Some(AttrValue::Value(d)) => Some(d.as_str()),
        _ => None,
    };

    if let Some(d) = driver {
        for key in [format!("diff.{d}.wordregex"), format!("diff.{d}.wordRegex")] {
            if let Some(raw) = config.get(&key) {
                if !raw.is_empty() {
                    return Some((raw, false));
                }
            }
        }
    }

    for key in ["diff.wordregex", "diff.wordRegex"] {
        if let Some(raw) = config.get(key) {
            if !raw.is_empty() {
                return Some((raw, false));
            }
        }
    }

    if let Some(d) = driver {
        if let Some((frag, ic)) = builtin_word_regex(d) {
            return Some((format!("{frag}{GIT_WORD_REGEX_DEFAULT_SUFFIX}"), ic));
        }
    }

    None
}

/// Resolve a function-name matcher for a named diff driver.
///
/// Returns `Ok(None)` when the driver has no built-in or configured funcname
/// pattern.
pub fn matcher_for_driver(
    config: &ConfigSet,
    driver: &str,
) -> Result<Option<FuncnameMatcher>, String> {
    if let Some(pattern) = config.get(&format!("diff.{driver}.xfuncname")) {
        return compile_matcher(&pattern, true, false).map(Some);
    }
    if let Some(pattern) = config.get(&format!("diff.{driver}.funcname")) {
        return compile_matcher(&pattern, false, false).map(Some);
    }
    if let Some(matcher) = cached_builtin_matcher(driver) {
        return Ok(Some((*matcher).clone()));
    }
    Ok(None)
}

fn cached_builtin_matcher(driver: &str) -> Option<Arc<FuncnameMatcher>> {
    let map = BUILTIN_FUNCNAME_MATCHERS.get_or_init(|| {
        builtin_patterns()
            .clone()
            .into_iter()
            .map(|(name, builtin)| {
                let matcher = compile_matcher(&builtin.pattern, true, builtin.ignore_case)
                    .unwrap_or_else(|e| panic!("built-in userdiff driver {name}: {e}"));
                (name, Arc::new(matcher))
            })
            .collect()
    });
    map.get(driver).cloned()
}

fn compile_matcher(
    pattern: &str,
    extended: bool,
    ignore_case: bool,
) -> Result<FuncnameMatcher, String> {
    let lines: Vec<&str> = pattern.split('\n').collect();
    if lines.is_empty() {
        return Ok(FuncnameMatcher { rules: Vec::new() });
    }

    let mut rules = Vec::with_capacity(lines.len());
    for (idx, raw) in lines.iter().enumerate() {
        let mut line = *raw;
        let negate = line.starts_with('!');
        if negate {
            if idx == lines.len() - 1 {
                return Err(format!("Last expression must not be negated: {line}"));
            }
            line = &line[1..];
        }

        let posix_pattern = if extended {
            line.to_owned()
        } else {
            bre_to_ere(line)
        };

        let matcher = compile_posix_ere_rule(&posix_pattern, ignore_case, line)?;
        rules.push(FuncRule { matcher, negate });
    }

    Ok(FuncnameMatcher { rules })
}

#[allow(dead_code)]
fn builtin_patterns() -> &'static BTreeMap<String, BuiltinPattern> {
    // hygiene: immutable built-in diff funcname patterns, initialized once
    static BUILTIN_PATTERNS: OnceLock<BTreeMap<String, BuiltinPattern>> = OnceLock::new();
    BUILTIN_PATTERNS.get_or_init(parse_builtin_patterns)
}

fn parse_builtin_patterns() -> BTreeMap<String, BuiltinPattern> {
    BUILTIN_PATTERN_DEFS
        .iter()
        .filter(|(name, _, _)| !name.is_empty() && *name != "default")
        .map(|(name, pattern, ignore_case)| {
            (
                (*name).to_owned(),
                BuiltinPattern {
                    pattern: (*pattern).to_owned(),
                    ignore_case: *ignore_case,
                },
            )
        })
        .collect()
}

fn bre_to_ere(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len());
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    let mut in_bracket = false;

    while i < chars.len() {
        if in_bracket {
            if chars[i] == ']' && i > 0 {
                result.push(']');
                in_bracket = false;
                i += 1;
            } else if chars[i] == '[' {
                result.push('[');
                i += 1;
            } else if chars[i] == '\\' {
                // Preserve literal backslashes inside character classes.
                // Rust `regex` understands POSIX classes like `[:alnum:]`,
                // so we only need to escape unknown escapes.
                if i + 1 < chars.len() {
                    let next = chars[i + 1];
                    if next.is_ascii_alphabetic() {
                        result.push('\\');
                        result.push('\\');
                        result.push(next);
                        i += 2;
                    } else {
                        result.push('\\');
                        result.push(next);
                        i += 2;
                    }
                } else {
                    result.push('\\');
                    i += 1;
                }
            } else {
                result.push(chars[i]);
                i += 1;
            }
        } else if chars[i] == '[' {
            result.push('[');
            in_bracket = true;
            i += 1;
            if i < chars.len() && (chars[i] == '^' || chars[i] == '!') {
                result.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                result.push(']');
                i += 1;
            }
        } else if chars[i] == '\\' && i + 1 < chars.len() {
            match chars[i + 1] {
                '+' | '?' | '{' | '}' | '(' | ')' | '|' => {
                    result.push(chars[i + 1]);
                    i += 2;
                }
                _ => {
                    result.push(chars[i]);
                    result.push(chars[i + 1]);
                    i += 2;
                }
            }
        } else if matches!(chars[i], '+' | '?' | '{' | '}' | '(' | ')' | '|') {
            result.push('\\');
            result.push(chars[i]);
            i += 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Translate a validated POSIX ERE (as Git uses for `xfuncname`) into Rust `regex` syntax.
///
/// Returns `None` when the pattern is not valid POSIX ERE (e.g. an unterminated `[` class).
fn posix_ere_to_rust_regex(pattern: &str) -> Option<String> {
    let mut out = String::with_capacity(pattern.len() + 16);
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '[' {
            let (translated, consumed) = translate_posix_bracket(&chars[i..])?;
            out.push_str(&translated);
            i += consumed;
        } else if chars[i] == '\\' && i + 1 < chars.len() {
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Translate one POSIX bracket expression starting at `[`; returns Rust class + bytes consumed.
fn translate_posix_bracket(src: &[char]) -> Option<(String, usize)> {
    debug_assert!(src.first() == Some(&'['));
    let mut out = String::from("[");
    let mut i = 1usize;
    if i < src.len() && (src[i] == '^' || src[i] == '!') {
        out.push(src[i]);
        i += 1;
    }
    if i < src.len() && src[i] == ']' {
        out.push(']');
        i += 1;
    }
    while i < src.len() {
        if src[i] == ']' {
            out.push(']');
            return Some((out, i + 1));
        }
        if src[i] == '[' && i + 2 < src.len() && src[i + 1] == ':' {
            let start = i;
            i += 2;
            while i + 1 < src.len() {
                if src[i] == ':' && src[i + 1] == ']' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            for ch in &src[start..i] {
                out.push(*ch);
            }
            continue;
        }
        if src[i] == '\\' && i + 1 < src.len() {
            out.push('\\');
            out.push(src[i + 1]);
            i += 2;
            continue;
        }
        let ch = src[i];
        if matches!(ch, '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
        i += 1;
    }
    None
}

/// Compile-check a POSIX ERE without shelling out to `grep`.
fn validate_posix_ere(pattern: &str, ignore_case: bool) -> Result<(), regex::Error> {
    let Some(rust_pattern) = posix_ere_to_rust_regex(pattern) else {
        return Err(regex::Error::Syntax(
            "unterminated bracket expression".into(),
        ));
    };
    RegexBuilder::new(&rust_pattern)
        .case_insensitive(ignore_case)
        .build()
        .map(|_| ())
}

/// Compile a POSIX ERE matcher rule after validation.
fn compile_posix_ere_rule(
    pattern: &str,
    ignore_case: bool,
    line: &str,
) -> Result<RuleMatcher, String> {
    validate_posix_ere(pattern, ignore_case)
        .map_err(|_| format!("Invalid regexp to look for hunk header: {line}"))?;
    let rust_pattern = posix_ere_to_rust_regex(pattern)
        .ok_or_else(|| format!("Invalid regexp to look for hunk header: {line}"))?;
    RegexBuilder::new(&rust_pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|_| format!("Invalid regexp to look for hunk header: {line}"))
        .map(RuleMatcher::Rust)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::config::{ConfigFile, ConfigScope, ConfigSet};

    fn config_with_xfuncname(driver: &str, pattern: &str) -> ConfigSet {
        let snippet = format!("[diff \"{driver}\"]\n\txfuncname = {pattern}\n");
        let file = ConfigFile::parse(Path::new(".git/config"), &snippet, ConfigScope::Local)
            .expect("parse config snippet");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn unterminated_posix_bracket_class_is_rejected() {
        let err = compile_matcher("[a", true, false).unwrap_err();
        assert!(err.contains("Invalid regexp"), "unexpected error: {err}");
        let cfg = config_with_xfuncname("probe", "[a");
        let err = matcher_for_driver(&cfg, "probe").unwrap_err();
        assert!(err.contains("Invalid regexp"), "unexpected error: {err}");
    }

    #[test]
    fn all_builtin_userdiff_drivers_compile_and_cache() {
        let config = ConfigSet::default();
        for (name, _, _) in BUILTIN_PATTERN_DEFS {
            if name.is_empty() || *name == "default" {
                continue;
            }
            let matcher = matcher_for_driver(&config, name)
                .unwrap_or_else(|e| panic!("driver {name}: {e}"))
                .unwrap_or_else(|| panic!("driver {name} missing builtin matcher"));
            assert!(
                cached_builtin_matcher(name).is_some(),
                "driver {name} must be cached"
            );
            let _ = matcher.match_line(" \n");
        }
    }
}
