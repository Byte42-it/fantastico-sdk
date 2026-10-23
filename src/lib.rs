//! Rust client for [FANTASTICO](https://fantastico.cc): one prepaid account, every major LLM.
//!
//! A FANTASTICO account holds a prepaid credit with spending caps. This crate turns that account
//! into an OpenAI-style chat-completions endpoint: it mints a short-lived, capped runtime key for
//! the account, keeps it in memory, renews it when needed, and sends your requests straight to
//! the LLM provider. The account service never sees your prompts.
//!
//! # Quick start
//!
//! ```no_run
//! use fantastico_sdk::{Fantastico, Options};
//! use serde_json::json;
//!
//! let mut ai = Fantastico::new("ck-fant-…", Options::default())?;
//!
//! // Check the key without spending anything, and read the suggested model.
//! let account = ai.config()?;
//! println!("account {} has {:?} USD left", account.account_id, account.balance);
//!
//! let reply = ai.chat_completion(&json!({
//!     "model": account.model,
//!     "messages": [{"role": "user", "content": "Hello"}]
//! }))?;
//! println!("{}", reply["choices"][0]["message"]["content"]);
//! # Ok::<(), fantastico_sdk::Error>(())
//! ```
//!
//! # Credentials
//!
//! The prefix of the key chooses the API surface, so there is nothing to configure:
//!
//! | Key | Surface | Use |
//! |---|---|---|
//! | `ck-fant-…` | `/api/edge` | An **account key**: one end user's account, safe to ship in a distributed app. |
//! | `sk-fant-…` | `/api/runtime` | A **project key**: a trusted backend that owns the project. |
//!
//! Any other string is rejected by [`Fantastico::new`] before a request is sent.
//!
//! # What `chat_completion` does for you
//!
//! - Mints the runtime key on first use and reuses it until a minute before it expires.
//! - Sends the request where the account service said the key is accepted, with the fields it
//!   asked to add: both come with the key, so a replacement key brings its own.
//! - Fills in `model` when the request names none: [`Options::default_model`] if set, otherwise
//!   the model the account service named when it minted the key.
//! - Fills in `max_tokens` from the key's spending cap when the request names none: the provider
//!   checks the worst-case cost of a request against the cap before generating anything.
//! - Retries the same request after `429` and `503`, honouring `Retry-After`, within
//!   [`Options::budget`].
//! - Mints a new key and retries **once** after `401` and `402`: the key may have been revoked,
//!   or its cap reached before it expired.
//! - Reports everything else as an [`Error`] whose `code` says who answered and what it means.
//!
//! # Errors
//!
//! Every fallible call returns [`Error`], with a machine-readable `code` and an HTTP-like
//! `status`. Codes come from three places:
//!
//! - the account service, for example `INVALID_CONTO_KEY` or `ALLOWANCE_EXHAUSTED`;
//! - the LLM provider, prefixed `PROVIDER_`, for example `PROVIDER_CONTENT_POLICY_VIOLATION`;
//! - this crate, for refusals decided before any request: `UNKNOWN_CREDENTIAL`,
//!   `SURFACE_UNSUPPORTED`, `INVALID_REQUEST`, `MODEL_REQUIRED`, `PROVIDER_ACCESS_MISSING`,
//!   `NETWORK_ERROR`, `EMPTY_COMPLETION`.
//!
//! The crate never panics on hostile input: malformed replies, dates and numbers become errors.
//!
//! # Secrets
//!
//! Neither the account key nor the runtime key appears in `Debug` output, error messages or
//! logs. [`Fantastico::current_key`] describes the runtime key (cap, expiry, handle) without
//! revealing it.
//!
//! # Feature flags
//!
//! - `ureq` (default): the built-in blocking HTTP transport, with rustls. Without it, bring your
//!   own [`Transport`] through [`Fantastico::with_transport`].
//!
//! The client is synchronous: call it from a worker thread in GUI or async applications.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod backoff;
mod client;
mod error;
mod http;
mod max_tokens;
mod provider_error;
mod surface;
mod time;

pub use backoff::RetryConfig;
pub use client::{Balance, EdgeConfig, Fantastico, KeyHandle, Options, DEFAULT_BASE_URL};
pub use error::Error;
#[cfg(feature = "ureq")]
pub use http::UreqTransport;
pub use http::{HttpRequest, HttpResponse, Method, Transport};
pub use surface::Surface;

#[cfg(test)]
mod tests;
