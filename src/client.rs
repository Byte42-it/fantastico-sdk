//! The client: runtime-key minting, chat completions, and the account routes.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::backoff::{delay_for, RetryConfig};
use crate::error::Error;
use crate::http::{HttpRequest, HttpResponse, Method, Transport};
use crate::max_tokens;
use crate::provider_error::{empty_completion, ProviderError};
use crate::surface::{unsupported, Surface};
use crate::time::{now_ms, parse_iso_ms};

/// The root of the FANTASTICO API.
pub const DEFAULT_BASE_URL: &str = "https://fantastico.cc/api";
/// A key that expires within this margin is replaced rather than reused.
const EXPIRY_SKEW_MS: i64 = 60_000;
/// How long the model price list is kept.
const MODELS_TTL: Duration = Duration::from_secs(5 * 60);
const CLOUD_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(90);

/// How a [`Fantastico`] client behaves. Start from [`Options::default`] and change what you need.
///
/// ```
/// use fantastico_sdk::Options;
///
/// let opts = Options { title: "My App".into(), ..Options::default() };
/// ```
#[derive(Clone, Debug)]
pub struct Options {
    /// The root of the FANTASTICO API, [`DEFAULT_BASE_URL`] by default.
    pub base_url: String,
    /// The model used when a request names none. `None` by default: the model the account
    /// service names when it mints the key is used instead.
    pub default_model: Option<String>,
    /// The marketplace product this client integrates, sent with every key request so the
    /// account service can apply that product's terms. `None` by default.
    pub product: Option<String>,
    /// The application name shown in the provider's usage reports (`X-Title`).
    pub title: String,
    /// The application URL shown in the provider's usage reports (`HTTP-Referer`).
    pub referer: String,
    /// How rate-limited or overloaded requests are retried.
    pub retry: RetryConfig,
    /// The longest a single [`Fantastico::chat_completion`] may spend on the provider, retries
    /// included: every request and every wait between them ends by then. Minting a key, when
    /// one is needed, is not counted.
    pub budget: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            default_model: None,
            product: None,
            title: "FANTASTICO SDK".to_string(),
            referer: "https://fantastico.cc".to_string(),
            retry: RetryConfig::default(),
            budget: Duration::from_secs(90),
        }
    }
}

/// The runtime key in hand. The secret never leaves this struct.
///
/// Where to spend the key, and on which terms, come with it from the same mint: the crate knows
/// nothing about who is on the other side, and a replacement key brings its own terms.
struct RuntimeKey {
    secret: String,
    /// The OpenAI-compatible base URL the key is accepted at, without a trailing `/`.
    base_url: String,
    /// The model for requests that name none, as the account service decided.
    default_model: Option<String>,
    /// Fields merged into every chat-completion body before the ones this crate writes.
    chat_defaults: Map<String, Value>,
    expires_at_ms: i64,
    expires_at: String,
    limit_usd: Option<f64>,
    record_id: String,
    cost_usd: Option<String>,
}

/// What can be known about the runtime key in hand: everything **except the secret**.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct KeyHandle {
    /// The key's handle, used to return it early with [`Fantastico::revoke_key`].
    pub record_id: String,
    /// The key's spending cap, in USD.
    pub limit_usd: Option<f64>,
    /// The amount the key reserved on the account, in USD. A string, because money is never
    /// a float.
    pub cost_usd: Option<String>,
    /// When the key expires, as an ISO 8601 timestamp.
    pub expires_at: String,
    /// The activity session the key belongs to, which groups spending in reports.
    pub activity_session_id: Option<String>,
}

/// An account's credit.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Balance {
    /// What can still be spent, in USD. Part of the account's balance may already be reserved
    /// by a live key, so this is the available amount, not the balance.
    pub remaining_usd: f64,
    /// Unspent reservations across all open keys, priced for the account.
    pub reserved_unspent_usd: Option<f64>,
    /// Residual credit including unspent reservations. Use for display, not authorization.
    /// Measured usage stays provisional until reconciliation. Absent on older servers.
    pub total_remaining_usd: Option<f64>,
    /// Recharge warning based on total residual credit, when provided by the server.
    pub low_total_balance: Option<bool>,
    /// Whether the credit is below the top-up threshold set by the account service.
    pub low_balance: bool,
    /// The account's state, such as `active` or `suspended`, when reported.
    pub status: Option<String>,
}

