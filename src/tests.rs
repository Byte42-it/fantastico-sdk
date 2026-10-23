//! The client against a fake transport: each request takes the next queued reply and must go to
//! the expected URL. No network calls.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::backoff::{delay_for, RetryConfig};
use crate::http::{HttpRequest, HttpResponse, Method, Transport};
use crate::provider_error::{retry_after_ms, ProviderError};
use crate::time::{format_iso_ms, now_ms, parse_http_date_ms, parse_iso_ms};
use crate::{max_tokens, Error, Fantastico, Options, Surface};

type Log = Arc<Mutex<Vec<HttpRequest>>>;

struct Fake {
    queue: Mutex<VecDeque<(&'static str, HttpResponse)>>,
    log: Log,
}

impl Transport for Fake {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        // The price list (for `max_tokens`) is requested when the key has a cap: unless the test
        // queued one, it is empty and not counted among the requests.
        let listed = self
            .queue
            .lock()
            .unwrap()
            .front()
            .is_some_and(|(w, _)| w.ends_with("/models"));
        if req.url.ends_with("/models") && !listed {
            return Ok(res(200, json!({"models": []})));
        }
        self.log.lock().unwrap().push(req.clone());
        let (want, res) = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected request: {req:?}"));
        if want == "NETWORK" {
            return Err("connection refused".into());
        }
        assert!(
            req.url.ends_with(want),
            "expected `{want}`, got {}",
            req.url
        );
        Ok(res)
    }
}

fn res(status: u16, body: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: body.to_string(),
    }
}

/// The address every fixture key is accepted at. A made-up host: the crate must send the
/// request wherever the mint says, not to an address of its own.
const KEY_BASE: &str = "https://llm.example.test/v1";

fn mint_at(limit: f64, record: &str, base_url: &str) -> HttpResponse {
    res(
        200,
        json!({"runtimeKey": format!("rk-{record}"), "expiresAt": format_iso_ms(now_ms() + 3_600_000), "recordId": record,
               "limit": limit, "cost": "0.50", "activitySessionId": "sess-1",
               "baseUrl": base_url, "defaultModel": "vendor/model-default",
               "chatDefaults": {"usage": {"include": true}}}),
    )
}

fn mint(limit: f64, record: &str) -> HttpResponse {
    mint_at(limit, record, KEY_BASE)
}

