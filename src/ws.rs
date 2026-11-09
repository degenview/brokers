//! Blocking WebSocket plumbing for account streams: bounded connects, read deadlines, heartbeat
//! checks and jittered reconnect pacing. Modelled on the DegenView application's market-feed
//! helper so both behave alike; it has no dependency on the application.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

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
pub fn connect(url: &str, read_timeout: Duration) -> Result<Socket, String> {
    let (host, port) = host_and_port(url).ok_or_else(|| "not a WebSocket address".to_owned())?;
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| error.to_string())?;
    let mut last_error = None;
    for address in addresses.take(4) {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(CONNECT_TIMEOUT))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(CONNECT_TIMEOUT))
                    .map_err(|error| error.to_string())?;
                let (mut socket, _) =
                    tungstenite::client_tls(url, stream).map_err(|error| error.to_string())?;
                set_timeouts(&mut socket, read_timeout).map_err(|error| error.to_string())?;
                return Ok(socket);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.map_or_else(
        || "no address for the WebSocket host".to_owned(),
        |error| error.to_string(),
    ))
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
        }
    }

    #[must_use]
    pub fn with_ping(mut self, ping_after: Duration, pong_timeout: Duration) -> Self {
        self.ping_after = ping_after;
        self.pong_timeout = pong_timeout;
        self
    }

    /// A message, or `None` when the read timed out and the socket is still believed healthy. An
    /// error means the socket is dead (closed, failed, or silent for too long).
    // `tungstenite::Error` is large, but it is the error type the socket API hands back.
    #[allow(clippy::result_large_err)]
    pub fn read(&mut self, socket: &mut Socket) -> tungstenite::Result<Option<Message>> {
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
}
