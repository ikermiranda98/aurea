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
//! * `closable_positions` stays empty because order entry is lane D3; the
//!   surface must not offer a close it cannot place;
//! * [`TradingService::disconnect`] goes back to the demo snapshot, so a dev
//!   machine without TWS keeps the dashboard it had.
//!
//! Live port 7496 is refused here, not only in the UI: this module is the last
//! place before a socket is opened.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{Mutex as AsyncMutex, watch};
use zeron_proto::trading::{
    Account, Broker, EquityPoint, Link, Log, LogLevel, ProgramState, Programs, Snapshot,
    TradingError,
};
use zeron_tws::client::{IbClient, IbClientConfig, IbEvent};
use zeron_tws::inbound::{self, IbInbound};
use zeron_tws::requests;

use crate::ibkr::IbkrLedger;
use crate::{SnapshotWatch, mock};

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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
}

impl Default for TradingService {
    fn default() -> Self {
        Self::new()
    }
}

impl TradingService {
    /// A service showing the demo snapshot, with no socket open.
    pub fn new() -> Self {
        let (snapshot, _rx) = watch::channel(mock::current());
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            snapshot,
            client: Mutex::new(None),
            connecting: AsyncMutex::new(()),
        });
        // The demo fixture ticks every 2 s; keep forwarding it while no live
        // link owns the snapshot. Without a runtime (unit tests, a CLI probe)
        // the initial snapshot is all there is, which is fine.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&inner);
            handle.spawn(async move {
                let mut demo = mock::watch();
                loop {
                    if demo.changed().await.is_err() {
                        break;
                    }
                    let Some(inner) = weak.upgrade() else { break };
                    let live = lock(&inner.state).live;
                    if !live {
                        inner.publish();
                    }
                }
            });
        }
        Self { inner }
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
        let (client, events) =
            IbClient::connect(config)
                .await
                .map_err(|error| TradingError::InvalidRequest {
                    message: format!(
                        "TWS no responde en {}:{}: {error}. Abre TWS y su API (paper).",
                        link.0, link.1
                    ),
                })?;
        {
            let mut state = lock(&self.inner.state);
            state.live = true;
            state.client_in_use = None;
            state.link = Some(link);
            state.ledger = IbkrLedger::default();
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
            return mock::account_select(account);
        }
        let mut state = lock(&self.inner.state);
        if !state.ledger.select(account) {
            return Err(TradingError::InvalidRequest {
                message: format!("Unknown account: {account}"),
            });
        }
        let account = state.ledger.account();
        drop(state);
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

    fn request(&self, fields: Vec<String>) {
        if let Some(client) = lock(&self.client).as_ref() {
            client.request(fields);
        }
    }

    /// Publishes what the state says: the demo fixture while no link is live.
    fn publish(&self) {
        let snapshot = {
            let mut state = lock(&self.state);
            if !state.live {
                mock::current()
            } else {
                state.sample_equity(now_ms());
                state.live_snapshot()
            }
        };
        self.snapshot.send_replace(snapshot);
    }

    /// Folds one event in; every path that changes the state republishes.
    async fn pump(&self, mut events: tokio::sync::mpsc::UnboundedReceiver<IbEvent>) {
        while let Some(event) = events.recv().await {
            match event {
                IbEvent::Connecting => {}
                IbEvent::ServerVersion { .. } => {}
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
                    if !state.ledger.apply(&message, now_ms()) {
                        continue;
                    }
                    let discovered = match &message {
                        IbInbound::ManagedAccounts { accounts } => Some(accounts.clone()),
                        _ => None,
                    };
                    drop(state);
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
            None => (String::new(), PAPER_PORT, false),
        };
        Snapshot {
            version: env!("CARGO_PKG_VERSION").into(),
            connected: self.ledger.connected(),
            client_in_use: self.client_in_use.map(i64::from),
            account: self.ledger.account(),
            // Systems, tasks and trades belong to the bots' folders (lane A3+);
            // a live link does not invent them.
            systems: Vec::new(),
            positions,
            open_positions: Some(self.ledger.open_positions()),
            // Order entry is D3: nothing can be closed from here yet, and the
            // surface must not pretend otherwise.
            closable_positions: Some(Vec::new()),
            logs: self.logs.clone(),
            tasks: Vec::new(),
            trades: None,
            equity: Some(self.equity.clone()),
            accounts: Some(self.ledger.accounts()),
            links: Some(vec![Link {
                id: format!("{host}:{port}"),
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

    /// A TWS that completes the handshake and hands the socket to the test.
    struct FakeTws {
        port: u16,
        sockets: tokio::sync::mpsc::UnboundedReceiver<TcpStream>,
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
        // Order entry is D3: a live position is shown, never offered for close.
        assert_eq!(snapshot.closable_positions.as_ref().map(Vec::len), Some(0));
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
        assert_eq!(service.snapshot().version, "0.1.0-mock");
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
        assert_eq!(service.snapshot().version, "0.1.0-mock");
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
}
