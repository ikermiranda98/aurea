//! Tokio client for the Interactive Brokers TWS API socket (lane D1).
//!
//! The link keeps a single owner of the socket in a manager task. The public
//! handle only flips atomic flags and sends a shutdown signal, which makes every
//! method safe to call from any task. Reconnection invariants:
//!
//! * a socket drop that was not requested and was not a 326 always schedules a
//!   reconnect after [`IbClientConfig::reconnect_interval`];
//! * [`IbEvent::ClientIdInUse`] is terminal — retrying a taken client id would
//!   be the silent connect-and-drop loop the spec forbids;
//! * [`IbEvent::Connected`] is only emitted once the socket has survived
//!   [`IbClientConfig::stability_window`], so a fast 326 never reads as a link
//!   that came up and was lost;
//! * [`IbClient::disconnect`] is deliberate: it emits
//!   `Disconnected { requested: true }` and stops the manager for good.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::codec::{DecodeError, Decoder};

/// Connection settings for [`IbClient::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IbClientConfig {
    /// TWS host. Defaults to the loopback API port's usual address.
    pub host: String,
    /// TWS API port: 7497 paper, 7496 live.
    pub port: u16,
    /// API client id. The trading layer resolves and persists the real value
    /// (1000–9999); this is the first valid id, used when nothing is supplied.
    pub client_id: i32,
    /// Delay between reconnect attempts after an unexpected socket loss.
    pub reconnect_interval: Duration,
    /// Close and rebuild the socket when no byte arrives for this long.
    pub connection_watchdog_interval: Duration,
    /// A link is "connected" only after the socket survives this window.
    pub stability_window: Duration,
}

impl Default for IbClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 7497,
            client_id: 1000,
            reconnect_interval: Duration::from_secs(5),
            connection_watchdog_interval: Duration::from_secs(15),
            stability_window: Duration::from_millis(1500),
        }
    }
}

/// Whether the link has passed its stability window and is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No established link: connecting, inside the stability window, or down.
    Disconnected,
    /// The socket survived the stability window and no event has torn it down.
    Connected,
}

/// Everything the manager reports to its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IbEvent {
    /// A TCP attempt is starting (also emitted before each reconnect).
    Connecting,
    /// The link survived the stability window at this address.
    Connected {
        host: String,
        port: u16,
    },
    /// TWS returned error 326 for `reqId = -1`: the client id is taken.
    /// No reconnect will be attempted.
    ClientIdInUse,
    /// The server-version frame that opens every session, answered with
    /// `START_API`. `version` is TWS's server version (193 in recent builds).
    ServerVersion {
        version: i32,
        time: String,
    },
    /// The socket went away. `requested` distinguishes a user disconnect from
    /// a drop or a watchdog expiry; a 326 emits neither variant.
    Disconnected {
        requested: bool,
    },
    /// One complete TWS message, already split into fields.
    Message(Vec<String>),
    /// A malformed frame was seen; the text is user-facing Spanish.
    ProtocolError(String),
}

/// Handle to a running TWS link. Cloning shares the same state and shutdown.
#[derive(Debug, Clone)]
pub struct IbClient {
    state: Arc<AtomicBool>,
    disconnect_requested: Arc<AtomicBool>,
    control_tx: mpsc::UnboundedSender<LinkCommand>,
}

/// What the owner asks of the link task between reads.
#[derive(Debug)]
enum LinkCommand {
    /// Write these fields as one framed message (see [`crate::requests`]).
    Send(Vec<String>),
    /// Stop for good: the task reports `Disconnected { requested: true }`.
    Disconnect,
}

impl IbClient {
    /// Opens the first TCP connection, writes the TWS handshake, starts the
    /// manager task and returns the handle plus its event stream.
    ///
    /// The raw opening of a v100+ session (`API\0` + framed version range) is
    /// written before the manager starts; the framed `START_API` with the client
    /// id follows once TWS sends its server-version frame.
    ///
    /// An error here means the *first* socket could not be opened or written;
    /// later reconnect failures are reported through [`IbEvent::Connecting`]
    /// and retried silently until [`IbClient::disconnect`] is called.
    pub async fn connect(
        config: IbClientConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<IbEvent>), std::io::Error> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let state = Arc::new(AtomicBool::new(false));
        let disconnect_requested = Arc::new(AtomicBool::new(false));

