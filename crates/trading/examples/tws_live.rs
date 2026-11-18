//! Live check against a running TWS paper session.
//!
//! `cargo run -p zeron-trading --example tws_live`
//!
//! Talks to the real API port (127.0.0.1:7497, paper only), subscribes the
//! read-only streams, prints what the dashboard would show for a few seconds
//! and disconnects. Never sends an order: this is the link/ledger probe used
//! when TWS is open and the fake TWS tests are not enough.

use std::time::Duration;

use zeron_trading::TradingService;

#[tokio::main]
async fn main() {
    let service = TradingService::new();
    println!("demo snapshot: live={}", service.live());

    match service.connect().await {
        Ok(account) => println!("connect ok: {account:?}"),
        Err(error) => {
            println!("connect failed: {error}");
            return;
        }
    }

    for tick in 1..=8u32 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let snapshot = service.snapshot();
        let accounts: Vec<String> = snapshot
            .accounts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|account| {
                format!(
                    "{} cash={:?} equity={:?} pnl_day={:?}",
                    account.id, account.cash, account.equity, account.pnl_day
                )
            })
            .collect();
        println!(
            "t+{tick}s live={} connected={} accounts={accounts:?} positions={} logs={}",
            service.live(),
            snapshot.connected,
            snapshot.positions.len(),
            snapshot.logs.len()
        );
        if tick == 3 || tick == 8 {
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).expect("snapshot serialises")
            );
        }
    }

    service.disconnect().await;
    println!("disconnected: live={}", service.live());
}
