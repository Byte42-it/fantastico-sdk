# Changelog

## 0.2.2

- **`Options::budget` now bounds each provider request**, not only the waits between retries:
  every request's timeout is what is left of the budget, at most 90 seconds. When nothing is left,
  `chat_completion` returns `TIMEOUT` (504) without sending the request. Minting a key is not
  counted, as before.
- README: a new "Keys and credit" section on what fantastico.cc does with runtime keys. Unspent
  credit returns to the account when a key expires or is revoked, so a key the client replaces
  is not lost money, and `revoke_key` only brings that forward. "Residual credit" moved there.

## 0.2.1

- The source is public at <https://github.com/Byte42-it/fantastico-sdk>, and `repository` in the
  manifest points there. No code change.

## 0.2.0

Breaking change: the crate no longer knows the model provider. The address, the default model and
extra request fields come with the key, from the mint's reply (`baseUrl`, `defaultModel`,
`chatDefaults`), and a newly minted key brings its own.

- **Removed the `DEFAULT_MODEL` constant.** Code that imported it drops the import: the default
  model is now chosen by fantastico.cc when the key is minted.
- **`Options::default_model` is now `Option<String>`**, `None` by default. Code that set it writes
  `default_model: Some("…".into())`.
- **`EdgeConfig::model` is now `Option<String>`**: `None` when neither the service nor
  `Options::default_model` names a model. Passing it as it is to `chat_completion` (even as
  `null`) is fine: the model named when the key was minted is used.
- Model precedence: the request's `model`, then `Options::default_model`, then the model named
  when the key was minted. When none names one, `chat_completion` refuses with `MODEL_REQUIRED`
  (400) without calling the provider.
- When the mint doesn't say where to use the key, `chat_completion` and `ensure_key` refuse with
  `PROVIDER_ACCESS_MISSING` (502): there is no fallback address.
- The crate no longer adds `usage: {include: true}` itself: the service asks for it through
  `chatDefaults`. Fields written by the caller and by the crate win over those in `chatDefaults`.
- Requires a server that sends `baseUrl` in the mint's reply.

## 0.1.1

- Expose `reserved_unspent_usd`, `total_remaining_usd` and `low_total_balance` on `Balance`,
  read from both account config and balance responses. Fields remain optional for older servers.
- Keep `remaining_usd` unchanged: free to reserve, not the customer-facing residual credit.

## 0.1.0

First release.

- `Fantastico::new` and `Fantastico::with_transport`. The API surface is chosen by the key's prefix.
- `chat_completion`:
  - mints the runtime key and reuses it;
  - fills in `max_tokens` from the key's cap;
  - retries `429` and `503`;
  - mints a new key once after `401` and `402`;
  - reports `PROVIDER_*` and `EMPTY_COMPLETION` errors.
- `config`, `balance`, `models`, `keys`, `revoke_key`, `ensure_key`, `current_key`.
- No secrets in `Debug` output, and no panics outside tests.
