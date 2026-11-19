//! Order entry on the TWS wire (lane D3), field by field.
//!
//! Every encoder here is a transcription of `@stoqey/ib@1.6.13`
//! (`dist/core/io/encoder.js`, the library Openbot drives TWS with) for the
//! server versions this client negotiates. The client announces `v100..193`,
//! so TWS answers with at most 193; order entry is supported for
//! [`MIN_ORDER_SERVER_VERSION`]`..=`[`MAX_ORDER_SERVER_VERSION`] and refused
//! outside it, because one misplaced field in `PLACE_ORDER` is not a parse
//! error TWS is guaranteed to raise — it can read as a different order.
//!
//! Serialisation follows the library's socket: an unset field is the empty
//! string, a boolean is `1`/`0`, a number is JavaScript's `String(n)`.
//!
//! The inbound side decodes the four answers the close cycle reads —
//! `ORDER_STATUS` (3), the fixed head of `OPEN_ORDER` (5) up to `orderRef`,
//! `CONTRACT_DATA` (10) up to `primaryExch`, `EXECUTION_DATA` (11) up to
//! `orderRef` — and their `*_END` markers. They are wired into
//! [`crate::inbound::decode`].

use crate::inbound;

/// Lowest server version whose `PLACE_ORDER` layout this module encodes
/// (`UNDO_RFQ_FIELDS`: the RFQ placeholders are gone).
pub const MIN_ORDER_SERVER_VERSION: i32 = 190;
/// Highest: the client's own announced maximum.
pub const MAX_ORDER_SERVER_VERSION: i32 = 193;
/// `CME_TAGGING_FIELDS`: `manualOrderIndicator` joins orders and cancels.
const CME_TAGGING_FIELDS: i32 = 192;
/// `Integer.MAX_VALUE`, the library's "unset" for `manualOrderIndicator`.
const INTEGER_MAX_VALUE: &str = "2147483647";

/// Why an order frame was not built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderEncodeError {
    /// The negotiated server version is outside the supported range.
    UnsupportedServerVersion(i32),
}

impl std::fmt::Display for OrderEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedServerVersion(version) => write!(
                f,
                "TWS server version {version} is outside the order layout this build encodes \
                 ({MIN_ORDER_SERVER_VERSION}..={MAX_ORDER_SERVER_VERSION})"
            ),
        }
    }
}

impl std::error::Error for OrderEncodeError {}

fn check_version(server_version: i32) -> Result<(), OrderEncodeError> {
    if (MIN_ORDER_SERVER_VERSION..=MAX_ORDER_SERVER_VERSION).contains(&server_version) {
        Ok(())
    } else {
        Err(OrderEncodeError::UnsupportedServerVersion(server_version))
    }
}

/// JavaScript's `String(n)` for the values orders carry.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    format!("{value}")
}

fn js_bool(value: bool) -> String {
    if value { "1" } else { "0" }.into()
}

/// A contract as an order or a details request names it.
///
/// `strike` and `multiplier` are numbers in the library (`readDouble` turns an
/// empty field into 0), so `None` encodes as `"0"` exactly like a contract the
/// library decoded itself.
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
    pub primary_exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
}

impl ContractSpec {
    /// The contract a position row describes.
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
            primary_exchange: contract.primary_exchange.clone(),
            currency: contract.currency.clone(),
            local_symbol: contract.local_symbol.clone(),
            trading_class: contract.trading_class.clone(),
        }
    }

    /// `contratoParaOrden` (`trading.ts:592-595`): TWS refuses an order whose
    /// expiry carries the hour and zone a details answer appends (error 321).
    pub fn for_order(&self) -> Self {
        let mut contract = self.clone();
        if let Some(date) = self.last_trade_date.split_whitespace().next() {
            contract.last_trade_date = date.to_string();
        }
        contract
    }

    /// The contract Openbot asks TWS to complete (`trading.ts:2930-2940`):
    /// identity and expiry only, the exchange deliberately empty.
    pub fn details_query(&self) -> Self {
        Self {
            con_id: self.con_id,
            symbol: self.symbol.clone(),
            sec_type: self.sec_type.clone(),
            last_trade_date: self.last_trade_date.clone(),
            strike: self.strike,
            right: self.right.clone(),
            multiplier: self.multiplier,
            currency: self.currency.clone(),
            ..Self::default()
        }
    }

    fn strike_field(&self) -> String {
        js_number(self.strike.unwrap_or(0.0))
    }

    fn multiplier_field(&self) -> String {
        js_number(self.multiplier.unwrap_or(0.0))
    }
}

