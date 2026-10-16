//! Git-compatible wildmatch pattern matching.
//!
//! Implements shell-style pattern matching for `?`, `\`, `[]`, and `*`
//! characters, with special handling of `**` for directory matching.
//!
//! Based on the algorithm by Rich Salz (1986), modified by Wayne Davison
//! for special `/` handling and `**` glob support.

/// `WM_CASEFOLD` constant.
pub const WM_CASEFOLD: u32 = 1;
/// `WM_PATHNAME` constant.
pub const WM_PATHNAME: u32 = 2;

const WM_MATCH: i32 = 0;
const WM_NOMATCH: i32 = 1;
const WM_ABORT_ALL: i32 = -1;
const WM_ABORT_TO_STARSTAR: i32 = -2;

const NEGATE_CLASS: u8 = b'!';
const NEGATE_CLASS2: u8 = b'^';

#[inline]
fn is_glob_special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

/// Core recursive matching function — closely follows the C `dowild()` for-loop
/// (`for (; *p; text++, p++)`), including advancing `text` only once per iteration
/// after `[` / `?` / literals (not inside `do_bracket` before the pattern `]` is consumed).
fn dowild(p: &[u8], text: &[u8], flags: u32) -> i32 {
    let mut pi = 0;
    let mut ti = 0;

    while pi < p.len() {
        let p_ch = p[pi];
        let t_ch = if ti < text.len() {
            text[ti]
        } else if p_ch != b'*' {
            return WM_ABORT_ALL;
        } else {
            0
        };

        let t_ch_fold = if (flags & WM_CASEFOLD) != 0 && t_ch.is_ascii_uppercase() {
            t_ch.to_ascii_lowercase()
        } else {
            t_ch
        };

        match p_ch {
            b'\\' => {
                pi += 1;
                if pi >= p.len() {
                    return WM_ABORT_ALL;
                }
                let mut lit = p[pi];
                if (flags & WM_CASEFOLD) != 0 && lit.is_ascii_uppercase() {
                    lit = lit.to_ascii_lowercase();
                }
                if t_ch_fold != lit {
                    return WM_NOMATCH;
                }
            }
            b'?' => {
                if (flags & WM_PATHNAME) != 0 && t_ch == b'/' {
                    return WM_NOMATCH;
                }
            }
            b'*' => {
                // Determine if this is ** and whether it matches slashes.
                let prev_p = pi; // position of first *
                pi += 1;

                let match_slash;
                if pi < p.len() && p[pi] == b'*' {
                    // Found '**' — skip all consecutive stars
                    while pi < p.len() && p[pi] == b'*' {
                        pi += 1;
                    }
                    if (flags & WM_PATHNAME) == 0 {
                        match_slash = true;
                    } else {
                        // Valid ** must be at start or after / , and before end or /
                        let prev_ok = prev_p < 2 || (prev_p >= 1 && p[prev_p - 1] == b'/');
                        let next_ok = pi >= p.len()
                            || p[pi] == b'/'
                            || (pi + 1 < p.len() && p[pi] == b'\\' && p[pi + 1] == b'/');

                        if prev_ok && next_ok {
                            if pi < p.len()
                                && p[pi] == b'/'
                                && dowild(&p[pi + 1..], &text[ti..], flags) == WM_MATCH
                            {
                                return WM_MATCH;
                            }
                            match_slash = true;
                        } else {
                            match_slash = false;
                        }
                    }
                } else {
                    match_slash = (flags & WM_PATHNAME) == 0;
                }

                // Trailing star(s)?
                if pi >= p.len() {
                    if !match_slash && text[ti..].contains(&b'/') {
                        return WM_ABORT_TO_STARSTAR;
                    }
                    return WM_MATCH;
                }

                // Single * followed by / with WM_PATHNAME: skip to next /
                if !match_slash && p[pi] == b'/' {
                    if let Some(pos) = text[ti..].iter().position(|&b| b == b'/') {
                        ti += pos;
                        // Git: leave `text` at `/`, then the for-loop does `text++, p++`.
                        ti += 1;
                        pi += 1;
                        continue;
                    } else {
                        return WM_ABORT_ALL;
                    }
                }

                // General star matching loop — mirrors C's while(1).
                let mut t_cur = t_ch; // current text byte
                loop {
                    if t_cur == 0 && ti >= text.len() {
                        break;
                    }

                    // Fast-advance when pattern continues with a literal
                    if !is_glob_special(p[pi]) {
                        let mut p_lit = p[pi];
                        if (flags & WM_CASEFOLD) != 0 && p_lit.is_ascii_uppercase() {
                            p_lit = p_lit.to_ascii_lowercase();
                        }
                        while ti < text.len() {
                            let mut tc = text[ti];
                            if !match_slash && tc == b'/' {
                                break;
                            }
                            if (flags & WM_CASEFOLD) != 0 && tc.is_ascii_uppercase() {
                                tc = tc.to_ascii_lowercase();
                            }
                            if tc == p_lit {
                                break;
                            }
                            ti += 1;
                        }
                        if ti >= text.len() || {
                            let mut tc = if ti < text.len() { text[ti] } else { 0 };
                            if (flags & WM_CASEFOLD) != 0 && tc.is_ascii_uppercase() {
                                tc = tc.to_ascii_lowercase();
                            }
                            tc != p_lit
                        } {
                            return if match_slash {
                                WM_ABORT_ALL
                            } else {
                                WM_ABORT_TO_STARSTAR
                            };
                        }
                    }

                    let matched = dowild(&p[pi..], &text[ti..], flags);
                    if matched != WM_NOMATCH {
                        if !match_slash || matched != WM_ABORT_TO_STARSTAR {
                            return matched;
                        }
                    } else if !match_slash && ti < text.len() && text[ti] == b'/' {
                        return WM_ABORT_TO_STARSTAR;
                    }

                    ti += 1;
                    t_cur = if ti < text.len() { text[ti] } else { 0 };
                }
                return WM_ABORT_ALL;
            }
            b'[' => {
                let result = do_bracket(p, &mut pi, t_ch_fold, t_ch, flags);
                if result != WM_MATCH {
                    return result;
                }
            }
            _ => {
                let mut p_fold = p_ch;
                if (flags & WM_CASEFOLD) != 0 && p_fold.is_ascii_uppercase() {
                    p_fold = p_fold.to_ascii_lowercase();
                }
                if t_ch_fold != p_fold {
                    return WM_NOMATCH;
                }
            }
        }

        // C `for` increment: `text++, p++` after each iteration. `do_bracket` leaves `pi`
        // past `]`; other branches leave `pi` on the byte that was matched.
        ti += 1;
        if p_ch != b'[' {
            pi += 1;
        }
    }

    if ti < text.len() {
        WM_NOMATCH
    } else {
        WM_MATCH
    }
}

