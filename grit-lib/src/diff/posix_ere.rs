//! POSIX extended regular expressions for pickaxe (`regcomp` / `regexec` semantics).
//!
//! Git compiles pickaxe patterns with `REG_EXTENDED | REG_NEWLINE` and matches with
//! leftmost-longest alternation. On Unix we call libc directly; on other targets we
//! use the `regex` crate so cross-compiles (e.g. Windows MSVC) still build.

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::sync::Arc;

    use libc::{
        regcomp, regex_t, regexec, regfree, regmatch_t, REG_EXTENDED, REG_ICASE, REG_NEWLINE,
        REG_NOMATCH,
    };

    /// Compiled POSIX BRE (`REG_NEWLINE` only), as Git uses for `blame -L` / line-range.
    #[derive(Debug, Clone)]
    pub(crate) struct PosixBre {
        inner: Arc<CompiledRegex>,
    }

    /// Compiled POSIX ERE (`REG_EXTENDED | REG_NEWLINE`).
    #[derive(Debug, Clone)]
    pub(crate) struct PosixEre {
        inner: Arc<CompiledRegex>,
    }

    #[derive(Debug)]
    struct CompiledRegex {
        re: regex_t,
    }

    unsafe impl Send for CompiledRegex {}
    unsafe impl Sync for CompiledRegex {}

    impl Drop for CompiledRegex {
        fn drop(&mut self) {
            unsafe {
                regfree(&mut self.re);
            }
        }
    }

    /// Apple's `REG_ENHANCED` (`<regex.h>`), absent from the `libc` crate.
    ///
    /// Apple libc only understands the GNU-style BRE escapes (`\+`, `\?`, `\|`, `\<`, …)
    /// when this flag is set. Git's Darwin build sets it for every basic regex
    /// (`USE_ENHANCED_BASIC_REGULAR_EXPRESSIONS`), so `git log -L '/a\+/'` on macOS
    /// treats `\+` as "one or more" exactly as glibc does. Passing it here keeps BRE
    /// semantics identical across glibc and Apple targets.
    #[cfg(target_vendor = "apple")]
    const BRE_PLATFORM_CFLAGS: libc::c_int = 0o400;
    #[cfg(not(target_vendor = "apple"))]
    const BRE_PLATFORM_CFLAGS: libc::c_int = 0;

    impl PosixBre {
        pub(crate) fn compile(pattern: &str) -> Result<Self, String> {
            let c_pat = CString::new(pattern)
                .map_err(|_| "pattern contains an interior NUL byte".to_owned())?;
            let mut re: regex_t = unsafe { std::mem::zeroed() };
            let cflags = REG_NEWLINE | BRE_PLATFORM_CFLAGS;
            let rc = unsafe { regcomp(&mut re, c_pat.as_ptr(), cflags) };
            if rc != 0 {
                return Err(regcomp_error_message(&re, rc));
            }
            Ok(Self {
                inner: Arc::new(CompiledRegex { re }),
            })
        }

        pub(crate) fn find_from(&self, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
            if from > haystack.len() {
                return None;
            }
            regexec_from(&self.inner.re, haystack, from)
        }
    }

    impl PosixEre {
        pub(crate) fn compile(pattern: &str, ignore_case: bool) -> Result<Self, String> {
            let c_pat = CString::new(pattern)
                .map_err(|_| "pattern contains an interior NUL byte".to_owned())?;
            let mut re: regex_t = unsafe { std::mem::zeroed() };
            let mut cflags = REG_EXTENDED | REG_NEWLINE;
            if ignore_case {
                cflags |= REG_ICASE;
            }
            let rc = unsafe { regcomp(&mut re, c_pat.as_ptr(), cflags) };
            if rc != 0 {
                return Err(regcomp_error_message(&re, rc));
            }
            Ok(Self {
                inner: Arc::new(CompiledRegex { re }),
            })
        }

        #[must_use]
        pub(crate) fn is_match(&self, haystack: &[u8]) -> bool {
            self.find_from(haystack, 0).is_some()
        }

        #[must_use]
        pub(crate) fn count_nonoverlapping(&self, haystack: &[u8]) -> usize {
            if haystack.is_empty() {
                return 0;
            }
            let mut count = 0usize;
            let mut pos = 0usize;
            while pos <= haystack.len() {
                let Some((_, end)) = self.find_from(haystack, pos) else {
                    break;
                };
                count += 1;
                if end > pos {
                    pos = end;
                } else {
                    pos += 1;
                }
            }
            count
        }

        /// Leftmost match starting at or after `from` (Git `regexec` with `REG_STARTEND`).
        pub(crate) fn find_from(&self, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
            if from > haystack.len() {
                return None;
            }
            regexec_from(&self.inner.re, haystack, from)
        }
    }

    /// Whether a scan starting at `from` begins mid-line, so `^` must not match there.
    ///
    /// Patterns are compiled with `REG_NEWLINE`, so `^` matches at the start of the
    /// buffer and right after every `\n`, and nowhere else.
    fn starts_mid_line(haystack: &[u8], from: usize) -> bool {
        from > 0 && haystack.get(from - 1) != Some(&b'\n')
    }

    /// Length-bounded search from `from` using `REG_STARTEND` (glibc, BSD, …).
    ///
    /// glibc derives the context at `rm_so` from the preceding byte, but Apple and the
    /// BSDs treat `string + rm_so` as the beginning of the line unless `REG_NOTBOL` is
    /// given. Passing `REG_NOTBOL` whenever the scan starts mid-line makes `^` behave
    /// the same on every libc (glibc ignores the flag for non-zero `rm_so`).
    #[cfg(not(target_env = "musl"))]
    fn regexec_from(re: &regex_t, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
        use libc::{REG_NOTBOL, REG_STARTEND};

        let mut eflags = REG_STARTEND;
        if starts_mid_line(haystack, from) {
            eflags |= REG_NOTBOL;
        }
        let mut pmatch = regmatch_t { rm_so: 0, rm_eo: 0 };
        pmatch.rm_so = from as _;
        pmatch.rm_eo = haystack.len() as _;
        let rc = unsafe { regexec(re, haystack.as_ptr().cast(), 1, &mut pmatch, eflags) };
        decode_regexec(haystack.len(), rc, &pmatch)
    }

    /// musl's Rust `libc` bindings omit `REG_STARTEND`. Copy the suffix into a
    /// NUL-terminated buffer and set `REG_NOTBOL` when the suffix starts mid-line so
    /// `^` does not re-anchor on continuation offsets.
    #[cfg(target_env = "musl")]
    fn regexec_from(re: &regex_t, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
        const REG_NOTBOL: i32 = 1;

        let suffix = &haystack[from..];
        let mut nul_terminated = Vec::with_capacity(suffix.len().saturating_add(1));
        nul_terminated.extend_from_slice(suffix);
        nul_terminated.push(0);

        let mut eflags = 0;
        if starts_mid_line(haystack, from) {
            eflags |= REG_NOTBOL;
        }

        let mut pmatch = regmatch_t { rm_so: 0, rm_eo: 0 };
        let rc = unsafe { regexec(re, nul_terminated.as_ptr().cast(), 1, &mut pmatch, eflags) };
        let region_len = suffix.len();
        decode_regexec(region_len, rc, &pmatch)
            .map(|(rel_start, rel_end)| (from + rel_start, from + rel_end))
    }

    fn decode_regexec(max_end: usize, rc: i32, pmatch: &regmatch_t) -> Option<(usize, usize)> {
        if rc == REG_NOMATCH || rc != 0 {
            return None;
        }
        let start = pmatch.rm_so.max(0) as usize;
        let end = pmatch.rm_eo.max(0) as usize;
        if start > end || end > max_end {
            return None;
        }
        Some((start, end))
    }

    fn regcomp_error_message(re: &regex_t, rc: i32) -> String {
        let mut buf = [0u8; 128];
        let n = unsafe { libc::regerror(rc, re, buf.as_mut_ptr().cast(), buf.len()) };
        if n == 0 {
            return format!("regcomp failed with code {rc}");
        }
        let len = n.min(buf.len()).saturating_sub(1);
        String::from_utf8_lossy(&buf[..len]).into_owned()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn alternation_is_leftmost_longest_like_git() {
            let re = PosixEre::compile("a|aa", false).expect("compile");
            assert_eq!(re.count_nonoverlapping(b"aa\n"), 1);
        }

        #[test]
        fn grouping_alternation_counts_once_on_shortest_match() {
            let re = PosixEre::compile("(a|aa)", false).expect("compile");
            assert_eq!(re.count_nonoverlapping(b"aa\n"), 1);
        }

        #[test]
        fn anchored_alternation_counts_once_in_ab() {
            let re = PosixEre::compile("a|^b", false).expect("compile");
            assert_eq!(re.count_nonoverlapping(b"ab"), 1);
        }

        #[test]
        fn anchored_branch_does_not_match_at_continuation_offset() {
            let re = PosixEre::compile("a|^b", false).expect("compile");
            assert_eq!(re.find_from(b"ab", 0), Some((0, 1)));
            assert_eq!(re.find_from(b"ab", 1), None);
        }

        #[test]
        fn anchored_branch_matches_at_continuation_offset_after_newline() {
            let re = PosixEre::compile("^b", false).expect("compile");
            assert_eq!(re.find_from(b"a\nb", 2), Some((2, 3)));
            assert_eq!(re.count_nonoverlapping(b"b\nb\nab"), 2);
        }

        #[test]
        fn bre_plus_escape_is_one_or_more_on_every_libc() {
            let re = PosixBre::compile("a\\+").expect("compile");
            assert_eq!(re.find_from(b"xaaa", 0), Some((1, 4)));
            let literal = PosixBre::compile("a+").expect("compile");
            assert_eq!(literal.find_from(b"aaa a+", 0), Some((4, 6)));
        }

        #[test]
        fn exact_length_buffer_without_trailing_nul_byte() {
            let re = PosixEre::compile("needle", false).expect("compile");
            let mut storage = Vec::from(b"prefixneedle");
            storage.shrink_to_fit();
            assert!(
                storage.capacity() == storage.len(),
                "test requires tight allocation without spare capacity"
            );
            let haystack = storage.as_slice();
            assert!(re.is_match(haystack));
            assert_eq!(re.find_from(haystack, 6), Some((6, 12)));
        }

        #[test]
        fn offset_scan_does_not_match_past_region_end() {
            let re = PosixEre::compile("z", false).expect("compile");
            let haystack = b"abc";
            assert_eq!(re.find_from(haystack, 1), None);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use regex::RegexBuilder;

    /// BRE fallback: literal-oriented search (libc `REG_NEWLINE` unavailable on this target).
    #[derive(Debug, Clone)]
    pub(crate) struct PosixBre {
        re: regex::Regex,
    }

    impl PosixBre {
        pub(crate) fn compile(pattern: &str) -> Result<Self, String> {
            RegexBuilder::new(pattern)
                .build()
                .map(|re| Self { re })
                .map_err(|e| e.to_string())
        }

        pub(crate) fn find_from(&self, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
            if from > haystack.len() {
                return None;
            }
            let text = String::from_utf8_lossy(haystack);
            self.re.find_at(&text, from).map(|m| (m.start(), m.end()))
        }
    }

    /// Compiled POSIX ERE (Rust `regex` fallback when libc is unavailable).
    #[derive(Debug, Clone)]
    pub(crate) struct PosixEre {
        re: regex::Regex,
    }

    impl PosixEre {
        pub(crate) fn compile(pattern: &str, ignore_case: bool) -> Result<Self, String> {
            RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .multi_line(true)
                .build()
                .map(|re| Self { re })
                .map_err(|e| e.to_string())
        }

        #[must_use]
        pub(crate) fn is_match(&self, haystack: &[u8]) -> bool {
            self.re.is_match(&String::from_utf8_lossy(haystack))
        }

        #[must_use]
        pub(crate) fn count_nonoverlapping(&self, haystack: &[u8]) -> usize {
            if haystack.is_empty() {
                return 0;
            }
            let text = String::from_utf8_lossy(haystack);
            self.re.find_iter(&text).count()
        }
    }
}

pub(crate) use imp::{PosixBre, PosixEre};
