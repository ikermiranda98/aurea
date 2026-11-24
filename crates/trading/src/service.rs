//! The live trading service: one TWS paper link, the ledger it fills and the
//! snapshot the dashboard streams.
//!
//! Until a link is asked for, the service is the demo fixture (`mock`) the
//! dashboard has always shown. [`TradingService::connect`] dials TWS paper,
//! starts the read-only subscription (`managed accounts`, `positions`, the
//! account ledger, the summary tags and per-account PnL — the requests in
//! [`zeron_tws::requests`]), and from then on publishes what the wire says:
//!
//! * `positions`/`open_positions` come from the [`IbkrLedger`] built by
//!   [`zeron_tws::inbound`], never from the fixture;
//! * `closable_positions` is the engine account's rows only (lane D4): a
//!   close is always placed on that account, never on a sibling the login
//!   also manages;
//! * the shown account is remembered in `trading_meta.cuenta_activa` when a
//!   [`Store`] is attached, and forgotten once a connected login stops
//!   naming it (`revalidarCuentaActiva`, `trading.ts:1036-1051`);
//! * [`TradingService::disconnect`] goes back to the demo snapshot, so a dev
//!   machine without TWS keeps the dashboard it had.
//!
//! Live port 7496 is refused here, not only in the UI: this module is the last
//! place before a socket is opened.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Mutex as AsyncMutex, broadcast, watch};
use zeron_proto::trading::{
    Account, Broker, EquityPoint, Link, Log, LogLevel, ProgramState, Programs, ProposalAction,
    ProposalStatus, Rule, RuleAddInput, RuleStatus, Snapshot, TradingError, TradingProposal,
};
use zeron_tws::client::{IbClient, IbClientConfig, IbEvent};
use zeron_tws::inbound::{self, IbInbound};
use zeron_tws::requests;

use crate::ibkr::IbkrLedger;
use crate::proposals;
use crate::rules;
use crate::store::Store;
use crate::SnapshotWatch;

/// The IB Gateway/TWS live API port. Aurea never trades live: it is refused at
/// the socket, whatever a caller asked for.
pub const LIVE_PORT: u16 = 7496;
/// TWS paper's default API port.
pub const PAPER_PORT: u16 = 7497;
/// The first client id the trading layer tries (lane D1 reserves 1000–9999).
const FIRST_CLIENT_ID: i32 = 1000;
/// Equity points are sampled at most this often, like `trading.ts`.
const EQUITY_SAMPLE_MS: i64 = 15_000;
/// How many engine log rows the live snapshot keeps.
const MAX_LOGS: usize = 200;
/// `reqId` for the account-summary burst (TWS only needs it to be unique).
const SUMMARY_REQ_ID: i64 = 9001;
/// The most proposals the desk keeps.
const MAX_PROPOSALS: usize = 50;
/// `trading_meta` key of the account the dashboard shows (`trading.ts:931`).
pub const ACTIVE_ACCOUNT_META: &str = "cuenta_activa";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Keeps the desk short: the newest 50, pending ones first kept.
fn trim_proposals(list: &mut Vec<TradingProposal>) {
    while list.len() > MAX_PROPOSALS {
        match list.iter().rposition(|p| p.status != ProposalStatus::Pending) {
            Some(old) => {
                list.remove(old);
            }
            None => {
                list.pop();
            }
        }
    }
}

/// Everything one link has taught the service. Guarded as a whole: the pump
/// task and the RPC handlers mutate it from different threads.
struct State {
    ledger: IbkrLedger,
    /// `true` from a successful [`TradingService::connect`] until disconnect.
    live: bool,
    /// The client id TWS refused as taken, while it stays refused.
    client_in_use: Option<i32>,
    /// The address the live link dialled.
    link: Option<(String, u16)>,
    equity: Vec<EquityPoint>,
    last_sample: i64,
    logs: Vec<Log>,
    /// The server version TWS opened this session with; order entry needs it
    /// to pick the wire layout (`zeron_tws::orders`).
    server_version: Option<i32>,
    /// The next order id this client may use (`nextValidId`, then +1 each).
    next_order_id: Option<i64>,
    /// The API client id the live link opened with.
    client_id: i32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            ledger: IbkrLedger::default(),
            live: false,
            client_in_use: None,
            link: None,
            equity: Vec::new(),
            last_sample: 0,
            logs: Vec::new(),
            server_version: None,
            next_order_id: None,
            client_id: FIRST_CLIENT_ID,
        }
    }
}

/// Handle to the trading service. Cloning shares one link and one snapshot.
#[derive(Clone)]
pub struct TradingService {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    snapshot: watch::Sender<Snapshot>,
    client: Mutex<Option<IbClient>>,
    connecting: AsyncMutex<()>,
    /// Where the active account is persisted; `None` keeps it in memory.
    store: Mutex<Option<Store>>,
    /// Every decoded inbound message, for the close cycle's request/answer
    /// waits (contract details, open orders, executions, order status).
    inbound: broadcast::Sender<IbInbound>,
    /// Agents' money requests, newest first (see [`TradingProposal`]).
    proposals: Mutex<Vec<TradingProposal>>,
    /// Watch rules, newest first; evaluated on every engine update.
    rules: Mutex<Vec<Rule>>,
    /// Told when a rule fires (the engine posts a desktop banner); `None`
    /// keeps the desk quiet.
    rule_listener: Mutex<Option<Arc<dyn Fn(&Rule) + Send + Sync>>>,
    /// Told when an agent files a request (the engine posts a desktop banner
    /// so the person sees the card without the window in front).
    proposal_listener: Mutex<Option<Arc<dyn Fn(&TradingProposal) + Send + Sync>>>,
    /// One rule pass at a time: a fired rule files a proposal, which publishes
    /// again, and that second pass must not loop.
    evaluating: AtomicBool,
    /// Proposals approved and running now: a second approval (a double
    /// click) must not run the same action twice.
    claimed: Mutex<std::collections::HashSet<String>>,
    /// The desk's MetaTrader 5 accounts (set by the engine's assembly).
    mt5: Mutex<Option<Arc<crate::mt5::escritorio::Mt5Escritorio>>>,
}

impl Default for TradingService {
    fn default() -> Self {
        Self::new()
    }
}

/// Which halves of a snapshot a rule pass may read (`TradingService`'s rules).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RuleTrust {
    /// The account and positions come from a live TWS link.
    pub broker: bool,
    /// The systems come from the engine's real pool.
    pub pool: bool,
}

