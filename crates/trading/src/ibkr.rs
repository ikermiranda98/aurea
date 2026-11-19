//! The IBKR ledger: folds the inbound TWS stream into the dashboard's view.
//!
//! `crates/tws` decodes the wire into typed rows (lane D2's first half); this
//! module keeps the state those rows describe — one ledger per login, with
//! accounts, positions, the market prices only `updatePortfolio` carries, and
//! per-account PnL — and answers the two questions the snapshot asks:
//!
//! * what does the **shown** account hold (`positions`)?
//! * what is open anywhere on this login (`open_positions`)?
//!
//! The mapping rules are `SPECS/D2.md` §2: a flat or non-finite row is
//! dropped, a derivative's `avgCost` is divided by its multiplier for `entry`,
//! `pnl_pct` divides by the raw `avgCost × |size|`, and a summary tag sent
//! once per currency reads from its USD row (else the first).

use std::collections::BTreeMap;

use zeron_proto::trading::{Account, Position, PositionSide};
use zeron_tws::inbound::{AccountSummaryRow, AccountValueRow, IbInbound, PnlRow, PositionRow};

/// One market price the ledger knows for a held contract.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct LedgerPrice {
    market: Option<f64>,
    unrealized: Option<f64>,
}

/// Everything one TWS login has told us, ready to become a snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IbkrLedger {
    /// The managed accounts, in the order TWS named them. The first is the
    /// login's default, exactly like `primeraId` in `trading.ts`.
    accounts: Vec<String>,
    /// The account the dashboard shows; when unset the first managed account
    /// is shown.
    selected: Option<String>,
    /// Positions per account, keyed by contract (`conId` when TWS sent one).
    positions: BTreeMap<String, BTreeMap<String, PositionRow>>,
    /// Ledger prices per account: the only source of market price and
    /// unrealized PnL for a position row.
    prices: BTreeMap<String, BTreeMap<String, LedgerPrice>>,
    /// Summary tags per account. A tag that repeats per currency keeps its USD
    /// row, else the first one seen (`firstTagValue` in `trading.ts:763-766`).
    summary: BTreeMap<String, BTreeMap<String, String>>,
    /// `reqPnL` figures per account, whichever stream named the account first.
    pnl: BTreeMap<String, PnlRow>,
    /// Epoch ms of the newest fact per account.
    updated: BTreeMap<String, i64>,
}

impl IbkrLedger {
    /// Folds one decoded message in. Returns whether the snapshot changed.
    ///
    /// `now` is epoch ms; the caller owns the clock so tests stay deterministic.
    pub fn apply(&mut self, message: &IbInbound, now: i64) -> bool {
        match message {
            IbInbound::ManagedAccounts { accounts } => {
                let before = self.accounts.len();
                for account in accounts {
                    if !self.accounts.contains(account) {
                        self.accounts.push(account.clone());
                    }
                }
                self.accounts.len() != before
            }
            IbInbound::Position(row) => {
                let key = row_key(row);
                let account = row.account.clone();
                self.positions
                    .entry(account.clone())
                    .or_default()
                    .insert(key, row.clone());
                self.updated.insert(account, now);
                true
            }
            IbInbound::PositionEnd => false,
            IbInbound::Portfolio(row) => {
                let key = row_key_of(&row.contract);
                let account = row.account.clone();
                self.prices.entry(account.clone()).or_default().insert(
                    key,
                    LedgerPrice {
                        market: row.market_price,
                        unrealized: row.unrealized_pnl,
                    },
                );
                self.updated.insert(account, now);
                true
            }
            IbInbound::AccountSummary(row) => {
                self.take_summary_tag(row);
                self.updated.insert(row.account.clone(), now);
                true
            }
            IbInbound::AccountSummaryEnd { .. } => false,
            IbInbound::AccountValue(row) => {
                self.take_ledger_value(row);
                self.updated.insert(row.account.clone(), now);
                true
            }
            IbInbound::AccountUpdateTime { .. } => false,
            IbInbound::AccountDownloadEnd { .. } => false,
            IbInbound::Pnl(row) => {
                let account = self.account_of_req_id(row.req_id);
                match account {
                    Some(account) => {
                        self.pnl.insert(account.clone(), row.clone());
                        self.updated.insert(account, now);
                        true
                    }
                    None => false,
                }
            }
            IbInbound::PnlSingle(_) => false,
            IbInbound::Error { .. } => false,
            IbInbound::NextValidId { .. } => false,
            IbInbound::CurrentTime(_) => false,
            IbInbound::Unknown { .. } => false,
            IbInbound::Unreadable { .. } => false,
            // Order entry answers belong to the close cycle, not the ledger.
            IbInbound::OrderStatus(_)
            | IbInbound::OpenOrder(_)
            | IbInbound::OpenOrderEnd
            | IbInbound::ContractDetails(_)
            | IbInbound::ContractDetailsEnd { .. }
            | IbInbound::Execution(_)
            | IbInbound::ExecutionEnd { .. } => false,
        }
    }

