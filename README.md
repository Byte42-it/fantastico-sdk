# fantastico-sdk

One prepaid credit for every major LLM.

A Rust client for [FANTASTICO](https://fantastico.cc). With a single account and one package you can send OpenAI-style chat completions to every major model. Spending runs against the account's prepaid credit, inside spending caps. Your prompts go straight to the model provider and FANTASTICO never sees them.

```toml
[dependencies]
fantastico-sdk = "0.2"
serde_json = "1"
```

```rust
use fantastico_sdk::{Fantastico, Options};
use serde_json::json;

let mut ai = Fantastico::new("ck-fant-…", Options::default())?;

// Check the key without spending anything, and read the suggested model.
let account = ai.config()?;

let reply = ai.chat_completion(&json!({
    "model": account.model,
    "messages": [{"role": "user", "content": "Name three primary colours."}],
    "tools": [ /* function calling, OpenAI format */ ]
}))?;
println!("{}", reply["choices"][0]["message"]["content"]);
```

## What it does for you

- **Keys**: the client mints a short-lived runtime key with a spending cap and reuses it until a minute before it expires. The key lives only in memory and never reaches logs, `Debug` output or error messages. `current_key()` describes it (cap, expiry, handle) without revealing it.
- **Where requests go**: the address, the default model and any extra request fields come with the key from the account service, not from this crate. A replacement key brings its own. The model is the request's `model`, else `Options::default_model`, else the one named with the key; with none of them the call fails with `MODEL_REQUIRED`.
- **`max_tokens`**: when the request doesn't set it, the client fills in a value that fits the key's cap. The provider checks the worst-case cost of a request against the cap before generating anything. An explicit value is never changed.
- **Retries**: decisions are taken on the HTTP status and on the provider's error type, never on message text.

  | Provider answer | What the client does |
  |---|---|
  | `429`, `503` | Retries the same request, honouring `Retry-After`, within `Options::budget` |
  | `401`, `402` | Mints a new key and retries once |
  | `403`, other `4xx`, `502`, `504` | Returns `PROVIDER_<TYPE>` with the provider's status; no new key |
  | `200` with an `error` body | Treated as that error |
  | `200` with an empty answer at the token limit | Returns `EMPTY_COMPLETION` |

- **Errors**: every failure is a `fantastico_sdk::Error` with a machine-readable `code` and a `status`. The crate never panics on malformed replies.

## Keys and credit

The lifecycle of runtime keys and of the credit behind them is run by fantastico.cc, not by this crate. The client only asks for a key, uses it, and asks for another when it stops working.

- **Minting reserves credit.** Each runtime key holds a reservation (its cap) taken from the account's available credit. The cap and the key's lifetime come from the project's plan. An account can have a few keys open at once, up to a limit set by the plan.
- **Actual spend is measured by the service**, from the provider's usage, while the key is in use. What the client reports is not used for billing.
- **Unspent credit comes back on its own.** When a key expires, the service disables it and settles it a few minutes later: the measured spend is charged and the rest of the reservation returns to the account's available credit. A key the client drops (after a `401`/`402`, or because it is about to expire) is not lost money: it settles the same way.
- **`revoke_key` only brings the settlement forward.** It returns the same amount, sooner, and frees one of the account's open-key slots. You never need to call it to get credit back.
- **Running out of a key is not running out of credit.** `max_tokens` is sized on the key's cap, so a key that is nearly used up can answer `402`; the client then mints a fresh key from the account's remaining credit and retries once. If the account can't fund a new key, the mint fails and that error is returned.

### Residual credit

`config().balance` and `balance()` expose two figures:

| Field | Meaning |
|---|---|
| `remaining_usd` | Credit free to commit to new keys: the balance minus what open keys hold. |
| `total_remaining_usd` | What the user has left: `remaining_usd` plus the unspent part of open keys' reservations. Provisional while keys are open; it settles as they close. |

Show users `total_remaining_usd` as it is: don't add or subtract the current key's `cost_usd`. `low_total_balance` is the matching recharge warning. `total_remaining_usd` is optional: an older server doesn't send it.

## Credentials

The key's prefix picks the API surface, so there is nothing to configure:

| Key | Surface | For |
|---|---|---|
| `ck-fant-…` | `/api/edge` | One end user's account. Safe to ship in a distributed app. |
| `sk-fant-…` | `/api/runtime` | A trusted backend that owns the project. |

`config()` and `balance()` are available with account keys. With a project key, setting `user` on a request returns an error rather than being silently ignored.

## Not included yet

- Marketplace tools.
- A built-in tool-calling loop: your application runs its own.
- Project-level account management.

## Transport

The `ureq` feature, on by default, provides a blocking HTTP client with rustls. Turn it off to bring your own `Transport` through `Fantastico::with_transport`. The client is synchronous: in GUI or async applications, call it from a worker thread.

## Trying a real key

```sh
FANTASTICO_KEY=ck-fant-… cargo run --example check
FANTASTICO_KEY=ck-fant-… cargo run --example check -- --chat "Say hello"   # spends credit
```

## Licence

At your option, [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE).