impl TradingService {
    /// A service with no link open: the desk shows nothing until TWS or an
    /// MT5 account connects (Aurea never invents a desk).
    pub fn new() -> Self {
        let (snapshot, _rx) = watch::channel(State::default().live_snapshot());
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            snapshot,
            client: Mutex::new(None),
            connecting: AsyncMutex::new(()),
            store: Mutex::new(None),
            inbound: broadcast::channel(1024).0,
            proposals: Mutex::new(Vec::new()),
            rules: Mutex::new(Vec::new()),
            rule_listener: Mutex::new(None),
            proposal_listener: Mutex::new(None),
            evaluating: AtomicBool::new(false),
            claimed: Mutex::new(std::collections::HashSet::new()),
            mt5: Mutex::new(None),
        });
        Self { inner }
    }

    /// A service that persists the active account in `store`'s `trading_meta`.
    pub fn with_store(store: Store) -> Self {
        let service = Self::new();
        *lock(&service.inner.store) = Some(store.clone());
        // Rules and requests outlast a restart: they live in the engine's store.
        if let Some(loaded) = service.inner.load_rules() {
            *lock(&service.inner.rules) = loaded;
        }
        if let Some(mut loaded) = service.inner.load_proposals() {
            trim_proposals(&mut loaded);
            *lock(&service.inner.proposals) = loaded;
        }
        service.inner.publish();
        service
    }

    /// Attaches the desk's MetaTrader 5 accounts: their bridges start now and
    /// the desk follows what they read (republished only when it changes).
    pub fn set_mt5(&self, mt5: Arc<crate::mt5::escritorio::Mt5Escritorio>) {
        *lock(&self.inner.mt5) = Some(Arc::clone(&mt5));
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&self.inner);
            handle.spawn(async move {
                for fallo in mt5.restaurar() {
                    tracing::warn!(target: "mt5", "{fallo}");
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let Some(inner) = weak.upgrade() else { break };
                    if mt5.hay_enlaces() {
                        inner.publish();
                    }
                }
            });
        }
        self.inner.publish();
    }

    /// The desk's MetaTrader 5 accounts, once attached.
    pub fn mt5(&self) -> Option<Arc<crate::mt5::escritorio::Mt5Escritorio>> {
        lock(&self.inner.mt5).clone()
    }

    /// Republishes the desk (after an MT5 account was added or removed).
    pub fn refresh(&self) {
        self.inner.publish();
    }

    /// The persisted active account, if a store is attached and holds one.
    pub fn remembered_account(&self) -> Option<String> {
        self.inner.remembered()
    }

    /// Open positions on the login's accounts other than the engine's: what a
    /// panic cannot sweep and must name (`leftover_symbols`).
    pub fn leftover_positions(&self) -> Vec<zeron_proto::trading::Position> {
        let state = lock(&self.inner.state);
        if !state.live {
            return Vec::new();
        }
        state.ledger.foreign_positions()
    }

    /// Files an agent's money request on the desk. Nothing runs until the
    /// person approves it ([`Self::start_proposal`]).
    pub fn propose(
        &self,
        action: ProposalAction,
        reason: String,
        chat_id: Option<String>,
    ) -> TradingProposal {
        let from_agent = chat_id.is_some();
        let proposal = self.inner.push_proposal(action, reason, chat_id);
        self.inner.publish();
        // A request the person did not ask for in this window: ring so the
        // card waiting in the chat is not missed.
        if from_agent
            && let Some(listener) = lock(&self.inner.proposal_listener).clone()
        {
            listener(&proposal);
        }
        proposal
    }

    /// The desk's proposals, newest first.
    pub fn proposals(&self) -> Vec<TradingProposal> {
        lock(&self.inner.proposals).clone()
    }

    /// The desk's watch rules, newest first.
    pub fn rules(&self) -> Vec<Rule> {
        lock(&self.inner.rules).clone()
    }

    /// Registers what to tell the person when a rule fires (the engine posts
    /// a desktop banner). Replaces any earlier listener.
    pub fn set_rule_listener(&self, listener: Arc<dyn Fn(&Rule) + Send + Sync>) {
        *lock(&self.inner.rule_listener) = Some(listener);
    }

    /// Registers what to tell the person when an agent files a request, so the
    /// card is not missed while another window is in front. Replaces any
    /// earlier listener; the listener decides (the engine applies the
    /// notifications setting and "only in the background").
    pub fn set_proposal_listener(
        &self,
        listener: Arc<dyn Fn(&TradingProposal) + Send + Sync>,
    ) {
        *lock(&self.inner.proposal_listener) = Some(listener);
    }

    /// Adds a rule that watches the desk until it fires, expires or is
    /// cancelled. It never runs its proposal by itself.
    pub fn add_rule(&self, input: RuleAddInput) -> Result<Rule, TradingError> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err(TradingError::InvalidRequest {
                message: "la regla necesita un nombre".into(),
            });
        }
        let reason = input.reason.trim();
        if reason.is_empty() {
            return Err(TradingError::InvalidRequest {
                message: "la regla necesita un motivo".into(),
            });
        }
        rules::validate(&input.condition).map_err(|message| TradingError::InvalidRequest {
            message,
        })?;
        // An agent that loops must not pile up watchers: the desk shows and
        // checks every one each second.
        let watching = lock(&self.inner.rules)
            .iter()
            .filter(|rule| rule.status == RuleStatus::Watching)
            .count();
        if watching >= rules::MAX_WATCHING {
            return Err(TradingError::InvalidRequest {
                message: format!(
                    "ya hay {watching} reglas vigilando (el máximo es {}); cancela alguna antes de crear otra",
                    rules::MAX_WATCHING
                ),
            });
        }
        let minutes = input
            .expires_minutes
            .unwrap_or(rules::DEFAULT_EXPIRES_MINUTES);
        if !minutes.is_finite() || minutes <= 0.0 || minutes > rules::MAX_EXPIRES_MINUTES {
            return Err(TradingError::InvalidRequest {
                message: "la caducidad debe estar entre 1 minuto y 30 días".into(),
            });
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = now_ms();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let rule = Rule {
            id: format!("rul-{now}-{seq}"),
            name: name.chars().take(80).collect(),
            condition: input.condition,
            proposal: input.proposal,
            reason: reason.chars().take(280).collect(),
            status: RuleStatus::Watching,
            created_at: now,
            expires_at: Some(now + (minutes * 60_000.0) as i64),
            fired_at: None,
            note: None,
        };
        lock(&self.inner.rules).insert(0, rule.clone());
        self.inner.save_rule(&rule);
        self.inner.publish();
        Ok(rule)
    }

    /// Stops watching a rule that has not fired yet.
    pub fn cancel_rule(&self, id: &str) -> Result<Rule, TradingError> {
        let cancelled = {
            let mut list = lock(&self.inner.rules);
            let Some(rule) = list.iter_mut().find(|rule| rule.id == id) else {
                return Err(TradingError::InvalidRequest {
                    message: format!("No such rule: {id}"),
                });
            };
            if rule.status != RuleStatus::Watching {
                return Err(TradingError::InvalidRequest {
                    message: "Esa regla ya terminó: no hay nada que cancelar.".into(),
                });
            }
            rule.status = RuleStatus::Cancelled;
            rule.note = Some("Cancelada en la mesa.".into());
            rule.clone()
        };
        self.inner.save_rule(&cancelled);
        self.inner.publish();
        Ok(cancelled)
    }

    /// Checks every watching rule against the desk as it stands. A rule that
    /// holds becomes `Fired`; if it carries a proposal, that proposal lands in
    /// the desk for the person to approve or reject.
    pub fn evaluate_rules(&self, snapshot: &Snapshot) {
        self.inner.evaluate_rules(snapshot, RuleTrust { broker: true, pool: true });
    }

    /// Evaluates the rules on every engine update (the engine ticks once a
    /// second). The broker half is the service's own latest snapshot.
    pub fn follow_engine(&self, watch: SnapshotWatch) {
        let service = self.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let mut watch = watch;
            loop {
                if watch.changed().await.is_err() {
                    break;
                }
                let engine = watch.borrow_and_update().clone();
                // The systems are real once the engine has a pool; the account
                // and positions once TWS is live. Demo figures fire nothing.
                let trust = RuleTrust {
                    broker: service.live(),
                    pool: !engine.systems.is_empty(),
                };
                let merged = service.merged_with(engine);
                service.inner.evaluate_rules(&merged, trust);
            }
        });
    }

    /// Takes a pending proposal's action to run it, or says why not. The
    /// proposal stays claimed until [`Self::finish_proposal`], so a second
    /// approval meanwhile is refused instead of running it again.
    pub fn start_proposal(&self, id: &str) -> Result<ProposalAction, String> {
        let list = lock(&self.inner.proposals);
        match list.iter().find(|p| p.id == id) {
            None => Err(format!("No such proposal: {id}")),
            Some(p) if p.status != ProposalStatus::Pending => {
                Err("That proposal was already resolved.".into())
            }
            Some(p) => {
                if !lock(&self.inner.claimed).insert(id.to_string()) {
                    return Err("Esa petición ya se está ejecutando.".into());
                }
                Ok(p.action.clone())
            }
        }
    }

    /// Records how a proposal ended (rejected, done, failed).
    pub fn finish_proposal(&self, id: &str, status: ProposalStatus, result: Option<String>) {
        lock(&self.inner.claimed).remove(id);
        let updated = {
            let mut list = lock(&self.inner.proposals);
            match list.iter_mut().find(|p| p.id == id) {
                Some(p) => {
                    p.status = status;
                    p.resolved_at = Some(now_ms());
                    p.result = result;
                    Some(p.clone())
                }
                None => None,
            }
        };
        if let Some(proposal) = updated {
            self.inner.save_proposal(&proposal);
        }
        self.inner.publish();
    }

    /// Every inbound message from now on. Subscribe *before* sending the
    /// request whose answer you wait for.
    pub fn subscribe_inbound(&self) -> broadcast::Receiver<IbInbound> {
        self.inner.inbound.subscribe()
    }

    /// The server version of the live session, once TWS has sent it.
    pub fn server_version(&self) -> Option<i32> {
        let state = lock(&self.inner.state);
        state.live.then_some(state.server_version).flatten()
    }

    /// The client id the service's own link uses (executions filter on it).
    pub fn client_id(&self) -> i64 {
        i64::from(lock(&self.inner.state).client_id)
    }

    /// Reserves the next order id, or `None` until TWS has sent `nextValidId`.
    pub fn allocate_order_id(&self) -> Option<i64> {
        let mut state = lock(&self.inner.state);
        if !state.live {
            return None;
        }
        let id = state.next_order_id?;
        state.next_order_id = Some(id + 1);
        Some(id)
    }

    /// Appends one engine row to the live log and republishes.
    pub fn push_log(&self, level: LogLevel, message: String) {
        {
            let mut state = lock(&self.inner.state);
            if !state.live {
                return;
            }
            state.push_log(level, message);
        }
        self.inner.publish();
    }

    /// Sends one request on the live link. `false` when no link is up.
    pub fn send(&self, fields: Vec<String>) -> bool {
        match lock(&self.inner.client).as_ref() {
            Some(client) if client.is_connected() => client.request(fields),
            _ => false,
        }
    }

    /// The contract TWS reported for `symbol` on `account`'s position.
    pub fn position_contract(
        &self,
        account: &str,
        symbol: &str,
    ) -> Option<zeron_tws::inbound::Contract> {
        lock(&self.inner.state).ledger.contract_of(account, symbol)
    }

    /// `account`'s open rows as the wire states them now (the close cycle
    /// re-reads its fixed account, never the shown one).
    pub fn account_positions(&self, account: &str) -> Vec<zeron_proto::trading::Position> {
        let state = lock(&self.inner.state);
        if !state.live {
            return Vec::new();
        }
        state.ledger.positions_of(account)
    }

    /// The snapshot stream `WatchTradingSnapshot` forwards.
    pub fn watch(&self) -> SnapshotWatch {
        self.inner.snapshot.subscribe()
    }

    /// The snapshot as it stands right now.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.borrow().clone()
    }

    /// Whether a live TWS link owns the snapshot.
    pub fn live(&self) -> bool {
        lock(&self.inner.state).live
    }

    /// Cloneable handle to the underlying TWS client, if connected.
    pub fn tws_client(&self) -> Option<zeron_tws::client::IbClient> {
        lock(&self.inner.client).clone()
    }

    /// Dials TWS paper (127.0.0.1:7497) and subscribes the read-only streams.
    ///
    /// Returns the shown account once known; the first connect usually answers
    /// `None` because TWS names the accounts after the socket is up (they
    /// arrive on the snapshot stream).
    pub async fn connect(&self) -> Result<Option<Account>, TradingError> {
        self.connect_to(IbClientConfig {
            port: PAPER_PORT,
            client_id: FIRST_CLIENT_ID,
            ..IbClientConfig::default()
        })
        .await
    }

    /// [`TradingService::connect`] against an explicit address, for custom TWS
    /// installs and tests. Refuses the live port.
    pub async fn connect_to(
        &self,
        config: IbClientConfig,
    ) -> Result<Option<Account>, TradingError> {
        if config.port == LIVE_PORT {
            return Err(TradingError::InvalidRequest {
                message: format!(
                    "Aurea solo opera en paper: el puerto {LIVE_PORT} (live) está prohibido."
                ),
            });
        }
        let _turn = self.inner.connecting.lock().await;
        if self.live() && self.inner.client_handle().is_some() {
            return Ok(lock(&self.inner.state).ledger.account());
        }
        let link = (config.host.clone(), config.port);
        let client_id = config.client_id;
        let (client, events) =
            IbClient::connect(config)
                .await
                .map_err(|error| TradingError::InvalidRequest {
                    message: format!(
                        "TWS no responde en {}:{}: {error}. Abre TWS y su API (paper).",
                        link.0, link.1
                    ),
                })?;
        let remembered = self.inner.remembered();
        {
            let mut state = lock(&self.inner.state);
            state.live = true;
            state.client_in_use = None;
            state.link = Some(link);
            state.ledger = IbkrLedger::default();
            state.ledger.prefer(remembered);
            state.server_version = None;
            state.next_order_id = None;
            state.client_id = client_id;
            state.equity.clear();
            state.last_sample = 0;
            state.logs.clear();
        }
        *lock(&self.inner.client) = Some(client);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.pump(events).await });
        self.inner.publish();
        Ok(None)
    }

    /// Closes the link and shows the demo snapshot again.
    pub async fn disconnect(&self) {
        let client = lock(&self.inner.client).take();
        if let Some(client) = client {
            client.disconnect().await;
        }
        {
            let mut state = lock(&self.inner.state);
            state.live = false;
            state.client_in_use = None;
            state.link = None;
            state.server_version = None;
            state.next_order_id = None;
            state.ledger = IbkrLedger::default();
            state.equity.clear();
            state.last_sample = 0;
            state.logs.clear();
        }
        self.inner.publish();
    }

    /// Switches the dashboard's shown account. In demo mode the mock owns it.
    pub fn select(&self, account: &str) -> Result<Option<Account>, TradingError> {
        if !self.live() {
            return Err(TradingError::InvalidRequest {
                message: "Conecta TWS para elegir una de sus cuentas.".into(),
            });
        }
        let selected = account.to_string();
        let mut state = lock(&self.inner.state);
        if !state.ledger.select(account) {
            return Err(TradingError::InvalidRequest {
                message: format!("Unknown account: {account}"),
            });
        }
        let account = state.ledger.account();
        drop(state);
        self.inner.remember(Some(selected));
        self.inner.publish();
        Ok(account)
    }

    /// Probes `host`'s IBKR ports. 7497 is paper, 7496 live; a probe never
    /// connects, it only asks whether something is listening. With `port`,
    /// only that port is probed (the panel's own candidate).
    pub async fn detect(
        host: Option<&str>,
        port: Option<u16>,
    ) -> Vec<zeron_proto::trading::IbkrPort> {
        let host = host.unwrap_or("127.0.0.1").to_string();
        let candidates = match port {
            Some(port) => vec![port],
            None => vec![PAPER_PORT, LIVE_PORT],
        };
        let mut ports = Vec::with_capacity(candidates.len());
        for port in candidates {
            let open = probe(&host, port).await;
            ports.push(zeron_proto::trading::IbkrPort {
                live: port == LIVE_PORT,
                open,
                host: host.clone(),
                port,
            });
        }
        ports
    }
}