/// Everything an installation needs to know about its own account (`GET /api/edge/config`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EdgeConfig {
    /// The account's identifier. It is not a secret: it can be shown and recorded.
    pub account_id: String,
    /// The account's key within its project.
    pub user_key: String,
    /// `active` or `suspended`.
    pub status: String,
    /// The model the account service suggests for this account, or [`Options::default_model`]
    /// when it suggests none. `None` when neither names one.
    pub model: Option<String>,
    /// The account's credit, or `None` when it could not be read: the client can still start.
    pub balance: Option<Balance>,
}

/// How one attempt at the provider ended.
enum Failure {
    /// The provider refused: the caller decides whether a new key may help.
    Provider(ProviderError),
    /// Anything else, already in the form the caller sees.
    Final(Error),
}

/// A FANTASTICO client: one credential, one account, every model.
///
/// The client is cheap to keep and reuse: it holds the runtime key in memory between calls.
/// Its `Debug` output never shows the account key or the runtime key.
pub struct Fantastico {
    api_key: String,
    surface: Surface,
    opts: Options,
    transport: Box<dyn Transport>,
    sleep: Box<dyn Fn(Duration) + Send + Sync>,
    key: Option<RuntimeKey>,
    activity_session_id: Option<String>,
    models: Option<(Instant, HashMap<String, f64>)>,
    rng: u64,
}

impl fmt::Debug for Fantastico {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fantastico")
            .field("surface", &self.surface)
            .field("base_url", &self.opts.base_url)
            .field("key", &self.current_key())
            .finish_non_exhaustive()
    }
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A number sent as a JSON number or a decimal string; never NaN or infinite.
fn finite(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    n.is_finite().then_some(n)
}

fn balance_of(v: &Value) -> Option<Balance> {
    Some(Balance {
        remaining_usd: finite(v.get("remainingUsd"))?,
        reserved_unspent_usd: finite(v.get("reservedUnspentUsd")),
        total_remaining_usd: finite(v.get("totalRemainingUsd")),
        low_total_balance: v.get("lowTotalBalance").and_then(Value::as_bool),
        low_balance: v
            .get("lowBalance")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        status: str_field(v, "status"),
    })
}

impl Fantastico {
    /// A client for `api_key` with the built-in HTTP transport.
    ///
    /// # Errors
    ///
    /// `UNKNOWN_CREDENTIAL` when the key is neither an account key (`ck-fant-…`) nor a project
    /// key (`sk-fant-…`). No request is sent.
    #[cfg(feature = "ureq")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ureq")))]
    pub fn new(api_key: impl Into<String>, opts: Options) -> Result<Self, Error> {
        Self::with_transport(
            api_key,
            opts,
            Box::new(crate::http::UreqTransport::default()),
        )
    }

    /// A client for `api_key` that sends its requests through `transport`.
    ///
    /// # Errors
    ///
    /// `UNKNOWN_CREDENTIAL` when the key is neither an account key (`ck-fant-…`) nor a project
    /// key (`sk-fant-…`). No request is sent.
    pub fn with_transport(
        api_key: impl Into<String>,
        opts: Options,
        transport: Box<dyn Transport>,
    ) -> Result<Self, Error> {
        let api_key = api_key.into().trim().to_string();
        let surface = Surface::of(&api_key)?;
        let seed = u64::try_from(now_ms()).unwrap_or(1) ^ 0x9E37_79B9_7F4A_7C15;
        Ok(Self {
            api_key,
            surface,
            opts: Options {
                base_url: opts.base_url.trim_end_matches('/').to_string(),
                ..opts
            },
            transport,
            sleep: Box::new(std::thread::sleep),
            key: None,
            activity_session_id: None,
            models: None,
            rng: seed | 1,
        })
    }

    /// Replaces how the client waits between retries (`std::thread::sleep` by default). Useful
    /// in tests.
    pub fn with_sleep(mut self, sleep: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleep = Box::new(sleep);
        self
    }

    /// The API surface the credential talks to.
    pub fn surface(&self) -> Surface {
        self.surface
    }

    /// The activity session of the current key, which groups spending in reports.
    pub fn activity_session_id(&self) -> Option<&str> {
        self.activity_session_id.as_deref()
    }

    // ── Account service ──────────────────────────────────────────────────────────────────

