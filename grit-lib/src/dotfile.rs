//! Git-compatible `.git*` / NTFS / HFS path checks (`path.c`, `utf8.c`).

/// HFS+ ignores certain Unicode code points when comparing (subset of `utf8.c`).
fn next_hfs_char(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
    loop {
        let ch = chars.next()?;
        match ch {
            '\u{200c}' | '\u{200d}' | '\u{200e}' | '\u{200f}' => continue,
            '\u{202a}'..='\u{202e}' => continue,
            '\u{206a}'..='\u{206f}' => continue,
            '\u{feff}' => continue,
            _ => return Some(ch),
        }
    }
}

fn is_hfs_dot_generic(path: &str, needle: &str) -> bool {
    let mut chars = path.chars().peekable();
    let mut c = match next_hfs_char(&mut chars) {
        Some(x) => x,
        None => return false,
    };
    if c != '.' {
        return false;
    }
    for nc in needle.chars() {
        c = match next_hfs_char(&mut chars) {
            Some(x) => x,
            None => return false,
        };
        if c as u32 > 127 {
            return false;
        }
        if !c.eq_ignore_ascii_case(&nc) {
            return false;
        }
    }
    match next_hfs_char(&mut chars) {
        None => true,
        Some('/') => true,
        Some(_) => false,
    }
}

/// Whether `path` matches `.git` under Git's HFS+ equivalence rules (`is_hfs_dotgit`).
#[must_use]
pub fn is_hfs_dotgit(path: &str) -> bool {
    is_hfs_dot_generic(path, "git")
}

/// Whether `name` matches `.git` under Git's NTFS equivalence rules (`is_ntfs_dotgit`).
#[must_use]
pub fn is_ntfs_dotgit(name: &str) -> bool {
    is_ntfs_dot_generic(name, "git", "g7c29")
}

/// Whether `path` matches `.gitmodules` under Git's HFS+ equivalence rules.
#[must_use]
pub fn is_hfs_dot_gitmodules(path: &str) -> bool {
    is_hfs_dot_generic(path, "gitmodules")
}

/// Whether `path` matches `.gitignore` under Git's HFS+ equivalence rules.
#[must_use]
pub fn is_hfs_dot_gitignore(path: &str) -> bool {
    is_hfs_dot_generic(path, "gitignore")
}

/// Whether `path` matches `.gitattributes` under Git's HFS+ equivalence rules.
#[must_use]
pub fn is_hfs_dot_gitattributes(path: &str) -> bool {
    is_hfs_dot_generic(path, "gitattributes")
}

/// Whether `path` matches `.mailmap` under Git's HFS+ equivalence rules.
#[must_use]
pub fn is_hfs_dot_mailmap(path: &str) -> bool {
    is_hfs_dot_generic(path, "mailmap")
}

fn only_spaces_and_periods(name: &str, mut i: usize) -> bool {
    let b = name.as_bytes();
    loop {
        let c = *b.get(i).unwrap_or(&0);
        if c == 0 || c == b':' {
            return true;
        }
        if c != b' ' && c != b'.' {
            return false;
        }
        i += 1;
    }
}

fn is_ntfs_dot_generic(name: &str, dotgit_name: &str, short_prefix: &str) -> bool {
    let b = name.as_bytes();
    let len = dotgit_name.len();
    if !b.is_empty()
        && b[0] == b'.'
        && name.len() > len
        && name[1..1 + len].eq_ignore_ascii_case(dotgit_name)
    {
        let i = len + 1;
        return only_spaces_and_periods(name, i);
    }

    let after_dotgit = 1 + len;
    if b.len() > after_dotgit + 1
        && b[0] == b'.'
        && name[1..after_dotgit].eq_ignore_ascii_case(dotgit_name)
        && b[after_dotgit] == b'~'
        && (b'1'..=b'4').contains(&b[after_dotgit + 1])
    {
        return only_spaces_and_periods(name, after_dotgit + 2);
    }

    let mut i = 0usize;
    let mut saw_tilde = false;
    while i < 8 {
        let c = *b.get(i).unwrap_or(&0);
        if c == 0 {
            return false;
        }
        if saw_tilde {
            if !c.is_ascii_digit() {
                return false;
            }
        } else if c == b'~' {
            i += 1;
            let d = *b.get(i).unwrap_or(&0);
            if !(b'1'..=b'9').contains(&d) {
                return false;
            }
            saw_tilde = true;
        } else if i >= 6 || c & 0x80 != 0 {
            return false;
        } else {
            let sc = short_prefix.as_bytes().get(i).copied().unwrap_or(0);
            if (c as char).to_ascii_lowercase() != sc as char {
                return false;
            }
        }
        i += 1;
    }
    only_spaces_and_periods(name, i)
}

/// Whether `name` matches `.gitmodules` under Git's NTFS equivalence rules.
#[must_use]
pub fn is_ntfs_dot_gitmodules(name: &str) -> bool {
    is_ntfs_dot_generic(name, "gitmodules", "gi7eba")
}

/// Whether `name` matches `.gitignore` under Git's NTFS equivalence rules.
#[must_use]
pub fn is_ntfs_dot_gitignore(name: &str) -> bool {
    is_ntfs_dot_generic(name, "gitignore", "gi250a")
}

/// Whether `name` matches `.gitattributes` under Git's NTFS equivalence rules.
#[must_use]
pub fn is_ntfs_dot_gitattributes(name: &str) -> bool {
    is_ntfs_dot_generic(name, "gitattributes", "gi7d29")
}

/// Whether `name` matches `.mailmap` under Git's NTFS equivalence rules.
#[must_use]
pub fn is_ntfs_dot_mailmap(name: &str) -> bool {
    is_ntfs_dot_generic(name, "mailmap", "maba30")
}

/// Dispatch a `test-tool path-utils` dotfile predicate by subcommand name.
#[must_use]
pub fn dotfile_matches(subcmd: &str, path: &str) -> bool {
    match subcmd {
        "is_dotgitmodules" => is_hfs_dot_gitmodules(path) || is_ntfs_dot_gitmodules(path),
        "is_dotgitignore" => is_hfs_dot_gitignore(path) || is_ntfs_dot_gitignore(path),
        "is_dotgitattributes" => is_hfs_dot_gitattributes(path) || is_ntfs_dot_gitattributes(path),
        "is_dotmailmap" => is_hfs_dot_mailmap(path) || is_ntfs_dot_mailmap(path),
        _ => false,
    }
}