async fn probe(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(400),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await,
        Ok(Ok(_))
    )
}

impl Inner {
    fn client_handle(&self) -> Option<IbClient> {
        lock(&self.client).clone()
    }

    fn remembered(&self) -> Option<String> {
        let store = lock(&self.store).clone()?;
        store.meta_get(ACTIVE_ACCOUNT_META).ok().flatten()
    }

    /// Persists (or, with `None`, forgets) the active account. A store error
    /// only costs the memory across restarts; the selection itself stands.
    fn remember(&self, account: Option<String>) {
        let Some(store) = lock(&self.store).clone() else {
            return;
        };
        let _ = match account {
            Some(account) => store.meta_set(ACTIVE_ACCOUNT_META, &account),
            None => store.meta_remove(ACTIVE_ACCOUNT_META),
        };
    }

    fn request(&self, fields: Vec<String>) {
        if let Some(client) = lock(&self.client).as_ref() {
            client.request(fields);
        }
    }

    /// Files a proposal on the desk without publishing; callers publish once.
    fn push_proposal(
        &self,
        action: ProposalAction,
        reason: String,
        chat_id: Option<String>,
    ) -> TradingProposal {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = now_ms();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let proposal = TradingProposal {
            id: format!("p-{now}-{seq}"),
            action,
            reason: reason.trim().chars().take(280).collect(),
            chat_id,
            status: ProposalStatus::Pending,
            created_at: now,
            resolved_at: None,
            result: None,
        };
        {
            let mut list = lock(&self.proposals);
            list.insert(0, proposal.clone());
            trim_proposals(&mut list);
        }
        self.save_proposal(&proposal);
        proposal
    }