    /// The account the dashboard shows: the selected one while it belongs to
    /// this login, else the first managed account.
    pub fn shown(&self) -> Option<&str> {
        match &self.selected {
            Some(account) if self.accounts.contains(account) => Some(account),
            _ => self.accounts.first().map(String::as_str),
        }
    }

    /// Remembers `account` as the one to show before TWS has named any
    /// account (the persisted `cuenta_activa`). [`IbkrLedger::shown`] ignores
    /// it while the login does not manage it, so a stale value is harmless.
    pub fn prefer(&mut self, account: Option<String>) {
        self.selected = account;
    }

    /// The selection as stored, whether or not the login manages it.
    pub fn preferred(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// `true` while the login manages `account`.
    pub fn manages(&self, account: &str) -> bool {
        self.accounts.iter().any(|known| known == account)
    }

    /// Shows `account` if this login manages it. Returns whether it was there.
    pub fn select(&mut self, account: &str) -> bool {
        if !self.accounts.iter().any(|known| known == account) {
            return false;
        }
        self.selected = Some(account.to_string());
        true
    }

    /// The shown account's positions, already normalised for the dashboard.
    pub fn positions(&self) -> Vec<Position> {
        match self.shown() {
            Some(account) => self.positions_of(account),
            None => Vec::new(),
        }
    }

    /// Every open position on the login, every account, ordered by account.
    pub fn open_positions(&self) -> Vec<Position> {
        self.positions
            .keys()
            .flat_map(|account| self.positions_of(account))
            .collect()
    }

    /// What the engine may close: the engine account's positions only
    /// (`cerrables()` in `trading.ts:4384-4389`). On one login every account
    /// belongs to the primary link, so the engine account is the shown one.
    pub fn closable_positions(&self) -> Vec<Position> {
        self.positions()
    }

    /// Open positions on the login's *other* accounts: what a panic leaves
    /// behind and names as leftovers (`sobrantes`, `trading.ts:3697-3746`).
    pub fn foreign_positions(&self) -> Vec<Position> {
        let shown = self.shown();
        self.positions
            .keys()
            .filter(|account| Some(account.as_str()) != shown)
            .flat_map(|account| self.positions_of(account))
            .collect()
    }

    /// The shown account's facts, or `None` until a summary row names it.
    pub fn account(&self) -> Option<Account> {
        self.shown().map(|account| self.account_of(account))
    }

    /// Every account the login has reported, the shown one first.
    pub fn accounts(&self) -> Vec<Account> {
        let mut ordered: Vec<&String> = Vec::new();
        if let Some(shown) = self.shown() {
            ordered.extend(
                self.accounts
                    .iter()
                    .filter(|account| account.as_str() == shown),
            );
        }
        ordered.extend(
            self.accounts
                .iter()
                .filter(|account| Some(account.as_str()) != self.shown()),
        );
        ordered
            .into_iter()
            .map(|account| self.account_of(account))
            .collect()
    }

    /// `true` while the login has named at least one account.
    pub fn connected(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// The contract of `account`'s open row shown as `symbol`.
    pub fn contract_of(&self, account: &str, symbol: &str) -> Option<zeron_tws::inbound::Contract> {
        self.positions
            .get(account)?
            .values()
            .find(|row| row.position != 0.0 && row.contract.display_symbol() == symbol)
            .map(|row| row.contract.clone())
    }

    /// `account`'s open rows, normalised for the dashboard.
    pub fn positions_of(&self, account: &str) -> Vec<Position> {
        let no_positions = BTreeMap::new();
        let no_prices = BTreeMap::new();
        let rows = self.positions.get(account).unwrap_or(&no_positions);
        let prices = self.prices.get(account).unwrap_or(&no_prices);
        rows.iter()
            .filter_map(|(key, row)| {
                let price = prices.get(key).copied().unwrap_or_default();
                row.to_snapshot_row(price.market, price.unrealized)
                    .map(|snapshot| Position {
                        symbol: snapshot.symbol,
                        side: if snapshot.long {
                            PositionSide::Long
                        } else {
                            PositionSide::Short
                        },
                        size: snapshot.size,
                        entry: snapshot.entry,
                        current: snapshot.current,
                        pnl: snapshot.pnl,
                        pnl_pct: snapshot.pnl_pct,
                    })
            })
            .collect()
    }

    fn account_of(&self, account: &str) -> Account {
        let tag = |name: &str| {
            self.summary
                .get(account)
                .and_then(|tags| tags.get(name))
                .and_then(|value| number(value))
        };
        let pnl = self.pnl.get(account);
        Account {
            id: account.to_string(),
            // Raw broker label, as the schema keeps it ("Individual", "Advisor"…).
            kind: self
                .summary
                .get(account)
                .and_then(|tags| tags.get("AccountType"))
                .cloned()
                .unwrap_or_default(),
            // TWS often omits the cash tags in the summary; the ledger's figure
            // stays instead of being wiped on every summary tick.
            cash: tag("TotalCashBalance").or_else(|| tag("CashBalance")),
            equity: tag("NetLiquidation"),
            pnl_day: pnl.and_then(|row| row.daily),
            pnl_realized: pnl.and_then(|row| row.realized),
            available_funds: tag("AvailableFunds"),
            // `demo` is an MT5 fact; IBKR never says.
            demo: None,
            buying_power: tag("BuyingPower"),
            margin_used: tag("MaintMarginReq"),
            excess_liquidity: tag("ExcessLiquidity"),
            updated_at: self.updated.get(account).copied(),
            // The link id is the engine's to fill: the ledger only knows accounts.
            link: None,
        }
    }

    /// Stores one summary tag, preferring the USD row of a per-currency tag.
    fn take_summary_tag(&mut self, row: &AccountSummaryRow) {
        let tags = self.summary.entry(row.account.clone()).or_default();
        if row.currency == "USD" || !tags.contains_key(&row.tag) {
            tags.insert(row.tag.clone(), row.value.clone());
        }
    }

    /// Ledger values arrive per account (`updateAccountValue`); only the tags
    /// the summary may omit are worth keeping — cash and the ledger's own
    /// `AccountType`.
    fn take_ledger_value(&mut self, row: &AccountValueRow) {
        let key = row.key.trim_start_matches("$LEDGER-");
        if key != "TotalCashBalance" && key != "CashBalance" && key != "AccountType" {
            return;
        }
        let value = number(&row.value);
        let tags = self.summary.entry(row.account.clone()).or_default();
        match (key, value) {
            ("AccountType", _) => {
                if !tags.contains_key("AccountType") {
                    tags.insert("AccountType".into(), row.value.clone());
                }
            }
            // A `titleCase` cash figure and the summary share one slot; the
            // summary's USD row wins above, the ledger only fills a gap.
            (_, Some(value)) => {
                if !tags.contains_key("TotalCashBalance") {
                    tags.insert("TotalCashBalance".into(), format!("{value}"));
                }
            }
            (_, None) => {}
        }
    }

    /// `reqPnL` answers carry no account, only the request id; the engine
    /// subscribes one request per account in discovery order, so the index
    /// resolves back to it.
    fn account_of_req_id(&self, req_id: i64) -> Option<String> {
        if req_id < 0 {
            return None;
        }
        self.accounts.get(req_id as usize).cloned()
    }
}

/// The key a position row is deduplicated by: `conId` when TWS sent one, else
/// the `localSymbol`/`symbol` pair the dashboard shows.
fn row_key(row: &PositionRow) -> String {
    row_key_of(&row.contract)
}

fn row_key_of(contract: &zeron_tws::inbound::Contract) -> String {
    if contract.con_id != 0 {
        return contract.con_id.to_string();
    }
    contract.display_symbol()
}

/// `parseValue` in `trading.ts:752-756`: empty or non-finite reads as `None`.
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
    use zeron_tws::inbound::{Contract, decode};

    fn fields(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|field| (*field).to_string()).collect()
    }