/// Handle `[...]` bracket expression matching.
/// Updates `pi` to point past the closing `]`.
fn do_bracket(pattern: &[u8], pi: &mut usize, t_ch: u8, t_ch_raw: u8, flags: u32) -> i32 {
    let mut idx = *pi + 1; // skip '['
    if idx >= pattern.len() {
        return WM_ABORT_ALL;
    }

    let mut p_ch = pattern[idx];

    // Handle negation (! or ^)
    if p_ch == NEGATE_CLASS2 {
        p_ch = NEGATE_CLASS;
    }
    let negated = p_ch == NEGATE_CLASS;
    if negated {
        idx += 1;
        if idx >= pattern.len() {
            return WM_ABORT_ALL;
        }
        p_ch = pattern[idx];
    }

    let mut prev_ch: u8 = 0;
    let mut matched = false;

    loop {
        if idx >= pattern.len() {
            return WM_ABORT_ALL;
        }

        if p_ch == b'\\' {
            idx += 1;
            if idx >= pattern.len() {
                return WM_ABORT_ALL;
            }
            p_ch = pattern[idx];
            if t_ch == p_ch {
                matched = true;
            }
        } else if p_ch == b'-'
            && prev_ch != 0
            && idx + 1 < pattern.len()
            && pattern[idx + 1] != b']'
        {
            idx += 1;
            p_ch = pattern[idx];
            if p_ch == b'\\' {
                idx += 1;
                if idx >= pattern.len() {
                    return WM_ABORT_ALL;
                }
                p_ch = pattern[idx];
            }
            if t_ch <= p_ch && t_ch >= prev_ch {
                matched = true;
            } else if (flags & WM_CASEFOLD) != 0 && t_ch.is_ascii_lowercase() {
                let t_upper = t_ch.to_ascii_uppercase();
                if t_upper <= p_ch && t_upper >= prev_ch {
                    matched = true;
                }
            }
            p_ch = 0; // makes prev_ch = 0
        } else if p_ch == b'[' && idx + 1 < pattern.len() && pattern[idx + 1] == b':' {
            // POSIX character class [:name:]
            let s = idx + 2;
            let mut end = s;
            while end < pattern.len() && pattern[end] != b']' {
                end += 1;
            }
            if end >= pattern.len() {
                return WM_ABORT_ALL;
            }
            let i = (end as isize) - (s as isize) - 1;
            if i < 0 || pattern[end - 1] != b':' {
                // Didn't find ":]", treat '[' as literal in the set
                if t_ch == b'[' {
                    matched = true;
                }
                // Continue processing from next char (the ':')
                prev_ch = b'[';
                idx += 1;
                p_ch = pattern[idx];
                continue;
            }
            let class_name = &pattern[s..end - 1];
            match match_posix_class(class_name, t_ch, flags) {
                Some(true) => matched = true,
                Some(false) => {}
                None => return WM_ABORT_ALL, // malformed class name
            }
            p_ch = 0; // makes prev_ch = 0
            idx = end; // points at ']'
        } else if t_ch == p_ch {
            matched = true;
        }

        prev_ch = p_ch;
        idx += 1;
        if idx >= pattern.len() {
            return WM_ABORT_ALL;
        }
        p_ch = pattern[idx];

        if p_ch == b']' {
            break;
        }
    }

    *pi = idx + 1; // past ']'

    if matched == negated || ((flags & WM_PATHNAME) != 0 && t_ch_raw == b'/') {
        WM_NOMATCH
    } else {
        WM_MATCH
    }
}