    fn load_rules(&self) -> Option<Vec<Rule>> {
        let store = lock(&self.store).clone()?;
        let conn = store.conn();
        let conn = conn.lock().unwrap_or_else(|poison| poison.into_inner());
        rules::list(&conn).ok()
    }

    fn save_rule(&self, rule: &Rule) {
        let Some(store) = lock(&self.store).clone() else {
            return;
        };
        let conn = store.conn();
        let conn = conn.lock().unwrap_or_else(|poison| poison.into_inner());
        let _ = rules::upsert(&conn, rule);
    }

    /// The stored requests, newest first, or `None` without a store.
    fn load_proposals(&self) -> Option<Vec<TradingProposal>> {
        let store = lock(&self.store).clone()?;
        let conn = store.conn();
        let conn = conn.lock().unwrap_or_else(|poison| poison.into_inner());
        proposals::list(&conn, proposals::KEEP).ok()
    }

    fn save_proposal(&self, proposal: &TradingProposal) {
        let Some(store) = lock(&self.store).clone() else {
            return;
        };
        let conn = store.conn();
        let conn = conn.lock().unwrap_or_else(|poison| poison.into_inner());
        let _ = proposals::upsert(&conn, proposal);
        let _ = proposals::prune(&conn, proposals::KEEP);
    }

    /// One rule pass. A fired rule files a proposal and publishes again; the
    /// guard makes that second pass return instead of looping.
    fn evaluate_rules(&self, snapshot: &Snapshot, trust: RuleTrust) {
        if self.evaluating.swap(true, Ordering::SeqCst) {
            return;
        }
        let now = now_ms();
        let mut changed: Vec<Rule> = Vec::new();
        {
            let mut list = lock(&self.rules);
            for rule in list.iter_mut() {
                if rule.status != RuleStatus::Watching {
                    continue;
                }
                if rule.expires_at.is_some_and(|at| at <= now) {
                    rule.status = RuleStatus::Expired;
                    rule.note = Some("Caducó sin cumplirse.".into());
                    changed.push(rule.clone());
                    continue;
                }
                // A half that isn't real (the demo fixture) is never read.
                let trusted = if rules::reads_broker(&rule.condition) {
                    trust.broker
                } else {
                    trust.pool
                };
                if !trusted {
                    continue;
                }
                if let Some(note) = rules::condition_met(&rule.condition, snapshot, now) {
                    rule.status = RuleStatus::Fired;
                    rule.fired_at = Some(now);
                    rule.note = Some(note);
                    changed.push(rule.clone());
                }
            }
        }
        if changed.is_empty() {
            self.evaluating.store(false, Ordering::SeqCst);
            return;
        }
        for rule in &changed {
            self.save_rule(rule);
        }
        let listener = lock(&self.rule_listener).clone();
        for rule in changed
            .iter()
            .filter(|rule| rule.status == RuleStatus::Fired)
        {
            if let Some(listener) = &listener {
                listener(rule);
            }
            if let Some(action) = rule.proposal.clone() {
                self.push_proposal(
                    action,
                    format!("Regla «{}»: {}", rule.name, rule.reason),
                    None,
                );
            }
        }
        self.publish();
        self.evaluating.store(false, Ordering::SeqCst);
    }

    /// Publishes what the links say: TWS (when live) and every MT5 account.
    /// With nothing connected the desk is empty, never a made-up one.
    fn publish(&self) {
        let (mut snapshot, live) = {
            let mut state = lock(&self.state);
            if state.live {
                state.sample_equity(now_ms());
            }
            (state.live_snapshot(), state.live)
        };
        let mt5 = lock(&self.mt5).clone();
        let mut mt5_read = false;
        if let Some(mt5) = mt5 {
            let vista = mt5.vista();
            mt5_read = !vista.accounts.is_empty();
            if !vista.links.is_empty() {
                snapshot.links.get_or_insert_with(Vec::new).extend(vista.links);
                snapshot
                    .programs
                    .get_or_insert(Programs { tws: ProgramState::Closed, mt5: None })
                    .mt5 = Some(vista.programa.unwrap_or(ProgramState::Closed));
            }
            if !vista.accounts.is_empty() {
                if snapshot.account.is_none() {
                    snapshot.account = vista.accounts.first().cloned();
                }
                snapshot.accounts.get_or_insert_with(Vec::new).extend(vista.accounts);
                snapshot.connected = true;
            }
            if !vista.positions.is_empty() {
                snapshot.positions.extend(vista.positions.iter().cloned());
                snapshot
                    .open_positions
                    .get_or_insert_with(Vec::new)
                    .extend(vista.positions);
            }
        }
        let proposals = lock(&self.proposals).clone();
        snapshot.proposals = (!proposals.is_empty()).then_some(proposals);
        let rules = lock(&self.rules).clone();
        snapshot.rules = (!rules.is_empty()).then_some(rules);
        crate::engine::publish_if_changed(&self.snapshot, snapshot.clone());
        // Rules see the desk as it now stands. A fired one files a proposal
        // and publishes again; the guard in `evaluate_rules` stops there.
        // This half has no pool; its account and positions are real once TWS
        // is live or an MT5 account reads.
        self.evaluate_rules(&snapshot, RuleTrust { broker: live || mt5_read, pool: false });
    }

