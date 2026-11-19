//! Inbound TWS messages, decoded into typed rows (lane D2).
//!
//! The wire is a flat list of NUL-separated fields whose first field is the
//! message id (see `docs/` and [`IN_MSG_ID`] in the upstream `@stoqey/ib`
//! package). This module turns those lists into the small, typed records the
//! trading engine needs — positions, the account's tag values, the portfolio
//! ledger and per-account PnL — and keeps everything else as
//! [`IbInbound::Unknown`] so a newer TWS can add fields without breaking the
//! link.
//!
//! Parsing never fails the connection: a truncated or malformed message that
//! the caller cares about becomes [`IbInbound::Unreadable`]; everything numeric
//! is optional because TWS sends empty strings for values it has no data for.

/// The contract shape TWS repeats inside positions, portfolio rows and
/// contract details: `conId`, symbol, security type, expiry, strike, right,
/// multiplier, exchange, currency, local symbol, class.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contract {
    /// Broker-side contract id (`conId`), 0 when TWS did not send one.
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    /// Expiry or last trading day exactly as TWS sent it.
    pub last_trade_date: String,
    pub strike: Option<f64>,
    /// `C`/`P` for options.
    pub right: String,
    /// Futures/options multiplier; `None` when empty.
    pub multiplier: Option<f64>,
    pub exchange: String,
    /// The primary exchange. `updatePortfolio` sends it where the position
    /// stream sends `exchange`, so only one of the two is filled per row.
    pub primary_exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
}

impl Contract {
    /// Parses the 11-field contract block that starts at `fields[at]`.
    ///
    /// Both `position` (61) and `portfolio` (7) encode the contract as
    /// `conId, symbol, secType, expiry, strike, right, multiplier, <exchange>,
    /// currency, localSymbol, tradingClass`; only the eighth field differs in
    /// meaning, which `primary_at_eighth` selects. Returns `None` only when the
    /// block does not fit, i.e. a truncated frame.
    pub fn parse(fields: &[String], at: usize, primary_at_eighth: bool) -> Option<Self> {
        let block = fields.get(at..at + 11)?;
        let eighth = block[7].clone();
        Some(Self {
            con_id: number(&block[0]).unwrap_or(0.0) as i64,
            symbol: block[1].clone(),
            sec_type: block[2].clone(),
            last_trade_date: block[3].clone(),
            strike: number(&block[4]),
            right: block[5].clone(),
            multiplier: number(&block[6]),
            exchange: if primary_at_eighth {
                String::new()
            } else {
                eighth.clone()
            },
            primary_exchange: if primary_at_eighth {
                eighth
            } else {
                String::new()
            },
            currency: block[8].clone(),
            local_symbol: block[9].clone(),
            trading_class: block[10].clone(),
        })
    }

    /// The name the dashboard shows: `localSymbol`, else `symbol`, else the
    /// `conId` as text. Mirrors `posicionDeIbkr` in `trading.ts`.
    pub fn display_symbol(&self) -> String {
        if !self.local_symbol.is_empty() {
            return self.local_symbol.clone();
        }
        if !self.symbol.is_empty() {
            return self.symbol.clone();
        }
        self.con_id.to_string()
    }

    /// Futures, futures options and options quote `avgCost` already multiplied
    /// by the contract multiplier; `entry` must divide it back out.
    pub fn cost_is_multiplied(&self) -> bool {
        matches!(self.sec_type.as_str(), "FUT" | "FOP" | "OPT")
            && self.multiplier.is_some_and(|value| value > 0.0)
    }

    /// `lastTradeDateOrContractMonth` trimmed to the date: TWS rejects a
    /// pending order whose expiry carries a time and zone (error 321).
    pub fn order_expiry(&self) -> String {
        match self.last_trade_date.find(' ') {
            Some(space) => self.last_trade_date[..space].to_string(),
            None => self.last_trade_date.clone(),
        }
    }

    /// The multiplier as a plain number, defaulting to 1 like the TS port.
    pub fn multiplier_or_one(&self) -> f64 {
        self.multiplier.filter(|value| *value > 0.0).unwrap_or(1.0)
    }
}

