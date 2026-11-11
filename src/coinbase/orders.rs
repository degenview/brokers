//! Order operations on a Coinbase connection: preview, place, cancel, edit, and reading orders,
//! fills and fees.

use rust_decimal::Decimal;
use serde_json::{Value, json};

use super::connection::{CoinbaseConnection, decode, path, unreadable};
use super::mapping::{
    self, WORKING_STATUSES, create_body, edit_failure, failure_text, fill_from, order_body,
    order_from,
};
use super::time::format_rfc3339_ms;
use super::wire::{
    CancelResponse, CreateOrderResponse, EditPreviewResponse, EditResponse, FillsPage,
    OrderEnvelope, OrdersPage, PreviewResponse, TransactionSummary, parse_decimal,
};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::model::orders::{
    CancelOutcome, CancelResult, EditOutcome, EditPreview, FeeSummary, FillFilter, HistoryFilter,
    OrderChange, OrderPreview, Page, PlacedOrder, Problem,
};
use crate::model::types::{
    ClientOrderId, LiveFill, LiveOrder, OrderDraft, OrderId, OrderStatus, OrderType,
};

/// The documented maximum orders in one cancel request.
const CANCEL_BATCH: usize = 100;
/// Pages read while looking for one order by client id.
const LOOKUP_PAGES: usize = 8;
const MAX_PAGE: u32 = 250;
const MAX_OPEN_PAGES: usize = 40;

fn problem(code: &str) -> Problem {
    Problem {
        code: Some(code.to_owned()),
        text: failure_text(code),
    }
}

fn amount(text: &str) -> Option<Decimal> {
    parse_decimal(text)
}

impl CoinbaseConnection {
    pub(super) fn do_preview(&self, draft: &OrderDraft) -> Result<OrderPreview> {
        let body = order_body(draft)?;
        let response: PreviewResponse = decode(
            self.http.post_read(&path("/orders/preview"), &body)?,
            "the order preview",
        )?;
        Ok(OrderPreview {
            order_total: amount(&response.order_total),
            fees: amount(&response.commission_total),
            base_size: amount(&response.base_size),
            quote_size: amount(&response.quote_size),
            best_bid: amount(&response.best_bid),
            best_ask: amount(&response.best_ask),
            estimated_average_price: amount(&response.est_average_filled_price),
            slippage: amount(&response.slippage),
            problems: response.errs.iter().map(|code| problem(code)).collect(),
            warnings: response
                .warning
                .iter()
                .map(|code| failure_text(code))
                .collect(),
        })
    }