    /// Folds one event in; every path that changes the state republishes.
    async fn pump(&self, mut events: tokio::sync::mpsc::UnboundedReceiver<IbEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                IbEvent::Connecting => {}
                IbEvent::ServerVersion { version, .. } => {
                    let mut state = lock(&self.state);
                    if state.live {
                        state.server_version = Some(version);
                    }
                }
                IbEvent::Connected { host, port } => {
                    {
                        let mut state = lock(&self.state);
                        if !state.live {
                            continue;
                        }
                        state.link = Some((host, port));
                    }
                    self.subscribe();
                    self.publish();
                }
                IbEvent::Disconnected { .. } => {
                    let mut state = lock(&self.state);
                    if !state.live {
                        continue;
                    }
                    // The client reconnects on its own; until then the rows it
                    // would describe are gone, and so is the "connected" flag.
                    state.ledger = IbkrLedger::default();
                    state.equity.clear();
                    state.last_sample = 0;
                    drop(state);
                    self.publish();
                }
                IbEvent::ClientIdInUse => {
                    // Drop the handle before the state lock: `disconnect` takes
                    // them in the opposite order, and this must not invert it.
                    let handle = lock(&self.client).take();
                    drop(handle);
                    let mut state = lock(&self.state);
                    if !state.live {
                        continue;
                    }
                    state.client_in_use = Some(FIRST_CLIENT_ID);
                    drop(state);
                    self.publish();
                }
                IbEvent::ProtocolError(message) => {
                    let mut state = lock(&self.state);
                    if !state.live {
                        continue;
                    }
                    state.push_log(LogLevel::Error, message);
                    drop(state);
                    self.publish();
                }
                IbEvent::Message(fields) => {
                    let message = inbound::decode(&fields);
                    let mut state = lock(&self.state);
                    if !state.live {
                        continue;
                    }
                    if let IbInbound::NextValidId { order_id } = &message {
                        // Never step back below an id already handed out.
                        let next = state.next_order_id.map_or(*order_id, |n| n.max(*order_id));
                        state.next_order_id = Some(next);
                    }
                    // No receiver is fine: nobody is waiting for an answer.
                    let _ = self.inbound.send(message.clone());
                    if !state.ledger.apply(&message, now_ms()) {
                        continue;
                    }
                    let discovered = match &message {
                        IbInbound::ManagedAccounts { accounts } => Some(accounts.clone()),
                        _ => None,
                    };
                    // The login is up and has named its accounts: a remembered
                    // account it does not manage is gone for good.
                    let stale = discovered.is_some()
                        && state
                            .ledger
                            .preferred()
                            .is_some_and(|account| !state.ledger.manages(account));
                    if stale {
                        state.ledger.prefer(None);
                    }
                    drop(state);
                    if stale {
                        self.remember(None);
                    }
                    if let Some(accounts) = discovered {
                        self.subscribe_accounts(&accounts);
                    }
                    self.publish();
                }
            }
        }
    }

    /// The read-only burst sent once the link is stable: what TWS holds and
    /// the streams that keep it current.
    fn subscribe(&self) {
        self.request(requests::req_managed_accounts());
        self.request(requests::req_positions());
        self.request(requests::req_account_updates(true, ""));
        self.request(requests::req_account_summary(
            SUMMARY_REQ_ID,
            "All",
            requests::ACCOUNT_SUMMARY_TAGS,
        ));
    }

    /// Once the accounts are known: the ledger for each of them (TWS subscribes
    /// per account) and one PnL stream each.
    ///
    /// The PnL request id is the account's index in discovery order because the
    /// answer carries no account: [`IbkrLedger`] resolves `req_id` back to it.
    fn subscribe_accounts(&self, accounts: &[String]) {
        for (index, account) in accounts.iter().enumerate() {
            self.request(requests::req_account_updates(true, account));
            self.request(requests::req_pnl(index as i64, account, ""));
        }
    }
}

impl State {
    /// Appends one live snapshot sample, at most once per [`EQUITY_SAMPLE_MS`].
    fn sample_equity(&mut self, now: i64) {
        if now - self.last_sample < EQUITY_SAMPLE_MS {
            return;
        }
        let Some(account) = self.ledger.account() else {
            return;
        };
        self.last_sample = now;
        self.equity.push(EquityPoint {
            ts: now,
            equity: account.equity,
            pnl_day: account.pnl_day,
        });
        if self.equity.len() > 1000 {
            self.equity.remove(0);
        }
    }

    fn push_log(&mut self, level: LogLevel, message: String) {
        self.logs.push(Log {
            ts: now_ms(),
            level,
            system: "motor".into(),
            message,
            message_key: None,
            message_params: None,
            event: None,
        });
        if self.logs.len() > MAX_LOGS {
            self.logs.remove(0);
        }
    }