fn completion(text: &str) -> HttpResponse {
    res(
        200,
        json!({"model": "m", "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}], "usage": {"cost": 0.001}}),
    )
}

fn provider_err(status: u16, error_type: Option<&str>) -> HttpResponse {
    let metadata = error_type.map_or(Value::Null, |t| json!({"error_type": t}));
    res(
        status,
        json!({"error": {"code": status, "message": format!("refused {status}"), "metadata": metadata}}),
    )
}

fn client(key: &str, script: Vec<(&'static str, HttpResponse)>) -> (Fantastico, Log) {
    let log: Log = Arc::default();
    let fake = Fake {
        queue: Mutex::new(script.into()),
        log: log.clone(),
    };
    let c = Fantastico::with_transport(key, Options::default(), Box::new(fake))
        .unwrap()
        .with_sleep(|_| {});
    (c, log)
}

fn ask() -> Value {
    json!({"messages": [{"role": "user", "content": "hello"}]})
}

fn body(r: &HttpRequest) -> Value {
    serde_json::from_str(r.body.as_deref().unwrap_or("{}")).unwrap()
}

#[test]
fn the_prefix_chooses_the_surface() {
    let (c, _) = client("ck-fant-abc", vec![]);
    assert_eq!(c.surface(), Surface::Edge);
    let (c, _) = client("sk-fant-prod-abc", vec![]);
    assert_eq!(c.surface(), Surface::Runtime);
    for bad in ["", "rk-123", "fant-abc", "CK-FANT-abc"] {
        let e = Fantastico::with_transport(
            bad,
            Options::default(),
            Box::new(Fake {
                queue: Mutex::default(),
                log: Arc::default(),
            }),
        )
        .unwrap_err();
        assert_eq!(e.code, "UNKNOWN_CREDENTIAL");
    }
}

#[test]
fn mints_once_and_reuses_the_key() {
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", completion("uno")),
            ("/chat/completions", completion("due")),
        ],
    );
    assert_eq!(
        c.chat_completion(&ask()).unwrap()["choices"][0]["message"]["content"],
        "uno"
    );
    assert_eq!(
        c.chat_completion(&ask()).unwrap()["choices"][0]["message"]["content"],
        "due"
    );
    let log = log.lock().unwrap();
    // The account service sees the account key, the provider the runtime key; never the reverse.
    assert_eq!(log[0].header("Authorization"), Some("Bearer ck-fant-abc"));
    assert_eq!(log[1].header("Authorization"), Some("Bearer rk-r1"));
    assert_eq!(log[1].header("X-Title"), Some("FANTASTICO SDK"));
    assert_eq!(log[1].url, format!("{KEY_BASE}/chat/completions"));
    let sent = body(&log[1]);
    assert_eq!(sent["model"], "vendor/model-default");
    // Asked for by the mint, not written by the crate.
    assert_eq!(sent["usage"], json!({"include": true}));
    // A roomy cap (5 USD): no max_tokens is written on the caller's behalf.
    assert!(sent.get("max_tokens").is_none());
    let h = c.current_key().unwrap();
    assert_eq!(
        (h.record_id.as_str(), h.limit_usd, h.cost_usd.as_deref()),
        ("r1", Some(5.0), Some("0.50"))
    );
    assert_eq!(c.activity_session_id(), Some("sess-1"));
}

#[test]
fn the_session_and_product_go_to_every_mint() {
    let log: Log = Arc::default();
    let script = vec![
        ("/api/edge/key", mint(5.0, "r1")),
        ("/chat/completions", provider_err(401, None)),
        ("/api/edge/key", mint(5.0, "r2")),
        ("/chat/completions", completion("ok")),
    ];
    let fake = Fake {
        queue: Mutex::new(script.into()),
        log: log.clone(),
    };
    let opts = Options {
        product: Some("example-product".into()),
        ..Options::default()
    };
    let mut c = Fantastico::with_transport("ck-fant-abc", opts, Box::new(fake)).unwrap();
    c.chat_completion(&ask()).unwrap();
    let log = log.lock().unwrap();
    assert_eq!(body(&log[0]), json!({"product": "example-product"}));
    assert_eq!(
        body(&log[2]),
        json!({"product": "example-product", "activitySessionId": "sess-1"})
    );
}

#[test]
fn an_expiring_key_is_replaced() {
    let soon = res(
        200,
        json!({"runtimeKey": "rk-old", "expiresAt": format_iso_ms(now_ms() + 30_000), "recordId": "old",
               "baseUrl": KEY_BASE, "defaultModel": "vendor/model-default"}),
    );
    let (mut c, _) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", soon),
            ("/chat/completions", completion("a")),
            ("/api/edge/key", mint(5.0, "new")),
            ("/chat/completions", completion("b")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    c.chat_completion(&ask()).unwrap();
    assert_eq!(c.current_key().unwrap().record_id, "new");
}

#[test]
fn payment_required_remints_once() {
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(0.5, "r1")),
            (
                "/chat/completions",
                provider_err(402, Some("payment_required")),
            ),
            ("/api/edge/key", mint(0.5, "r2")),
            ("/chat/completions", completion("ok")),
        ],
    );
    assert!(c.chat_completion(&ask()).is_ok());
    assert_eq!(
        log.lock().unwrap()[3].header("Authorization"),
        Some("Bearer rk-r2")
    );

    // Twice in a row: a provider error, not a verdict on the account's funds.
    let (mut c, _) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(0.5, "r1")),
            ("/chat/completions", provider_err(402, None)),
            ("/api/edge/key", mint(0.5, "r2")),
            ("/chat/completions", provider_err(402, None)),
        ],
    );
    let e = c.chat_completion(&ask()).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("LLM_PROVIDER_ERROR", 502));
}