        let _ = event_tx.send(IbEvent::Connecting);
        let stream = TcpStream::connect((config.host.as_str(), config.port)).await?;
        let mut stream = stream;
        stream.write_all(&handshake()).await?;

        tokio::spawn(run(
            stream,
            config,
            event_tx,
            control_rx,
            Arc::clone(&state),
            Arc::clone(&disconnect_requested),
        ));

        Ok((
            Self {
                state,
                disconnect_requested,
                control_tx,
            },
            event_rx,
        ))
    }

    /// Deliberate shutdown: no reconnect after this call. Repeated calls are
    /// no-ops; the manager emits exactly one `Disconnected { requested: true }`.
    pub async fn disconnect(&self) {
        if self.disconnect_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.store(false, Ordering::Release);
        self.control_tx.send(LinkCommand::Disconnect).ok();
    }

    /// Queues one message for the link (see [`crate::requests`]).
    ///
    /// Returns `false` when the task is gone (the socket died for good, or the
    /// client id was refused). A message sent before TWS reports its server
    /// version is dropped on purpose: no request is valid before `START_API`,
    /// and the owner re-subscribes on every [`IbEvent::Connected`] anyway.
    pub fn request(&self, fields: Vec<String>) -> bool {
        self.control_tx.send(LinkCommand::Send(fields)).is_ok()
    }

    /// Current link state as observed by this handle.
    pub fn state(&self) -> ConnectionState {
        if self.state.load(Ordering::Acquire) {
            ConnectionState::Connected
        } else {
            ConnectionState::Disconnected
        }
    }

    /// Shorthand for `state() == ConnectionState::Connected`.
    pub fn is_connected(&self) -> bool {
        self.state.load(Ordering::Acquire)
    }
}

/// Minimum protocol version of the v100+ (length-prefixed) API.
const MIN_VERSION_V100: u16 = 100;
/// Highest server version this client understands, as negotiated by
/// `@stoqey/ib@1.6.13` (`CME_TAGGING_FIELDS_IN_OPEN_ORDER`). TWS answers with
/// its own version, which is what the decoder must target from then on.
const MAX_SERVER_VERSION: u16 = 193;

/// The raw opening of a v100+ session: `API\0`, a big-endian `u32` with the
/// version-string length, and the string itself (`v100..193`). The client id is
/// **not** part of this header; it travels in the framed `START_API` message
/// once TWS reports its server version. This mirrors `Socket.onConnect` and
/// `Socket.send`'s `API\0` special case in `@stoqey/ib@1.6.13`.
fn handshake() -> Vec<u8> {
    let versions = format!("v{MIN_VERSION_V100}..{MAX_SERVER_VERSION}");
    let mut frame = Vec::with_capacity(4 + 4 + versions.len());
    frame.extend_from_slice(b"API\0");
    frame.extend_from_slice(&(versions.len() as u32).to_be_bytes());
    frame.extend_from_slice(versions.as_bytes());
    frame
}

/// The first message every IB session sends after the server-version frame:
/// `START_API` (71), version 2, the client id and empty optional capabilities.
fn start_api(client_id: i32) -> Vec<u8> {
    let client = client_id.to_string();
    crate::codec::encode_message(&["71", "2", client.as_str(), ""])
}


/// True for TWS's structured error message `["4", version, "-1", "326", ...]`.
fn is_client_id_in_use(fields: &[String]) -> bool {
    fields.len() >= 4
        && fields[0] == "4"
        && fields[2] == "-1"
        && fields[3] == "326"
}

/// How the most recent socket stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    /// The peer closed the socket or the watchdog fired: reconnect later.
    Dropped,
    /// The owner asked for shutdown.
    Requested,
    /// Error 326 arrived: stop, the client id is taken.
    IdInUse,
    /// The owner dropped the handle: stop without a further reconnect.
    Stop,
}