/// The order fields Openbot sets on a close (`trading.ts:3048-3056`,
/// `:3184-3196`); everything else in `PLACE_ORDER` goes unset.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderSpec {
    /// `BUY` / `SELL`.
    pub action: String,
    pub quantity: f64,
    /// `MKT` / `LMT`.
    pub order_type: String,
    pub lmt_price: Option<f64>,
    /// `DAY` / `GTC`.
    pub tif: String,
    pub oca_group: Option<String>,
    pub oca_type: Option<i32>,
    pub account: String,
    pub order_ref: String,
    pub outside_rth: bool,
    /// Always stated: an unset flag reads as "hold, don't transmit" and TWS
    /// discards the order (error 202).
    pub transmit: bool,
}

/// `PLACE_ORDER` (out 3) for a non-combo contract, every field in order.
pub fn place_order(
    server_version: i32,
    order_id: i64,
    contract: &ContractSpec,
    order: &OrderSpec,
) -> Result<Vec<String>, OrderEncodeError> {
    check_version(server_version)?;
    let empty = String::new;
    let mut t: Vec<String> = Vec::with_capacity(120);
    t.push("3".into());
    // ORDER_CONTAINER (145) dropped the version field.
    t.push(order_id.to_string());
    // Contract.
    t.push(contract.con_id.to_string());
    t.push(contract.symbol.clone());
    t.push(contract.sec_type.clone());
    t.push(contract.last_trade_date.clone());
    t.push(contract.strike_field());
    t.push(contract.right.clone());
    t.push(contract.multiplier_field());
    t.push(contract.exchange.clone());
    t.push(contract.primary_exchange.clone());
    t.push(contract.currency.clone());
    t.push(contract.local_symbol.clone());
    t.push(contract.trading_class.clone());
    t.push(empty()); // secIdType
    t.push(empty()); // secId
    // Main order fields.
    t.push(order.action.clone());
    t.push(js_number(order.quantity));
    t.push(order.order_type.clone());
    t.push(order.lmt_price.map(js_number).unwrap_or_default());
    t.push(empty()); // auxPrice
    // Extended order fields.
    t.push(order.tif.clone());
    t.push(order.oca_group.clone().unwrap_or_default());
    t.push(order.account.clone());
    t.push(empty()); // openClose
    t.push(empty()); // origin
    t.push(order.order_ref.clone());
    t.push(js_bool(order.transmit));
    t.push(empty()); // parentId
    t.push(empty()); // blockOrder
    t.push(empty()); // sweepToFill
    t.push(empty()); // displaySize
    t.push(empty()); // triggerMethod
    t.push(js_bool(order.outside_rth));
    t.push(empty()); // hidden
    t.push(empty()); // deprecated sharesAllocation
    t.push(empty()); // discretionaryAmt
    t.push(empty()); // goodAfterTime
    t.push(empty()); // goodTillDate
    t.push(empty()); // faGroup
    t.push(empty()); // faMethod
    t.push(empty()); // faPercentage (faProfile gone since 177)
    t.push(empty()); // modelCode
    t.push(empty()); // shortSaleSlot
    t.push(empty()); // designatedLocation
    t.push(empty()); // exemptCode
    t.push(order.oca_type.map(|v| v.to_string()).unwrap_or_default());
    for _ in 0..14 {
        // rule80A, settlingFirm, allOrNone, minQty, percentOffset, eTradeOnly,
        // firmQuoteOnly, nbboPriceCap, auctionStrategy, startingPrice,
        // stockRefPrice, delta, stockRangeLower, stockRangeUpper
        t.push(empty());
    }
    t.push(empty()); // overridePercentageConstraints
    t.push(empty()); // volatility
    t.push(empty()); // volatilityType
    t.push(empty()); // deltaNeutralOrderType (unset: no delta-neutral block)
    t.push(empty()); // deltaNeutralAuxPrice
    t.push(empty()); // continuousUpdate
    t.push(empty()); // referencePriceType
    t.push(empty()); // trailStopPrice
    t.push(empty()); // trailingPercent
    t.push(empty()); // scaleInitLevelSize
    t.push(empty()); // scaleSubsLevelSize
    t.push(empty()); // scalePriceIncrement (unset: no SCALE_ORDERS3 block)
    t.push(empty()); // scaleTable
    t.push(empty()); // activeStartTime
    t.push(empty()); // activeStopTime
    t.push(empty()); // hedgeType (unset: no hedgeParam)
    t.push(empty()); // optOutSmartRouting
    t.push(empty()); // clearingAccount
    t.push(empty()); // clearingIntent
    t.push(empty()); // notHeld
    t.push(js_bool(false)); // no deltaNeutralContract
    t.push(empty()); // algoStrategy (unset: no params count)
    t.push(empty()); // algoId
    t.push(empty()); // whatIf
    t.push(empty()); // orderMiscOptions
    t.push(empty()); // solicited
    t.push(empty()); // randomizeSize
    t.push(empty()); // randomizePrice
    // Not a pegged-to-benchmark order: no reference block.
    t.push("0".into()); // conditions count
    for _ in 0..7 {
        // adjustedOrderType, triggerPrice, lmtPriceOffset, adjustedStopPrice,
        // adjustedStopLimitPrice, adjustedTrailingAmount, adjustableTrailingUnit
        t.push(empty());
    }
    t.push(empty()); // extOperator
    t.push(empty()); // softDollarTier name
    t.push(empty()); // softDollarTier value
    t.push(empty()); // cashQty
    t.push(empty()); // mifid2DecisionMaker
    t.push(empty()); // mifid2DecisionAlgo
    t.push(empty()); // mifid2ExecutionTrader
    t.push(empty()); // mifid2ExecutionAlgo
    t.push(empty()); // dontUseAutoPriceForHedge
    t.push(empty()); // isOmsContainer
    t.push(empty()); // discretionaryUpToLimitPrice
    t.push(empty()); // usePriceMgmtAlgo
    t.push(empty()); // duration
    t.push(empty()); // postToAts
    t.push(empty()); // autoCancelParent
    t.push(empty()); // advancedErrorOverride
    t.push(empty()); // manualOrderTime
    // PEGBEST/PEGMID: nothing for a non-IBKRATS MKT/LMT order.
    t.push(empty()); // customerAccount
    t.push(empty()); // professionalCustomer
    // RFQ placeholders only for 187..190.
    t.push(empty()); // includeOvernight
    if server_version >= CME_TAGGING_FIELDS {
        t.push(empty()); // manualOrderIndicator
    }
    Ok(t)
}

