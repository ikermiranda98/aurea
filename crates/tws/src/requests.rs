//! Outbound TWS requests (lane D2).
//!
//! Each function returns the message's fields as TWS expects them: the outbound
//! message id, its version, then the arguments. [`crate::codec::encode_message`]
//! turns the list into bytes.
//!
//! These are the read-only requests the engine needs to fill the dashboard:
//! managed accounts, positions, the account ledger, the summary tags and PnL.
//! Order entry lives in D3.

/// `REQ_MANAGED_ACCTS`: which accounts this login can trade.
pub fn req_managed_accounts() -> Vec<String> {
    row(["17", "1"])
}

/// `REQ_IDS`: asks TWS for the next valid order id (answers as `NextValidId`).
pub fn req_ids(count: i32) -> Vec<String> {
    row(["8", "1", &count.to_string()])
}

/// `REQ_POSITIONS`: every account's positions, terminator included.
pub fn req_positions() -> Vec<String> {
    row(["61", "1"])
}

/// `CANCEL_POSITIONS`: stops the position stream.
pub fn cancel_positions() -> Vec<String> {
    row(["64", "1"])
}

/// `REQ_ACCOUNT_DATA`: subscribes (`subscribe = true`) or unsubscribes the
/// account ledger — the only source of market price and unrealized PnL for a
/// held contract. `account` is empty unless the login serves several accounts.
pub fn req_account_updates(subscribe: bool, account: &str) -> Vec<String> {
    row(["6", "2", if subscribe { "1" } else { "0" }, account])
}

/// `CANCEL_ACCOUNT_DATA`.
pub fn cancel_account_updates() -> Vec<String> {
    row(["7", "1"])
}

/// `REQ_ACCOUNT_SUMMARY`: the tag burst for `group` (usually `"All"`).
pub fn req_account_summary(req_id: i64, group: &str, tags: &str) -> Vec<String> {
    row(["62", "1", &req_id.to_string(), group, tags])
}

/// `CANCEL_ACCOUNT_SUMMARY`.
pub fn cancel_account_summary(req_id: i64) -> Vec<String> {
    row(["63", "1", &req_id.to_string()])
}

/// `REQ_PNL`: the account's day, unrealized and realized PnL.
pub fn req_pnl(req_id: i64, account: &str, model: &str) -> Vec<String> {
    row(["92", &req_id.to_string(), account, model])
}

/// `CANCEL_PNL`.
pub fn cancel_pnl(req_id: i64) -> Vec<String> {
    row(["93", &req_id.to_string()])
}

fn row<const N: usize>(fields: [&str; N]) -> Vec<String> {
    fields.iter().map(|field| (*field).to_string()).collect()
}

/// `PLACE_ORDER` (msg 3): sends a new order. `contract_fields` are the contract
/// fields in wire order. `oca_group`/`oca_type` are optional.
pub fn place_order<Oid, C, S, A, O, R, T, F>(
    order_id: Oid,
    contract_fields: C,
    action: A,
    quantity: f64,
    order_type: O,
    tif: T,
    order_ref: R,
    transmit: bool,
    account: F,
    outside_rth: bool,
    oca_group: Option<impl Into<String>>,
    oca_type: Option<i32>,
) -> Vec<String>
where
    Oid: Into<i64>,
    C: IntoIterator<Item = S>,
    S: Into<String>,
    A: AsRef<str>,
    O: AsRef<str>,
    T: AsRef<str>,
    R: AsRef<str>,
    F: AsRef<str>,
{
    let mut fields: Vec<String> = Vec::new();
    fields.push("3".to_string());
    fields.push(order_id.into().to_string());
    for field in contract_fields {
        fields.push(field.into());
    }
    fields.push(action.as_ref().to_string());
    fields.push(quantity.to_string());
    fields.push(order_type.as_ref().to_string());
    fields.push(tif.as_ref().to_string());
    fields.push(order_ref.as_ref().to_string());
    fields.push(if transmit { "1" } else { "0" }.to_string());
    fields.push(account.as_ref().to_string());
    fields.push(if outside_rth { "1" } else { "0" }.to_string());
    fields.push(oca_group.map(|value| value.into()).unwrap_or_default());
    fields.push(oca_type.map(|value| value.to_string()).unwrap_or_default());
    fields
}

/// `CANCEL_ORDER` (msg 4).
pub fn cancel_order<Oid: Into<i64>>(order_id: Oid) -> Vec<String> {
    vec!["4".to_string(), "1".to_string(), order_id.into().to_string()]
}

/// `REQ_ALL_OPEN_ORDERS` (msg 16).
pub fn req_all_open_orders() -> Vec<String> {
    vec!["16".to_string(), "1".to_string()]
}

/// `REQ_EXECUTIONS` (msg 7).
pub fn req_executions<Cid: Into<i64>, Rid: Into<i64>>(client_id: Cid, req_id: Rid) -> Vec<String> {
    vec![
        "7".to_string(),
        "3".to_string(),
        req_id.into().to_string(),
        client_id.into().to_string(),
    ]
}

/// `REQ_CONTRACT_DETAILS` (msg 9).
pub fn req_contract_details<Rid, C, S>(req_id: Rid, contract_fields: C) -> Vec<String>
where
    Rid: Into<i64>,
    C: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut fields: Vec<String> = Vec::new();
    fields.push("9".to_string());
    fields.push("8".to_string());
    fields.push(req_id.into().to_string());
    for field in contract_fields {
        fields.push(field.into());
    }
    fields
}

/// The summary tags Openbot asks for (`ACCOUNT_SUMMARY_TAGS` in `trading.ts`).
pub const ACCOUNT_SUMMARY_TAGS: &str = "NetLiquidation,TotalCashBalance,CashBalance,UnrealizedPnL,RealizedPnL,AccountType,Currency,AvailableFunds,BuyingPower,ExcessLiquidity,MaintMarginReq";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_requests_match_the_frames_the_oracle_answers() {
        // The fake TWS in `trading-engine.test.ts` keys its answers on exactly
        // these ids, so a change here must show up as a red test there too.
        assert_eq!(req_positions(), vec!["61", "1"]);
        assert_eq!(req_managed_accounts(), vec!["17", "1"]);
        assert_eq!(req_ids(1), vec!["8", "1", "1"]);
        assert_eq!(req_account_updates(true, "A"), vec!["6", "2", "1", "A"]);
        assert_eq!(req_account_updates(false, ""), vec!["6", "2", "0", ""]);
        assert_eq!(cancel_account_updates(), vec!["7", "1"]);
        assert_eq!(cancel_positions(), vec!["64", "1"]);
    }

    #[test]
    fn account_summary_and_pnl_carry_their_request_id() {
        assert_eq!(
            req_account_summary(9001, "All", ACCOUNT_SUMMARY_TAGS),
            vec!["62", "1", "9001", "All", ACCOUNT_SUMMARY_TAGS]
        );
        assert_eq!(cancel_account_summary(9001), vec!["63", "1", "9001"]);
        assert_eq!(req_pnl(9002, "A", ""), vec!["92", "9002", "A", ""]);
        assert_eq!(cancel_pnl(9002), vec!["93", "9002"]);
    }
}
