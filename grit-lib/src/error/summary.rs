//! [`ErrorSummary`]: a snapshot of a failure carried as the cause of another error.

/// Classification and text of a failure recorded inside another error.
///
/// Composite failures (a rollback that failed after a checkout stopped, one remote's failure
/// among several promisor remotes) keep each inner failure as a summary instead of nesting
/// another [`Error`](enum@super::Error): the outer variant names what happened, and the summary
/// keeps the inner failure's stable [`kind`](Self::kind), its [`area`](Self::area), its
/// [`message`](Self::message), and whether it was a [cancellation](Self::is_cancelled).
///
/// The summary is one pointer wide so composite variants stay small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorSummary {
    inner: Box<SummaryInner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SummaryInner {
    kind: &'static str,
    area: &'static str,
    message: String,
    cancelled: bool,
}

impl ErrorSummary {
    /// The inner failure's [`Error::kind_name`](super::Error::kind_name).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.inner.kind
    }

    /// The inner failure's [`Error::area`](super::Error::area).
    #[must_use]
    pub fn area(&self) -> &'static str {
        self.inner.area
    }

    /// The inner failure's display text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// Whether the inner failure was a cancellation
    /// ([`Error::is_cancelled`](super::Error::is_cancelled)).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled
    }
}

impl From<&super::Error> for ErrorSummary {
    fn from(err: &super::Error) -> Self {
        Self {
            inner: Box::new(SummaryInner {
                kind: err.kind_name(),
                area: err.area(),
                message: err.to_string(),
                cancelled: err.is_cancelled(),
            }),
        }
    }
}

impl From<super::Error> for ErrorSummary {
    fn from(err: super::Error) -> Self {
        Self::from(&err)
    }
}

impl std::fmt::Display for ErrorSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.inner.message)
    }
}

impl std::error::Error for ErrorSummary {}

#[cfg(feature = "serde-models")]
impl serde::Serialize for ErrorSummary {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut out = serializer.serialize_struct("ErrorSummary", 4)?;
        out.serialize_field("kind", self.kind())?;
        out.serialize_field("area", self.area())?;
        out.serialize_field("message", self.message())?;
        out.serialize_field("cancelled", &self.is_cancelled())?;
        out.end()
    }
}