/// `CANCEL_ORDER` (out 4).
pub fn cancel_order(server_version: i32, order_id: i64) -> Result<Vec<String>, OrderEncodeError> {
    check_version(server_version)?;
    let mut t = vec!["4".to_string()];
    if server_version < CME_TAGGING_FIELDS {
        t.push("1".into()); // version
    }
    t.push(order_id.to_string());
    t.push(String::new()); // manualOrderCancelTime
    if server_version >= CME_TAGGING_FIELDS {
        t.push(String::new()); // extOperator
        t.push(INTEGER_MAX_VALUE.into()); // manualOrderIndicator unset
    }
    Ok(t)
}

/// `REQ_ALL_OPEN_ORDERS` (out 16): every open order of the login.
pub fn req_all_open_orders() -> Vec<String> {
    vec!["16".into(), "1".into()]
}

/// `REQ_IDS` (out 8): asks for the next valid order id.
pub fn req_ids() -> Vec<String> {
    vec!["8".into(), "1".into(), "1".into()]
}

/// `REQ_EXECUTIONS` (out 7) filtered by this client id only, like
/// `getExecutionDetails({ clientId })` in `trading.ts:2873-2887`.
pub fn req_executions(req_id: i64, client_id: i64) -> Vec<String> {
    let mut t = vec!["7".to_string(), "3".into(), req_id.to_string(), client_id.to_string()];
    // acctCode, time, symbol, secType, exchange, side
    t.extend(std::iter::repeat_n(String::new(), 6));
    t
}