#[test]
fn a_refused_mint_is_the_clouds_verdict() {
    let refused = res(
        402,
        json!({"message": "Credit exhausted", "type": "ALLOWANCE_EXHAUSTED", "code": 402}),
    );
    let (mut c, _) = client("ck-fant-abc", vec![("/api/edge/key", refused)]);
    let e = c.chat_completion(&ask()).unwrap_err();
    assert_eq!(
        e,
        Error {
            message: "Credit exhausted".into(),
            code: "ALLOWANCE_EXHAUSTED".into(),
            status: 402
        }
    );
}

#[test]
fn moderation_is_not_reminted_and_keeps_its_name() {
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            (
                "/chat/completions",
                provider_err(403, Some("content_policy_violation")),
            ),
        ],
    );
    let e = c.chat_completion(&ask()).unwrap_err();
    assert_eq!(
        (e.code.as_str(), e.status),
        ("PROVIDER_CONTENT_POLICY_VIOLATION", 403)
    );
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[test]
fn rate_limits_are_retried_with_backoff() {
    let mut limited = provider_err(429, Some("rate_limit_exceeded"));
    limited.headers.push(("retry-after".into(), "2".into()));
    let slept: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let log: Log = Arc::default();
    let script = vec![
        ("/api/edge/key", mint(5.0, "r1")),
        ("/chat/completions", limited.clone()),
        ("/chat/completions", limited),
        ("/chat/completions", completion("ok")),
    ];
    let fake = Fake {
        queue: Mutex::new(script.into()),
        log,
    };
    let s = slept.clone();
    let mut c = Fantastico::with_transport("ck-fant-abc", Options::default(), Box::new(fake))
        .unwrap()
        .with_sleep(move |d| s.lock().unwrap().push(d));
    assert!(c.chat_completion(&ask()).is_ok());
    let slept = slept.lock().unwrap();
    assert_eq!(slept.len(), 2);
    // Between half and all of Retry-After, never less.
    assert!(
        slept.iter().all(|d| (1000..=2000).contains(&d.as_millis())),
        "{slept:?}"
    );

    // Once the retries are used up, the 429 arrives under its own name.
    let l = provider_err(429, Some("rate_limit_exceeded"));
    let (mut c, _) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", l.clone()),
            ("/chat/completions", l.clone()),
            ("/chat/completions", l),
        ],
    );
    assert_eq!(
        c.chat_completion(&ask()).unwrap_err().code,
        "PROVIDER_RATE_LIMIT_EXCEEDED"
    );
}

#[test]
fn an_error_inside_a_200_triggers_the_remint() {
    let embedded = res(
        200,
        json!({"error": {"code": 402, "message": "no credits", "metadata": {"error_type": "payment_required"}}}),
    );
    let (mut c, _) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", embedded),
            ("/api/edge/key", mint(5.0, "r2")),
            ("/chat/completions", completion("ok")),
        ],
    );
    assert!(c.chat_completion(&ask()).is_ok());
}

#[test]
fn an_empty_completion_at_the_limit_is_an_error() {
    let empty = res(
        200,
        json!({"choices": [{"message": {"content": ""}, "finish_reason": "length"}], "usage": {"completion_tokens": 64}}),
    );
    let (mut c, _) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", empty),
        ],
    );
    assert_eq!(
        c.chat_completion(&ask()).unwrap_err().code,
        "EMPTY_COMPLETION"
    );
}