    fn live_snapshot(&self) -> Snapshot {
        let positions = self.ledger.positions();
        let (host, port, up) = match &self.link {
            Some((host, port)) => (host.clone(), *port, self.ledger.connected()),
            None => ("127.0.0.1".to_string(), PAPER_PORT, false),
        };
        let link_id = format!("{host}:{port}");
        let with_link = |mut account: Account| {
            account.link = Some(link_id.clone());
            account
        };
        Snapshot {
            version: env!("CARGO_PKG_VERSION").into(),
            connected: self.ledger.connected(),
            client_in_use: self.client_in_use.map(i64::from),
            account: self.ledger.account().map(with_link),
            // Systems, tasks and trades belong to the bots' folders (lane A3+);
            // a live link does not invent them.
            systems: Vec::new(),
            positions,
            open_positions: Some(self.ledger.open_positions()),
            // Only the engine account's rows: a close is placed there.
            closable_positions: Some(self.ledger.closable_positions()),
            logs: self.logs.clone(),
            tasks: Vec::new(),
            rules: None,
            trades: None,
            equity: Some(self.equity.clone()),
            accounts: Some(self.ledger.accounts().into_iter().map(with_link).collect()),
            links: Some(vec![Link {
                id: link_id.clone(),
                host: Some(host),
                port: Some(port),
                label: Some("TWS paper".into()),
                broker: Some(Broker::Ibkr),
                mt5_login: None,
                mt5_server: None,
                connected: up,
            }]),
            systems_dir: None,
            programs: Some(Programs {
                tws: if up {
                    ProgramState::Running
                } else {
                    ProgramState::Closed
                },
                mt5: None,
            }),
            proposals: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use zeron_tws::codec::{Decoder, encode_message};

    const WAIT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_proposal_waits_for_the_person_and_records_the_outcome() {
        use zeron_proto::trading::{ProposalAction, ProposalStatus};
        let service = TradingService::new();
        let p = service.propose(
            ProposalAction::ClosePosition { symbol: "MNQZ6".into() },
            "  The person asked to close MNQ  ".into(),
            Some("chat-1".into()),
        );
        assert_eq!(p.status, ProposalStatus::Pending);
        assert_eq!(p.reason, "The person asked to close MNQ");
        // Published on the snapshot the desk and the agent read.
        let on_desk = service.snapshot().proposals.unwrap_or_default();
        assert_eq!(on_desk.len(), 1);
        assert_eq!(on_desk[0].id, p.id);

        assert_eq!(
            service.start_proposal(&p.id).unwrap(),
            ProposalAction::ClosePosition { symbol: "MNQZ6".into() }
        );
        // While it runs (still pending), a second approval — a double click —
        // is refused instead of running the action again.
        assert!(service.start_proposal(&p.id).is_err());
        service.finish_proposal(&p.id, ProposalStatus::Done, Some("Cerrada.".into()));
        let done = &service.proposals()[0];
        assert_eq!(done.status, ProposalStatus::Done);
        assert_eq!(done.result.as_deref(), Some("Cerrada."));
        assert!(done.resolved_at.is_some());
        // Answered once: a second approval is refused, nothing runs twice.
        assert!(service.start_proposal(&p.id).is_err());
        assert!(service.start_proposal("nope").is_err());
    }

    #[tokio::test]
    async fn the_desk_keeps_at_most_fifty_proposals_pending_first() {
        use zeron_proto::trading::{ProposalAction, ProposalStatus};
        let service = TradingService::new();
        let first = service.propose(ProposalAction::Panic, "keep me".into(), None);
        for i in 0..60 {
            let p = service.propose(ProposalAction::CloseAll, format!("{i}"), None);
            service.finish_proposal(&p.id, ProposalStatus::Rejected, None);
        }
        let list = service.proposals();
        assert_eq!(list.len(), MAX_PROPOSALS);
        assert!(list.iter().any(|p| p.id == first.id), "a pending one is never dropped first");
    }

    /// A rule watches, fires once, rings once and files its proposal; it never
    /// runs the action itself.
    #[tokio::test]
    async fn a_rule_fires_once_files_a_proposal_and_rings() {
        use zeron_proto::trading::{ProposalAction, RuleAddInput, RuleCondition, RuleStatus};
        let service = TradingService::new();
        let rang = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = rang.clone();
        service.set_rule_listener(Arc::new(move |rule| {
            seen.lock().unwrap().push(rule.name.clone());
        }));
        let rule = service
            .add_rule(RuleAddInput {
                name: "NQ parado".into(),
                condition: RuleCondition::SystemStopped {
                    system_id: "02_nq_tws".into(),
                },
                proposal: Some(ProposalAction::StopSystems {
                    system_ids: vec!["02_nq_tws".into()],
                }),
                reason: "pararlo si se cae".into(),
                expires_minutes: None,
            })
            .unwrap();

        // The publish that follows its creation reads the demo fixture,
        // whose stopped NQ is made up: nothing fires on it.
        let watching = service.rules().into_iter().find(|r| r.id == rule.id).unwrap();
        assert_eq!(watching.status, RuleStatus::Watching);
        assert!(service.proposals().is_empty());
        assert!(rang.lock().unwrap().is_empty());

        // Read as a real desk, the stopped NQ fires it: one banner, one
        // proposal, no action run.
        service.evaluate_rules(&crate::mock::current());
        let fired = service
            .rules()
            .into_iter()
            .find(|r| r.id == rule.id)
            .unwrap();
        assert_eq!(fired.status, RuleStatus::Fired);
        assert!(fired.note.as_deref().unwrap().contains("NQ TWS"));
        assert_eq!(rang.lock().unwrap().as_slice(), [rule.name.clone()]);
        let proposals = service.proposals();
        assert_eq!(proposals.len(), 1);
        assert!(proposals[0].reason.contains("NQ parado"), "{proposals:?}");

        // Another pass neither rings nor proposes again.
        service.evaluate_rules(&crate::mock::current());
        assert_eq!(rang.lock().unwrap().len(), 1);
        assert_eq!(service.proposals().len(), 1);
    }

    /// Without TWS the account and positions are the demo fixture: a rule
    /// they would satisfy (the fixture's +0,85 %) must not fire on them.
    #[tokio::test]
    async fn demo_figures_never_fire_a_broker_rule() {
        use zeron_proto::trading::{RuleAddInput, RuleCondition, RuleStatus};
        let service = TradingService::new();
        assert!(!service.live());
        let rule = service
            .add_rule(RuleAddInput {
                name: "La cuenta gana".into(),
                condition: RuleCondition::AccountPnlPct {
                    at_most: None,
                    at_least: Some(0.5),
                },
                proposal: Some(ProposalAction::CloseAll),
                reason: "asegurar".into(),
                expires_minutes: None,
            })
            .unwrap();
        // The publishes (creation, a later one) read the fixture: untouched.
        service.inner.publish();
        let still = service.rules().into_iter().find(|r| r.id == rule.id).unwrap();
        assert_eq!(still.status, RuleStatus::Watching);
        assert!(service.proposals().is_empty());
    }

    #[tokio::test]
    async fn an_expired_rule_stops_watching_without_a_proposal() {
        use zeron_proto::trading::{RuleAddInput, RuleCondition, RuleStatus};
        let service = TradingService::new();
        let rule = service
            .add_rule(RuleAddInput {
                name: "EURUSD parado".into(),
                condition: RuleCondition::SystemStopped {
                    system_id: "01_eurusd_mt5".into(),
                },
                proposal: None,
                reason: "avisar".into(),
                // Six milliseconds: enough to outlive the publish that follows.
                expires_minutes: Some(0.0001),
            })
            .unwrap();
        assert_eq!(
            service.rules().iter().find(|r| r.id == rule.id).unwrap().status,
            RuleStatus::Watching
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        service.evaluate_rules(&crate::mock::current());
        let expired = service
            .rules()
            .into_iter()
            .find(|r| r.id == rule.id)
            .unwrap();
        assert_eq!(expired.status, RuleStatus::Expired);
        assert!(expired.note.as_deref().unwrap().contains("Caducó"));
        assert!(service.proposals().is_empty());
    }

    #[tokio::test]
    async fn rules_outlast_a_restart_and_cancelling_stops_the_watch() {
        use zeron_proto::trading::{RuleAddInput, RuleCondition, RuleStatus};
        let store = Store::open_in_memory().unwrap();
        let service = TradingService::with_store(store.clone());
        let rule = service
            .add_rule(RuleAddInput {
                name: "Cuenta".into(),
                condition: RuleCondition::AccountPnlPct {
                    at_most: Some(-1.0),
                    at_least: None,
                },
                proposal: None,
                reason: "avisar si la cuenta pierde 1 %".into(),
                expires_minutes: Some(60.0),
            })
            .unwrap();
        // A fresh service on the same store loads it back.
        let restarted = TradingService::with_store(store.clone());
        assert_eq!(restarted.rules(), vec![rule.clone()]);

        let cancelled = restarted.cancel_rule(&rule.id).unwrap();
        assert_eq!(cancelled.status, RuleStatus::Cancelled);
        assert!(restarted.cancel_rule(&rule.id).is_err(), "already resolved");
        // The cancellation is persisted too.
        let after = TradingService::with_store(store);
        assert_eq!(after.rules()[0].status, RuleStatus::Cancelled);
    }

    /// A request the agent files rings once (the card waits in a chat the
    /// person may not have in front); the desk's own proposals stay quiet.
    #[tokio::test]
    async fn an_agent_request_rings_once_and_a_rule_s_proposal_stays_quiet() {
        use zeron_proto::trading::{ProposalAction, RuleAddInput, RuleCondition};
        let service = TradingService::new();
        let rung = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = rung.clone();
        service.set_proposal_listener(Arc::new(move |proposal| {
            seen.lock().unwrap().push(proposal.reason.clone());
        }));

        // No chat: the desk itself (a rule's proposal), not the agent.
        service.propose(ProposalAction::CloseAll, "sin chat".into(), None);
        assert!(rung.lock().unwrap().is_empty());

        service.propose(
            ProposalAction::CloseAll,
            "cierra todo".into(),
            Some("chat-1".into()),
        );
        assert_eq!(rung.lock().unwrap().as_slice(), ["cierra todo"]);

        // A fired rule files its proposal without ringing as an agent request:
        // the rule has its own banner.
        service
            .add_rule(RuleAddInput {
                name: "NQ parado".into(),
                condition: RuleCondition::SystemStopped {
                    system_id: "02_nq_tws".into(),
                },
                proposal: Some(ProposalAction::StopSystems {
                    system_ids: vec!["02_nq_tws".into()],
                }),
                reason: "pararlo si se cae".into(),
                expires_minutes: None,
            })
            .unwrap();
        service.evaluate_rules(&crate::mock::current());
        assert_eq!(rung.lock().unwrap().len(), 1);
        assert_eq!(service.proposals().len(), 3);
    }

    /// The audit outlives the engine: what was asked, what the person decided
    /// and the verdict all come back with the store.
    #[tokio::test]
    async fn requests_outlast_a_restart_with_their_outcome() {
        use zeron_proto::trading::{ProposalAction, ProposalStatus};
        let store = Store::open_in_memory().unwrap();
        let service = TradingService::with_store(store.clone());
        let pending = service.propose(
            ProposalAction::CloseAll,
            "cierra todo".into(),
            Some("chat-1".into()),
        );
        let done = service.propose(
            ProposalAction::Panic,
            "pánico".into(),
            Some("chat-1".into()),
        );
        service.finish_proposal(&done.id, ProposalStatus::Done, Some("todo cerrado".into()));

        let restarted = TradingService::with_store(store.clone());
        let back = restarted.proposals();
        assert_eq!(back.len(), 2);
        let restored_done = back.iter().find(|p| p.id == done.id).unwrap();
        assert_eq!(restored_done.status, ProposalStatus::Done);
        assert_eq!(restored_done.result.as_deref(), Some("todo cerrado"));
        assert_eq!(
            back.iter().find(|p| p.id == pending.id).unwrap().status,
            ProposalStatus::Pending
        );
        // The desk snapshot carries them again: the card and the audit survive.
        assert_eq!(restarted.snapshot().proposals.unwrap_or_default().len(), 2);

        // Answering a restored request is persisted too.
        restarted.finish_proposal(
            &pending.id,
            ProposalStatus::Rejected,
            Some("Rechazada en la mesa.".into()),
        );
        let third = TradingService::with_store(store);
        let rejected = third
            .proposals()
            .into_iter()
            .find(|p| p.id == pending.id)
            .unwrap();
        assert_eq!(rejected.status, ProposalStatus::Rejected);
        assert_eq!(rejected.resolved_at.is_some(), true);
    }

    /// A TWS that completes the handshake and hands the socket to the test.
    struct FakeTws {
        port: u16,        sockets: tokio::sync::mpsc::UnboundedReceiver<TcpStream>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn fake_tws() -> FakeTws {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, sockets) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                let mut buf = [0u8; 128];
                while bytes.len() < 8 {
                    let Ok(n) = socket.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                if bytes.len() >= 8 {
                    let announced = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                    let total = 8usize.saturating_add(announced as usize);
                    while bytes.len() < total && total <= 128 {
                        let Ok(n) = socket.read(&mut buf).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                }
                let _ = socket
                    .write_all(&encode_message(&["193", "20261118 00:00:00 CET"]))
                    .await;
                let _ = tx.send(socket);
            }
        });
        FakeTws {
            port,
            sockets,
            _task: task,
        }
    }

    fn fast(port: u16) -> IbClientConfig {
        IbClientConfig {
            port,
            client_id: 4242,
            reconnect_interval: Duration::from_millis(50),
            connection_watchdog_interval: Duration::from_secs(5),
            stability_window: Duration::from_millis(40),
            ..IbClientConfig::default()
        }
    }

    /// One complete framed message from the service side of the fake TWS. The
    /// decoder is shared across calls: one TCP read can carry two frames.
    async fn read_frame(socket: &mut TcpStream, decoder: &mut Decoder) -> Vec<String> {
        let mut buf = [0u8; 512];
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(fields) = decoder.next_message() {
                return fields;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let read = tokio::time::timeout(remaining, socket.read(&mut buf))
                .await
                .expect("a frame before the deadline");
            let n = read.expect("socket read");
            assert!(n > 0, "socket closed before a complete frame");
            decoder.push(&buf[..n]);
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the snapshot must catch up with the wire");
    }

    /// The oracle's frames for one MNQ position on account `A`.
    fn oracle_frames() -> Vec<Vec<&'static str>> {
        vec![
            vec!["15", "1", "A"],
            vec!["63", "1", "9001", "A", "AccountType", "Individual", "USD"],
            vec!["63", "1", "9001", "A", "NetLiquidation", "101234.56", "USD"],
            vec!["63", "1", "9001", "A", "TotalCashBalance", "50000", "USD"],
            vec!["64", "1", "9001"],
            vec![
                "61", "3", "A", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37000",
            ],
            vec!["62", "1"],
            vec![
                "7", "8", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37250", "74500", "37000", "500", "0", "A",
            ],
            vec!["94", "0", "125.50", "30.25", "95.25"],
        ]
    }

    #[tokio::test]
    async fn a_live_link_fills_the_snapshot_from_the_wire() {
        let mut tws = fake_tws().await;
        let service = TradingService::new();
        assert!(service.connect_to(fast(tws.port)).await.unwrap().is_none());

        let mut socket = tws.sockets.recv().await.unwrap();
        let mut decoder = Decoder::new();
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["71", "2", "4242", ""]
        );
        // The read-only burst, in the order the spec lists it.
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["17", "1"]
        );
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["61", "1"]
        );
        let updates = read_frame(&mut socket, &mut decoder).await;
        assert_eq!(updates[0], "6");
        assert_eq!(updates[2], "1");
        let summary = read_frame(&mut socket, &mut decoder).await;
        assert_eq!(summary[0], "62");
        assert_eq!(summary[2], "9001");