/// Returns Some(true/false) for known classes, None for unknown (malformed).
fn match_posix_class(class: &[u8], ch: u8, flags: u32) -> Option<bool> {
    Some(match class {
        b"alnum" => ch.is_ascii_alphanumeric(),
        b"alpha" => ch.is_ascii_alphabetic(),
        b"blank" => ch == b' ' || ch == b'\t',
        b"cntrl" => ch.is_ascii_control(),
        b"digit" => ch.is_ascii_digit(),
        b"graph" => ch.is_ascii_graphic(),
        b"lower" => ch.is_ascii_lowercase(),
        b"print" => ch.is_ascii_graphic() || ch == b' ',
        b"punct" => ch.is_ascii_punctuation(),
        b"space" => ch.is_ascii_whitespace(),
        b"upper" => {
            ch.is_ascii_uppercase() || ((flags & WM_CASEFOLD) != 0 && ch.is_ascii_lowercase())
        }
        b"xdigit" => ch.is_ascii_hexdigit(),
        _ => return None,
    })
}

/// Match the pattern against the text string.
///
/// Returns `true` if the pattern matches the text.
///
/// # Flags
/// - `WM_CASEFOLD` (1): Case-insensitive matching
/// - `WM_PATHNAME` (2): `*` and `?` don't match `/`; `**` is required
pub fn wildmatch(pattern: &[u8], text: &[u8], flags: u32) -> bool {
    dowild(pattern, text, flags) == WM_MATCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_match() {
        assert!(wildmatch(b"foo", b"foo", 0));
        assert!(!wildmatch(b"foo", b"bar", 0));
        assert!(!wildmatch(b"e", b"e\"", WM_PATHNAME));
        assert!(wildmatch(b"e\"", b"e\"", WM_PATHNAME));
    }

    #[test]
    fn question_mark() {
        assert!(wildmatch(b"???", b"foo", 0));
        assert!(!wildmatch(b"??", b"foo", 0));
    }

    #[test]
    fn star() {
        assert!(wildmatch(b"*", b"foo", 0));
        assert!(wildmatch(b"f*", b"foo", 0));
        assert!(!wildmatch(b"*f", b"foo", 0));
    }

    #[test]
    fn bracket() {
        assert!(wildmatch(b"*[al]?", b"ball", 0));
        assert!(!wildmatch(b"[ten]", b"ten", 0));
        assert!(wildmatch(b"[o][o]", b"oo", 0));
        assert!(!wildmatch(b"[o][o]", b"[o][o]", 0));
    }

    #[test]
    fn double_star_pathname() {
        assert!(wildmatch(b"foo/**/bar", b"foo/baz/bar", WM_PATHNAME));
        assert!(wildmatch(b"**/foo", b"foo", WM_PATHNAME));
        assert!(wildmatch(b"**/foo", b"XXX/foo", WM_PATHNAME));
    }

    #[test]
    fn casefold() {
        assert!(wildmatch(b"[A-Z]", b"a", WM_CASEFOLD));
        assert!(wildmatch(b"[a-z]", b"A", WM_CASEFOLD));
    }

    #[test]
    fn character_classes() {
        assert!(wildmatch(b"[[:alpha:]][[:digit:]][[:upper:]]", b"a1B", 0));
    }

    #[test]
    fn malformed_class() {
        // [:spaci:] is not a valid class → should abort (no match)
        assert!(!wildmatch(b"[[:digit:][:upper:][:spaci:]]", b"1", 0));
    }

    #[test]
    fn starstar_deep_match() {
        assert!(wildmatch(
            b"**/*a*b*g*n*t",
            b"abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt",
            WM_PATHNAME,
        ));
    }
}