#[test]
fn max_tokens_follows_the_key_limit() {
    let models = res(
        200,
        json!({"models": [{"id": "vendor/model-default", "pricing": {"completion": "0.000015"}}]}),
    );
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(0.05, "r1")),
            ("/api/edge/models", models),
            ("/chat/completions", completion("ok")),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    // 0.05 * 0.8 / 0.000015 = 2666
    assert_eq!(body(&log.lock().unwrap()[2])["max_tokens"], 2666);
    // An explicit max_tokens passes untouched (and the price list stays cached).
    c.chat_completion(&json!({"messages": [], "max_tokens": 10}))
        .unwrap();
    assert_eq!(body(&log.lock().unwrap()[3])["max_tokens"], 10);

    assert_eq!(max_tokens::affordable(Some(100.0), Some(0.000015)), None);
    assert_eq!(
        max_tokens::affordable(Some(0.0001), Some(0.000015)),
        Some(64)
    );
    for (l, p) in [
        (None, Some(1.0)),
        (Some(1.0), None),
        (Some(f64::NAN), Some(1.0)),
        (Some(1.0), Some(f64::INFINITY)),
        (Some(-1.0), Some(1.0)),
        (Some(1.0), Some(0.0)),
    ] {
        assert_eq!(max_tokens::affordable(l, p), None);
    }
}

#[test]
fn user_is_refused_before_any_request() {
    let (mut c, log) = client("ck-fant-abc", vec![]);
    assert_eq!(
        c.chat_completion(&json!({"messages": [], "user": "studio-1"}))
            .unwrap_err()
            .code,
        "SURFACE_UNSUPPORTED"
    );
    let (mut c, _) = client("sk-fant-prod-abc", vec![]);
    assert_eq!(
        c.chat_completion(&json!({"messages": [], "user": "studio-1"}))
            .unwrap_err()
            .code,
        "NOT_AVAILABLE"
    );
    assert_eq!(c.balance().unwrap_err().code, "SURFACE_UNSUPPORTED");
    assert_eq!(c.config().unwrap_err().code, "SURFACE_UNSUPPORTED");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn config_and_balance_read_the_account() {
    let config = res(
        200,
        json!({"contoId": "conto-42", "userKey": "u-7", "status": "active", "model": "vendor/model-suggested", "balance": {"remainingUsd": 0.5, "lowBalance": false}}),
    );
    let balance = res(
        200,
        json!({"remainingUsd": "0.03", "lowBalance": true, "status": "active"}),
    );
    let (c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/config", config),
            ("/api/edge/balance", balance),
            (
                "/api/edge/config",
                res(200, json!({"contoId": "c", "balance": null})),
            ),
        ],
    );
    let cfg = c.config().unwrap();
    assert_eq!(
        (cfg.account_id.as_str(), cfg.model.as_deref()),
        ("conto-42", Some("vendor/model-suggested"))
    );
    assert_eq!(cfg.balance.unwrap().remaining_usd, 0.5);
    let b = c.balance().unwrap();
    assert!(b.low_balance && (b.remaining_usd - 0.03).abs() < 1e-12);
    // An unreadable balance doesn't stop the client from starting.
    assert_eq!(c.config().unwrap().balance, None);
    assert_eq!(log.lock().unwrap()[0].method, Method::Get);
}

#[test]
fn revoking_the_key_in_hand_forgets_it() {
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", mint(5.0, "rec/1")),
            ("/api/edge/keys/rec%2F1", res(200, json!({"revoked": true}))),
        ],
    );
    c.ensure_key().unwrap();
    c.revoke_key("rec/1").unwrap();
    assert!(c.current_key().is_none());
    assert_eq!(log.lock().unwrap()[1].method, Method::Delete);
}

#[test]
fn secrets_never_reach_debug_output() {
    let (mut c, log) = client("ck-fant-secret", vec![("/api/edge/key", mint(5.0, "r1"))]);
    c.ensure_key().unwrap();
    let shown = format!("{c:?} {:?}", log.lock().unwrap());
    assert!(
        !shown.contains("ck-fant-secret") && !shown.contains("rk-r1"),
        "{shown}"
    );
}