    pub(super) fn do_place(
        &self,
        draft: &OrderDraft,
        client_id: &ClientOrderId,
    ) -> Result<PlacedOrder> {
        let body = create_body(draft, client_id)?;
        let value = self.http.post(&path("/orders"), &body)?;
        // The broker answered; from here a reply we cannot understand still means it may have acted.
        let response: CreateOrderResponse = serde_json::from_value(value).map_err(|_| {
            BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "Coinbase's reply to the order could not be read. It may have been placed.",
            )
        })?;
        if response.success && !response.success_response.order_id.is_empty() {
            return Ok(PlacedOrder {
                order_id: OrderId::new(&response.success_response.order_id),
                product: draft.product.clone(),
                side: draft.side,
            });
        }
        if response.success {
            return Err(BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "Coinbase accepted the order but did not say which one. It must be looked up.",
            ));
        }
        let error = response.error_response;
        let code = [
            &error.new_order_failure_reason,
            &error.preview_failure_reason,
            &error.error,
        ]
        .into_iter()
        .find(|code| !code.is_empty() && code.as_str() != "UNKNOWN_FAILURE_REASON")
        .cloned()
        .unwrap_or_default();
        if code == "DUPLICATE_CLIENT_ORDER_ID" {
            // An order already exists under this id: it is probably ours.
            return Err(BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "An order with this id already exists. It is being looked up.",
            )
            .with_code(code));
        }
        let text = if code.is_empty() {
            if error.message.is_empty() {
                "Coinbase refused the order.".to_owned()
            } else {
                self.http.scrubber().scrub(&error.message)
            }
        } else {
            failure_text(&code)
        };
        let detail = self
            .http
            .scrubber()
            .scrub(&format!("{} {}", error.message, error.error_details));
        let mut refusal = BrokerError::new(ErrorKind::Rejected, text).with_detail(detail);
        if !code.is_empty() {
            refusal = refusal.with_code(code);
        }
        Err(refusal)
    }

    pub(super) fn do_cancel(&self, orders: &[OrderId]) -> Result<Vec<CancelResult>> {
        let mut results = Vec::new();
        let mut stopped = false;
        for (index, chunk) in orders.chunks(CANCEL_BATCH).enumerate() {
            if stopped {
                // Not tried: an earlier batch failed, and the same thing would likely fail again.
                results.extend(chunk.iter().map(|id| CancelResult {
                    order_id: id.clone(),
                    outcome: CancelOutcome::Rejected(
                        "Not sent: an earlier cancel failed, so this one was not tried.".to_owned(),
                    ),
                }));
                continue;
            }
            match self.cancel_batch(chunk) {
                Ok(done) => results.extend(done),
                // Nothing to keep yet: the whole request failed.
                Err(error) if index == 0 => return Err(error),
                // Earlier batches were sent and answered: their results stand. This batch is
                // reported by what is known of it, never dropped with them.
                Err(error) => {
                    let text = error.to_string();
                    results.extend(chunk.iter().map(|id| CancelResult {
                        order_id: id.clone(),
                        outcome: if error.kind == ErrorKind::OutcomeUnknown {
                            CancelOutcome::Unknown(text.clone())
                        } else {
                            CancelOutcome::Rejected(text.clone())
                        },
                    }));
                    stopped = true;
                }
            }
        }
        Ok(results)
    }

    fn cancel_batch(&self, chunk: &[OrderId]) -> Result<Vec<CancelResult>> {
        let ids: Vec<&str> = chunk.iter().map(OrderId::as_str).collect();
        let value = self
            .http
            .post(&path("/orders/batch_cancel"), &json!({ "order_ids": ids }))?;
        let response: CancelResponse = serde_json::from_value(value).map_err(|_| {
            BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "Coinbase's reply to the cancel could not be read. Check the order's status.",
            )
        })?;
        let mut results = Vec::with_capacity(chunk.len());
        for id in chunk {
            let entry = response.results.iter().find(|r| r.order_id == id.as_str());
            let outcome = match entry {
                None => CancelOutcome::Rejected(
                    "Coinbase did not say what happened to this cancel.".to_owned(),
                ),
                Some(entry) if entry.success => CancelOutcome::Requested,
                Some(entry) => match entry.failure_reason.as_str() {
                    "ORDER_IS_FULLY_FILLED" => CancelOutcome::AlreadyFilled,
                    "UNKNOWN_CANCEL_ORDER" => CancelOutcome::NotFound,
                    "ORDER_IS_BEING_REPLACED" => CancelOutcome::BeingReplaced,
                    // A cancel is already on its way.
                    "DUPLICATE_CANCEL_REQUEST" => CancelOutcome::Requested,
                    "NOT_ALLOWED_TO_CANCEL" => {
                        CancelOutcome::Rejected("This order cannot be cancelled.".to_owned())
                    }
                    other => CancelOutcome::Rejected(mapping::humanise(other)),
                },
            };
            results.push(CancelResult {
                order_id: id.clone(),
                outcome,
            });
        }
        Ok(results)
    }

    /// The request body for an edit, or why the order cannot be edited at all.
    fn edit_body(&self, order: &LiveOrder, change: &OrderChange) -> Result<Value> {
        if change.is_empty() {
            return Err(BrokerError::invalid("Nothing was changed."));
        }
        if order.status != OrderStatus::Open {
            return Err(BrokerError::unsupported("Only open orders can be edited."));
        }
        if !matches!(order.order_type, OrderType::Limit | OrderType::StopLimit) {
            return Err(BrokerError::unsupported(
                "This type of order cannot be edited.",
            ));
        }
        let Some(crate::model::types::OrderSize::Base(current_size)) = order.size else {
            return Err(BrokerError::unsupported(
                "Orders sized by an amount to spend cannot be edited.",
            ));
        };
        let Some(price) = change.price.or(order.limit_price) else {
            return Err(BrokerError::invalid("The order has no price to edit."));
        };
        let mut body = json!({
            "order_id": order.id.as_str(),
            "price": price.to_string(),
            "size": change.size.unwrap_or(current_size).to_string(),
        });
        if order.order_type == OrderType::StopLimit
            && let Some(stop) = change.stop_price.or(order.stop_price)
        {
            body["stop_price"] = json!(stop.to_string());
        }
        Ok(body)
    }

    pub(super) fn do_preview_edit(
        &self,
        order: &LiveOrder,
        change: &OrderChange,
    ) -> Result<EditPreview> {
        let body = self.edit_body(order, change)?;
        let response: EditPreviewResponse = decode(
            self.http.post_read(&path("/orders/edit_preview"), &body)?,
            "the edit preview",
        )?;
        let problems = response
            .errors
            .iter()
            .filter_map(|e| {
                let code = [&e.edit_failure_reason, &e.preview_failure_reason]
                    .into_iter()
                    .find(|c| !c.is_empty() && !c.starts_with("UNKNOWN_"))?;
                Some(Problem {
                    code: Some(code.clone()),
                    text: edit_failure(code).0,
                })
            })
            .collect();
        Ok(EditPreview {
            order_total: amount(&response.order_total),
            fees: amount(&response.commission_total),
            base_size: amount(&response.base_size),
            quote_size: amount(&response.quote_size),
            best_bid: amount(&response.best_bid),
            best_ask: amount(&response.best_ask),
            problems,
            // The runtime stamps the order the preview was worked out for.
            base: None,
        })
    }

    pub(super) fn do_amend(&self, order: &LiveOrder, change: &OrderChange) -> Result<EditOutcome> {
        let body = self.edit_body(order, change)?;
        let value = self.http.post(&path("/orders/edit"), &body)?;
        let response: EditResponse = serde_json::from_value(value).map_err(|_| {
            BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "Coinbase's reply to the edit could not be read. Check the order.",
            )
        })?;
        if response.success {
            return Ok(EditOutcome::Native);
        }
        let first = response.errors.iter().find_map(|e| {
            [&e.edit_failure_reason, &e.preview_failure_reason]
                .into_iter()
                .find(|c| !c.is_empty() && !c.starts_with("UNKNOWN_"))
        });
        let (text, kind) = first.map_or(
            ("Coinbase refused the edit.".to_owned(), ErrorKind::Rejected),
            |code| edit_failure(code),
        );
        let mut error = BrokerError::new(kind, text);
        if let Some(code) = first {
            error = error.with_code(code.clone());
        }
        Err(error)
    }

    pub(super) fn do_order(&self, id: &OrderId) -> Result<LiveOrder> {
        let value = self
            .http
            .get(&path(&format!("/orders/historical/{}", id.as_str())), &[])?;
        let envelope: OrderEnvelope = decode(value, "the order")?;
        order_from(&envelope.order)
            .ok_or_else(|| BrokerError::new(ErrorKind::NotFound, "Coinbase has no such order."))
    }

    fn orders_page(
        &self,
        statuses: &[&str],
        filter: &HistoryFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<LiveOrder>> {
        let limit_text = limit.clamp(1, MAX_PAGE).to_string();
        let start = filter.start_ms.map(format_rfc3339_ms);
        let end = filter.end_ms.map(format_rfc3339_ms);
        let mut query: Vec<(&str, &str)> = vec![("limit", limit_text.as_str())];
        for status in statuses {
            query.push(("order_status", status));
        }
        if let Some(product) = &filter.product {
            query.push(("product_ids", product.as_str()));
        }
        if let Some(start) = &start {
            query.push(("start_date", start));
        }
        if let Some(end) = &end {
            query.push(("end_date", end));
        }
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            query.push(("cursor", cursor));
        }
        let page: OrdersPage = decode(
            self.http.get(&path("/orders/historical/batch"), &query)?,
            "the order list",
        )?;
        Ok(Page {
            items: page.orders.iter().filter_map(order_from).collect(),
            next_cursor: (page.has_next && !page.cursor.is_empty()).then_some(page.cursor),
        })
    }

    /// Every order that may still trade. Coinbase refuses a listing that names several active
    /// statuses (it said so on the first real run), so each is asked for on its own and the answers
    /// are merged, an order once even if two answers carry it.
    pub(super) fn do_open_orders(&self) -> Result<Vec<LiveOrder>> {
        let filter = HistoryFilter::default();
        let mut orders: Vec<LiveOrder> = Vec::new();
        for status in WORKING_STATUSES {
            let mut cursor: Option<String> = None;
            let mut finished = false;
            for _ in 0..MAX_OPEN_PAGES {
                let page = self.orders_page(&[status], &filter, cursor.as_deref(), MAX_PAGE)?;
                for order in page.items {
                    if !orders.iter().any(|known| known.id == order.id) {
                        orders.push(order);
                    }
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => {
                        finished = true;
                        break;
                    }
                }
            }
            if !finished {
                return Err(unreadable("an order list that never ends"));
            }
        }
        Ok(orders)
    }

    pub(super) fn do_order_history(
        &self,
        filter: &HistoryFilter,
        cursor: Option<&str>,
    ) -> Result<Page<LiveOrder>> {
        let statuses: Vec<&str> = filter
            .statuses
            .iter()
            .flat_map(|status| mapping::status_query(*status).iter().copied())
            .collect();
        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        // Several statuses with an active one among them is refused by Coinbase, and one cursor
        // cannot page through several listings. So ask for everything and keep what was wanted.
        if statuses.len() > 1 && statuses.iter().any(|s| WORKING_STATUSES.contains(s)) {
            let mut page = self.orders_page(&[], filter, cursor, limit)?;
            page.items
                .retain(|order| filter.statuses.contains(&order.status));
            return Ok(page);
        }
        self.orders_page(&statuses, filter, cursor, limit)
    }

    pub(super) fn do_fills(
        &self,
        filter: &FillFilter,
        cursor: Option<&str>,
    ) -> Result<Page<LiveFill>> {
        let limit = if filter.limit == 0 { 100 } else { filter.limit }
            .clamp(1, MAX_PAGE)
            .to_string();
        let start = filter.start_ms.map(format_rfc3339_ms);
        let end = filter.end_ms.map(format_rfc3339_ms);
        let mut query: Vec<(&str, &str)> = vec![("limit", limit.as_str())];
        if let Some(product) = &filter.product {
            query.push(("product_ids", product.as_str()));
        }
        if let Some(order) = &filter.order {
            query.push(("order_ids", order.as_str()));
        }
        if let Some(start) = &start {
            query.push(("start_sequence_timestamp", start));
        }
        if let Some(end) = &end {
            query.push(("end_sequence_timestamp", end));
        }
        if let Some(cursor) = cursor.filter(|c| !c.is_empty()) {
            query.push(("cursor", cursor));
        }
        let page: FillsPage = decode(
            self.http.get(&path("/orders/historical/fills"), &query)?,
            "the fill list",
        )?;
        let next = (!page.cursor.is_empty() && !page.fills.is_empty()).then_some(page.cursor);
        Ok(Page {
            items: page.fills.iter().filter_map(fill_from).collect(),
            next_cursor: next,
        })
    }

    pub(super) fn do_find_by_client_id(
        &self,
        client_id: &ClientOrderId,
        since_ms: i64,
    ) -> Result<Option<LiveOrder>> {
        let filter = HistoryFilter {
            start_ms: Some(since_ms),
            ..HistoryFilter::default()
        };
        let mut cursor: Option<String> = None;
        for _ in 0..LOOKUP_PAGES {
            let page = self.orders_page(&[], &filter, cursor.as_deref(), MAX_PAGE)?;
            if let Some(found) = page
                .items
                .into_iter()
                .find(|order| order.client_id.as_ref() == Some(client_id))
            {
                return Ok(Some(found));
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
        // Not found in the pages we were willing to read: that is not "not placed".
        Err(BrokerError::new(
            ErrorKind::Transient,
            "Could not finish looking for the order; there are too many recent orders to search.",
        ))
    }

    pub(super) fn do_fee_summary(&self) -> Result<FeeSummary> {
        let summary: TransactionSummary = decode(
            self.http.get(&path("/transaction_summary"), &[])?,
            "the fee summary",
        )?;
        Ok(FeeSummary {
            tier: summary.fee_tier.pricing_tier,
            maker_rate: amount(&summary.fee_tier.maker_fee_rate),
            taker_rate: amount(&summary.fee_tier.taker_fee_rate),
        })
    }
}
