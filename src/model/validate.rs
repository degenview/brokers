//! Checks an order against the market's rules before anything is sent. Pure and exact.
//!
//! Nothing here rounds an order. A value that is off its increment is reported with the nearest
//! valid value below it as a suggestion; the user (or an explicit "snap" action) decides. An order
//! must never silently become a materially different order.

use std::fmt;

use rust_decimal::Decimal;

use super::capabilities::Capabilities;
use super::types::{
    Balance, OrderDraft, OrderKind, OrderSize, OrderType, ProductSpec, Side, TifKind, TimeInForce,
};

/// Which input an [`Issue`] belongs to, so a form can mark the right field.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Field {
    Product,
    Size,
    Price,
    StopPrice,
    LimitPrice,
    TimeInForce,
    Type,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueKind {
    Required,
    NotPositive,
    /// Not a multiple of the market's increment. `nearest_below` is the valid value just under it.
    OffIncrement {
        increment: Decimal,
        nearest_below: Decimal,
    },
    BelowMinimum {
        minimum: Decimal,
    },
    AboveMaximum {
        maximum: Decimal,
    },
    /// This market or broker cannot do it; the sentence says why.
    Unsupported(String),
    /// The draft is for a different market than the spec.
    WrongProduct,
    /// The market's rules were read too long ago to place an order against.
    StaleRules,
    /// A time-in-force that has already passed.
    InThePast,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    pub field: Field,
    pub kind: IssueKind,
}

impl Issue {
    fn new(field: Field, kind: IssueKind) -> Self {
        Self { field, kind }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            IssueKind::Required => f.write_str("Required."),
            IssueKind::NotPositive => f.write_str("Must be greater than zero."),
            IssueKind::OffIncrement {
                increment,
                nearest_below,
            } => write!(
                f,
                "Must be a multiple of {}. The nearest valid value below is {}.",
                increment.normalize(),
                nearest_below.normalize()
            ),
            IssueKind::BelowMinimum { minimum } => {
                write!(f, "Below the minimum of {}.", minimum.normalize())
            }
            IssueKind::AboveMaximum { maximum } => {
                write!(f, "Above the maximum of {}.", maximum.normalize())
            }
            IssueKind::Unsupported(reason) => f.write_str(reason),
            IssueKind::WrongProduct => f.write_str("This order is for a different market."),
            IssueKind::StaleRules => f.write_str(
                "This market's rules are out of date. They are being refreshed; try again in a moment.",
            ),
            IssueKind::InThePast => f.write_str("That time has already passed."),
        }
    }
}

/// Whether `value` is an exact multiple of `increment`. A zero increment accepts anything (a
/// broker that publishes no increment imposes none we know of).
/// A clicked price as an exact decimal on the market's increment: the *nearest* valid price (a click
/// has no precision to preserve; this is the one place a price is rounded, and only on purpose).
/// `None` for a price that is not positive and finite, or that rounds to nothing.
pub fn snap_to_increment(price: f64, increment: Decimal) -> Option<Decimal> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    // Through text, so 0.1 stays 0.1 and not 0.1000000000000000055.
    let exact: Decimal = format!("{price}").parse().ok()?;
    let snapped = if increment.is_zero() {
        exact
    } else {
        (exact / increment).round() * increment
    };
    (snapped > Decimal::ZERO).then_some(snapped.normalize())
}

pub fn is_multiple(value: Decimal, increment: Decimal) -> bool {
    increment.is_zero() || (value % increment).is_zero()
}

/// The largest multiple of `increment` not above `value`. This is a *suggestion*; nothing calls it
/// to alter an order unprompted.
pub fn floor_to_increment(value: Decimal, increment: Decimal) -> Decimal {
    if increment.is_zero() {
        value
    } else {
        value - value % increment
    }
}

