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

        fn find_from(&self, haystack: &[u8], from: usize) -> Option<(usize, usize)> {
            if from > haystack.len() {
                return None;
            }
            // Search from `from` via a suffix slice. `REG_STARTEND` is a BSD/glibc extension and
            // is not exposed by musl's libc bindings, so we avoid it for portable static builds.
            let suffix = &haystack[from..];
            let mut pmatch = regmatch_t { rm_so: 0, rm_eo: 0 };
            let rc = unsafe { regexec(&self.inner.re, suffix.as_ptr().cast(), 1, &mut pmatch, 0) };
            if rc == REG_NOMATCH {
                return None;
            }
            if rc != 0 {
                return None;
            }
            let start = from + pmatch.rm_so.max(0) as usize;
            let end = from + pmatch.rm_eo.max(0) as usize;
            if start > end || end > haystack.len() {
                return None;
            }
            Some((start, end))
        }
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
    }
}

#[cfg(not(unix))]
mod imp {
    use regex::RegexBuilder;

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

pub(crate) use imp::PosixEre;