        for frame in oracle_frames() {
            socket.write_all(&encode_message(&frame)).await.unwrap();
        }
        // The PnL answer is the last frame; the account's day figure is the
        // proof every earlier one was folded in too.
        wait_until(|| {
            let snapshot = service.snapshot();
            snapshot.positions.len() == 1
                && snapshot.positions[0].current == Some(37_250.0)
                && snapshot
                    .account
                    .as_ref()
                    .and_then(|account| account.pnl_day)
                    == Some(125.5)
        })
        .await;

        let snapshot = service.snapshot();
        assert!(snapshot.connected);
        assert!(snapshot.client_in_use.is_none());
        let account = snapshot.account.expect("the summary names the account");
        assert_eq!(account.id, "A");
        assert_eq!(account.equity, Some(101_234.56));
        assert_eq!(account.pnl_day, Some(125.5));
        assert_eq!(snapshot.accounts.as_ref().map(Vec::len), Some(1));
        assert_eq!(snapshot.open_positions.as_ref().map(Vec::len), Some(1));
        // The one account is the engine's: its position is closable.
        assert_eq!(snapshot.closable_positions.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            snapshot.accounts.as_ref().and_then(|a| a[0].link.clone()),
            Some(format!("127.0.0.1:{}", tws.port))
        );
        assert_eq!(
            snapshot.links.as_ref().map(|links| links[0].id.clone()),
            Some(format!("127.0.0.1:{}", tws.port))
        );
        assert_eq!(
            snapshot.programs.map(|programs| programs.tws),
            Some(ProgramState::Running)
        );