    /// The frame sequence the oracle fixture sends for one MNQ position.
    fn oracle_ledger() -> IbkrLedger {
        let mut ledger = IbkrLedger::default();
        let frames = [
            fields(&["15", "1", "A"]),
            fields(&["63", "1", "9001", "A", "AccountType", "Individual", "USD"]),
            fields(&["63", "1", "9001", "A", "NetLiquidation", "101234.56", "USD"]),
            fields(&["63", "1", "9001", "A", "TotalCashBalance", "50000", "USD"]),
            fields(&["63", "1", "9001", "A", "AvailableFunds", "48000", "USD"]),
            fields(&["64", "1", "9001"]),
            fields(&[
                "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37000",
            ]),
            fields(&["62", "1"]),
            fields(&[
                "7", "8", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37250", "74500", "37000", "500", "0", "A",
            ]),
            fields(&["94", "0", "125.50", "30.25", "95.25"]),
        ];
        for (index, frame) in frames.iter().enumerate() {
            ledger.apply(&decode(frame), 1_700_000_000_000 + index as i64);
        }
        ledger
    }

    #[test]
    fn the_oracle_frames_fill_the_shown_account_and_its_position() {
        let ledger = oracle_ledger();
        assert!(ledger.connected());
        assert_eq!(ledger.shown(), Some("A"));

        let account = ledger.account().unwrap();
        assert_eq!(account.id, "A");
        assert_eq!(account.kind, "Individual");
        assert_eq!(account.equity, Some(101_234.56));
        assert_eq!(account.cash, Some(50_000.0));
        assert_eq!(account.available_funds, Some(48_000.0));
        assert_eq!(account.pnl_day, Some(125.5));
        assert_eq!(account.pnl_realized, Some(95.25));
        assert!(account.updated_at.is_some());

        let positions = ledger.positions();
        assert_eq!(positions.len(), 1);
        let row = &positions[0];
        assert_eq!(row.symbol, "MNQZ6");
        assert_eq!(row.side, PositionSide::Long);
        assert_eq!(row.size, 1.0);
        // A future's avgCost is already multiplied; the desk sees half of it.
        assert_eq!(row.entry, Some(18_500.0));
        assert_eq!(row.current, Some(37_250.0));
        assert_eq!(row.pnl, Some(500.0));
        assert_eq!(row.pnl_pct, Some(500.0 / 37_000.0 * 100.0));
    }

