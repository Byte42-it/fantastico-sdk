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

### Residual credit

`config().balance` and `balance()` expose `total_remaining_usd` when the server supports it.
It includes unspent reservations across all open keys, minus measured account-priced consumption.
Display that value directly: do not add the current key's `cost_usd` again. `remaining_usd` keeps
its original meaning (free to commit to new keys). The new fields are optional; an older server
has no known total. `low_total_balance` is the matching recharge warning.