/// One row of `reqPositions`: the account holding it plus the contract.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionRow {
    pub account: String,
    pub contract: Contract,
    /// Signed size: positive is long, negative is short.
    pub position: f64,
    /// Average cost as TWS reports it (multiplied for derivatives).
    pub avg_cost: f64,
}

impl PositionRow {
    /// The entry price a trader expects to see, or `None` for a broken row.
    ///
    /// A flat row (`position == 0`) carries no exposure and is dropped by the
    /// snapshot; [`PositionRow::to_snapshot_row`] returns `None` for it.
    pub fn entry(&self) -> Option<f64> {
        if !self.avg_cost.is_finite() || !self.position.is_finite() {
            return None;
        }
        if self.contract.cost_is_multiplied() {
            return Some(self.avg_cost / self.contract.multiplier_or_one());
        }
        Some(self.avg_cost)
    }

    /// The snapshot row for this position, with the ledger price if it arrived.
    ///
    /// `market` is the last portfolio price for this symbol (the position
    /// stream itself never carries one); `pnl` is the ledger's unrealized PnL.
    /// `pnl_pct` stays `None` when there is no cost basis to divide by.
    pub fn to_snapshot_row(
        &self,
        market: Option<f64>,
        pnl: Option<f64>,
    ) -> Option<PositionSnapshot> {
        if !self.position.is_finite() || self.position == 0.0 {
            return None;
        }
        let entry = self.entry();
        Some(PositionSnapshot {
            symbol: self.contract.display_symbol(),
            long: self.position > 0.0,
            size: self.position.abs(),
            entry,
            current: market,
            pnl,
            // `preciosDe` divides by the raw `avgCost`, not the per-contract
            // entry: the percentage is the same either way only for CASH.
            pnl_pct: percent_of_cost(
                self.avg_cost.is_finite().then_some(self.avg_cost),
                self.position,
                pnl,
            ),
        })
    }
}

/// The engine-facing view of one position: already normalised to what the
/// dashboard shows (absolute size, entry divided by the multiplier).
#[derive(Debug, Clone, PartialEq)]
pub struct PositionSnapshot {
    pub symbol: String,
    pub long: bool,
    pub size: f64,
    pub entry: Option<f64>,
    pub current: Option<f64>,
    pub pnl: Option<f64>,
    pub pnl_pct: Option<f64>,
}

/// `pnl / |entry * |size|| * 100`, or `None` when the denominator is unusable.
///
/// This is `preciosDe` in `trading.ts:1204-1208`.
pub fn percent_of_cost(entry: Option<f64>, size: f64, pnl: Option<f64>) -> Option<f64> {
    let entry = entry?;
    let pnl = pnl?;
    if !entry.is_finite() || !size.is_finite() || !pnl.is_finite() {
        return None;
    }
    let denominator = (entry * size.abs()).abs();
    if denominator == 0.0 || !denominator.is_finite() {
        return None;
    }
    Some(pnl / denominator * 100.0)
}

/// One `accountSummary` tag (`reqAccountSummary`), e.g. `NetLiquidation`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSummaryRow {
    pub req_id: i64,
    pub account: String,
    pub tag: String,
    pub value: String,
    pub currency: String,
}

impl AccountSummaryRow {
    /// The tag's value as a number; empty or non-finite reads as `None`
    /// (`parseValue` in `trading.ts:752-756`).
    pub fn number(&self) -> Option<f64> {
        number(&self.value)
    }
}

/// One `portfolio` row of the account ledger: market price and unrealized PnL.
#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioRow {
    pub contract: Contract,
    pub position: f64,
    pub market_price: Option<f64>,
    pub market_value: Option<f64>,
    pub average_cost: Option<f64>,
    pub unrealized_pnl: Option<f64>,
    pub realized_pnl: Option<f64>,
    pub account: String,
}

/// One `updateAccountValue` tag (`reqAccountUpdates`).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountValueRow {
    pub key: String,
    pub value: String,
    pub currency: String,
    pub account: String,
}