/// Checks one numeric field against an increment, a minimum and an optional maximum.
fn check_amount(
    issues: &mut Vec<Issue>,
    field: Field,
    value: Decimal,
    increment: Decimal,
    minimum: Decimal,
    maximum: Option<Decimal>,
) {
    if value <= Decimal::ZERO {
        issues.push(Issue::new(field, IssueKind::NotPositive));
        return;
    }
    if !is_multiple(value, increment) {
        issues.push(Issue::new(
            field,
            IssueKind::OffIncrement {
                increment,
                nearest_below: floor_to_increment(value, increment),
            },
        ));
        return;
    }
    if value < minimum {
        issues.push(Issue::new(field, IssueKind::BelowMinimum { minimum }));
    }
    if let Some(maximum) = maximum
        && value > maximum
    {
        issues.push(Issue::new(field, IssueKind::AboveMaximum { maximum }));
    }
}

fn check_price(issues: &mut Vec<Issue>, field: Field, value: Decimal, spec: &ProductSpec) {
    if value <= Decimal::ZERO {
        issues.push(Issue::new(field, IssueKind::NotPositive));
    } else if !is_multiple(value, spec.price_increment) {
        issues.push(Issue::new(
            field,
            IssueKind::OffIncrement {
                increment: spec.price_increment,
                nearest_below: floor_to_increment(value, spec.price_increment),
            },
        ));
    }
}

fn check_tif(
    issues: &mut Vec<Issue>,
    tif: TimeInForce,
    post_only: bool,
    allowed: bool,
    now_ms: i64,
) {
    if !allowed {
        issues.push(Issue::new(
            Field::TimeInForce,
            IssueKind::Unsupported("This time in force is not available here.".to_owned()),
        ));
    }
    if let TimeInForce::GoodTilDate(end) = tif
        && end <= now_ms
    {
        issues.push(Issue::new(Field::TimeInForce, IssueKind::InThePast));
    }
    if post_only && !matches!(tif.kind(), TifKind::GoodTilCancelled | TifKind::GoodTilDate) {
        issues.push(Issue::new(
            Field::TimeInForce,
            IssueKind::Unsupported(
                "Post-only orders must stay on the book (good-til-cancelled or good-til-date)."
                    .to_owned(),
            ),
        ));
    }
}