    #[test]
    fn a_flat_row_never_reaches_the_snapshot() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "0", "37000",
            ])),
            0,
        );
        assert!(ledger.positions().is_empty());
        assert!(ledger.open_positions().is_empty());
    }

    #[test]
    fn positions_follow_the_shown_account_and_open_positions_hold_them_all() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A,B"])), 0);
        let eurusd = fields(&[
            "61", "3", "A", "83626", "EURUSD", "CASH", "", "", "", "", "IDEALPRO", "USD", "EURUSD",
            "", "1000", "1.05",
        ]);
        let mnq = fields(&[
            "61", "3", "B", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
            "MNQZ6", "MNQ", "-1", "37000",
        ]);
        ledger.apply(&decode(&eurusd), 0);
        ledger.apply(&decode(&mnq), 0);

        let shown = ledger.positions();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].symbol, "EURUSD");

        let all = ledger.open_positions();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|row| row.symbol == "MNQZ6"));

        assert!(ledger.select("B"));
        assert_eq!(ledger.positions()[0].symbol, "MNQZ6");
        assert_eq!(ledger.positions()[0].side, PositionSide::Short);
        assert_eq!(ledger.positions()[0].size, 1.0);

        // A stale selection never wins over the login it does not belong to.
        assert!(!ledger.select("Z"));
        assert_eq!(ledger.shown(), Some("B"));
    }

    /// E1 of `trading-engine.test.ts:728-819`: `positions` of A, open of A+B,
    /// closable only A; the panic's leftovers are B's.
    #[test]
    fn closable_rows_are_the_engine_account_s_and_leftovers_the_rest() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A,B"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "61", "3", "A", "83626", "EURUSD", "CASH", "", "", "", "", "IDEALPRO", "USD",
                "EURUSD", "", "1000", "1.05",
            ])),
            0,
        );
        ledger.apply(
            &decode(&fields(&[
                "61", "3", "B", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37000",
            ])),
            0,
        );
        let closable: Vec<_> = ledger.closable_positions().into_iter().map(|p| p.symbol).collect();
        assert_eq!(closable, vec!["EURUSD"]);
        let leftovers: Vec<_> = ledger.foreign_positions().into_iter().map(|p| p.symbol).collect();
        assert_eq!(leftovers, vec!["MNQZ6"]);
        assert_eq!(ledger.open_positions().len(), 2);
    }

    #[test]
    fn a_preferred_account_waits_for_the_login_to_name_it() {
        let mut ledger = IbkrLedger::default();
        ledger.prefer(Some("B".into()));
        assert_eq!(ledger.shown(), None);
        ledger.apply(&decode(&fields(&["15", "1", "A,B"])), 0);
        assert_eq!(ledger.shown(), Some("B"));

        // A remembered account the login does not manage falls back to the first.
        let mut ledger = IbkrLedger::default();
        ledger.prefer(Some("Z".into()));
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        assert_eq!(ledger.shown(), Some("A"));
        assert_eq!(ledger.preferred(), Some("Z"));
        assert!(!ledger.manages("Z"));
    }

    #[test]
    fn a_per_currency_tag_reads_from_its_usd_row() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "63",
                "1",
                "9001",
                "A",
                "NetLiquidation",
                "1000",
                "EUR",
            ])),
            0,
        );
        ledger.apply(
            &decode(&fields(&[
                "63",
                "1",
                "9001",
                "A",
                "NetLiquidation",
                "1100",
                "USD",
            ])),
            0,
        );
        assert_eq!(ledger.account().unwrap().equity, Some(1_100.0));
    }

    #[test]
    fn the_ledger_fills_cash_when_the_summary_omits_it() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "6",
                "3",
                "$LEDGER-TotalCashBalance",
                "2500.75",
                "USD",
                "A",
            ])),
            0,
        );
        assert_eq!(ledger.account().unwrap().cash, Some(2_500.75));
    }

    #[test]
    fn a_position_without_a_ledger_row_has_no_market_price_or_pnl_percent() {
        // The position stream itself never carries a price; until a portfolio
        // row arrives, `current` and `pnl_pct` stay null.
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37000",
            ])),
            0,
        );
        let row = &ledger.positions()[0];
        assert_eq!(row.current, None);
        assert_eq!(row.pnl_pct, None);
    }

    #[test]
    fn every_account_reported_is_listed_with_the_shown_one_first() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A,B"])), 0);
        ledger.apply(
            &decode(&fields(&[
                "63",
                "1",
                "9001",
                "B",
                "AccountType",
                "Individual",
                "USD",
            ])),
            0,
        );
        let accounts = ledger.accounts();
        assert_eq!(accounts[0].id, "A");
        assert_eq!(accounts[1].id, "B");
        assert_eq!(accounts[1].kind, "Individual");
    }

    #[test]
    fn a_pnl_answer_is_attributed_by_request_index_and_ignores_unknown_ids() {
        let mut ledger = IbkrLedger::default();
        ledger.apply(&decode(&fields(&["15", "1", "A"])), 0);
        // Index 0 belongs to account A.
        assert!(ledger.apply(&decode(&fields(&["94", "0", "10.0", "2.0", "8.0"])), 5));
        assert_eq!(ledger.account().unwrap().pnl_day, Some(10.0));
        // An out-of-range request id names no account: nothing changes.
        assert!(!ledger.apply(&decode(&fields(&["94", "7", "10.0", "2.0", "8.0"])), 5));
    }

    #[test]
    fn a_zero_con_id_falls_back_to_the_symbol_for_deduplication() {
        let contract = Contract::default();
        assert_eq!(
            row_key_of(&Contract {
                con_id: 0,
                symbol: "EURUSD".into(),
                ..contract
            }),
            "EURUSD"
        );
        assert_eq!(
            row_key_of(&Contract {
                con_id: 42,
                ..Contract::default()
            }),
            "42"
        );
    }
}
