//! Typed order-entry layer for TWS (lane D3).
//!
//! Builds on top of [`crate::requests`] and [`crate::inbound`]: the small
//! structs the trading engine reasons about, plus parsing helpers for the
//! inbound `openOrder` (msg 5) and `execDetails` (msg 11) frames.

use crate::inbound;

/// Broker-side contract description, in the order TWS repeats it on the wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContractSpec {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub last_trade_date: String,
    pub strike: Option<f64>,
    pub right: String,
    pub multiplier: Option<f64>,
    pub exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
}

impl ContractSpec {
    /// Builds a spec from the inbound contract shape.
    pub fn from_contract(contract: &inbound::Contract) -> Self {
        Self {
            con_id: contract.con_id,
            symbol: contract.symbol.clone(),
            sec_type: contract.sec_type.clone(),
            last_trade_date: contract.last_trade_date.clone(),
            strike: contract.strike,
            right: contract.right.clone(),
            multiplier: contract.multiplier,
            exchange: contract.exchange.clone(),
            currency: contract.currency.clone(),
            local_symbol: contract.local_symbol.clone(),
            trading_class: contract.trading_class.clone(),
        }
    }

    /// The 11 contract fields TWS expects on the wire, in order.
    pub fn to_fields(&self) -> Vec<String> {
        vec![
            self.con_id.to_string(),
            self.symbol.clone(),
            self.sec_type.clone(),
            self.last_trade_date.clone(),
            self.strike.map(|value| value.to_string()).unwrap_or_default(),
            self.right.clone(),
            self.multiplier.map(|value| value.to_string()).unwrap_or_default(),
            self.exchange.clone(),
            self.currency.clone(),
            self.local_symbol.clone(),
            self.trading_class.clone(),
        ]
    }
}

/// One order the engine wants to send.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderSpec {
    pub order_id: i64,
    pub contract: ContractSpec,
    pub action: String,
    pub quantity: f64,
    pub order_type: String,
    pub tif: String,
    pub order_ref: String,
    pub transmit: bool,
    pub account: String,
    pub outside_rth: bool,
    pub oca_group: Option<String>,
    pub oca_type: Option<i32>,
}

/// One open order reported by TWS.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    pub order_id: i64,
    pub order_ref: String,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub contract: ContractSpec,
}

/// One execution reported by TWS.
#[derive(Debug, Clone, PartialEq)]
pub struct Execution {
    pub order_id: i64,
    pub order_ref: String,
    pub shares: f64,
    pub price: f64,
    pub time: String,
}

/// Statuses TWS reports once an order is done.
pub const TERMINAL_STATUSES: [&str; 4] = ["Filled", "Cancelled", "ApiCancelled", "Inactive"];

/// `true` when `status` is one of [`TERMINAL_STATUSES`].
pub fn is_terminal(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

/// Parses an inbound `openOrder` frame (msg 5).
pub fn parse_open_order(fields: &[String]) -> Option<OpenOrder> {
    // openOrder has no version field: msgId, orderId, then the contract block.
    // Accept an optional version field by checking whether fields[2] is the
    // conId.
    let (order_id, contract_at) = if fields.len() >= 3
        && fields.get(1).and_then(|v| v.parse::<i64>().ok()).is_some()
        && fields.get(2).and_then(|v| v.parse::<i64>().ok()).is_some()
    {
        (fields.get(1)?.parse::<i64>().ok()?, 2usize)
    } else if fields.len() >= 4
        && fields.get(2).and_then(|v| v.parse::<i64>().ok()).is_some()
        && fields.get(3).and_then(|v| v.parse::<i64>().ok()).is_some()
    {
        (fields.get(2)?.parse::<i64>().ok()?, 3usize)
    } else {
        return None;
    };

    let block = fields.get(contract_at..contract_at + 11)?;
    let contract = ContractSpec {
        con_id: block[0].parse().unwrap_or(0),
        symbol: block[1].clone(),
        sec_type: block[2].clone(),
        last_trade_date: block[3].clone(),
        strike: block[4].parse().ok(),
        right: block[5].clone(),
        multiplier: block[6].parse().ok(),
        exchange: block[7].clone(),
        currency: block[8].clone(),
        local_symbol: block[9].clone(),
        trading_class: block[10].clone(),
    };

    // orderRef lives 11 fields after the contract block's first field in the
    // standard layout (action, totalQuantity, orderType, lmtPrice, auxPrice,
    // tif, ocaGroup, account, openClose, origin, orderRef).
    let order_ref_index = contract_at + 11 + 10;
    let order_ref = fields.get(order_ref_index).cloned().unwrap_or_default();

    // status is the first field that looks like a TWS order status.
    let status = fields
        .iter()
        .find(|value| is_order_status(value))
        .cloned()
        .unwrap_or_default();

    // totalQuantity, when present, gives a sensible default for `remaining`.
    let quantity_index = contract_at + 11 + 1;
    let remaining = fields
        .get(quantity_index)
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);

    Some(OpenOrder {
        order_id,
        order_ref,
        status,
        filled: 0.0,
        remaining,
        contract,
    })
}