/// Every problem with `draft`, against `spec` and `caps`. Empty means the draft is well formed for
/// this market; the broker's own preview still has the last word.
pub fn validate_draft(
    spec: &ProductSpec,
    caps: &Capabilities,
    draft: &OrderDraft,
    now_ms: i64,
) -> Vec<Issue> {
    let mut issues = Vec::new();

    if draft.product != spec.id {
        issues.push(Issue::new(Field::Product, IssueKind::WrongProduct));
        return issues;
    }
    if let Some(reason) = caps.blocked_reason() {
        issues.push(Issue::new(
            Field::Product,
            IssueKind::Unsupported(reason.to_owned()),
        ));
        return issues;
    }
    if spec.is_stale(now_ms) {
        issues.push(Issue::new(Field::Product, IssueKind::StaleRules));
        return issues;
    }
    if !caps.allows(draft.order_type()) {
        issues.push(Issue::new(
            Field::Type,
            IssueKind::Unsupported(format!(
                "{} orders are not available in this market.",
                type_name(draft.order_type())
            )),
        ));
        return issues;
    }
    if spec.restrictions.limit_only && draft.order_type() != OrderType::Limit {
        issues.push(Issue::new(
            Field::Type,
            IssueKind::Unsupported("This market currently accepts limit orders only.".to_owned()),
        ));
    }

    match &draft.kind {
        OrderKind::Market { size } => match *size {
            OrderSize::Base(amount) => check_amount(
                &mut issues,
                Field::Size,
                amount,
                spec.base_increment,
                spec.base_min,
                spec.base_max,
            ),
            OrderSize::Quote(amount) => {
                if draft.side == Side::Sell {
                    issues.push(Issue::new(
                        Field::Size,
                        IssueKind::Unsupported(
                            "A market sell is sized in the asset you are selling.".to_owned(),
                        ),
                    ));
                } else if !caps.quote_sized_market {
                    issues.push(Issue::new(
                        Field::Size,
                        IssueKind::Unsupported(
                            "This market cannot size a market order by amount to spend.".to_owned(),
                        ),
                    ));
                } else {
                    check_amount(
                        &mut issues,
                        Field::Size,
                        amount,
                        spec.quote_increment,
                        spec.quote_min,
                        spec.quote_max,
                    );
                }
            }
        },
        OrderKind::Limit {
            base_size,
            price,
            tif,
            post_only,
        } => {
            check_amount(
                &mut issues,
                Field::Size,
                *base_size,
                spec.base_increment,
                spec.base_min,
                spec.base_max,
            );
            check_price(&mut issues, Field::Price, *price, spec);
            check_tif(
                &mut issues,
                *tif,
                *post_only,
                caps.allows_tif(tif.kind()),
                now_ms,
            );
            if spec.restrictions.post_only && !post_only {
                issues.push(Issue::new(
                    Field::TimeInForce,
                    IssueKind::Unsupported(
                        "This market currently accepts post-only limit orders only.".to_owned(),
                    ),
                ));
            }
            if *post_only && !caps.post_only {
                issues.push(Issue::new(
                    Field::TimeInForce,
                    IssueKind::Unsupported("Post-only is not available here.".to_owned()),
                ));
            }
        }
        OrderKind::StopLimit {
            base_size,
            stop_price,
            limit_price,
            tif,
            ..
        } => {
            check_amount(
                &mut issues,
                Field::Size,
                *base_size,
                spec.base_increment,
                spec.base_min,
                spec.base_max,
            );
            check_price(&mut issues, Field::StopPrice, *stop_price, spec);
            check_price(&mut issues, Field::LimitPrice, *limit_price, spec);
            check_tif(
                &mut issues,
                *tif,
                false,
                caps.allows_stop_limit_tif(tif.kind()),
                now_ms,
            );
        }
    }
    issues
}

fn type_name(order_type: OrderType) -> &'static str {
    match order_type {
        OrderType::Market => "Market",
        OrderType::Limit => "Limit",
        OrderType::StopLimit => "Stop-limit",
        OrderType::Other => "This kind of",
    }
}

/// What an order ties up, so it can be compared with a balance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Requirement {
    pub asset: String,
    pub amount: Decimal,
    /// The amount is a guess (a market order priced from a quote, fees left out). The broker's
    /// preview is the authority.
    pub estimated: bool,
}

/// What `draft` needs in the account. `reference` is the price a market order is expected to
/// trade at (the ask for a buy), when known.
pub fn requirement(
    spec: &ProductSpec,
    draft: &OrderDraft,
    reference: Option<Decimal>,
) -> Option<Requirement> {
    match draft.side {
        Side::Sell => Some(Requirement {
            asset: spec.base.clone(),
            amount: draft.base_size()?,
            estimated: false,
        }),
        Side::Buy => match &draft.kind {
            OrderKind::Market {
                size: OrderSize::Quote(amount),
            } => Some(Requirement {
                asset: spec.quote.clone(),
                amount: *amount,
                estimated: false,
            }),
            OrderKind::Market {
                size: OrderSize::Base(size),
            } => Some(Requirement {
                asset: spec.quote.clone(),
                amount: *size * reference?,
                estimated: true,
            }),
            OrderKind::Limit {
                base_size, price, ..
            } => Some(Requirement {
                asset: spec.quote.clone(),
                amount: *base_size * *price,
                estimated: false,
            }),
            OrderKind::StopLimit {
                base_size,
                limit_price,
                ..
            } => Some(Requirement {
                asset: spec.quote.clone(),
                amount: *base_size * *limit_price,
                estimated: false,
            }),
        },
    }
}

/// How far short of an order the account is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shortfall {
    pub asset: String,
    pub needed: Decimal,
    pub available: Decimal,
    pub estimated: bool,
}