    fn cloud_request(&self, method: Method, path: &str, body: Option<Value>) -> HttpRequest {
        let mut headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.api_key),
        )];
        if body.is_some() {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
        }
        HttpRequest {
            method,
            url: format!("{}/{}{path}", self.opts.base_url, self.surface.path()),
            headers,
            body: body.map(|b| b.to_string()),
            timeout: CLOUD_TIMEOUT,
        }
    }

    /// A call to the account service. A refusal carries the service's `message` and `type`,
    /// or `fallback` as the code when the body has none.
    fn api(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        fallback: &str,
    ) -> Result<Value, Error> {
        let req = self.cloud_request(method, path, body);
        let res = self.transport.send(&req).map_err(|e| {
            Error::new(
                format!("FANTASTICO cannot be reached: {e}"),
                "NETWORK_ERROR",
                503,
            )
        })?;
        let json = res.json();
        if !res.ok() {
            let b = json.unwrap_or(Value::Null);
            let message =
                str_field(&b, "message").unwrap_or_else(|| format!("{fallback} ({})", res.status));
            let code = str_field(&b, "type").unwrap_or_else(|| fallback.to_string());
            return Err(Error::new(message, &code, res.status));
        }
        json.ok_or_else(|| {
            Error::new(
                "The account service sent an unreadable reply",
                "INVALID_RESPONSE",
                502,
            )
        })
    }

    /// The configuration of this account. Account keys only.
    ///
    /// It checks the key **without minting anything** and returns the account's identifier,
    /// which can be recorded in place of the key: use it to validate a key a user pasted.
    ///
    /// # Errors
    ///
    /// - `SURFACE_UNSUPPORTED` with a project key (no request is sent).
    /// - `INVALID_CONTO_KEY` when the account service doesn't recognise the key.
    /// - `NETWORK_ERROR` when the account service cannot be reached.
    pub fn config(&self) -> Result<EdgeConfig, Error> {
        if self.surface != Surface::Edge {
            return Err(unsupported(
                "Reading the account configuration",
                Surface::Edge,
            ));
        }
        let v = self.api(Method::Get, "/config", None, "CONFIG_FAILED")?;
        let bad = || {
            Error::new(
                "The account configuration is incomplete",
                "INVALID_RESPONSE",
                502,
            )
        };
        Ok(EdgeConfig {
            account_id: str_field(&v, "contoId").ok_or_else(bad)?,
            user_key: str_field(&v, "userKey").unwrap_or_default(),
            status: str_field(&v, "status").unwrap_or_else(|| "active".to_string()),
            model: str_field(&v, "model").or_else(|| self.opts.default_model.clone()),
            balance: v.get("balance").and_then(balance_of),
        })
    }

    /// The credit of this account. Account keys only.
    ///
    /// # Errors
    ///
    /// - `SURFACE_UNSUPPORTED` with a project key, whose project has many accounts (no request
    ///   is sent).
    /// - An account-service code when the key is refused, or `NETWORK_ERROR`.
    pub fn balance(&self) -> Result<Balance, Error> {
        if self.surface != Surface::Edge {
            return Err(unsupported("Reading the account balance", Surface::Edge));
        }
        let v = self.api(Method::Get, "/balance", None, "BALANCE_FAILED")?;
        balance_of(&v)
            .ok_or_else(|| Error::new("The account balance is unreadable", "INVALID_RESPONSE", 502))
    }

    /// The models available to this credential, with their prices, as the account service lists
    /// them.
    ///
    /// # Errors
    ///
    /// An account-service code when the key is refused, or `NETWORK_ERROR`.
    pub fn models(&self) -> Result<Vec<Value>, Error> {
        let v = self.api(Method::Get, "/models", None, "MODELS_FAILED")?;
        Ok(v.get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// The runtime keys still open on this account, including ones minted by other processes.
    ///
    /// # Errors
    ///
    /// An account-service code when the key is refused, or `NETWORK_ERROR`.
    pub fn keys(&self) -> Result<Value, Error> {
        self.api(Method::Get, "/keys", None, "KEYS_FAILED")
    }

    /// Returns a runtime key before it expires, so its reserved credit is released at once.
    /// When it is the key in hand, the client forgets it and the next call mints a new one.
    ///
    /// # Errors
    ///
    /// An account-service code when the key is unknown or refused, or `NETWORK_ERROR`.
    pub fn revoke_key(&mut self, record_id: &str) -> Result<Value, Error> {
        let encoded: String = record_id
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let r = self.api(
            Method::Delete,
            &format!("/keys/{encoded}"),
            None,
            "REVOKE_FAILED",
        )?;
        if self.key.as_ref().is_some_and(|k| k.record_id == record_id) {
            self.key = None;
        }
        Ok(r)
    }

    // ── Runtime key ──────────────────────────────────────────────────────────────────────

    /// The runtime key the client works with, minting one if it has none.
    ///
    /// # Errors
    ///
    /// The account service's refusal (for example `ALLOWANCE_EXHAUSTED` when the account has
    /// no credit left), `MINT_FAILED` for an incomplete reply, `PROVIDER_ACCESS_MISSING` when the
    /// reply doesn't say where the key is accepted, or `NETWORK_ERROR`.
    pub fn ensure_key(&mut self) -> Result<KeyHandle, Error> {
        self.mint_if_needed()?;
        self.current_key()
            .ok_or_else(|| Error::new("No runtime key was minted", "MINT_FAILED", 402))
    }

    /// The runtime key in hand, if any, without minting.
    pub fn current_key(&self) -> Option<KeyHandle> {
        self.key.as_ref().map(|k| KeyHandle {
            record_id: k.record_id.clone(),
            limit_usd: k.limit_usd,
            cost_usd: k.cost_usd.clone(),
            expires_at: k.expires_at.clone(),
            activity_session_id: self.activity_session_id.clone(),
        })
    }

    fn mint_if_needed(&mut self) -> Result<(), Error> {
        if self
            .key
            .as_ref()
            .is_some_and(|k| k.expires_at_ms - now_ms() > EXPIRY_SKEW_MS)
        {
            return Ok(());
        }
        self.key = None;
        // The product and the session go with every request, not only the first: a key can be
        // replaced mid-conversation, and a replacement minted without them would change the
        // terms of a conversation already under way.
        let mut body = Map::new();
        if let Some(p) = &self.opts.product {
            body.insert("product".into(), json!(p));
        }
        if let Some(s) = &self.activity_session_id {
            body.insert("activitySessionId".into(), json!(s));
        }
        let data = self.api(
            Method::Post,
            "/key",
            Some(Value::Object(body)),
            "MINT_FAILED",
        )?;
        let incomplete = || {
            Error::new(
                "The account service sent an incomplete key",
                "MINT_FAILED",
                502,
            )
        };
        let secret = str_field(&data, "runtimeKey").ok_or_else(incomplete)?;
        let expires_at = str_field(&data, "expiresAt").ok_or_else(incomplete)?;
        let expires_at_ms = parse_iso_ms(&expires_at).ok_or_else(incomplete)?;
        let record_id = str_field(&data, "recordId").ok_or_else(incomplete)?;
        // No fallback address: a key spent somewhere the account service didn't name would be
        // a guess, and the crate has nothing to guess from.
        let base_url = str_field(&data, "baseUrl")
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                Error::new(
                    "The account service didn't say where to use the key (no baseUrl)",
                    "PROVIDER_ACCESS_MISSING",
                    502,
                )
            })?;
        let chat_defaults = data
            .get("chatDefaults")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(s) = str_field(&data, "activitySessionId") {
            self.activity_session_id = Some(s);
        }
        self.key = Some(RuntimeKey {
            secret,
            base_url,
            default_model: str_field(&data, "defaultModel"),
            chat_defaults,
            expires_at_ms,
            expires_at,
            limit_usd: finite(data.get("limit")).filter(|l| *l >= 0.0),
            record_id,
            cost_usd: data
                .get("cost")
                .filter(|c| !c.is_null())
                .map(|c| c.as_str().map_or_else(|| c.to_string(), str::to_string)),
        });
        Ok(())
    }

    /// The USD price of one output token of `model`, from the price list (kept five minutes).
    fn output_price(&mut self, model: &str) -> Option<f64> {
        if self
            .models
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > MODELS_TTL)
        {
            let models = self.models().ok()?;
            let by_id = models
                .iter()
                .filter_map(|m| {
                    Some((
                        str_field(m, "id")?,
                        finite(m.pointer("/pricing/completion")).filter(|p| *p > 0.0)?,
                    ))
                })
                .collect();
            self.models = Some((Instant::now(), by_id));
        }
        self.models
            .as_ref()
            .and_then(|(_, m)| m.get(model).copied())
    }

    /// A number in `[0, 1)` for retry jitter (xorshift64*: enough for jitter, nothing more).
    fn random(&mut self) -> f64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    // ── Inference ────────────────────────────────────────────────────────────────────────

    /// Sends an OpenAI-style chat-completions request (`messages`, `tools`, `model`, …) to the
    /// LLM provider with this account's runtime key, and returns the provider's reply. Where the
    /// request goes, and the fields the account service adds to it, come with the key.
    ///
    /// - Without `model`, [`Options::default_model`] is used, then the model the account service
    ///   named when it minted the key.
    /// - Without `max_tokens`, a value that fits the key's spending cap is filled in.
    /// - After `429` or `503` the same request is retried, waiting as the provider asks, within
    ///   [`Options::budget`].
    /// - After `401` or `402` a new key is minted and the request retried **once**.
    ///
    /// ```no_run
    /// # use fantastico_sdk::{Fantastico, Options};
    /// # use serde_json::json;
    /// # let mut ai = Fantastico::new("ck-fant-…", Options::default())?;
    /// let reply = ai.chat_completion(&json!({
    ///     "messages": [{"role": "user", "content": "Name three primary colours."}]
    /// }))?;
    /// let text = reply["choices"][0]["message"]["content"].as_str().unwrap_or_default();
    /// # Ok::<(), fantastico_sdk::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - `INVALID_REQUEST` when `request` is not a JSON object.
    /// - `MODEL_REQUIRED` when neither the request, [`Options::default_model`] nor the account
    ///   service names a model.
    /// - `PROVIDER_ACCESS_MISSING` when the account service didn't say where the key is accepted.
    /// - `SURFACE_UNSUPPORTED` (account keys) or `NOT_AVAILABLE` (project keys) when the request
    ///   sets `user`: attributing spending to another account is not supported by this client.
    /// - The account service's refusal to mint a key, for example `ALLOWANCE_EXHAUSTED` when
    ///   the account has no credit left.
    /// - `PROVIDER_<TYPE>` when the provider classified its refusal, for example
    ///   `PROVIDER_CONTEXT_LENGTH_EXCEEDED` or `PROVIDER_CONTENT_POLICY_VIOLATION`.
    /// - `LLM_PROVIDER_ERROR` when a new key didn't help either, `INTERNAL_ERROR` for an
    ///   unclassified provider failure.
    /// - `EMPTY_COMPLETION` when the model used its whole output budget and returned nothing.
    /// - `TIMEOUT` when [`Options::budget`] ran out before a request could be sent.
    /// - `NETWORK_ERROR` or `INVALID_RESPONSE` for transport and protocol failures.
    pub fn chat_completion(&mut self, request: &Value) -> Result<Value, Error> {
        let Some(obj) = request.as_object() else {
            return Err(Error::new(
                "The request must be a JSON object",
                "INVALID_REQUEST",
                400,
            ));
        };
        if obj.get("user").is_some_and(|u| !u.is_null()) {
            return Err(match self.surface {
                Surface::Edge => unsupported(
                    "Attributing a request to an account with `user` (an account key already stands for one account)",
                    Surface::Runtime,
                ),
                Surface::Runtime => Error::new(
                    "Attributing a request to an account with `user` is not available in this client yet",
                    "NOT_AVAILABLE",
                    400,
                ),
            });
        }
        let mut body = obj.clone();
        let explicit_max = body.get("max_tokens").is_some_and(|v| !v.is_null());
        let deadline = Instant::now() + self.opts.budget;

        // A refused mint is the account service's verdict: pass it on.
        self.mint_if_needed()?;
        // The model is chosen once, before the first attempt: a replacement key carries on the
        // same conversation, with the same model.
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or_else(|| self.opts.default_model.clone())
            .or_else(|| self.key.as_ref().and_then(|k| k.default_model.clone()))
            .ok_or_else(|| {
                Error::new(
                    "No model: set `model` on the request or `Options::default_model`",
                    "MODEL_REQUIRED",
                    400,
                )
            })?;
        body.insert("model".into(), json!(model));
        let first = match self.attempt(&body, &model, explicit_max, deadline) {
            Ok(v) => return Ok(v),
            Err(Failure::Final(e)) => return Err(e),
            Err(Failure::Provider(p)) => p,
        };
        // A new key doesn't fix a moderation block or a rate limit that outlasted the retries:
        // don't waste one.
        if !first.is_remintable() {
            return Err(first.into_error());
        }
        self.key = None;
        self.mint_if_needed()?;
        match self.attempt(&body, &model, explicit_max, deadline) {
            Ok(v) => Ok(v),
            Err(Failure::Final(e)) => Err(e),
            // The second attempt can fail for another reason: if the provider named it, say so.
            Err(Failure::Provider(second)) => match second.named() {
                Some(e) if !second.is_remintable() => Err(e),
                _ => Err(Error::new(
                    "The LLM provider kept refusing the request",
                    "LLM_PROVIDER_ERROR",
                    502,
                )),
            },
        }
    }

    /// One attempt with the key in hand, retrying transient failures around the single request.
    /// `max_tokens`, the address and the default fields are taken here, after minting, because a
    /// new key brings its own cap and its own terms.
    fn attempt(
        &mut self,
        request: &Map<String, Value>,
        model: &str,
        explicit_max: bool,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        let Some((secret, base_url, mut body)) = self.key.as_ref().map(|k| {
            (
                k.secret.clone(),
                k.base_url.clone(),
                k.chat_defaults.clone(),
            )
        }) else {
            return Err(Failure::Final(Error::new(
                "No runtime key in hand",
                "MINT_FAILED",
                402,
            )));
        };
        // The account service's fields first, then the request's: what the caller and this crate
        // write always wins.
        body.extend(request.iter().map(|(k, v)| (k.clone(), v.clone())));
        if !explicit_max {
            let limit = self.key.as_ref().and_then(|k| k.limit_usd);
            let price = if limit.is_some() {
                self.output_price(model)
            } else {
                None
            };
            if let Some(n) = max_tokens::affordable(limit, price) {
                body.insert("max_tokens".into(), json!(n));
            }
        }
        let max_tokens = body.get("max_tokens").and_then(Value::as_u64);
        let mut request = HttpRequest {
            method: Method::Post,
            url: format!("{base_url}/chat/completions"),
            headers: vec![
                ("Authorization".to_string(), format!("Bearer {secret}")),
                ("Content-Type".to_string(), "application/json".to_string()),
                ("HTTP-Referer".to_string(), self.opts.referer.clone()),
                ("X-Title".to_string(), self.opts.title.clone()),
            ],
            body: Some(Value::Object(body).to_string()),
            timeout: PROVIDER_TIMEOUT,
        };
        let mut retries = 0;
        loop {
            // The budget bounds the requests too, not only the waits between them.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Failure::Final(Error::new(
                    "The request ran out of Options::budget before reaching the provider",
                    "TIMEOUT",
                    504,
                )));
            }
            request.timeout = left.min(PROVIDER_TIMEOUT);
            let outcome = match self.transport.send(&request) {
                Ok(res) => classify(&res, model, max_tokens),
                Err(e) => Err(Failure::Final(Error::new(
                    format!("The LLM provider cannot be reached: {e}"),
                    "NETWORK_ERROR",
                    503,
                ))),
            };
            let err = match outcome {
                Err(Failure::Provider(p))
                    if p.is_transient() && retries < self.opts.retry.attempts =>
                {
                    p
                }
                other => return other,
            };
            let random = self.random();
            let delay = delay_for(&err, retries + 1, &self.opts.retry, random);
            // Never sleep past the budget: reporting the rate limit is more useful than a timeout.
            if Instant::now() + delay > deadline {
                return Err(Failure::Provider(err));
            }
            (self.sleep)(delay);
            retries += 1;
        }
    }
}

/// The provider's reply: a structured failure, or the completion.
fn classify(res: &HttpResponse, model: &str, max_tokens: Option<u64>) -> Result<Value, Failure> {
    if !res.ok() {
        return Err(Failure::Provider(ProviderError::from_response(
            res,
            "LLM provider",
        )));
    }
    let Some(payload) = res.json() else {
        return Err(Failure::Final(Error::new(
            "The LLM provider sent an unreadable reply",
            "INVALID_RESPONSE",
            502,
        )));
    };
    // A `200 OK` is not a promise: it can carry an `error` instead of a completion.
    if let Some(p) = ProviderError::in_body(&payload, "LLM provider") {
        return Err(Failure::Provider(p));
    }
    if payload
        .pointer("/choices/0/message")
        .is_none_or(|m| !m.is_object())
    {
        return Err(Failure::Final(Error::new(
            "The LLM provider's reply has no completion",
            "INVALID_RESPONSE",
            502,
        )));
    }
    if let Some(e) = empty_completion(&payload, model, max_tokens) {
        return Err(Failure::Final(e));
    }
    Ok(payload)
}
