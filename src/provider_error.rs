//! Failures reported by the LLM provider, kept in structured form.
//!
//! Every decision (retry, mint a new key, give up) is taken on the HTTP status and on the
//! provider's `metadata.error_type`, **never on the message text**: moderation errors echo the
//! user's prompt back, and matching on prose would pick up whatever the user wrote.

use serde_json::Value;

use crate::error::Error;
use crate::http::HttpResponse;
use crate::time::{now_ms, parse_http_date_ms};

/// The `error_type` values the provider documents. Anything else (a new name, or `unmapped`)
/// decides nothing, and the numeric status is used instead.
const CONCLUSIVE: &[&str] = &[
    "context_length_exceeded",
    "max_tokens_exceeded",
    "token_limit_exceeded",
    "string_too_long",
    "authentication",
    "permission_denied",
    "payment_required",
    "rate_limit_exceeded",
    "provider_overloaded",
    "provider_unavailable",
    "invalid_request",
    "invalid_prompt",
    "not_found",
    "precondition_failed",
    "payload_too_large",
    "unprocessable",
    "content_policy_violation",
    "refusal",
    "invalid_image",
    "image_too_large",
    "image_too_small",
    "unsupported_image_format",
    "image_not_found",
    "image_download_failed",
    "server",
    "timeout",
];
/// The only failures a new runtime key can fix: a key that is no longer valid, or a cap reached.
const REMINTABLE: &[&str] = &["authentication", "payment_required"];
/// Failures where the provider certainly generated nothing, so the same request can be sent
/// again. Timeouts and unavailable upstreams are left out: the answer may have been generated.
const TRANSIENT: &[&str] = &["rate_limit_exceeded", "provider_overloaded"];
/// A non-JSON error body is cut to this many characters in the message.
const MAX_RAW_MESSAGE: usize = 300;

/// A failed provider call: status, classification and the provider's own message.
#[derive(Clone, Debug)]
pub(crate) struct ProviderError {
    pub message: String,
    pub http_status: u16,
    pub provider_code: Option<u16>,
    pub error_type: Option<String>,
    pub retry_after_ms: Option<u64>,
}

/// The provider's error body: `{"error": {"code", "message", "metadata": {"error_type"}}}`.
struct Envelope {
    message: Option<String>,
    code: Option<u16>,
    error_type: Option<String>,
}

