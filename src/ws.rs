//! Blocking WebSocket plumbing for account streams: bounded connects through the consumer's
//! [`Network`], read deadlines, heartbeat checks, route-change detection and jittered reconnect
//! pacing. Modelled on the DegenView application's market-feed helper so both behave alike; it
//! has no dependency on the application.

use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

use crate::net::{Network, Route};

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How often an open socket asks the network whether its route changed.
const ROUTE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Splits `ws(s)://host[:port]/path` into the pieces `connect` needs.
pub fn host_and_port(url: &str) -> Option<(String, u16)> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        (true, rest)
    } else {
        (false, url.strip_prefix("ws://")?)
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => {
            (host.trim_matches(['[', ']']).to_owned(), port.parse().ok()?)
        }
        _ => (authority.to_owned(), if secure { 443 } else { 80 }),
    };
    (!host.is_empty()).then_some((host, port))
}

/// Opens a WebSocket with every network step bounded, and `read_timeout` set on the stream so a
/// quiet socket returns to the caller instead of blocking.
///
/// The TCP connection comes from `network` (direct, or tunnelled through a proxy); TLS is then
/// negotiated end to end with the host in `url`. Also returns the route in force when connecting,
/// which [`Health::watch_route`] compares against later.
pub fn connect(
    url: &str,
    read_timeout: Duration,
    network: &dyn Network,
) -> Result<(Socket, Route), String> {
    let (host, port) = host_and_port(url).ok_or_else(|| "not a WebSocket address".to_owned())?;
    let route = network.route(url);
    let stream = network
        .dial(&host, port, CONNECT_TIMEOUT)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let (mut socket, _) =
        tungstenite::client_tls(url, stream).map_err(|error| error.to_string())?;
    set_timeouts(&mut socket, read_timeout).map_err(|error| error.to_string())?;
    Ok((socket, route))
}

fn set_timeouts(socket: &mut Socket, timeout: Duration) -> io::Result<()> {
    let stream = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => &mut stream.sock,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported TLS stream",
            ));
        }
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

/// Tracks whether a socket is alive: pings after a quiet spell, fails when nothing (not even a
/// pong) comes back, and when no *data* has arrived for too long.
pub struct Health {
    last_message: Instant,
    last_data: Instant,
    ping_sent: Option<Instant>,
    max_data_silence: Duration,
    ping_after: Duration,
    pong_timeout: Duration,
    route_watch: Option<RouteWatch>,
}

impl Health {
    pub fn new(max_data_silence: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_message: now,
            last_data: now,
            ping_sent: None,
            max_data_silence,
            ping_after: Duration::from_secs(15),
            pong_timeout: Duration::from_secs(10),
            route_watch: None,
        }
    }

    #[must_use]
    pub fn with_ping(mut self, ping_after: Duration, pong_timeout: Duration) -> Self {
        self.ping_after = ping_after;
        self.pong_timeout = pong_timeout;
        self
    }

    /// Fails the socket when the route `network` gives for `url` is no longer `route` (the one the
    /// socket connected over), checked at most once a second. The stream's reconnect logic then
    /// connects again over the new route.
    #[must_use]
    pub fn watch_route(mut self, network: Arc<dyn Network>, url: &str, route: Route) -> Self {
        self.route_watch = Some(RouteWatch::new(network, url, route, ROUTE_CHECK_INTERVAL));
        self
    }

    /// A message, or `None` when the read timed out and the socket is still believed healthy. An
    /// error means the socket is dead (closed, failed, or silent for too long).
    // `tungstenite::Error` is large, but it is the error type the socket API hands back.
    #[allow(clippy::result_large_err)]
    pub fn read(&mut self, socket: &mut Socket) -> tungstenite::Result<Option<Message>> {
        if self
            .route_watch
            .as_mut()
            .is_some_and(RouteWatch::has_changed)
        {
            return Err(tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the network route changed",
            )));
        }
        match socket.read() {
            Ok(Message::Pong(_)) => {
                self.last_message = Instant::now();
                self.ping_sent = None;
                Ok(None)
            }
            Ok(message) => {
                let now = Instant::now();
                self.last_message = now;
                self.last_data = now;
                self.ping_sent = None;
                Ok(Some(message))
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                self.on_timeout(socket)
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::result_large_err)]
    fn on_timeout(&mut self, socket: &mut Socket) -> tungstenite::Result<Option<Message>> {
        let now = Instant::now();
        if now.duration_since(self.last_data) >= self.max_data_silence
            || self
                .ping_sent
                .is_some_and(|sent| now.duration_since(sent) >= self.pong_timeout)
        {
            return Err(tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "websocket heartbeat timed out",
            )));
        }
        if self.ping_sent.is_none() && now.duration_since(self.last_message) >= self.ping_after {
            socket.send(Message::Ping(Vec::new().into()))?;
            self.ping_sent = Some(now);
        }
        Ok(None)
    }
}

/// Notices that the route a connection was made over is no longer the one in force.
struct RouteWatch {
    network: Arc<dyn Network>,
    url: String,
    route: Route,
    interval: Duration,
    last_check: Option<Instant>,
}

impl RouteWatch {
    fn new(network: Arc<dyn Network>, url: &str, route: Route, interval: Duration) -> Self {
        Self {
            network,
            url: url.to_owned(),
            route,
            interval,
            last_check: None,
        }
    }

    /// Whether the network now names another route. Asks at most once per interval; in between
    /// the answer is "no".
    fn has_changed(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_check
            .is_some_and(|last| now.duration_since(last) < self.interval)
        {
            return false;
        }
        self.last_check = Some(now);
        self.network.route(&self.url) != self.route
    }
}