/// `Some` when the available balance of the asset the order needs is below what it needs. This is
/// advisory (fees are left out and a market price is a guess); `None` does not promise the order
/// will be accepted.
pub fn funds_shortfall(
    spec: &ProductSpec,
    draft: &OrderDraft,
    balances: &[Balance],
    reference: Option<Decimal>,
) -> Option<Shortfall> {
    let needed = requirement(spec, draft, reference)?;
    let available: Decimal = balances
        .iter()
        .filter(|balance| balance.asset == needed.asset)
        .map(|balance| balance.available)
        .sum();
    (available < needed.amount).then_some(Shortfall {
        asset: needed.asset,
        needed: needed.amount,
        available,
        estimated: needed.estimated,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_click_snaps_to_the_nearest_valid_price_and_stays_exact() {
        let d = crate::model::test_support::d;
        assert_eq!(snap_to_increment(90_000.004, d("0.01")), Some(d("90000")));
        assert_eq!(
            snap_to_increment(90_000.006, d("0.01")),
            Some(d("90000.01"))
        );
        assert_eq!(snap_to_increment(0.1, d("0.1")), Some(d("0.1")));
        assert_eq!(
            snap_to_increment(123.456, Decimal::ZERO),
            Some(d("123.456"))
        );
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(snap_to_increment(bad, d("0.01")), None, "{bad}");
        }
        // Rounds to nothing: not a price.
        assert_eq!(snap_to_increment(0.001, d("1")), None);
    }

    use super::*;
    use crate::model::capabilities::Tradability;
    use crate::model::test_support::d;
    use crate::model::types::{AccountId, ProductId, Restrictions, StopDirection};

    const NOW: i64 = 1_800_000_000_000;

    fn spec() -> ProductSpec {
        ProductSpec {
            id: ProductId::new("BTC-USD"),
            base: "BTC".to_owned(),
            quote: "USD".to_owned(),
            display_name: "BTC-USD".to_owned(),
            price_increment: d("0.01"),
            base_increment: d("0.00000001"),
            quote_increment: d("0.01"),
            base_min: d("0.00001"),
            base_max: Some(d("1000")),
            quote_min: d("1"),
            quote_max: Some(d("1000000")),
            restrictions: Restrictions::default(),
            fetched_ms: NOW,
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            tradable: Tradability::Yes,
            order_types: vec![OrderType::Market, OrderType::Limit, OrderType::StopLimit],
            tif: vec![
                TifKind::GoodTilCancelled,
                TifKind::GoodTilDate,
                TifKind::FillOrKill,
            ],
            stop_limit_tif: vec![TifKind::GoodTilCancelled, TifKind::GoodTilDate],
            post_only: true,
            quote_sized_market: true,
            native_edit: true,
            atomic_replace: false,
            brackets: false,
        }
    }

    fn limit(size: &str, price: &str) -> OrderDraft {
        OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Limit {
                base_size: d(size),
                price: d(price),
                tif: TimeInForce::GoodTilCancelled,
                post_only: false,
            },
        }
    }

    fn issues(draft: &OrderDraft) -> Vec<Issue> {
        validate_draft(&spec(), &caps(), draft, NOW)
    }

    #[test]
    fn a_well_formed_limit_order_has_no_issues() {
        assert!(issues(&limit("0.001", "90000.00")).is_empty());
    }

    #[test]
    fn a_price_off_its_increment_is_reported_with_a_suggestion_and_not_rounded() {
        let draft = limit("0.001", "90000.005");
        let found = issues(&draft);
        assert_eq!(
            found,
            vec![Issue::new(
                Field::Price,
                IssueKind::OffIncrement {
                    increment: d("0.01"),
                    nearest_below: d("90000.00"),
                }
            )]
        );
        // The draft is untouched.
        assert_eq!(draft.working_price(), Some(d("90000.005")));
        assert!(found[0].to_string().contains("90000"));
    }

    #[test]
    fn sizes_are_checked_against_increment_minimum_and_maximum() {
        assert_eq!(
            issues(&limit("0.000000001", "100.00"))[0].kind,
            IssueKind::OffIncrement {
                increment: d("0.00000001"),
                nearest_below: d("0"),
            }
        );
        assert_eq!(
            issues(&limit("0.000001", "100.00"))[0].kind,
            IssueKind::BelowMinimum {
                minimum: d("0.00001")
            }
        );
        assert_eq!(
            issues(&limit("1001", "100.00"))[0].kind,
            IssueKind::AboveMaximum { maximum: d("1000") }
        );
        assert_eq!(
            issues(&limit("0", "100.00"))[0].kind,
            IssueKind::NotPositive
        );
        assert_eq!(issues(&limit("1", "-5.00"))[0].kind, IssueKind::NotPositive);
    }

    #[test]
    fn a_market_buy_by_quote_checks_the_quote_rules() {
        let mut draft = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Market {
                size: OrderSize::Quote(d("25.00")),
            },
        };
        assert!(issues(&draft).is_empty());
        draft.kind = OrderKind::Market {
            size: OrderSize::Quote(d("0.50")),
        };
        assert_eq!(
            issues(&draft)[0].kind,
            IssueKind::BelowMinimum { minimum: d("1") }
        );
        draft.kind = OrderKind::Market {
            size: OrderSize::Quote(d("25.001")),
        };
        assert!(matches!(
            issues(&draft)[0].kind,
            IssueKind::OffIncrement { .. }
        ));
    }

    #[test]
    fn a_market_sell_cannot_be_sized_in_the_quote_asset() {
        let draft = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Sell,
            kind: OrderKind::Market {
                size: OrderSize::Quote(d("25.00")),
            },
        };
        assert!(matches!(issues(&draft)[0].kind, IssueKind::Unsupported(_)));
    }

    #[test]
    fn unsupported_quote_sizing_and_types_are_refused() {
        let mut limited = caps();
        limited.quote_sized_market = false;
        limited.order_types = vec![OrderType::Limit];
        let market = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Market {
                size: OrderSize::Base(d("0.01")),
            },
        };
        let found = validate_draft(&spec(), &limited, &market, NOW);
        assert_eq!(found[0].field, Field::Type);
        let quote = OrderDraft {
            kind: OrderKind::Market {
                size: OrderSize::Quote(d("25")),
            },
            ..market
        };
        limited.order_types.push(OrderType::Market);
        assert_eq!(
            validate_draft(&spec(), &limited, &quote, NOW)[0].field,
            Field::Size
        );
    }

    #[test]
    fn restrictions_narrow_what_a_tradable_market_accepts() {
        let mut spec = spec();
        spec.restrictions.limit_only = true;
        let market = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Market {
                size: OrderSize::Base(d("0.01")),
            },
        };
        assert_eq!(
            validate_draft(&spec, &caps(), &market, NOW)[0].field,
            Field::Type
        );
        spec.restrictions = Restrictions {
            post_only: true,
            ..Restrictions::default()
        };
        let plain = validate_draft(&spec, &caps(), &limit("0.01", "100.00"), NOW);
        assert_eq!(plain[0].field, Field::TimeInForce);
    }

    #[test]
    fn time_in_force_rules() {
        let mut draft = limit("0.01", "100.00");
        let OrderKind::Limit { tif, .. } = &mut draft.kind else {
            unreachable!()
        };
        *tif = TimeInForce::ImmediateOrCancel;
        assert_eq!(
            issues(&draft)[0].field,
            Field::TimeInForce,
            "IOC is not in caps"
        );
        let OrderKind::Limit { tif, post_only, .. } = &mut draft.kind else {
            unreachable!()
        };
        *tif = TimeInForce::FillOrKill;
        *post_only = true;
        assert!(
            issues(&draft)
                .iter()
                .any(|issue| issue.to_string().contains("Post-only orders must stay"))
        );
        let OrderKind::Limit { tif, post_only, .. } = &mut draft.kind else {
            unreachable!()
        };
        *post_only = false;
        *tif = TimeInForce::GoodTilDate(NOW - 1);
        assert_eq!(issues(&draft)[0].kind, IssueKind::InThePast);
        let OrderKind::Limit { tif, .. } = &mut draft.kind else {
            unreachable!()
        };
        *tif = TimeInForce::GoodTilDate(NOW + 60_000);
        assert!(issues(&draft).is_empty());
    }

    #[test]
    fn stop_limit_orders_check_both_prices() {
        let draft = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Sell,
            kind: OrderKind::StopLimit {
                base_size: d("0.01"),
                stop_price: d("90000.001"),
                limit_price: d("89990.00"),
                direction: StopDirection::Down,
                tif: TimeInForce::GoodTilCancelled,
            },
        };
        let found = issues(&draft);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].field, Field::StopPrice);
    }

    #[test]
    fn a_stale_spec_a_wrong_product_and_a_blocked_market_stop_everything_else() {
        let mut old = spec();
        old.fetched_ms = NOW - ProductSpec::STALE_AFTER_MS - 1;
        assert_eq!(
            validate_draft(&old, &caps(), &limit("0.01", "100.00"), NOW)[0].kind,
            IssueKind::StaleRules
        );
        let other = OrderDraft {
            product: ProductId::new("ETH-USD"),
            ..limit("0.01", "100.00")
        };
        assert_eq!(issues(&other)[0].kind, IssueKind::WrongProduct);
        let blocked = Capabilities::blocked("Trading is disabled for this market.");
        let found = validate_draft(&spec(), &blocked, &limit("0.01", "100.00"), NOW);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].to_string(), "Trading is disabled for this market.");
    }

    #[test]
    fn decimal_arithmetic_is_exact_where_floats_are_not() {
        // 0.1 + 0.2 != 0.3 in binary floating point; as decimals the increments line up exactly.
        let sum = d("0.1") + d("0.2");
        assert!(is_multiple(sum, d("0.1")));
        assert_eq!(sum, d("0.3"));
        assert_eq!(floor_to_increment(d("1.2399"), d("0.01")), d("1.23"));
        assert!(is_multiple(d("5"), Decimal::ZERO));
    }

    #[test]
    fn requirements_and_shortfalls() {
        let spec = spec();
        let balances = vec![
            Balance {
                account: AccountId::new("a"),
                asset: "USD".to_owned(),
                available: d("50"),
                held: d("10"),
            },
            Balance {
                account: AccountId::new("a"),
                asset: "BTC".to_owned(),
                available: d("0.5"),
                held: d("0"),
            },
        ];
        // 0.001 BTC at 90,000 needs 90 USD; only 50 is available.
        let buy = limit("0.001", "90000.00");
        let shortfall = funds_shortfall(&spec, &buy, &balances, None).unwrap();
        assert_eq!(shortfall.asset, "USD");
        assert_eq!(shortfall.needed, d("90"));
        assert_eq!(shortfall.available, d("50"));
        assert!(!shortfall.estimated);
        // A market buy by base needs a reference price to say anything.
        let market = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Market {
                size: OrderSize::Base(d("0.001")),
            },
        };
        assert_eq!(funds_shortfall(&spec, &market, &balances, None), None);
        assert!(
            funds_shortfall(&spec, &market, &balances, Some(d("90000")))
                .unwrap()
                .estimated
        );
        // A sell needs the base asset.
        let sell = OrderDraft {
            side: Side::Sell,
            ..limit("0.6", "90000.00")
        };
        let shortfall = funds_shortfall(&spec, &sell, &balances, None).unwrap();
        assert_eq!(
            (shortfall.asset.as_str(), shortfall.needed),
            ("BTC", d("0.6"))
        );
        let ok = OrderDraft {
            side: Side::Sell,
            ..limit("0.5", "90000.00")
        };
        assert_eq!(funds_shortfall(&spec, &ok, &balances, None), None);
    }
}