impl AccountValueRow {
    pub fn number(&self) -> Option<f64> {
        number(&self.value)
    }
}

/// `reqPnL`: the whole account's day and unrealized/realized PnL.
#[derive(Debug, Clone, PartialEq)]
pub struct PnlRow {
    pub req_id: i64,
    pub daily: Option<f64>,
    pub unrealized: Option<f64>,
    pub realized: Option<f64>,
}

/// `reqPnLSingle`: one contract's PnL, used when the account books several
/// instruments and the dashboard shows a per-position figure.
#[derive(Debug, Clone, PartialEq)]
pub struct PnlSingleRow {
    pub req_id: i64,
    pub position: Option<f64>,
    pub daily: Option<f64>,
    pub unrealized: Option<f64>,
    pub realized: Option<f64>,
    pub value: Option<f64>,
}

/// One decoded inbound message. Variants the engine does not use yet stay as
/// [`IbInbound::Unknown`] so nothing is lost while D3/A4 land.
#[derive(Debug, Clone, PartialEq)]
pub enum IbInbound {
    /// `nextValidId`: the first order id this client may use.
    NextValidId {
        order_id: i64,
    },
    /// `managedAccounts`: the accounts this login can trade.
    ManagedAccounts {
        accounts: Vec<String>,
    },
    /// One position row; several arrive before `PositionEnd`.
    Position(PositionRow),
    PositionEnd,
    /// One summary tag; several arrive before `AccountSummaryEnd`.
    AccountSummary(AccountSummaryRow),
    AccountSummaryEnd {
        req_id: i64,
    },
    /// One ledger tag (`updateAccountValue`).
    AccountValue(AccountValueRow),
    /// `updatePortfolio`: price and unrealized PnL for a held contract.
    Portfolio(PortfolioRow),
    AccountUpdateTime {
        time: String,
    },
    /// The account update burst is complete.
    AccountDownloadEnd {
        account: String,
    },
    Pnl(PnlRow),
    PnlSingle(PnlSingleRow),
    /// TWS error frame. `code` is the broker's numeric code (326 = client id
    /// in use, 200 = missing exchange, 321 = bad expiry…).
    Error {
        req_id: i64,
        code: i64,
        message: String,
    },
    /// The session clock (`reqCurrentTime`).
    CurrentTime(i64),
    /// `ORDER_STATUS`: an order's state and fill (see [`crate::orders`]).
    OrderStatus(crate::orders::OrderStatusRow),
    /// `OPEN_ORDER`: one open order's fixed head.
    OpenOrder(crate::orders::OpenOrderRow),
    /// The `reqAllOpenOrders` answer is complete.
    OpenOrderEnd,
    /// `CONTRACT_DATA`: one contract-details answer.
    ContractDetails(crate::orders::ContractDetailsRow),
    ContractDetailsEnd {
        req_id: i64,
    },
    /// `EXECUTION_DATA`: one fill of a `reqExecutions` answer (or live).
    Execution(crate::orders::ExecutionRow),
    ExecutionEnd {
        req_id: i64,
    },
    /// A message this build does not interpret yet.
    Unknown {
        code: i64,
    },
    /// A message this build knows but could not read (truncated frame).
    Unreadable {
        code: i64,
    },
}

