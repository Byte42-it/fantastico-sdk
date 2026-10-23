use std::fmt;

/// The error type of every fallible call in this crate.
///
/// `code` is meant for programs and `message` for people. The code tells who answered and what
/// it means:
///
/// - a code from the account service, such as `INVALID_CONTO_KEY` or `ALLOWANCE_EXHAUSTED`;
/// - `PROVIDER_<TYPE>` when the LLM provider classified the failure, such as
///   `PROVIDER_CONTENT_POLICY_VIOLATION`;
/// - a code of this crate for refusals decided before any request, such as
///   `UNKNOWN_CREDENTIAL` or `SURFACE_UNSUPPORTED`.
///
/// `status` is the matching HTTP status. The same status can come from different senders with
/// opposite remedies (a `403` from the account service is an account problem, from the provider
/// a moderation block), so decide on `code`, not on `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// A human-readable description.
    pub message: String,
    /// A machine-readable code.
    pub code: String,
    /// The matching HTTP status.
    pub status: u16,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>, code: &str, status: u16) -> Self {
        Self {
            message: message.into(),
            code: code.to_string(),
            status,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}, {})", self.message, self.code, self.status)
    }
}

impl std::error::Error for Error {}