        service.disconnect().await;
        assert!(!service.live());
        // Nothing connected: an empty desk, never a made-up one.
        let offline = service.snapshot();
        assert!(offline.account.is_none() && offline.positions.is_empty());
    }

    #[tokio::test]
    async fn the_live_port_is_refused_before_any_socket_opens() {
        let service = TradingService::new();
        let error = service
            .connect_to(IbClientConfig {
                port: LIVE_PORT,
                ..IbClientConfig::default()
            })
            .await
            .expect_err("live trading must be refused");
        assert!(error.to_string().contains("7496"), "{error}");
        assert!(!service.live());
    }

    #[tokio::test]
    async fn an_unreachable_tws_is_an_error_not_an_empty_dashboard() {
        // A port that was just free: nothing listens there.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let service = TradingService::new();
        assert!(service.connect_to(fast(port)).await.is_err());
        assert!(!service.live());
        // Nothing connected: an empty desk, never a made-up one.
        let offline = service.snapshot();
        assert!(offline.account.is_none() && offline.positions.is_empty());
    }

    #[tokio::test]
    async fn selecting_an_account_reads_that_account_s_rows() {
        let mut tws = fake_tws().await;
        let service = TradingService::new();
        service.connect_to(fast(tws.port)).await.unwrap();
        let mut socket = tws.sockets.recv().await.unwrap();
        let mut decoder = Decoder::new();
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["71", "2", "4242", ""]
        );

        let frames: Vec<Vec<&str>> = vec![
            vec!["15", "1", "A,B"],
            vec![
                "61", "3", "A", "83626", "EURUSD", "CASH", "", "", "", "", "IDEALPRO", "USD",
                "EURUSD", "", "1000", "1.05",
            ],
            vec![
                "61", "3", "B", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "-1", "37000",
            ],
        ];
        for frame in frames {
            socket.write_all(&encode_message(&frame)).await.unwrap();
        }
        wait_until(|| {
            service
                .snapshot()
                .open_positions
                .as_ref()
                .is_some_and(|rows| rows.len() == 2)
        })
        .await;
        assert_eq!(service.snapshot().positions[0].symbol, "EURUSD");

        assert_eq!(
            service.select("B").unwrap().map(|account| account.id),
            Some("B".to_string())
        );
        let snapshot = service.snapshot();
        assert_eq!(snapshot.positions[0].symbol, "MNQZ6");
        assert_eq!(snapshot.open_positions.as_ref().map(Vec::len), Some(2));
        assert!(service.select("Z").is_err());

        service.disconnect().await;
    }

    /// Writes the managed-accounts frame and one position per account.
    async fn two_accounts(socket: &mut TcpStream, accounts: &str) {
        let frames: Vec<Vec<&str>> = vec![
            vec!["15", "1", accounts],
            vec![
                "61", "3", "A", "83626", "EURUSD", "CASH", "", "", "", "", "IDEALPRO", "USD",
                "EURUSD", "", "1000", "1.05",
            ],
            vec![
                "61", "3", "B", "711712", "MNQZ6", "FUT", "20261218", "", "", "2", "GLOBEX", "USD",
                "MNQZ6", "MNQ", "1", "37000",
            ],
        ];
        for frame in frames {
            socket.write_all(&encode_message(&frame)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_active_account_survives_a_reconnect_and_scopes_what_is_closable() {
        let store = Store::open_in_memory().unwrap();
        let mut tws = fake_tws().await;
        let service = TradingService::with_store(store.clone());

        service.connect_to(fast(tws.port)).await.unwrap();
        let mut socket = tws.sockets.recv().await.unwrap();
        two_accounts(&mut socket, "A,B").await;
        wait_until(|| service.snapshot().open_positions.is_some_and(|rows| rows.len() == 2)).await;
        service.select("B").unwrap();
        assert_eq!(store.meta_get(ACTIVE_ACCOUNT_META).unwrap().as_deref(), Some("B"));

        // Closable is the engine account only; the sibling's row is a leftover.
        let snapshot = service.snapshot();
        let closable: Vec<_> = snapshot.closable_positions.unwrap().into_iter().map(|p| p.symbol).collect();
        assert_eq!(closable, vec!["MNQZ6"]);
        let leftovers: Vec<_> = service.leftover_positions().into_iter().map(|p| p.symbol).collect();
        assert_eq!(leftovers, vec!["EURUSD"]);
        service.disconnect().await;
        assert!(service.leftover_positions().is_empty());

        // A fresh service on the same store shows B as soon as TWS names it.
        let again = TradingService::with_store(store.clone());
        again.connect_to(fast(tws.port)).await.unwrap();
        let mut socket = tws.sockets.recv().await.unwrap();
        two_accounts(&mut socket, "A,B").await;
        wait_until(|| again.snapshot().open_positions.is_some_and(|rows| rows.len() == 2)).await;
        assert_eq!(again.snapshot().account.map(|a| a.id).as_deref(), Some("B"));
        assert_eq!(again.snapshot().positions[0].symbol, "MNQZ6");
        again.disconnect().await;
    }

    #[tokio::test]
    async fn a_remembered_account_the_login_no_longer_names_is_forgotten() {
        let store = Store::open_in_memory().unwrap();
        store.meta_set(ACTIVE_ACCOUNT_META, "GONE").unwrap();
        let mut tws = fake_tws().await;
        let service = TradingService::with_store(store.clone());
        assert_eq!(service.remembered_account().as_deref(), Some("GONE"));

        service.connect_to(fast(tws.port)).await.unwrap();
        let mut socket = tws.sockets.recv().await.unwrap();
        two_accounts(&mut socket, "A,B").await;
        wait_until(|| service.snapshot().open_positions.is_some_and(|rows| rows.len() == 2)).await;
        assert_eq!(service.snapshot().account.map(|a| a.id).as_deref(), Some("A"));
        assert_eq!(store.meta_get(ACTIVE_ACCOUNT_META).unwrap(), None);
        service.disconnect().await;
    }
}