/// Reconnect pacing: doubles from `initial` to `max` with 75-125% jitter, and wakes early when
/// asked to stop.
pub struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new() -> Self {
        Self::with(Duration::from_secs(1), Duration::from_secs(30))
    }

    pub fn with(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            next: initial,
        }
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    /// The delay the next wait will use, jitter applied.
    pub fn next_delay(&mut self, random: u32) -> Duration {
        let delay = jittered(self.next, random);
        self.next = (self.next * 2).min(self.max);
        delay
    }

    /// Sleeps one backoff step, in short slices so `stop` is noticed.
    pub fn wait(&mut self, stop: &AtomicBool) {
        let mut bytes = [0_u8; 4];
        let random = if getrandom::fill(&mut bytes).is_ok() {
            u32::from_le_bytes(bytes)
        } else {
            0
        };
        let delay = self.next_delay(random);
        let started = Instant::now();
        while !stop.load(Ordering::Acquire) {
            let remaining = delay.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

fn jittered(delay: Duration, random: u32) -> Duration {
    delay.mul_f64(f64::from(75 + random % 51) / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_host_and_port() {
        assert_eq!(
            host_and_port("wss://stream.example.com"),
            Some(("stream.example.com".to_owned(), 443))
        );
        assert_eq!(
            host_and_port("ws://127.0.0.1:8123/x"),
            Some(("127.0.0.1".to_owned(), 8123))
        );
        assert_eq!(host_and_port("ws://[::1]:9"), Some(("::1".to_owned(), 9)));
        assert_eq!(host_and_port("http://example.com"), None);
        assert_eq!(host_and_port("wss://"), None);
    }

    #[test]
    fn backoff_doubles_to_its_ceiling_with_bounded_jitter() {
        let mut backoff = Backoff::with(Duration::from_secs(1), Duration::from_secs(8));
        let mut centers = Vec::new();
        for _ in 0..6 {
            let low = backoff.next;
            let delay = backoff.next_delay(25);
            assert!(delay >= low.mul_f64(0.75) && delay <= low.mul_f64(1.25));
            centers.push(low.as_secs());
        }
        assert_eq!(centers, [1, 2, 4, 8, 8, 8]);
        backoff.reset();
        assert_eq!(backoff.next, Duration::from_secs(1));
    }

    #[test]
    fn a_waiting_backoff_returns_at_once_when_stopped() {
        let stop = AtomicBool::new(true);
        let started = Instant::now();
        Backoff::with(Duration::from_secs(30), Duration::from_secs(30)).wait(&stop);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    use crate::net::{Proxy, ProxyProtocol};
    use crate::testing::network::ScriptedNetwork;
    use std::net::TcpListener;

    /// A plain WebSocket server on loopback that accepts one client and then holds it open until
    /// the returned sender is dropped. Returns its `ws://` address.
    fn quiet_server() -> (String, std::sync::mpsc::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("ws://{}", listener.local_addr().unwrap());
        let (release, held) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _socket = tungstenite::accept(stream);
                let _ = held.recv();
            }
        });
        (address, release)
    }

    fn other_route() -> Route {
        Route::Proxy(Proxy::new(ProxyProtocol::Http, "proxy.example", 3128))
    }

    #[test]
    fn a_connection_is_dialled_through_the_network_and_reports_its_route() {
        let (url, _release) = quiet_server();
        let network = ScriptedNetwork::new(Route::Direct);
        let (_socket, route) = connect(&url, Duration::from_millis(50), &network).unwrap();
        assert_eq!(route, Route::Direct);
        let port = url.rsplit(':').next().unwrap().parse().unwrap();
        assert_eq!(network.dialled(), [("127.0.0.1".to_owned(), port)]);
    }

    #[test]
    fn a_failed_dial_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        drop(listener);
        let network = ScriptedNetwork::new(Route::Direct);
        assert!(connect(&url, Duration::from_millis(50), &network).is_err());
    }

    #[test]
    fn a_route_watch_notices_a_change_and_asks_only_once_per_interval() {
        let network = Arc::new(ScriptedNetwork::new(Route::Direct));
        let mut watch = RouteWatch::new(
            network.clone(),
            "wss://stream.example.com",
            Route::Direct,
            Duration::from_secs(3600),
        );
        assert!(!watch.has_changed());
        network.set_route(other_route());
        // Within the interval the network is not asked again.
        assert!(!watch.has_changed());
        assert_eq!(network.routes_asked(), 1);

        let mut eager = RouteWatch::new(
            network.clone(),
            "wss://stream.example.com",
            Route::Direct,
            Duration::ZERO,
        );
        assert!(eager.has_changed());
    }

    #[test]
    fn a_socket_whose_route_changed_fails_its_next_read() {
        let (url, _release) = quiet_server();
        let network = Arc::new(ScriptedNetwork::new(Route::Direct));
        let (mut socket, route) =
            connect(&url, Duration::from_millis(50), network.as_ref()).unwrap();
        let mut health =
            Health::new(Duration::from_secs(60)).watch_route(network.clone(), &url, route);

        // Same route: the read just times out.
        assert!(matches!(health.read(&mut socket), Ok(None)));
        // A watch checks at most once a second, so a fresh one sees the change on its first read.
        network.set_route(other_route());
        let mut health =
            Health::new(Duration::from_secs(60)).watch_route(network.clone(), &url, Route::Direct);
        let error = health.read(&mut socket).unwrap_err();
        assert!(
            matches!(&error, tungstenite::Error::Io(io) if io.kind() == io::ErrorKind::ConnectionAborted),
            "{error:?}"
        );
    }
}
