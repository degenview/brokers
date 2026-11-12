//! What the text typed into an order ticket means: the order it builds, what is wrong with it, and
//! what it would cost. Pure and exact: every number is a [`Decimal`] and nothing is rounded to make
//! an order acceptable. A value off the market's increment is reported with the nearest valid value as
//! a suggestion; the user decides.
//!
//! The market's own rules (increments, minimums, what an order type allows) are [`validate`]; this
//! module parses the text into an order, hands it over, and puts each answer next to the field it
//! concerns. Every problem is a value ([`TicketProblem`]); the words belong to the application.

use std::str::FromStr;

use rust_decimal::Decimal;

use super::capabilities::Capabilities;
use super::orders::FeeSummary;
use super::types::{
    Balance, OrderDraft, OrderKind, OrderSize, ProductSpec, Quote, Side, StopDirection, TimeInForce,
};
use super::validate::{self, Field, Shortfall};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TicketKind {
    Market,
    Limit,
    StopLimit,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TicketTif {
    GoodTilCancelled,
    FillOrKill,
}

/// Which input a problem belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TicketField {
    Size,
    Price,
    StopPrice,
    LimitPrice,
    TimeInForce,
    Type,
}

/// What is typed in the ticket.
#[derive(Clone, Debug, PartialEq)]
pub struct TicketDraft {
    pub side: Side,
    pub kind: TicketKind,
    pub size_text: String,
    /// A market buy states an amount to spend (in the quote asset) rather than a size to buy.
    pub spend_in_quote: bool,
    pub price_text: String,
    pub stop_text: String,
    pub tif: TicketTif,
    pub post_only: bool,
}

impl TicketDraft {
    pub fn new(side: Side) -> Self {
        Self {
            side,
            kind: TicketKind::Limit,
            size_text: String::new(),
            spend_in_quote: false,
            price_text: String::new(),
            stop_text: String::new(),
            tif: TicketTif::GoodTilCancelled,
            post_only: false,
        }
    }

    /// A ticket opened at a clicked price on the chart.
    pub fn at_price(side: Side, kind: TicketKind, price: Decimal) -> Self {
        let mut draft = Self::new(side);
        draft.kind = kind;
        let text = price.normalize().to_string();
        match kind {
            TicketKind::Market => {}
            TicketKind::Limit => draft.price_text = text,
            TicketKind::StopLimit => {
                draft.stop_text = text.clone();
                draft.price_text = text;
            }
        }
        draft
    }
}

/// What the ticket knows about the market and the account.
pub struct TicketContext<'a> {
    pub spec: &'a ProductSpec,
    pub caps: &'a Capabilities,
    pub quote: Option<&'a Quote>,
    pub balances: &'a [Balance],
    pub fees: Option<&'a FeeSummary>,
    pub now_ms: i64,
}

/// What is wrong with a ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TicketProblem {
    /// The field is empty. Which one it is is the issue's field.
    Missing,
    /// The text is not a number.
    NotANumber,
    /// A stop needs a current price to say which way it triggers, and there is none yet.
    NoStopReference,
    /// The market's rules refuse it.
    Rule(validate::Issue),
    /// The account cannot afford it. Advisory: fees are left out and a market price is a guess.
    Funds(Shortfall),
}

/// A problem, next to the field it concerns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TicketIssue {
    pub field: TicketField,
    pub problem: TicketProblem,
}

fn issue(field: TicketField, problem: TicketProblem) -> TicketIssue {
    TicketIssue { field, problem }
}

/// A number the user typed that is not one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotANumber;

/// `1234.5`, or `1234,5` when there is no `.`; empty is `None`. Thousands separators are not
/// accepted: a comma next to a point is ambiguous, and an order must mean exactly what it says. The
/// digits survive untouched, trailing zeros included.
pub fn parse_decimal(text: &str) -> Result<Option<Decimal>, NotANumber> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let normalised = if text.contains('.') {
        if text.contains(',') {
            return Err(NotANumber);
        }
        text.to_owned()
    } else {
        text.replace(',', ".")
    };
    if normalised.matches('.').count() > 1
        || !normalised.chars().all(|c| c.is_ascii_digit() || c == '.')
        || normalised == "."
    {
        return Err(NotANumber);
    }
    Decimal::from_str(&normalised)
        .map(Some)
        .map_err(|_| NotANumber)
}

fn number(text: &str, field: TicketField, issues: &mut Vec<TicketIssue>) -> Option<Decimal> {
    match parse_decimal(text) {
        Ok(Some(value)) => Some(value),
        Ok(None) => {
            issues.push(issue(field, TicketProblem::Missing));
            None
        }
        Err(NotANumber) => {
            issues.push(issue(field, TicketProblem::NotANumber));
            None
        }
    }
}