/// `REQ_CONTRACT_DATA` (out 9).
pub fn req_contract_details(
    server_version: i32,
    req_id: i64,
    contract: &ContractSpec,
) -> Result<Vec<String>, OrderEncodeError> {
    check_version(server_version)?;
    Ok(vec![
        "9".into(),
        "8".into(),
        req_id.to_string(),
        contract.con_id.to_string(),
        contract.symbol.clone(),
        contract.sec_type.clone(),
        contract.last_trade_date.clone(),
        contract.strike_field(),
        contract.right.clone(),
        contract.multiplier_field(),
        contract.exchange.clone(),
        contract.primary_exchange.clone(),
        contract.currency.clone(),
        contract.local_symbol.clone(),
        contract.trading_class.clone(),
        String::new(), // includeExpired
        String::new(), // secIdType
        String::new(), // secId
        String::new(), // issuerId
    ])
}

// --------------------------------------------------------------------- inbound

/// The order states after which an order can no longer fill
/// (`TERMINACIONES`, `trading.ts:2853-2858`).
pub const TERMINAL_STATUSES: [&str; 4] = ["Filled", "Cancelled", "ApiCancelled", "Inactive"];

/// `true` when `status` is one of [`TERMINAL_STATUSES`].
pub fn is_terminal(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

/// `ORDER_STATUS` (in 3), no version field since `MARKET_CAP_PRICE` (131).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderStatusRow {
    pub order_id: i64,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub avg_fill_price: Option<f64>,
    pub perm_id: i64,
    pub parent_id: i64,
    pub last_fill_price: Option<f64>,
    pub client_id: i64,
    pub why_held: String,
}

/// The fixed head of `OPEN_ORDER` (in 5): what the close cycle matches on.
/// Everything after `permId` is variable-length and deliberately not read;
/// the order's state comes from `ORDER_STATUS`.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrderRow {
    pub order_id: i64,
    pub contract: inbound::Contract,
    pub action: String,
    pub total_quantity: f64,
    pub order_type: String,
    pub lmt_price: Option<f64>,
    pub tif: String,
    pub oca_group: String,
    pub account: String,
    pub order_ref: String,
    pub client_id: i64,
    pub perm_id: i64,
}

/// `CONTRACT_DATA` (in 10) up to `primaryExch`, for server versions with
/// `SIZE_RULES` (164, no version field) and `LAST_TRADE_DATE` (182).
#[derive(Debug, Clone, PartialEq)]
pub struct ContractDetailsRow {
    pub req_id: i64,
    pub contract: inbound::Contract,
    /// The bare `lastTradeDate` TWS sends beside the hour-and-zone form.
    pub last_trade_date: String,
    pub market_name: String,
    pub min_tick: Option<f64>,
}

/// `EXECUTION_DATA` (in 11) up to `orderRef`, no version field since
/// `LAST_LIQUIDITY` (136).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionRow {
    pub req_id: i64,
    pub order_id: i64,
    pub contract: inbound::Contract,
    pub exec_id: String,
    pub time: String,
    pub account: String,
    pub exchange: String,
    pub side: String,
    pub shares: f64,
    pub price: Option<f64>,
    pub perm_id: i64,
    pub client_id: i64,
    pub cum_qty: f64,
    pub avg_price: Option<f64>,
    pub order_ref: String,
}

fn field(fields: &[String], at: usize) -> String {
    fields.get(at).cloned().unwrap_or_default()
}

