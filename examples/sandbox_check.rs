//! Talks to Coinbase's real sandbox (no key, canned answers) through the crate and prints what
//! each call returns: `cargo run --example sandbox_check`.

use degenview_brokers::model::orders::{FillFilter, HistoryFilter};
use degenview_brokers::{
    BrokerId, ClientOrderId, Credentials, Env, OrderDraft, OrderKind, ProductId, Side, TimeInForce,
};

fn main() {
    let broker = degenview_brokers::registry::by_id(BrokerId::new("coinbase-sandbox"))
        .expect("the sandbox is compiled in");
    let connection = broker
        .connect(&Credentials::new(), &Env::default())
        .expect("connect to the sandbox");
    println!("session: {:?}", connection.session());
    println!("balances: {:?}", connection.balances());
    println!("products: {:?}", connection.products().map(|p| p.len()));
    let open = connection.open_orders();
    println!("open orders: {:?}", open.as_ref().map(Vec::len));
    println!(
        "fills: {:?}",
        connection
            .fills(&FillFilter::default(), None)
            .map(|p| p.items.len())
    );
    println!(
        "history: {:?}",
        connection
            .order_history(&HistoryFilter::default(), None)
            .map(|p| p.items.len())
    );
    let draft = OrderDraft {
        product: ProductId::new("BTC-USD"),
        side: Side::Buy,
        kind: OrderKind::Limit {
            base_size: "0.001".parse().unwrap(),
            price: "60000.00".parse().unwrap(),
            tif: TimeInForce::GoodTilCancelled,
            post_only: false,
        },
    };
    println!("preview: {:?}", connection.preview(&draft));
    let placed = connection.place(&draft, &ClientOrderId::new("sandbox-check-1"));
    println!("place: {placed:?}");
    if let Ok(placed) = placed {
        println!(
            "order by id: {:?}",
            connection.order(&placed.order_id).map(|o| o.status)
        );
        println!("cancel: {:?}", connection.cancel(&[placed.order_id]));
    }
    println!("quote: {:?}", connection.quote(&ProductId::new("BTC-USD")));
}