/// Emits a protocol warning. Returns true when the frame desynchronised the
/// stream (an impossible length) and the session must be recycled.
fn report_decode_error(error: DecodeError, events: &mpsc::UnboundedSender<IbEvent>) -> bool {
    let fatal = matches!(&error, DecodeError::FrameTooLarge { .. });
    let _ = events.send(IbEvent::ProtocolError(format!(
        "Trama TWS inválida: {error}"
    )));
    fatal
}

/// Drains messages from one socket until it ends or is stopped.
async fn session(
    stream: &mut TcpStream,
    config: &IbClientConfig,
    events: &mpsc::UnboundedSender<IbEvent>,
    control_rx: &mut mpsc::UnboundedReceiver<LinkCommand>,
    state: &Arc<AtomicBool>,
    disconnect_requested: &Arc<AtomicBool>,
) -> SessionOutcome {
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 4096];
    let stable_at = tokio::time::Instant::now() + config.stability_window;
    let mut stable = false;
    let mut started = false;
    let mut watchdog_at = tokio::time::Instant::now() + config.connection_watchdog_interval;

    loop {
        tokio::select! {
            // Owner signals first, so a deliberate disconnect always wins over
            // a concurrently readable socket or an expiring timer.
            biased;

            control = control_rx.recv() => {
                match control {
                    Some(LinkCommand::Send(fields)) => {
                        // A queued request must not outlive a shutdown order.
                        if disconnect_requested.load(Ordering::Acquire) {
                            return SessionOutcome::Requested;
                        }
                        // Before the server-version frame answered with
                        // START_API no request is valid; the owner resends
                        // what it needs on the next `Connected`.
                        if !started {
                            continue;
                        }
                        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                        if stream
                            .write_all(&crate::codec::encode_message(&refs))
                            .await
                            .is_err()
                        {
                            return SessionOutcome::Dropped;
                        }
                        // A write is activity enough; TWS may stay silent
                        // while nothing it watches changes.
                        watchdog_at = tokio::time::Instant::now()
                            + config.connection_watchdog_interval;
                    }
                    Some(LinkCommand::Disconnect)
                        if disconnect_requested.load(Ordering::Acquire) =>
                    {
                        return SessionOutcome::Requested;
                    }
                    // `None` means every handle was dropped: stop for good.
                    Some(LinkCommand::Disconnect) | None => return SessionOutcome::Stop,
                }
            }

            read = stream.read(&mut buf) => {
                match read {
                    Ok(0) => return SessionOutcome::Dropped,
                    Ok(n) => {
                        decoder.push(&buf[..n]);
                        watchdog_at = tokio::time::Instant::now()
                            + config.connection_watchdog_interval;

                        while let Some(fields) = decoder.next_message() {
                            if let Some(error) = decoder.take_error() {
                                if report_decode_error(error, events) {
                                    return SessionOutcome::Dropped;
                                }
                            }
                            // Checked before the stability timer can fire: a
                            // 326 at ~100 ms must never read as a good link.
                            if is_client_id_in_use(&fields) {
                                return SessionOutcome::IdInUse;
                            }
                            // The first frame of every IB session is the server
                            // version and connection time, not a message id.
                            // Answer it with START_API before forwarding anything
                            // else; until then no API request is valid.
                            if !started {
                                if let Some(version) =
                                    fields.first().and_then(|field| field.parse::<i32>().ok())
                                {
                                    if stream
                                        .write_all(&start_api(config.client_id))
                                        .await
                                        .is_err()
                                    {
                                        return SessionOutcome::Dropped;
                                    }
                                    started = true;
                                    let _ = events.send(IbEvent::ServerVersion {
                                        version,
                                        time: fields.get(1).cloned().unwrap_or_default(),
                                    });
                                    continue;
                                }
                            }
                            let _ = events.send(IbEvent::Message(fields));
                        }
                        if let Some(error) = decoder.take_error() {
                            if report_decode_error(error, events) {
                                return SessionOutcome::Dropped;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(?error, "TWS socket read failed");
                        return SessionOutcome::Dropped;
                    }
                }
            }

            _ = tokio::time::sleep_until(stable_at), if !stable => {
                stable = true;
                state.store(true, Ordering::Release);
                let _ = events.send(IbEvent::Connected {
                    host: config.host.clone(),
                    port: config.port,
                });
                tracing::debug!(host = %config.host, port = config.port,
                    "TWS link survived the stability window");
            }

            _ = tokio::time::sleep_until(watchdog_at) => {
                tracing::debug!(interval = ?config.connection_watchdog_interval,
                    "TWS watchdog fired: no bytes from the broker");
                return SessionOutcome::Dropped;
            }
        }
    }
}

/// Owns the socket for the lifetime of the link and drives reconnection.
async fn run(
    mut stream: TcpStream,
    config: IbClientConfig,
    events: mpsc::UnboundedSender<IbEvent>,
    mut control_rx: mpsc::UnboundedReceiver<LinkCommand>,
    state: Arc<AtomicBool>,
    disconnect_requested: Arc<AtomicBool>,
) {
    loop {
        let outcome = session(
            &mut stream,
            &config,
            &events,
            &mut control_rx,
            &state,
            &disconnect_requested,
        )
        .await;
        state.store(false, Ordering::Release);

        match outcome {
            SessionOutcome::Requested => {
                let _ = events.send(IbEvent::Disconnected { requested: true });
                return;
            }
            SessionOutcome::IdInUse => {
                let _ = events.send(IbEvent::ClientIdInUse);
                return; // terminal: never spin on a rejected client id
            }
            SessionOutcome::Stop => {
                let _ = events.send(IbEvent::Disconnected { requested: false });
                return;
            }
            SessionOutcome::Dropped => {
                let _ = events.send(IbEvent::Disconnected { requested: false });
            }
        }

        let interrupted = tokio::select! {
            _ = tokio::time::sleep(config.reconnect_interval) => false,
            command = control_rx.recv() => match command {
                // A request for a link that is not up yet: the owner asks
                // again on the next `Connected`, so it is dropped here.
                Some(LinkCommand::Send(_)) => false,
                Some(LinkCommand::Disconnect) | None => true,
            },
        };
        if interrupted || disconnect_requested.load(Ordering::Acquire) {
            let _ = events.send(IbEvent::Disconnected { requested: true });
            return;
        }

        // Reconnect until a socket with a written v100 header is available.
        // Every attempt is announced; a failed one waits another interval
        // instead of re-entering `session` on the dead socket.
        loop {
            let _ = events.send(IbEvent::Connecting);
            match TcpStream::connect((config.host.as_str(), config.port)).await {
                Ok(mut new_stream) => match new_stream.write_all(&handshake()).await {
                    Ok(()) => {
                        stream = new_stream;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(?error, "TWS handshake write failed during reconnect");
                    }
                },
                Err(error) => {
                    tracing::debug!(?error, "TWS reconnect attempt failed");
                }
            }
            let interrupted = tokio::select! {
                _ = tokio::time::sleep(config.reconnect_interval) => false,
                command = control_rx.recv() => match command {
                    Some(LinkCommand::Send(_)) => false,
                    Some(LinkCommand::Disconnect) | None => true,
                },
            };
            if interrupted || disconnect_requested.load(Ordering::Acquire) {
                let _ = events.send(IbEvent::Disconnected { requested: true });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_message;
    use tokio::net::TcpListener;

    const WAIT: Duration = Duration::from_secs(2);

    /// Fake TWS: answers every handshake with the server-version frame and
    /// exposes the raw handshake bytes and each accepted socket to the test.
    struct FakeTws {
        port: u16,
        handshakes: mpsc::UnboundedReceiver<Vec<u8>>,
        sockets: mpsc::UnboundedReceiver<TcpStream>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn fake_tws() -> FakeTws {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hs_tx, handshakes) = mpsc::unbounded_channel();
        let (sock_tx, sockets) = mpsc::unbounded_channel();

        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                let mut buf = [0u8; 128];
                // "API\0" + big-endian u32 body length + version string.
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
                    if bytes.len() > total {
                        bytes.truncate(total);
                    }
                }
                let _ = hs_tx.send(bytes);
                let _ = socket
                    .write_all(&encode_message(&["193", "20261105 03:55:06 CET"]))
                    .await;
                let _ = sock_tx.send(socket);
            }
        });

        FakeTws {
            port,
            handshakes,
            sockets,
            _task: task,
        }
    }

    fn config(port: u16) -> IbClientConfig {
        IbClientConfig {
            port,
            client_id: 4242,
            reconnect_interval: Duration::from_millis(50),
            connection_watchdog_interval: Duration::from_secs(5),
            stability_window: Duration::from_millis(40),
            ..IbClientConfig::default()
        }
    }

    /// Collects events until `stop` matches (inclusive) or the limit elapses.
    async fn collect_until(
        events: &mut mpsc::UnboundedReceiver<IbEvent>,
        stop: impl Fn(&IbEvent) -> bool,
        limit: Duration,
    ) -> Vec<IbEvent> {
        let mut seen = Vec::new();
        let _ = tokio::time::timeout(limit, async {
            while let Some(event) = events.recv().await {
                let done = stop(&event);
                seen.push(event);
                if done {
                    break;
                }
            }
        })
        .await;
        seen
    }

    async fn no_event_for(events: &mut mpsc::UnboundedReceiver<IbEvent>, limit: Duration) -> bool {
        matches!(
            tokio::time::timeout(limit, events.recv()).await,
            Err(_) | Ok(None)
        )
    }

    /// Reads one complete framed message from the client side of a fake TWS.
    /// The decoder is shared across calls: one TCP read can carry two frames.
    async fn read_frame(socket: &mut TcpStream, decoder: &mut Decoder) -> Vec<String> {
        let mut buf = [0u8; 256];
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

    /// Waits for `wanted` distinct Connected events, ignoring other traffic.
    async fn collect_n_connects(
        events: &mut mpsc::UnboundedReceiver<IbEvent>,
        wanted: usize,
        limit: Duration,
    ) -> usize {
        let deadline = tokio::time::Instant::now() + limit;
        let mut connects = 0;
        while connects < wanted {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, events.recv())
                .await
                .ok()
                .flatten()
                .expect("another Connected before the deadline");
            if matches!(event, IbEvent::Connected { .. }) {
                connects += 1;
            }
        }
        connects
    }

    #[tokio::test]
    async fn requests_are_framed_on_the_link_after_start_api() {
        let mut tws = fake_tws().await;
        let (client, mut events) = IbClient::connect(config(tws.port)).await.unwrap();
        let mut socket = tws.sockets.recv().await.unwrap();
        let mut decoder = Decoder::new();
        // START_API first, exactly once, before any owner request.
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["71", "2", "4242", ""]
        );
        collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Connected { .. }),
            WAIT,
        )
        .await;

        // Each request is one length-prefixed message, in order.
        assert!(client.request(crate::requests::req_managed_accounts()));
        assert!(client.request(crate::requests::req_positions()));
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["17", "1"]
        );
        assert_eq!(
            read_frame(&mut socket, &mut decoder).await,
            vec!["61", "1"]
        );
        client.disconnect().await;
    }

    #[tokio::test]
    async fn handshake_is_written_and_connected_comes_after_the_window() {
        let mut tws = fake_tws().await;
        let (client, mut events) = IbClient::connect(config(tws.port)).await.unwrap();

        let handshake = tokio::time::timeout(WAIT, tws.handshakes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&handshake[..4], &b"API\0"[..]);
        assert_eq!(
            u32::from_be_bytes([handshake[4], handshake[5], handshake[6], handshake[7]]),
            "v100..193".len() as u32
        );
        assert_eq!(&handshake[8..], b"v100..193");

        let mut socket = tws.sockets.recv().await.unwrap();
        let mut decoder = Decoder::new();
        let start = read_frame(&mut socket, &mut decoder).await;
        assert_eq!(start, vec!["71", "2", "4242", ""]);

        let seen = collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Connected { .. }),
            WAIT,
        )
        .await;
        assert_eq!(seen.first(), Some(&IbEvent::Connecting));
        assert_eq!(
            seen.iter().find(|event| matches!(event, IbEvent::ServerVersion { .. })),
            Some(&IbEvent::ServerVersion {
                version: 193,
                time: "20261105 03:55:06 CET".into(),
            })
        );
        assert_eq!(
            seen.last(),
            Some(&IbEvent::Connected {
                host: "127.0.0.1".into(),
                port: tws.port,
            })
        );
        assert_eq!(client.state(), ConnectionState::Connected);
        assert!(client.is_connected());

        client.disconnect().await;
    }

    #[tokio::test]
    async fn client_id_in_use_is_terminal_without_connected_or_retry() {
        let mut tws = fake_tws().await;
        let mut cfg = config(tws.port);
        cfg.stability_window = Duration::from_millis(500);
        let (client, mut events) = IbClient::connect(cfg).await.unwrap();

        let mut socket = tws.sockets.recv().await.unwrap();
        let mut decoder = Decoder::new();
        let start = read_frame(&mut socket, &mut decoder).await;
        assert_eq!(start, vec!["71", "2", "4242", ""]);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = socket
                .write_all(&encode_message(&[
                    "4",
                    "2",
                    "-1",
                    "326",
                    "Unable to connect as the client id is already in use. Retry with a unique client id.",
                    "",
                ]))
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(socket);
        });

        let seen = collect_until(&mut events, |event| *event == IbEvent::ClientIdInUse, WAIT).await;
        assert!(
            seen.iter().any(|event| *event == IbEvent::ClientIdInUse),
            "326 was not reported: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| matches!(event, IbEvent::Connected { .. })),
            "a rejected id must never be reported as connected: {seen:?}"
        );
        assert!(!client.is_connected());
        assert!(
            no_event_for(&mut events, Duration::from_millis(200)).await,
            "a 326 must not reconnect"
        );
    }

    #[tokio::test]
    async fn unexpected_drop_reconnects_and_reports_a_second_connected() {
        let mut tws = fake_tws().await;
        let (client, mut events) = IbClient::connect(config(tws.port)).await.unwrap();

        let first = collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Connected { .. }),
            WAIT,
        )
        .await;
        assert!(matches!(first.last(), Some(IbEvent::Connected { .. })));
        let socket = tws.sockets.recv().await.unwrap();
        drop(socket); // the broker vanished after the link was established

        let seen = collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Connected { .. }),
            WAIT,
        )
        .await;
        assert_eq!(
            seen.iter().filter(|event| matches!(event, IbEvent::Connected { .. })).count(),
            1,
            "expected exactly one further Connected: {seen:?}"
        );
        assert!(
            seen.iter().any(|event| *event == IbEvent::Connecting),
            "a reconnect must announce Connecting: {seen:?}"
        );
        assert!(client.is_connected());
        client.disconnect().await;
    }

    #[tokio::test]
    async fn deliberate_disconnect_reports_requested_and_never_reconnects() {
        let tws = fake_tws().await;
        let (client, mut events) = IbClient::connect(config(tws.port)).await.unwrap();
        let seen = collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Connected { .. }),
            WAIT,
        )
        .await;
        assert_eq!(seen.first(), Some(&IbEvent::Connecting));
        assert!(matches!(seen.last(), Some(IbEvent::Connected { .. })));

        client.disconnect().await;
        client.disconnect().await; // idempotent

        let seen = collect_until(
            &mut events,
            |event| matches!(event, IbEvent::Disconnected { requested: true }),
            WAIT,
        )
        .await;
        assert_eq!(
            seen.last(),
            Some(&IbEvent::Disconnected { requested: true }),
            "disconnect must be reported once: {seen:?}"
        );
        assert_eq!(client.state(), ConnectionState::Disconnected);
        assert!(
            no_event_for(&mut events, Duration::from_millis(200)).await,
            "no reconnect after a deliberate disconnect"
        );
    }

    #[tokio::test]
    async fn silent_server_trips_the_watchdog_and_reconnects() {
        let tws = fake_tws().await;
        let mut cfg = config(tws.port);
        cfg.connection_watchdog_interval = Duration::from_millis(100);
        cfg.stability_window = Duration::from_millis(20);
        let (client, mut events) = IbClient::connect(cfg).await.unwrap();

        let connects = collect_n_connects(&mut events, 2, WAIT).await;
        assert!(connects >= 2, "the watchdog never recycled the socket");
        assert!(client.is_connected());
        client.disconnect().await;
    }
}