fn number(fields: &[String], at: usize) -> Option<f64> {
    let raw = fields.get(at)?.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<f64>().ok().filter(|value| value.is_finite() && *value != f64::MAX)
}

fn int(fields: &[String], at: usize) -> i64 {
    number(fields, at).map(|value| value as i64).unwrap_or(0)
}

/// A contract laid out as `conId, symbol, secType, expiry, strike, right,
/// multiplier, exchange, currency, localSymbol, tradingClass` from `at`.
fn contract_at(fields: &[String], at: usize) -> Option<inbound::Contract> {
    fields.get(at + 10)?;
    Some(inbound::Contract {
        con_id: int(fields, at),
        symbol: field(fields, at + 1),
        sec_type: field(fields, at + 2),
        last_trade_date: field(fields, at + 3),
        strike: number(fields, at + 4),
        right: field(fields, at + 5),
        multiplier: number(fields, at + 6),
        exchange: field(fields, at + 7),
        primary_exchange: String::new(),
        currency: field(fields, at + 8),
        local_symbol: field(fields, at + 9),
        trading_class: field(fields, at + 10),
    })
}

pub(crate) fn decode_order_status(fields: &[String]) -> Option<OrderStatusRow> {
    fields.get(10)?;
    Some(OrderStatusRow {
        order_id: number(fields, 1)? as i64,
        status: field(fields, 2),
        filled: number(fields, 3).unwrap_or(0.0),
        remaining: number(fields, 4).unwrap_or(0.0),
        avg_fill_price: number(fields, 5),
        perm_id: int(fields, 6),
        parent_id: int(fields, 7),
        last_fill_price: number(fields, 8),
        client_id: int(fields, 9),
        why_held: field(fields, 10),
    })
}

pub(crate) fn decode_open_order(fields: &[String]) -> Option<OpenOrderRow> {
    fields.get(25)?;
    Some(OpenOrderRow {
        order_id: number(fields, 1)? as i64,
        contract: contract_at(fields, 2)?,
        action: field(fields, 13),
        total_quantity: number(fields, 14).unwrap_or(0.0),
        order_type: field(fields, 15),
        lmt_price: number(fields, 16),
        // 17 auxPrice
        tif: field(fields, 18),
        oca_group: field(fields, 19),
        account: field(fields, 20),
        // 21 openClose, 22 origin
        order_ref: field(fields, 23),
        client_id: int(fields, 24),
        perm_id: int(fields, 25),
    })
}

pub(crate) fn decode_contract_details(fields: &[String]) -> Option<ContractDetailsRow> {
    fields.get(21)?;
    // `readLastTradeDate`: the first token is the date, the rest the hour.
    let expiry = field(fields, 4);
    Some(ContractDetailsRow {
        req_id: number(fields, 1)? as i64,
        last_trade_date: field(fields, 5),
        market_name: field(fields, 11),
        min_tick: number(fields, 14),
        contract: inbound::Contract {
            con_id: int(fields, 13),
            symbol: field(fields, 2),
            sec_type: field(fields, 3),
            last_trade_date: expiry.split_whitespace().next().unwrap_or("").to_string(),
            strike: number(fields, 6),
            right: field(fields, 7),
            multiplier: number(fields, 15),
            exchange: field(fields, 8),
            primary_exchange: field(fields, 21),
            currency: field(fields, 9),
            local_symbol: field(fields, 10),
            trading_class: field(fields, 12),
        },
    })
}

