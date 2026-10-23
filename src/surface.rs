//! The two API surfaces of the account service, and the credential that picks one.

use crate::error::Error;

/// Which API surface a credential talks to. It is a property of the key, not an option: the
/// prefix decides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    /// `/api/runtime`, reached with a project key (`sk-fant-…`).
    Runtime,
    /// `/api/edge`, reached with an account key (`ck-fant-…`).
    Edge,
}

impl Surface {
    /// The surface of `api_key`.
    ///
    /// # Errors
    ///
    /// `UNKNOWN_CREDENTIAL` when the key starts with neither `sk-fant-` nor `ck-fant-`.
    pub fn of(api_key: &str) -> Result<Surface, Error> {
        if api_key.starts_with("sk-fant-") {
            Ok(Surface::Runtime)
        } else if api_key.starts_with("ck-fant-") {
            Ok(Surface::Edge)
        } else {
            Err(Error::new(
                "Unrecognised credential: a project key starts with \"sk-fant-\", an account key with \"ck-fant-\"",
                "UNKNOWN_CREDENTIAL",
                400,
            ))
        }
    }

    /// The path segment of the surface: `runtime` or `edge`.
    pub fn path(self) -> &'static str {
        match self {
            Surface::Runtime => "runtime",
            Surface::Edge => "edge",
        }
    }
}

/// The error for a call that only `needs` supports, refused before any request is sent.
pub(crate) fn unsupported(what: &str, needs: Surface) -> Error {
    let credential = match needs {
        Surface::Runtime => "a project key (`sk-fant-…`)",
        Surface::Edge => "an account key (`ck-fant-…`)",
    };
    Error::new(
        format!(
            "{what} needs {credential}, that is the /api/{} surface",
            needs.path()
        ),
        "SURFACE_UNSUPPORTED",
        400,
    )
}