fn read_envelope(payload: &Value) -> Option<Envelope> {
    let err = payload.get("error")?.as_object()?;
    Some(Envelope {
        message: err
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string),
        code: err
            .get("code")
            .and_then(Value::as_u64)
            .and_then(|c| u16::try_from(c).ok()),
        error_type: err
            .get("metadata")
            .and_then(|m| m.get("error_type"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(MAX_RAW_MESSAGE) {
        Some((at, _)) => format!("{}…", text.get(..at).unwrap_or(text)),
        None => text.to_string(),
    }
}

/// The wait a `Retry-After` header asks for, in milliseconds. Both standard forms are read
/// (seconds, or an HTTP date); a nonsensical value counts as no value.
pub(crate) fn retry_after_ms(header: Option<&str>) -> Option<u64> {
    let h = header?.trim();
    if let Ok(seconds) = h.parse::<f64>() {
        return (seconds.is_finite() && seconds > 0.0)
            .then(|| (seconds * 1000.0).min(86_400_000.0).round() as u64);
    }
    let delta = parse_http_date_ms(h)? - now_ms();
    u64::try_from(delta).ok().filter(|d| *d > 0)
}

impl ProviderError {
    /// The failure described by a non-`2xx` response.
    pub fn from_response(res: &HttpResponse, label: &str) -> Self {
        let parsed = res.json().as_ref().and_then(read_envelope);
        let detail = parsed
            .as_ref()
            .and_then(|p| p.message.clone())
            .unwrap_or_else(|| truncate(&res.body));
        ProviderError {
            message: format!("{label} {}: {detail}", res.status),
            http_status: res.status,
            provider_code: parsed.as_ref().and_then(|p| p.code),
            error_type: parsed.and_then(|p| p.error_type),
            retry_after_ms: retry_after_ms(res.header("Retry-After")),
        }
    }

    /// The failure delivered inside a `200 OK` body, which the provider documents as possible.
    /// The status that matters is then `error.code`, not the transport's `200`.
    pub fn in_body(payload: &Value, label: &str) -> Option<Self> {
        let p = read_envelope(payload)?;
        Some(ProviderError {
            message: format!(
                "{label} {}: {}",
                p.code.unwrap_or(200),
                p.message.as_deref().unwrap_or("error in the response body")
            ),
            http_status: 200,
            provider_code: p.code,
            error_type: p.error_type,
            retry_after_ms: None,
        })
    }

    fn conclusive_type(&self) -> Option<&str> {
        self.error_type
            .as_deref()
            .filter(|t| CONCLUSIVE.contains(t))
    }

    fn code(&self) -> u16 {
        self.provider_code.unwrap_or(self.http_status)
    }

    /// Whether a new runtime key may fix this: only `401` (key no longer valid) and `402` (cap
    /// reached). A `402` doesn't say whether the key's cap or the account ran out, so a new key
    /// is tried rather than guessed about.
    pub fn is_remintable(&self) -> bool {
        match self.conclusive_type() {
            Some(t) => REMINTABLE.contains(&t),
            None => matches!(self.code(), 401 | 402),
        }
    }

    /// Whether the same request may succeed a moment later. Never true together with
    /// [`Self::is_remintable`], so a retry never chains into a new key.
    pub fn is_transient(&self) -> bool {
        match self.conclusive_type() {
            Some(t) => TRANSIENT.contains(&t),
            None => matches!(self.code(), 429 | 503),
        }
    }

    /// The caller-facing error when the provider classified the failure: `PROVIDER_<TYPE>` with
    /// the provider's status.
    pub fn named(&self) -> Option<Error> {
        let t = self.conclusive_type()?;
        let status = self.provider_code.unwrap_or(if self.http_status == 200 {
            502
        } else {
            self.http_status
        });
        Some(Error::new(
            self.message.clone(),
            &format!("PROVIDER_{}", t.to_ascii_uppercase()),
            status,
        ))
    }

    /// The caller-facing error: [`Self::named`], or `INTERNAL_ERROR` when unclassified.
    pub fn into_error(self) -> Error {
        self.named()
            .unwrap_or_else(|| Error::new(self.message, "INTERNAL_ERROR", 500))
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

/// The `200 OK` that is not a success: the output budget ran out and nothing usable came back.
///
/// Exactly one combination is caught (`finish_reason: "length"`, no tool calls, no text): a
/// false positive here would fail an answer the caller could have used.
pub(crate) fn empty_completion(
    payload: &Value,
    model: &str,
    max_tokens: Option<u64>,
) -> Option<Error> {
    let choice = payload.pointer("/choices/0")?;
    if choice.get("finish_reason").and_then(Value::as_str) != Some("length") {
        return None;
    }
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    if message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|c| !c.is_empty())
    {
        return None;
    }
    if !text_of(message.get("content").unwrap_or(&Value::Null))
        .trim()
        .is_empty()
    {
        return None;
    }
    let used = payload
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = payload
        .pointer("/usage/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let budget = max_tokens.map_or_else(
        || "its output budget".to_string(),
        |n| format!("{n} output tokens"),
    );
    let (cause, remedy) = if reasoning > 0 {
        (
            format!("spent {reasoning} on reasoning and returned no content"),
            "turn reasoning off or raise `max_tokens`",
        )
    } else {
        (
            "ran out of budget without returning content".to_string(),
            "raise `max_tokens`",
        )
    };
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(model);
    Some(Error::new(
        format!("{model} reached {budget} ({used} used), {cause}: {remedy}"),
        "EMPTY_COMPLETION",
        502,
    ))
}
