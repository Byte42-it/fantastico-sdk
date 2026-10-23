//! Checks an account key against FANTASTICO: prints the account and its credit and, with
//! `--chat`, sends one chat completion (this spends real credit).
//!
//! ```sh
//! FANTASTICO_KEY=ck-fant-… cargo run --example check
//! FANTASTICO_KEY=ck-fant-… cargo run --example check -- --chat "Say hello"
//! ```

use fantastico_sdk::{Fantastico, Options};
use serde_json::json;

fn main() -> Result<(), fantastico_sdk::Error> {
    let key = std::env::var("FANTASTICO_KEY").unwrap_or_default();
    let mut ai = Fantastico::new(key, Options::default())?;
    let account = ai.config()?;
    println!(
        "account {} ({}), suggested model {}",
        account.account_id,
        account.status,
        account
            .model
            .as_deref()
            .unwrap_or("(chosen when the key is minted)")
    );
    match &account.balance {
        Some(b) => println!(
            "{:.4} USD available{}",
            b.remaining_usd,
            if b.low_balance { " (low)" } else { "" }
        ),
        None => println!("balance not available"),
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, prompt] = args.as_slice() {
        if flag == "--chat" {
            let reply = ai.chat_completion(&json!({
                "model": account.model,
                "messages": [{"role": "user", "content": prompt}]
            }))?;
            println!("{}", reply["choices"][0]["message"]["content"]);
            println!("cost: {} USD", reply["usage"]["cost"]);
            // Return the key now rather than at expiry, releasing its reserved credit.
            if let Some(k) = ai.current_key() {
                ai.revoke_key(&k.record_id)?;
            }
        }
    }
    Ok(())
}