fn is_order_status(value: &str) -> bool {
    matches!(
        value,
        "PendingSubmit"
            | "PendingCancel"
            | "PreSubmitted"
            | "Submitted"
            | "ApiPending"
            | "ApiCancelled"
            | "Cancelled"
            | "Filled"
            | "Inactive"
    )
}

/// Parses an inbound `execDetails` frame (msg 11).
pub fn parse_execution(fields: &[String]) -> Option<Execution> {
    // execDetails: msgId, reqId, orderId, then the contract block.
    let (order_id, contract_at) = if fields.len() >= 4
        && fields.get(2).and_then(|v| v.parse::<i64>().ok()).is_some()
        && fields.get(3).and_then(|v| v.parse::<i64>().ok()).is_some()
    {
        (fields.get(2)?.parse::<i64>().ok()?, 3usize)
    } else if fields.len() >= 5
        && fields.get(3).and_then(|v| v.parse::<i64>().ok()).is_some()
        && fields.get(4).and_then(|v| v.parse::<i64>().ok()).is_some()
    {
        (fields.get(3)?.parse::<i64>().ok()?, 4usize)
    } else {
        return None;
    };

    let exec_id_index = contract_at + 11;
    let time_index = exec_id_index + 1;
    let shares_index = exec_id_index + 5;
    let price_index = exec_id_index + 6;
    let order_ref_index = exec_id_index + 12;

    let shares = fields.get(shares_index).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let price = fields.get(price_index).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let time = fields.get(time_index).cloned().unwrap_or_default();
    let order_ref = fields.get(order_ref_index).cloned().unwrap_or_default();

    Some(Execution {
        order_id,
        order_ref,
        shares,
        price,
        time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_statuses() {
        assert!(is_terminal("Filled"));
        assert!(is_terminal("Cancelled"));
        assert!(is_terminal("ApiCancelled"));
        assert!(is_terminal("Inactive"));
        assert!(!is_terminal("Submitted"));
        assert!(!is_terminal(""));
    }

    #[test]
    fn contract_spec_roundtrip_fields() {
        let spec = ContractSpec {
            con_id: 123,
            symbol: "AAPL".into(),
            sec_type: "STK".into(),
            last_trade_date: "".into(),
            strike: None,
            right: "".into(),
            multiplier: Some(1.0),
            exchange: "SMART".into(),
            currency: "USD".into(),
            local_symbol: "AAPL".into(),
            trading_class: "".into(),
        };
        let fields = spec.to_fields();
        assert_eq!(fields.len(), 11);
        assert_eq!(fields[0], "123");
    }

    #[test]
    fn parses_a_sample_open_order_frame() {
        let mut frame: Vec<String> = Vec::new();
        frame.push("5".into()); // msg id
        frame.push("100".into()); // orderId
        // contract block (11 fields)
        frame.extend([
            "265598".to_string(),
            "AAPL".to_string(),
            "STK".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "1".to_string(),
            "SMART".to_string(),
            "USD".to_string(),
            "AAPL".to_string(),
            "".to_string(),
        ]);
        // order fields: action..orderRef
        frame.extend([
            "BUY".to_string(),
            "10".to_string(),
            "MKT".to_string(),
            "".to_string(),
            "".to_string(),
            "DAY".to_string(),
            "".to_string(),
            "DU1".to_string(),
            "".to_string(),
            "".to_string(),
            "ref-1".to_string(),
        ]);
        // pad to include a status somewhere
        frame.push("Submitted".to_string());

        let parsed = parse_open_order(&frame).unwrap();
        assert_eq!(parsed.order_id, 100);
        assert_eq!(parsed.order_ref, "ref-1");
        assert_eq!(parsed.status, "Submitted");
        assert_eq!(parsed.contract.symbol, "AAPL");
    }

    #[test]
    fn parses_a_sample_execution_frame() {
        let mut frame: Vec<String> = Vec::new();
        frame.push("11".into()); // msg id
        frame.push("1".into()); // reqId
        frame.push("100".into()); // orderId
        // contract block (11 fields)
        frame.extend([
            "265598".to_string(),
            "AAPL".to_string(),
            "STK".to_string(),
            "".to_string(),
            "".to_string(),
            "".to_string(),
            "1".to_string(),
            "SMART".to_string(),
            "USD".to_string(),
            "AAPL".to_string(),
            "".to_string(),
        ]);
        // execDetails trailing fields: execId, time, acctNumber, exchange, side, shares, price, permId, clientId, liquidation, cumQty, avgPrice, orderRef...
        frame.extend([
            "exec-1".to_string(),
            "20260101 12:00:00".to_string(),
            "DU1".to_string(),
            "SMART".to_string(),
            "BOT".to_string(),
            "5".to_string(),
            "150.25".to_string(),
            "1234".to_string(),
            "1".to_string(),
            "0".to_string(),
            "5".to_string(),
            "150.25".to_string(),
            "ref-1".to_string(),
        ]);

        let parsed = parse_execution(&frame).unwrap();
        assert_eq!(parsed.order_id, 100);
        assert_eq!(parsed.order_ref, "ref-1");
        assert_eq!(parsed.shares, 5.0);
        assert_eq!(parsed.price, 150.25);
        assert_eq!(parsed.time, "20260101 12:00:00");
    }
}
