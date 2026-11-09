//! A mock exchange to try an application against, with no account and no money: the same
//! stateful fake the tests use, built from the exchange's published specification.
//!
//!     cargo run --example mock_exchange --features testing
//!
//! It prints the REST and WebSocket addresses and a throwaway key to paste into the application's
//! broker settings. It accepts any key; nothing it does touches a real exchange.

use degenview_brokers::coinbase::fake::{CoinbaseExchange, KEY_NAME, generate_key_pem};

fn main() {
    let exchange = CoinbaseExchange::start();
    println!("Mock exchange running. Leave this window open.\n");
    println!("REST:      {}", exchange.rest.url());
    println!("WebSocket: {}\n", exchange.stream.url);
    println!("Start the application with:");
    println!(
        "  DEGENVIEW_MOCK_BROKER_URL={} DEGENVIEW_MOCK_BROKER_WS_URL={} \\",
        exchange.rest.url(),
        exchange.stream.url
    );
    println!("  cargo run --release --features dev-mock-broker\n");
    println!("API key name:\n  {KEY_NAME}\n");
    println!("Private key (a throwaway; the mock accepts any key):\n{}", generate_key_pem());
    std::thread::park();
}