/// Decodes one wire message (already split into fields).
///
/// The first field is the inbound message id. Unknown ids are safe: they come
/// back as [`IbInbound::Unknown`] and the caller keeps reading.
pub fn decode(fields: &[String]) -> IbInbound {
    let Some(code) = fields
        .first()
        .and_then(|value| number(value))
        .map(|value| value as i64)
    else {
        return IbInbound::Unknown { code: -1 };
    };
    match code {
        9 => match fields.get(2).and_then(|value| number(value)) {
            Some(order_id) => IbInbound::NextValidId {
                order_id: order_id as i64,
            },
            None => IbInbound::Unreadable { code },
        },
        15 => IbInbound::ManagedAccounts {
            accounts: fields
                .get(2)
                .map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|account| !account.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        },
        61 => match fields.get(2) {
            Some(account) => match Contract::parse(fields, 3, false) {
                Some(contract) => {
                    let position = fields
                        .get(14)
                        .and_then(|value| number(value))
                        .unwrap_or(0.0);
                    let avg_cost = fields
                        .get(15)
                        .and_then(|value| number(value))
                        .unwrap_or(0.0);
                    IbInbound::Position(PositionRow {
                        account: account.clone(),
                        contract,
                        position,
                        avg_cost,
                    })
                }
                None => IbInbound::Unreadable { code },
            },
            None => IbInbound::Unreadable { code },
        },
        62 => IbInbound::PositionEnd,
        63 => match (fields.get(2), fields.get(3)) {
            (Some(req_id), Some(account)) => IbInbound::AccountSummary(AccountSummaryRow {
                req_id: number(req_id).unwrap_or(0.0) as i64,
                account: account.clone(),
                tag: fields.get(4).cloned().unwrap_or_default(),
                value: fields.get(5).cloned().unwrap_or_default(),
                currency: fields.get(6).cloned().unwrap_or_default(),
            }),
            _ => IbInbound::Unreadable { code },
        },
        64 => IbInbound::AccountSummaryEnd {
            req_id: fields.get(2).and_then(|value| number(value)).unwrap_or(0.0) as i64,
        },
        6 => IbInbound::AccountValue(AccountValueRow {
            key: fields.get(2).cloned().unwrap_or_default(),
            value: fields.get(3).cloned().unwrap_or_default(),
            currency: fields.get(4).cloned().unwrap_or_default(),
            account: fields.get(5).cloned().unwrap_or_default(),
        }),
        7 => match Contract::parse(fields, 2, true) {
            Some(contract) => IbInbound::Portfolio(PortfolioRow {
                position: fields
                    .get(13)
                    .and_then(|value| number(value))
                    .unwrap_or(0.0),
                market_price: fields.get(14).and_then(|value| number(value)),
                market_value: fields.get(15).and_then(|value| number(value)),
                average_cost: fields.get(16).and_then(|value| number(value)),
                unrealized_pnl: fields.get(17).and_then(|value| number(value)),
                realized_pnl: fields.get(18).and_then(|value| number(value)),
                account: fields.get(19).cloned().unwrap_or_default(),
                contract,
            }),
            None => IbInbound::Unreadable { code },
        },
        8 => IbInbound::AccountUpdateTime {
            time: fields.get(2).cloned().unwrap_or_default(),
        },
        54 => IbInbound::AccountDownloadEnd {
            account: fields.get(2).cloned().unwrap_or_default(),
        },
        // `reqPnL` carries no version field: reqId comes straight after the id.
        94 => IbInbound::Pnl(PnlRow {
            req_id: number(fields.get(1).unwrap_or(&String::new())).unwrap_or(0.0) as i64,
            daily: fields.get(2).and_then(|value| number(value)),
            unrealized: fields.get(3).and_then(|value| number(value)),
            realized: fields.get(4).and_then(|value| number(value)),
        }),
        95 => IbInbound::PnlSingle(PnlSingleRow {
            req_id: number(fields.get(1).unwrap_or(&String::new())).unwrap_or(0.0) as i64,
            position: fields.get(2).and_then(|value| number(value)),
            daily: fields.get(3).and_then(|value| number(value)),
            unrealized: fields.get(4).and_then(|value| number(value)),
            realized: fields.get(5).and_then(|value| number(value)),
            value: fields.get(6).and_then(|value| number(value)),
        }),
        4 => IbInbound::Error {
            req_id: number(fields.get(2).unwrap_or(&String::new())).unwrap_or(-1.0) as i64,
            code: number(fields.get(3).unwrap_or(&String::new())).unwrap_or(0.0) as i64,
            message: fields.get(4).cloned().unwrap_or_default(),
        },
        49 => IbInbound::CurrentTime(
            fields.get(2).and_then(|value| number(value)).unwrap_or(0.0) as i64
        ),
        3 => crate::orders::decode_order_status(fields)
            .map(IbInbound::OrderStatus)
            .unwrap_or(IbInbound::Unreadable { code }),
        5 => crate::orders::decode_open_order(fields)
            .map(IbInbound::OpenOrder)
            .unwrap_or(IbInbound::Unreadable { code }),
        53 => IbInbound::OpenOrderEnd,
        10 => crate::orders::decode_contract_details(fields)
            .map(IbInbound::ContractDetails)
            .unwrap_or(IbInbound::Unreadable { code }),
        52 => IbInbound::ContractDetailsEnd {
            req_id: fields.get(2).and_then(|value| number(value)).unwrap_or(-1.0) as i64,
        },
        11 => crate::orders::decode_execution(fields)
            .map(IbInbound::Execution)
            .unwrap_or(IbInbound::Unreadable { code }),
        55 => IbInbound::ExecutionEnd {
            req_id: fields.get(2).and_then(|value| number(value)).unwrap_or(-1.0) as i64,
        },
        _ => IbInbound::Unknown { code },
    }
}