pub(crate) fn decode_execution(fields: &[String]) -> Option<ExecutionRow> {
    fields.get(26)?;
    Some(ExecutionRow {
        req_id: int(fields, 1),
        order_id: number(fields, 2)? as i64,
        contract: contract_at(fields, 3)?,
        exec_id: field(fields, 14),
        time: field(fields, 15),
        account: field(fields, 16),
        exchange: field(fields, 17),
        side: field(fields, 18),
        shares: number(fields, 19).unwrap_or(0.0),
        price: number(fields, 20),
        perm_id: int(fields, 21),
        client_id: int(fields, 22),
        // 23 liquidation
        cum_qty: number(fields, 24).unwrap_or(0.0),
        avg_price: number(fields, 25),
        order_ref: field(fields, 26),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::{IbInbound, decode};

    fn mnq() -> ContractSpec {
        ContractSpec {
            con_id: 711712,
            symbol: "MNQ".into(),
            sec_type: "FUT".into(),
            last_trade_date: "20261218".into(),
            strike: Some(0.0),
            right: String::new(),
            multiplier: Some(2.0),
            exchange: "CME".into(),
            primary_exchange: String::new(),
            currency: "USD".into(),
            local_symbol: "MNQZ6".into(),
            trading_class: "MNQ".into(),
        }
    }

    fn market_close() -> OrderSpec {
        OrderSpec {
            action: "SELL".into(),
            quantity: 1.0,
            order_type: "MKT".into(),
            lmt_price: None,
            tif: "DAY".into(),
            oca_group: None,
            oca_type: None,
            account: "DU123".into(),
            order_ref: "escritorio-cierre-711712-1".into(),
            outside_rth: true,
            transmit: true,
        }
    }

    fn fields(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|value| (*value).to_string()).collect()
    }

    /// The library's `placeOrder` at server 193 for Openbot's market close:
    /// 118 tokens, read off `encoder.js:735-1221` field by field.
    #[test]
    fn a_market_close_matches_the_library_layout_at_193() {
        let t = place_order(193, 42, &mnq(), &market_close()).unwrap();
        assert_eq!(t.len(), 118);
        assert_eq!(
            &t[..16],
            &fields(&[
                "3", "42", "711712", "MNQ", "FUT", "20261218", "0", "", "2", "CME", "", "USD",
                "MNQZ6", "MNQ", "", ""
            ])[..]
        );
        // action, qty, type, lmt, aux, tif, ocaGroup, account, openClose, origin, orderRef, transmit
        assert_eq!(
            &t[16..28],
            &fields(&[
                "SELL", "1", "MKT", "", "", "DAY", "", "DU123", "", "", "escritorio-cierre-711712-1", "1"
            ])[..]
        );
        // parentId, blockOrder, sweepToFill, displaySize, triggerMethod, outsideRth, hidden
        assert_eq!(&t[28..35], &fields(&["", "", "", "", "", "1", ""])[..]);
        assert_eq!(t[46], "", "ocaType unset on a market order");
        assert_eq!(t[81], "0", "deltaNeutralContract = false");
        assert_eq!(t[89], "0", "conditions count");
        assert_eq!(t[117], "", "manualOrderIndicator at 192+");
        // Nothing but the fields Openbot sets is ever filled in.
        let filled: Vec<usize> = t
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            filled,
            vec![0, 1, 2, 3, 4, 5, 6, 8, 9, 11, 12, 13, 16, 17, 18, 21, 23, 26, 27, 33, 81, 89]
        );
    }

    #[test]
    fn an_oca_limit_carries_price_group_and_type() {
        let mut order = market_close();
        order.order_type = "LMT".into();
        order.lmt_price = Some(18_500.25);
        order.tif = "GTC".into();
        order.oca_group = Some("cierre-711712-1".into());
        order.oca_type = Some(1);
        let t = place_order(193, 43, &mnq(), &order).unwrap();
        assert_eq!(t[18], "LMT");
        assert_eq!(t[19], "18500.25");
        assert_eq!(t[21], "GTC");
        assert_eq!(t[22], "cierre-711712-1");
        assert_eq!(t[46], "1");
    }

    #[test]
    fn versions_change_only_the_cme_tail_and_others_are_refused() {
        let at_191 = place_order(191, 1, &mnq(), &market_close()).unwrap();
        assert_eq!(at_191.len(), 117);
        assert!(place_order(189, 1, &mnq(), &market_close()).is_err());
        assert!(place_order(194, 1, &mnq(), &market_close()).is_err());
        assert_eq!(
            cancel_order(193, 7).unwrap(),
            fields(&["4", "7", "", "", "2147483647"])
        );
        assert_eq!(cancel_order(191, 7).unwrap(), fields(&["4", "1", "7", ""]));
        assert!(cancel_order(150, 7).is_err());
    }

    #[test]
    fn the_details_query_and_executions_filter_match_the_library() {
        let query = mnq().details_query();
        assert_eq!(
            req_contract_details(193, 9, &query).unwrap(),
            fields(&[
                "9", "8", "9", "711712", "MNQ", "FUT", "20261218", "0", "", "2", "", "", "USD", "",
                "", "", "", "", ""
            ])
        );
        assert_eq!(
            req_executions(5, 1000),
            fields(&["7", "3", "5", "1000", "", "", "", "", "", ""])
        );
    }

    #[test]
    fn a_details_expiry_is_trimmed_for_the_order() {
        let mut contract = mnq();
        contract.last_trade_date = "20261218 08:30:00 US/Central".into();
        assert_eq!(contract.for_order().last_trade_date, "20261218");
    }

    #[test]
    fn order_status_open_order_details_and_executions_decode() {
        let status = decode(&fields(&[
            "3", "42", "Filled", "1", "0", "18500.5", "99", "0", "18500.5", "1000", "", "",
        ]));
        let IbInbound::OrderStatus(status) = status else { panic!("{status:?}") };
        assert_eq!((status.order_id, status.status.as_str(), status.filled), (42, "Filled", 1.0));

        let open = decode(&fields(&[
            "5", "42", "711712", "MNQ", "FUT", "20261218", "0", "", "2", "CME", "USD", "MNQZ6",
            "MNQ", "SELL", "1", "MKT", "", "", "DAY", "", "DU123", "", "0",
            "escritorio-cierre-711712-1", "1000", "99", "1", "0",
        ]));
        let IbInbound::OpenOrder(open) = open else { panic!("{open:?}") };
        assert_eq!(open.contract.con_id, 711712);
        assert_eq!(open.order_ref, "escritorio-cierre-711712-1");
        assert_eq!(open.account, "DU123");

        let details = decode(&fields(&[
            "10", "9", "MNQ", "FUT", "20261218 08:30:00 US/Central", "20261218", "0", "", "CME",
            "USD", "MNQZ6", "MNQ", "MNQ", "711712", "0.25", "2", "ACTIVETIM,LMT,MKT", "CME", "1",
            "0", "Micro E-Mini Nasdaq-100", "", "202612",
        ]));
        let IbInbound::ContractDetails(details) = details else { panic!("{details:?}") };
        assert_eq!(details.req_id, 9);
        assert_eq!(details.contract.con_id, 711712);
        assert_eq!(details.contract.exchange, "CME");
        assert_eq!(details.contract.last_trade_date, "20261218");
        assert_eq!(details.min_tick, Some(0.25));
        assert_eq!(details.contract.multiplier, Some(2.0));

        let exec = decode(&fields(&[
            "11", "5", "42", "711712", "MNQ", "FUT", "20261218", "0", "", "2", "CME", "USD",
            "MNQZ6", "MNQ", "0001.01", "20261119 15:30:00", "DU123", "CME", "SLD", "1", "18500.5",
            "99", "1000", "0", "1", "18500.5", "escritorio-cierre-711712-1", "", "", "", "1", "0",
        ]));
        let IbInbound::Execution(exec) = exec else { panic!("{exec:?}") };
        assert_eq!((exec.req_id, exec.order_id, exec.shares), (5, 42, 1.0));
        assert_eq!(exec.order_ref, "escritorio-cierre-711712-1");

        assert_eq!(decode(&fields(&["53", "1"])), IbInbound::OpenOrderEnd);
        assert_eq!(decode(&fields(&["52", "1", "9"])), IbInbound::ContractDetailsEnd { req_id: 9 });
        assert_eq!(decode(&fields(&["55", "1", "5"])), IbInbound::ExecutionEnd { req_id: 5 });
    }

    #[test]
    fn terminal_statuses() {
        assert!(is_terminal("Filled"));
        assert!(is_terminal("Inactive"));
        assert!(!is_terminal("Submitted"));
        assert!(!is_terminal("PreSubmitted"));
    }
}
