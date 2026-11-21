//! The outcome type shared by the interactive requests (completion, completion resolve,
//! signature help). Cancellation uses [`lector_base::CancelToken`]: cancelling a request's token
//! sends `$/cancelRequest` and makes the future resolve with [`RequestOutcome::Cancelled`].

/// How an interactive request ended (when the server did not fail).
#[derive(Debug, Clone, PartialEq)]
pub enum RequestOutcome<T> {
    /// The answer, for the document version the request was made at.
    Done(T),
    /// Cancelled by the caller's token or superseded by a newer request of the
    /// same kind for the same document. Nothing to show.
    Cancelled,
    /// The document changed (a newer version was synced, or the server
    /// answered `ContentModified`) while the request was in flight; the
    /// answer was dropped. Re-issue the request if it is still wanted.
    Stale,
    /// No language server for the document, the server is not running, or it
    /// does not support the feature.
    Unavailable,
}

impl<T> RequestOutcome<T> {
    /// The answer, if there is one.
    pub fn done(self) -> Option<T> {
        match self {
            RequestOutcome::Done(v) => Some(v),
            _ => None,
        }
    }

    /// Whether this is [`RequestOutcome::Done`].
    pub fn is_done(&self) -> bool {
        matches!(self, RequestOutcome::Done(_))
    }
}