/// The price a stop is compared with: the last trade, else the middle of the book.
fn reference_for_stop(quote: Option<&Quote>) -> Option<Decimal> {
    let quote = quote?;
    quote.last.or_else(|| match (quote.bid, quote.ask) {
        (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::TWO),
        (Some(price), None) | (None, Some(price)) => Some(price),
        (None, None) => None,
    })
}

impl TicketDraft {
    /// The order this text means, or why it does not mean one yet. Only the *typed* faults are here;
    /// the market's rules come from [`TicketDraft::issues`].
    pub fn build(&self, ctx: &TicketContext) -> Result<OrderDraft, Vec<TicketIssue>> {
        let mut issues = Vec::new();
        let size = number(&self.size_text, TicketField::Size, &mut issues);
        let kind = match self.kind {
            TicketKind::Market => size.map(|size| OrderKind::Market {
                size: if self.spend_in_quote && self.side == Side::Buy {
                    OrderSize::Quote(size)
                } else {
                    OrderSize::Base(size)
                },
            }),
            TicketKind::Limit => {
                let price = number(&self.price_text, TicketField::Price, &mut issues);
                size.zip(price).map(|(base_size, price)| OrderKind::Limit {
                    base_size,
                    price,
                    tif: match self.tif {
                        TicketTif::GoodTilCancelled => TimeInForce::GoodTilCancelled,
                        TicketTif::FillOrKill => TimeInForce::FillOrKill,
                    },
                    post_only: self.post_only,
                })
            }
            TicketKind::StopLimit => {
                let stop = number(&self.stop_text, TicketField::StopPrice, &mut issues);
                let limit = number(&self.price_text, TicketField::LimitPrice, &mut issues);
                match (size, stop, limit) {
                    (Some(base_size), Some(stop_price), Some(limit_price)) => {
                        match reference_for_stop(ctx.quote) {
                            Some(reference) => Some(OrderKind::StopLimit {
                                base_size,
                                stop_price,
                                limit_price,
                                direction: StopDirection::for_stop(
                                    self.side, stop_price, reference,
                                ),
                                tif: TimeInForce::GoodTilCancelled,
                            }),
                            None => {
                                issues.push(issue(
                                    TicketField::StopPrice,
                                    TicketProblem::NoStopReference,
                                ));
                                None
                            }
                        }
                    }
                    _ => None,
                }
            }
        };
        match kind {
            Some(kind) if issues.is_empty() => Ok(OrderDraft {
                product: ctx.spec.id.clone(),
                side: self.side,
                kind,
            }),
            _ => Err(issues),
        }
    }

    /// Everything wrong with the ticket as typed, field by field: what does not parse, then what
    /// the market's rules refuse, then (as a note on the size) what the account cannot afford.
    pub fn issues(&self, ctx: &TicketContext) -> Vec<TicketIssue> {
        let draft = match self.build(ctx) {
            Ok(draft) => draft,
            Err(issues) => return issues,
        };
        let mut issues: Vec<TicketIssue> =
            validate::validate_draft(ctx.spec, ctx.caps, &draft, ctx.now_ms)
                .into_iter()
                .map(|found| {
                    let field = match found.field {
                        Field::Size => TicketField::Size,
                        Field::Price => TicketField::Price,
                        Field::StopPrice => TicketField::StopPrice,
                        Field::LimitPrice => TicketField::LimitPrice,
                        Field::TimeInForce => TicketField::TimeInForce,
                        Field::Type | Field::Product => TicketField::Type,
                    };
                    issue(field, TicketProblem::Rule(found))
                })
                .collect();
        if issues.is_empty()
            && let Some(short) =
                validate::funds_shortfall(ctx.spec, &draft, ctx.balances, self.reference_price(ctx))
        {
            issues.push(issue(TicketField::Size, TicketProblem::Funds(short)));
        }
        issues
    }

    /// The price a market order is expected to trade at: the ask for a buy, the bid for a sell.
    fn reference_price(&self, ctx: &TicketContext) -> Option<Decimal> {
        match self.side {
            Side::Buy => ctx.quote.and_then(|q| q.ask),
            Side::Sell => ctx.quote.and_then(|q| q.bid),
        }
    }

    /// Whether the ticket builds an order and nothing is wrong with it.
    pub fn can_submit(&self, ctx: &TicketContext) -> bool {
        self.issues(ctx).is_empty() && self.build(ctx).is_ok()
    }