/// `safeNumber` in `trading.ts`: empty, `NaN` and infinities read as `None`.
fn number(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|field| (*field).to_string()).collect()
    }

    #[test]
    fn decodes_the_mnq_future_row_from_the_oracle_fixture() {
        let decoded = decode(&fields(&[
            "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
            "MNQZ6", "MNQ", "1", "37000",
        ]));
        let IbInbound::Position(row) = decoded else {
            panic!("expected a position row, got {decoded:?}");
        };

        assert_eq!(row.account, "A");
        assert_eq!(row.contract.con_id, 711_712);
        assert_eq!(row.contract.symbol, "MNQZ6");
        assert_eq!(row.contract.sec_type, "FUT");
        assert_eq!(row.contract.multiplier, Some(2.0));
        assert_eq!(row.position, 1.0);
        assert_eq!(row.avg_cost, 37_000.0);

        // A future's avgCost is already multiplied: 37.000 × 2 reads 74.000,
        // and the entry the desk sees is half of it.
        let snapshot = row.to_snapshot_row(Some(37_250.0), Some(500.0)).unwrap();
        assert_eq!(snapshot.symbol, "MNQZ6");
        assert!(snapshot.long);
        assert_eq!(snapshot.size, 1.0);
        assert_eq!(snapshot.entry, Some(18_500.0));
        assert_eq!(snapshot.current, Some(37_250.0));
        assert_eq!(snapshot.pnl, Some(500.0));
        // pnl_pct uses the raw avgCost × size, exactly like `preciosDe`.
        assert_eq!(snapshot.pnl_pct, Some(500.0 / 37_000.0 * 100.0));
    }

    #[test]
    fn a_forex_row_keeps_its_avg_cost_and_a_short_reads_negative_size() {
        let decoded = decode(&fields(&[
            "61", "3", "A", "83626", "EURUSD", "CASH", "", "", "", "", "IDEALPRO", "USD", "EURUSD",
            "", "-1000", "1.05",
        ]));
        let IbInbound::Position(row) = decoded else {
            panic!("expected a position row");
        };
        let snapshot = row.to_snapshot_row(None, None).unwrap();
        assert_eq!(snapshot.symbol, "EURUSD");
        assert!(!snapshot.long);
        assert_eq!(snapshot.size, 1_000.0);
        assert_eq!(snapshot.entry, Some(1.05));
        assert_eq!(snapshot.current, None);
        assert_eq!(snapshot.pnl_pct, None);
    }

    #[test]
    fn a_flat_row_is_dropped_by_the_snapshot_mapping() {
        let decoded = decode(&fields(&[
            "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
            "MNQZ6", "MNQ", "0", "37000",
        ]));
        let IbInbound::Position(row) = decoded else {
            panic!("expected a position row");
        };
        assert_eq!(row.to_snapshot_row(Some(37_250.0), Some(0.0)), None);
    }

    #[test]
    fn account_summary_and_its_end_carry_the_request_id() {
        assert_eq!(
            decode(&fields(&[
                "63",
                "1",
                "9001",
                "A",
                "NetLiquidation",
                "101234.56",
                "USD"
            ])),
            IbInbound::AccountSummary(AccountSummaryRow {
                req_id: 9001,
                account: "A".into(),
                tag: "NetLiquidation".into(),
                value: "101234.56".into(),
                currency: "USD".into(),
            })
        );
        assert_eq!(
            decode(&fields(&["64", "1", "9001"])),
            IbInbound::AccountSummaryEnd { req_id: 9001 }
        );

        let row = AccountSummaryRow {
            req_id: 9001,
            account: "A".into(),
            tag: "TotalCashBalance".into(),
            value: "".into(),
            currency: "USD".into(),
        };
        // An empty tag never turns into a zero: it must keep the previous value.
        assert_eq!(row.number(), None);
    }

    #[test]
    fn ledger_portfolio_row_carries_the_market_price_the_position_stream_lacks() {
        let decoded = decode(&fields(&[
            "7", "8", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD", "MNQZ6",
            "MNQ", "1", "37250", "74500", "37000", "500", "0", "A",
        ]));
        let IbInbound::Portfolio(row) = decoded else {
            panic!("expected a portfolio row, got {decoded:?}");
        };
        assert_eq!(row.contract.local_symbol, "MNQZ6");
        assert_eq!(row.position, 1.0);
        assert_eq!(row.market_price, Some(37_250.0));
        assert_eq!(row.average_cost, Some(37_000.0));
        assert_eq!(row.unrealized_pnl, Some(500.0));
        assert_eq!(row.account, "A");
    }

    #[test]
    fn error_frames_keep_the_broker_code() {
        assert_eq!(
            decode(&fields(&[
                "4",
                "2",
                "-1",
                "326",
                "Unable to connect as the client id is already in use.",
                ""
            ])),
            IbInbound::Error {
                req_id: -1,
                code: 326,
                message: "Unable to connect as the client id is already in use.".into(),
            }
        );
    }

    #[test]
    fn pnl_frames_expose_the_day_figure_and_realized_pnl() {
        assert_eq!(
            decode(&fields(&["94", "9002", "125.50", "30.25", "95.25"])),
            IbInbound::Pnl(PnlRow {
                req_id: 9002,
                daily: Some(125.5),
                unrealized: Some(30.25),
                realized: Some(95.25),
            })
        );
        assert_eq!(
            decode(&fields(&[
                "95", "9002", "1", "125.50", "30.25", "95.25", "37400"
            ])),
            IbInbound::PnlSingle(PnlSingleRow {
                req_id: 9002,
                position: Some(1.0),
                daily: Some(125.5),
                unrealized: Some(30.25),
                realized: Some(95.25),
                value: Some(37_400.0),
            })
        );
    }

    #[test]
    fn the_expiry_of_an_order_is_the_date_without_time_or_zone() {
        let contract = Contract {
            last_trade_date: "20261218 08:30:00 US/Central".into(),
            ..Contract::default()
        };
        assert_eq!(contract.order_expiry(), "20261218");
    }

    #[test]
    fn a_truncated_row_is_unreadable_instead_of_panicking() {
        assert_eq!(
            decode(&fields(&["61", "3", "A", "711712"])),
            IbInbound::Unreadable { code: 61 }
        );
        assert_eq!(
            decode(&fields(&["7", "8", "711712"])),
            IbInbound::Unreadable { code: 7 }
        );
    }

    #[test]
    fn unknown_ids_and_the_server_version_frame_are_not_errors() {
        // The opening version frame has no message id; `IcClient` reads it
        // before delegating anything here, so a bare number is just unknown.
        assert_eq!(
            decode(&fields(&["193", "20261105 03:55:06 CET"])),
            IbInbound::Unknown { code: 193 }
        );
        assert_eq!(
            decode(&fields(&["999", "x"])),
            IbInbound::Unknown { code: 999 }
        );
        assert_eq!(decode(&[]), IbInbound::Unknown { code: -1 });
    }
}