#[test]
fn hostile_replies_never_panic() {
    let garbage = [
        res(200, json!({})),
        res(
            200,
            json!({"runtimeKey": 7, "expiresAt": "yesterday", "recordId": null}),
        ),
        res(
            200,
            json!({"runtimeKey": "k", "expiresAt": "2026-13-45T99:99:99Z", "recordId": "r"}),
        ),
        HttpResponse {
            status: 200,
            headers: vec![],
            body: "<html>".into(),
        },
        HttpResponse {
            status: 500,
            headers: vec![],
            body: "x".repeat(10_000),
        },
        res(200, json!([1, 2, 3])),
    ];
    for g in garbage {
        let (mut c, _) = client("ck-fant-abc", vec![("/api/edge/key", g)]);
        assert!(c.chat_completion(&ask()).is_err());
    }
    let hostile_completions = [
        res(200, json!({"choices": []})),
        res(200, json!({"choices": [{"message": null}]})),
        HttpResponse {
            status: 200,
            headers: vec![],
            body: "not json".into(),
        },
        res(200, json!({"error": "a string"})),
        HttpResponse {
            status: 503,
            headers: vec![("Retry-After".into(), "-5".into())],
            body: "{".into(),
        },
    ];
    for h in hostile_completions {
        let (mut c, _) = client(
            "ck-fant-abc",
            vec![
                ("/api/edge/key", mint(5.0, "r")),
                ("/chat/completions", h.clone()),
                ("/chat/completions", h.clone()),
                ("/chat/completions", h),
            ],
        );
        let _ = c.chat_completion(&ask());
    }
    let (mut c, _) = client("ck-fant-abc", vec![("NETWORK", res(0, json!({})))]);
    assert_eq!(c.chat_completion(&ask()).unwrap_err().code, "NETWORK_ERROR");
    let (mut c, _) = client("ck-fant-abc", vec![]);
    assert_eq!(
        c.chat_completion(&json!("text")).unwrap_err().code,
        "INVALID_REQUEST"
    );
}

#[test]
fn dates_parse_or_are_none() {
    assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
    assert_eq!(
        parse_iso_ms("2026-10-13T12:00:00+02:00"),
        parse_iso_ms("2026-10-13T10:00:00Z")
    );
    let t = now_ms();
    assert_eq!(parse_iso_ms(&format_iso_ms(t)), Some(t));
    for bad in [
        "",
        "2026-10-13",
        "2026-10-13T25:00:00Z",
        "x-y-zTa:b:cZ",
        "2026-10-13T10:00:00",
        "9999999-01-01T00:00:00Z",
    ] {
        assert_eq!(parse_iso_ms(bad), None, "{bad}");
    }
    assert_eq!(
        parse_http_date_ms("Thu, 01 Jan 1970 00:00:10 GMT"),
        Some(10_000)
    );
    assert_eq!(parse_http_date_ms("tomorrow"), None);
    assert_eq!(retry_after_ms(Some("1.5")), Some(1500));
    assert_eq!(retry_after_ms(Some("Thu, 01 Jan 1970 00:00:10 GMT")), None);
    for bad in ["0", "-1", "NaN", "inf", "abc"] {
        assert_eq!(retry_after_ms(Some(bad)), None, "{bad}");
    }
}

#[test]
fn transient_and_remintable_are_disjoint() {
    for status in [
        400u16, 401, 402, 403, 404, 408, 413, 429, 500, 502, 503, 504,
    ] {
        for t in [
            None,
            Some("authentication"),
            Some("payment_required"),
            Some("rate_limit_exceeded"),
            Some("provider_overloaded"),
            Some("unmapped"),
            Some("timeout"),
        ] {
            let p = ProviderError::from_response(&provider_err(status, t), "x");
            assert!(!(p.is_transient() && p.is_remintable()), "{status} {t:?}");
        }
    }
    let p = ProviderError::from_response(&provider_err(429, None), "x");
    let cfg = RetryConfig::default();
    assert_eq!(delay_for(&p, 1, &cfg, 0.0), Duration::from_millis(250));
    assert_eq!(
        delay_for(&p, 30, &cfg, 0.999_999),
        Duration::from_millis(10_000)
    );
}

