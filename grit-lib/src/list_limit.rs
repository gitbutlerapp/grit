//! Bounded list results shared by notes, hooks, bundle heads, and CLI list commands.
//!
//! ```rust
//! use grit_lib::list_limit::Limited;
//!
//! let limited = Limited::from_vec(vec!["a", "b", "c"], Some(2));
//! assert_eq!(limited.items, ["a", "b"]);
//! assert!(limited.truncated);
//! ```

/// A list capped by an optional limit, with truncation when more items existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limited<T> {
    /// Items returned (at most the requested limit).
    pub items: Vec<T>,
    /// `true` when more matching items existed beyond [`Self::items`].
    pub truncated: bool,
}

impl<T> Limited<T> {
    /// Empty list, not truncated.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
        }
    }

    /// Apply `limit`, keeping at most `limit` items and setting [`Self::truncated`] when
    /// `items.len()` exceeded `limit`.
    #[must_use]
    pub fn from_vec(mut items: Vec<T>, limit: Option<usize>) -> Self {
        let Some(n) = limit else {
            return Self {
                items,
                truncated: false,
            };
        };
        if items.len() > n {
            items.truncate(n);
            Self {
                items,
                truncated: true,
            }
        } else {
            Self {
                items,
                truncated: false,
            }
        }
    }
}

/// Fetch one extra row so callers can detect truncation without a second pass.
#[must_use]
pub fn fetch_plus_one(limit: Option<usize>) -> Option<usize> {
    limit.map(|n| n.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_vec_no_limit() {
        let limited = Limited::from_vec(vec![1, 2, 3], None);
        assert_eq!(limited.items, [1, 2, 3]);
        assert!(!limited.truncated);
    }

    #[test]
    fn from_vec_truncates() {
        let limited = Limited::from_vec(vec![1, 2, 3, 4], Some(2));
        assert_eq!(limited.items, [1, 2]);
        assert!(limited.truncated);
    }

    #[test]
    fn fetch_plus_one_none() {
        assert_eq!(fetch_plus_one(None), None);
        assert_eq!(fetch_plus_one(Some(0)), Some(1));
    }
}