    /// What the order would cost, as far as it can be known before the broker's preview.
    pub fn estimate(&self, ctx: &TicketContext) -> Estimate {
        let Ok(draft) = self.build(ctx) else {
            return Estimate::default();
        };
        let reference = match &draft.kind {
            OrderKind::Market { .. } => self.reference_price(ctx),
            OrderKind::Limit { price, .. } => Some(*price),
            OrderKind::StopLimit { limit_price, .. } => Some(*limit_price),
        };
        let notional = match &draft.kind {
            OrderKind::Market {
                size: OrderSize::Quote(amount),
            } => Some(*amount),
            _ => draft
                .base_size()
                .zip(reference)
                .map(|(size, price)| size * price),
        };
        // A resting post-only order pays the maker rate; anything that may take liquidity pays the
        // taker rate, which is the safer figure to show.
        let rate = ctx.fees.and_then(|fees| {
            if self.post_only && self.kind == TicketKind::Limit {
                fees.maker_rate
            } else {
                fees.taker_rate
            }
        });
        Estimate {
            reference,
            notional,
            fee_rate: rate,
            fee: notional.zip(rate).map(|(notional, rate)| notional * rate),
        }
    }
}

/// What an order would cost, estimated from the fee schedule.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Estimate {
    /// The price the figures assume.
    pub reference: Option<Decimal>,
    /// The order's value in the quote asset.
    pub notional: Option<Decimal>,
    pub fee_rate: Option<Decimal>,
    pub fee: Option<Decimal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::capabilities::Tradability;
    use crate::model::test_support::d;
    use crate::model::types::{
        AccountId, OrderType, ProductId, Restrictions, StopDirection, TifKind,
    };
    use crate::model::validate::IssueKind;

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
            quote_max: None,
            restrictions: Restrictions::default(),
            fetched_ms: NOW,
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            tradable: Tradability::Yes,
            order_types: vec![OrderType::Market, OrderType::Limit, OrderType::StopLimit],
            tif: vec![TifKind::GoodTilCancelled, TifKind::FillOrKill],
            stop_limit_tif: vec![TifKind::GoodTilCancelled],
            post_only: true,
            quote_sized_market: true,
            native_edit: true,
            atomic_replace: false,
            brackets: false,
        }
    }

    fn quote() -> Quote {
        Quote {
            bid: Some(d("89999.50")),
            ask: Some(d("90000.50")),
            last: Some(d("90000.00")),
            time_ms: NOW,
        }
    }

    fn balances() -> Vec<Balance> {
        vec![Balance {
            account: AccountId::new("a"),
            asset: "USD".to_owned(),
            available: d("1000"),
            held: d("0"),
        }]
    }

    fn with<R>(f: impl FnOnce(&TicketContext) -> R) -> R {
        let (spec, caps, quote, balances) = (spec(), caps(), quote(), balances());
        let fees = FeeSummary {
            tier: "t".to_owned(),
            maker_rate: Some(d("0.004")),
            taker_rate: Some(d("0.006")),
        };
        f(&TicketContext {
            spec: &spec,
            caps: &caps,
            quote: Some(&quote),
            balances: &balances,
            fees: Some(&fees),
            now_ms: NOW,
        })
    }

    fn limit(size: &str, price: &str) -> TicketDraft {
        TicketDraft {
            size_text: size.to_owned(),
            price_text: price.to_owned(),
            ..TicketDraft::new(Side::Buy)
        }
    }

    #[test]
    fn typed_numbers_mean_exactly_what_they_say() {
        assert_eq!(parse_decimal("0.001"), Ok(Some(d("0.001"))));
        assert_eq!(parse_decimal(" 12,5 "), Ok(Some(d("12.5"))));
        assert_eq!(parse_decimal(""), Ok(None));
        for bad in ["1,234.5", "1.2.3", "abc", "-1", "1e3", ".", "1 000"] {
            assert_eq!(parse_decimal(bad), Err(NotANumber), "{bad}");
        }
        // The digits survive untouched, trailing zeros included.
        assert_eq!(
            parse_decimal("90000.10").unwrap().unwrap().to_string(),
            "90000.10"
        );
    }

    #[test]
    fn a_good_limit_order_builds_and_can_be_submitted() {
        with(|ctx| {
            let ticket = limit("0.001", "90000.00");
            assert!(ticket.issues(ctx).is_empty());
            assert!(ticket.can_submit(ctx));
            let draft = ticket.build(ctx).unwrap();
            assert_eq!(draft.working_price(), Some(d("90000.00")));
        });
    }

    #[test]
    fn missing_and_malformed_fields_are_marked_where_they_are() {
        with(|ctx| {
            assert_eq!(
                limit("", "x").issues(ctx),
                vec![
                    issue(TicketField::Size, TicketProblem::Missing),
                    issue(TicketField::Price, TicketProblem::NotANumber),
                ]
            );
        });
    }

    #[test]
    fn the_markets_rules_arrive_on_the_field_they_concern_and_nothing_is_rounded() {
        with(|ctx| {
            let ticket = limit("0.001", "90000.005");
            let issues = ticket.issues(ctx);
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].field, TicketField::Price);
            assert!(matches!(
                &issues[0].problem,
                TicketProblem::Rule(found)
                    if matches!(found.kind, IssueKind::OffIncrement { nearest_below, .. }
                        if nearest_below == d("90000"))
            ));
            // The text is what the user typed.
            assert_eq!(ticket.price_text, "90000.005");
            assert!(!ticket.can_submit(ctx));

            let small = limit("0.000001", "90000.00").issues(ctx);
            assert_eq!(small[0].field, TicketField::Size);
            assert!(matches!(
                &small[0].problem,
                TicketProblem::Rule(found) if matches!(found.kind, IssueKind::BelowMinimum { .. })
            ));
        });
    }

    #[test]
    fn not_having_the_money_is_said_on_the_size() {
        with(|ctx| {
            let issues = limit("0.5", "90000.00").issues(ctx);
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].field, TicketField::Size);
            let TicketProblem::Funds(short) = &issues[0].problem else {
                panic!("a shortfall, got {:?}", issues[0].problem)
            };
            assert_eq!(short.asset, "USD");
            assert_eq!(short.available, d("1000"));
            assert_eq!(short.needed, d("45000"));
        });
    }

    #[test]
    fn a_market_buy_can_state_an_amount_to_spend_and_a_sell_cannot() {
        with(|ctx| {
            let mut buy = TicketDraft::new(Side::Buy);
            buy.kind = TicketKind::Market;
            buy.spend_in_quote = true;
            buy.size_text = "25.00".to_owned();
            assert!(matches!(
                buy.build(ctx).unwrap().kind,
                OrderKind::Market {
                    size: OrderSize::Quote(_)
                }
            ));
            assert!(buy.can_submit(ctx));

            let mut sell = TicketDraft::new(Side::Sell);
            sell.kind = TicketKind::Market;
            sell.spend_in_quote = true; // ignored for a sell
            sell.size_text = "0.001".to_owned();
            assert!(matches!(
                sell.build(ctx).unwrap().kind,
                OrderKind::Market {
                    size: OrderSize::Base(_)
                }
            ));
        });
    }

    #[test]
    fn a_stop_limit_derives_which_way_the_stop_triggers_and_needs_a_price_to_do_so() {
        with(|ctx| {
            let mut sell = TicketDraft::new(Side::Sell);
            sell.kind = TicketKind::StopLimit;
            sell.size_text = "0.001".to_owned();
            sell.stop_text = "85000".to_owned();
            sell.price_text = "84990".to_owned();
            let draft = sell.build(ctx).unwrap();
            assert!(matches!(
                draft.kind,
                OrderKind::StopLimit {
                    direction: StopDirection::Down,
                    ..
                }
            ));
        });
        let (spec, caps) = (spec(), caps());
        let blind = TicketContext {
            spec: &spec,
            caps: &caps,
            quote: None,
            balances: &[],
            fees: None,
            now_ms: NOW,
        };
        let mut sell = TicketDraft::new(Side::Sell);
        sell.kind = TicketKind::StopLimit;
        sell.size_text = "0.001".to_owned();
        sell.stop_text = "85000".to_owned();
        sell.price_text = "84990".to_owned();
        let issues = sell.issues(&blind);
        assert_eq!(issues[0].field, TicketField::StopPrice);
        assert_eq!(issues[0].problem, TicketProblem::NoStopReference);
    }

    #[test]
    fn the_estimate_uses_the_maker_rate_only_for_a_post_only_limit() {
        with(|ctx| {
            let mut ticket = limit("0.001", "90000.00");
            let taker = ticket.estimate(ctx);
            assert_eq!(taker.notional, Some(d("90")));
            assert_eq!(taker.fee, Some(d("0.54")));
            ticket.post_only = true;
            assert_eq!(ticket.estimate(ctx).fee, Some(d("0.36")));
            // A market buy is priced from the ask.
            let mut market = TicketDraft::new(Side::Buy);
            market.kind = TicketKind::Market;
            market.size_text = "0.001".to_owned();
            assert_eq!(market.estimate(ctx).notional, Some(d("90.0005")));
            // Nothing is guessed from a ticket that does not parse yet.
            assert_eq!(limit("", "").estimate(ctx), Estimate::default());
        });
    }

    #[test]
    fn an_order_clicked_on_the_chart_opens_at_that_price() {
        let ticket = TicketDraft::at_price(Side::Sell, TicketKind::Limit, d("91000.50"));
        assert_eq!(ticket.price_text, "91000.5");
        let stop = TicketDraft::at_price(Side::Sell, TicketKind::StopLimit, d("85000"));
        assert_eq!(
            (stop.stop_text.as_str(), stop.price_text.as_str()),
            ("85000", "85000")
        );
        let market = TicketDraft::at_price(Side::Buy, TicketKind::Market, d("1"));
        assert!(market.price_text.is_empty() && market.stop_text.is_empty());
    }
}