#[test]
fn balance_exposes_the_server_total_without_adding_local_reservations() {
    let data = json!({"remainingUsd": 5, "reservedUnspentUsd": "4.97", "totalRemainingUsd": "9.97",
        "lowBalance": true, "lowTotalBalance": false, "status": "open"});
    let (c, _) = client(
        "ck-fant-abc",
        vec![
            (
                "/api/edge/config",
                res(200, json!({"contoId": "a", "balance": data.clone()})),
            ),
            ("/api/edge/balance", res(200, data)),
        ],
    );
    let balance = c.config().unwrap().balance.unwrap();
    assert_eq!(balance.remaining_usd, 5.0);
    assert_eq!(balance.reserved_unspent_usd, Some(4.97));
    assert_eq!(balance.total_remaining_usd, Some(9.97));
    assert_eq!(balance.low_total_balance, Some(false));
    assert_eq!(c.balance().unwrap(), balance);
}

#[test]
fn older_servers_do_not_create_a_guessed_total() {
    let (c, _) = client(
        "ck-fant-abc",
        vec![(
            "/api/edge/balance",
            res(200, json!({"remainingUsd": 5, "lowBalance": false})),
        )],
    );
    let balance = c.balance().unwrap();
    assert_eq!(balance.remaining_usd, 5.0);
    assert_eq!(balance.total_remaining_usd, None);
    assert_eq!(balance.reserved_unspent_usd, None);
    assert_eq!(balance.low_total_balance, None);
}

