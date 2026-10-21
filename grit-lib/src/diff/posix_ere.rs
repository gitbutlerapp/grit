//! POSIX extended regular expressions for pickaxe (`regcomp` / `regexec` semantics).
//!
//! Git compiles pickaxe patterns with `REG_EXTENDED | REG_NEWLINE` and matches with
//! leftmost-longest alternation. Rust's `regex` crate uses leftmost-first and must not
//! be used for `-G` or `-S --pickaxe-regex`.

use std::ffi::CString;
use std::sync::Arc;

use libc::{
    regcomp, regex_t, regexec, regfree, regmatch_t, REG_EXTENDED, REG_ICASE, REG_NEWLINE,
    REG_NOMATCH, REG_STARTEND,
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

// `regexec(3)` may run concurrently on the same compiled regex; only `regfree` mutates.
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
    /// Compile a pickaxe ERE pattern.
    ///
    /// # Errors
    ///
    /// Returns [`PickaxeError::InvalidPattern`] when `regcomp` fails.
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

    /// Returns true when `haystack` contains at least one match.
    #[must_use]
    pub(crate) fn is_match(&self, haystack: &[u8]) -> bool {
        self.find_from(haystack, 0).is_some()
    }

    /// Non-overlapping occurrence count (Git pickaxe / `grep` iteration).
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
        let mut pmatch = regmatch_t {
            rm_so: from as libc::regoff_t,
            rm_eo: haystack.len() as libc::regoff_t,
        };
        let rc = unsafe {
            regexec(
                &self.inner.re,
                haystack.as_ptr().cast(),
                1,
                &mut pmatch,
                REG_STARTEND,
            )
        };
        if rc == REG_NOMATCH {
            return None;
        }
        if rc != 0 {
            return None;
        }
        let start = pmatch.rm_so.max(0) as usize;
        let end = pmatch.rm_eo.max(0) as usize;
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