fn client_with(opts: Options, script: Vec<(&'static str, HttpResponse)>) -> (Fantastico, Log) {
    let log: Log = Arc::default();
    let fake = Fake {
        queue: Mutex::new(script.into()),
        log: log.clone(),
    };
    let c = Fantastico::with_transport("ck-fant-abc", opts, Box::new(fake))
        .unwrap()
        .with_sleep(|_| {});
    (c, log)
}

#[test]
fn the_budget_bounds_each_request() {
    // The provider request may not outlast the budget, whatever the transport's own timeout.
    let opts = Options {
        budget: Duration::from_secs(5),
        ..Options::default()
    };
    let (mut c, log) = client_with(
        opts,
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    let timeout = log.lock().unwrap()[1].timeout;
    assert!(timeout <= Duration::from_secs(5), "{timeout:?}");

    // A spent budget sends nothing to the provider: the key is minted, the request isn't.
    let opts = Options {
        budget: Duration::ZERO,
        ..Options::default()
    };
    let (mut c, log) = client_with(opts, vec![("/api/edge/key", mint(5.0, "r1"))]);
    assert_eq!(c.chat_completion(&ask()).unwrap_err().code, "TIMEOUT");
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn the_request_goes_where_the_mint_says() {
    // A trailing slash on the mint's address is dropped, not doubled.
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            (
                "/api/edge/key",
                mint_at(5.0, "r1", "https://gateway.example.test/api/v1/"),
            ),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    assert_eq!(
        log.lock().unwrap()[1].url,
        "https://gateway.example.test/api/v1/chat/completions"
    );
}

#[test]
fn chat_defaults_are_merged_under_what_the_caller_and_the_crate_write() {
    let minted = res(
        200,
        json!({"runtimeKey": "rk-1", "expiresAt": format_iso_ms(now_ms() + 3_600_000), "recordId": "r1",
               "baseUrl": KEY_BASE, "defaultModel": "vendor/model-default",
               "chatDefaults": {"usage": {"include": true}, "temperature": 0.1,
                                "model": "vendor/from-defaults", "max_tokens": 1}}),
    );
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", minted),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&json!({"messages": [], "temperature": 0.7, "max_tokens": 50}))
        .unwrap();
    let sent = body(&log.lock().unwrap()[1]);
    // The mint's own fields arrive untouched…
    assert_eq!(sent["usage"], json!({"include": true}));
    // …and every field the caller or the crate writes wins over them.
    assert_eq!(sent["temperature"], 0.7);
    assert_eq!(sent["max_tokens"], 50);
    assert_eq!(sent["model"], "vendor/model-default");
}

#[test]
fn without_chat_defaults_the_crate_adds_nothing_of_its_own() {
    let minted = res(
        200,
        json!({"runtimeKey": "rk-1", "expiresAt": format_iso_ms(now_ms() + 3_600_000), "recordId": "r1",
               "baseUrl": KEY_BASE, "defaultModel": "m"}),
    );
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            ("/api/edge/key", minted),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    let sent = body(&log.lock().unwrap()[1]);
    assert!(sent.get("usage").is_none(), "{sent}");
}

#[test]
fn the_model_comes_from_the_request_then_the_options_then_the_mint() {
    // The request's model wins over everything.
    let opts = Options {
        default_model: Some("vendor/configured".into()),
        ..Options::default()
    };
    let (mut c, log) = client_with(
        opts,
        vec![
            ("/api/edge/key", mint(5.0, "r1")),
            ("/chat/completions", completion("ok")),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&json!({"model": "vendor/explicit", "messages": []}))
        .unwrap();
    // Then the integrator's default, over the one the mint names.
    c.chat_completion(&ask()).unwrap();
    let log = log.lock().unwrap();
    assert_eq!(body(&log[1])["model"], "vendor/explicit");
    assert_eq!(body(&log[2])["model"], "vendor/configured");

    // Nobody names one: refused before reaching the provider.
    let anonymous = res(
        200,
        json!({"runtimeKey": "rk-1", "expiresAt": format_iso_ms(now_ms() + 3_600_000), "recordId": "r1",
               "baseUrl": KEY_BASE, "defaultModel": null}),
    );
    let (mut c, log) = client("ck-fant-abc", vec![("/api/edge/key", anonymous)]);
    let e = c.chat_completion(&ask()).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("MODEL_REQUIRED", 400));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn a_mint_without_an_address_is_refused_with_no_fallback() {
    for base in [Value::Null, json!(""), json!(7)] {
        let minted = res(
            200,
            json!({"runtimeKey": "rk-1", "expiresAt": format_iso_ms(now_ms() + 3_600_000), "recordId": "r1",
                   "baseUrl": base, "defaultModel": "m"}),
        );
        let (mut c, log) = client("ck-fant-abc", vec![("/api/edge/key", minted)]);
        let e = c.chat_completion(&ask()).unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.status),
            ("PROVIDER_ACCESS_MISSING", 502)
        );
        // Nothing was sent anywhere after the mint, and no key is kept.
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(c.current_key().is_none());
    }
}

#[test]
fn a_reminted_key_is_spent_at_its_own_address() {
    let (mut c, log) = client(
        "ck-fant-abc",
        vec![
            (
                "/api/edge/key",
                mint_at(5.0, "r1", "https://one.example.test/v1"),
            ),
            (
                "/chat/completions",
                provider_err(402, Some("payment_required")),
            ),
            (
                "/api/edge/key",
                mint_at(5.0, "r2", "https://two.example.test/v1"),
            ),
            ("/chat/completions", completion("ok")),
        ],
    );
    c.chat_completion(&ask()).unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log[1].url, "https://one.example.test/v1/chat/completions");
    assert_eq!(log[3].url, "https://two.example.test/v1/chat/completions");
    assert_eq!(log[3].header("Authorization"), Some("Bearer rk-r2"));
}

#[test]
fn invalid_total_credit_is_unavailable_instead_of_nan_or_infinite() {
    for value in [
        json!("NaN"),
        json!("inf"),
        Value::Null,
        json!({}),
        json!("not money"),
    ] {
        let (c, _) = client(
            "ck-fant-abc",
            vec![(
                "/api/edge/balance",
                res(200, json!({"remainingUsd": 5, "totalRemainingUsd": value})),
            )],
        );
        assert_eq!(c.balance().unwrap().total_remaining_usd, None);
    }
}
