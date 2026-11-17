//! How this crate reaches the network: a small port the consumer fills in.
//!
//! The crate knows *how* to connect through a [`Route`] (a direct connection, or a proxy); it
//! knows nothing about *which* route to use. That decision (settings, the operating system's proxy
//! configuration, bypass lists, stored proxy credentials) belongs to the application, which hands
//! the crate a [`Network`] through [`Env::with_network`](crate::Env::with_network).
//!
//! A proxy never changes the target: a request still goes to the broker's own host, and TLS is
//! negotiated end to end with that host, so a proxy sees only the host name and encrypted bytes.
//!
//! The default network is [`Direct`]. Nothing here reads `HTTP_PROXY`, `HTTPS_PROXY` or
//! `ALL_PROXY`: an environment variable never reroutes a signed request on its own.

use std::fmt;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How many resolved addresses a direct connection tries before giving up.
const DIRECT_ADDRESS_ATTEMPTS: usize = 4;

/// The way to reach a host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// Connect to the host itself.
    Direct,
    /// Connect through a proxy.
    Proxy(Proxy),
}

/// What a proxy speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyProtocol {
    /// An HTTP proxy, reached over plain TCP, that tunnels with `CONNECT`.
    Http,
    /// An HTTP proxy reached over TLS that tunnels with `CONNECT`.
    Https,
    /// SOCKS4, with the 4a extension so the proxy resolves the target host name (a proxy that
    /// speaks only plain SOCKS4 cannot be used for a host name).
    Socks4,
    /// SOCKS5 with the target host name resolved by the proxy, so no lookup is made locally.
    Socks5,
}

/// The user name and password a proxy asks for. `Debug` never shows the password.
#[derive(Clone, Eq, PartialEq)]
pub struct ProxyCredentials {
    username: String,
    password: String,
}

impl ProxyCredentials {
    /// Credentials for a proxy. The password may be empty.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    /// The user name.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The password.
    pub fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A proxy to connect through.
#[derive(Clone, Eq, PartialEq)]
pub struct Proxy {
    protocol: ProxyProtocol,
    host: String,
    port: u16,
    credentials: Option<ProxyCredentials>,
}

impl Proxy {
    /// A proxy at `host:port`. `host` is a bare name or address, without brackets around an IPv6
    /// literal.
    pub fn new(protocol: ProxyProtocol, host: impl Into<String>, port: u16) -> Self {
        Self {
            protocol,
            host: host.into(),
            port,
            credentials: None,
        }
    }

    /// The same proxy, with credentials to present to it.
    #[must_use]
    pub fn with_credentials(mut self, credentials: ProxyCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// What the proxy speaks.
    pub fn protocol(&self) -> ProxyProtocol {
        self.protocol
    }

    /// The proxy's host, without brackets.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The proxy's port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The credentials to present, if the proxy asks for any.
    pub fn credentials(&self) -> Option<&ProxyCredentials> {
        self.credentials.as_ref()
    }
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// The port through which every connection this crate makes is routed.
///
/// An implementation is shared between threads and consulted often (once per HTTP request and
/// about once a second per open stream), so `route` must be cheap and must answer from the
/// current settings: a change takes effect on the next request and the next reconnect.
pub trait Network: Send + Sync {
    /// The route to use for `url` (an `https://` or `wss://` address). Called again before every
    /// request, so a change of setting is followed without rebuilding anything.
    fn route(&self, url: &str) -> Route;

    /// A TCP stream to `host:port`, already connected (and tunnelled, when the route is a proxy),
    /// before any TLS. `host` is a bare name or address. A connection that takes longer than
    /// `timeout` is an error.
    ///
    /// When the route is a proxy, only the [`io::ErrorKind`] of a failure is ever shown: the error
    /// text may quote the proxy's own words or the credentials it was given, so the crate does not
    /// trust it for display.
    fn dial(&self, host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream>;
}

/// Connects straight to the host. The default [`Network`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Direct;

impl Network for Direct {
    fn route(&self, _url: &str) -> Route {
        Route::Direct
    }

    fn dial(&self, host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
        dial_direct(host, port, timeout)
    }
}

/// Resolves `host` and connects to the first of its first few addresses that answers, each attempt
/// bounded by `timeout`.
fn dial_direct(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port)
        .to_socket_addrs()?
        .take(DIRECT_ADDRESS_ATTEMPTS)
    {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address for the host")))
}

/// What to show when dialling over `route` failed with `error`.
///
/// Over a [`Route::Direct`] connection the error text is shown as it is: no proxy credentials exist
/// on that path. Over a proxy only the error's kind is shown, because a [`Network`]'s error text is
/// written by the consumer and may carry the proxy's reply or the credentials it was given; the
/// scrubber knows only broker credentials, and ignores short values.
pub(crate) fn dial_failure(route: &Route, error: &io::Error) -> String {
    match route {
        Route::Direct => error.to_string(),
        Route::Proxy(_) => format!("could not connect through the proxy ({})", error.kind()),
    }
}

/// The proxy a [`Route`] names, in `ureq`'s terms; `None` for a direct route.
///
/// Always explicit: `ureq` would otherwise read `*_PROXY` variables for an agent built without one,
/// and a [`Route::Direct`] must mean direct. An error means the proxy cannot be described to
/// `ureq` (an invalid host, say). The caller must then refuse to send, never fall back to a direct
/// connection.
pub(crate) fn to_ureq(route: &Route) -> Result<Option<ureq::Proxy>, ureq::Error> {
    let Route::Proxy(proxy) = route else {
        return Ok(None);
    };
    let protocol = match proxy.protocol {
        ProxyProtocol::Http => ureq::ProxyProtocol::Http,
        ProxyProtocol::Https => ureq::ProxyProtocol::Https,
        ProxyProtocol::Socks4 => ureq::ProxyProtocol::Socks4A,
        ProxyProtocol::Socks5 => ureq::ProxyProtocol::Socks5h,
    };
    let mut builder = ureq::Proxy::builder(protocol)
        .host(&bracketed(&proxy.host))
        .port(proxy.port);
    if let Some(credentials) = &proxy.credentials {
        builder = builder
            .username(&credentials.username)
            .password(&credentials.password);
    }
    builder.build().map(Some)
}

/// `host` as it appears in a URL: an IPv6 literal gets its brackets.
fn bracketed(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn debug_never_shows_a_proxy_password() {
        let proxy = Proxy::new(ProxyProtocol::Http, "proxy.example", 3128)
            .with_credentials(ProxyCredentials::new("alice", "hunter2-secret"));
        for text in [
            format!("{proxy:?}"),
            format!("{:?}", Route::Proxy(proxy.clone())),
            format!("{:?}", proxy.credentials()),
        ] {
            assert!(!text.contains("hunter2-secret"), "{text}");
            assert!(text.contains("alice"), "{text}");
        }
    }

    #[test]
    fn a_direct_route_has_no_ureq_proxy() {
        assert!(to_ureq(&Route::Direct).unwrap().is_none());
    }

    #[test]
    fn each_protocol_maps_to_its_ureq_proxy() {
        let cases = [
            (ProxyProtocol::Http, ureq::ProxyProtocol::Http),
            (ProxyProtocol::Https, ureq::ProxyProtocol::Https),
            (ProxyProtocol::Socks4, ureq::ProxyProtocol::Socks4A),
            (ProxyProtocol::Socks5, ureq::ProxyProtocol::Socks5h),
        ];
        for (ours, theirs) in cases {
            let route = Route::Proxy(Proxy::new(ours, "proxy.example", 1081));
            let proxy = to_ureq(&route).unwrap().expect("a proxy");
            assert_eq!(proxy.protocol(), theirs);
            assert_eq!(proxy.host(), "proxy.example");
            assert_eq!(proxy.port(), 1081);
            assert_eq!(proxy.username(), None);
        }
    }

    #[test]
    fn credentials_and_ipv6_hosts_survive_the_mapping() {
        let route = Route::Proxy(
            Proxy::new(ProxyProtocol::Socks5, "::1", 1080)
                .with_credentials(ProxyCredentials::new("alice", "p@ss:word")),
        );
        let proxy = to_ureq(&route).unwrap().expect("a proxy");
        assert_eq!(proxy.username(), Some("alice"));
        assert_eq!(proxy.password(), Some("p@ss:word"));
        assert_eq!(proxy.port(), 1080);
    }

    #[test]
    fn awkward_proxy_credentials_reach_ureq_unchanged() {
        let cases = [
            ("alice", "p@ss:word"),
            ("bob", "a/b#c?d"),
            ("me@work", "pw1"),
        ];
        for (user, password) in cases {
            let route = Route::Proxy(
                Proxy::new(ProxyProtocol::Http, "proxy.example", 3128)
                    .with_credentials(ProxyCredentials::new(user, password)),
            );
            let proxy = to_ureq(&route).unwrap().expect("a proxy");
            assert_eq!(proxy.username(), Some(user));
            assert_eq!(proxy.password(), Some(password));
            assert_eq!(proxy.host(), "proxy.example");
            assert_eq!(proxy.port(), 3128);
        }
    }

    #[test]
    fn a_dial_failure_over_a_proxy_shows_only_the_error_kind() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "407 for alice:pw1");
        let proxied = Route::Proxy(Proxy::new(ProxyProtocol::Http, "proxy.example", 3128));
        let shown = dial_failure(&proxied, &error);
        assert!(shown.contains("permission denied"), "{shown}");
        assert!(!shown.contains("pw1") && !shown.contains("407"), "{shown}");
        assert_eq!(dial_failure(&Route::Direct, &error), "407 for alice:pw1");
    }

    #[test]
    fn an_unusable_proxy_host_is_an_error_not_a_direct_connection() {
        let route = Route::Proxy(Proxy::new(ProxyProtocol::Http, "bad host/", 8080));
        assert!(to_ureq(&route).is_err());
    }

    #[test]
    fn a_direct_dial_reaches_a_listener_and_reports_a_refusal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let timeout = Duration::from_secs(5);
        assert!(Direct.dial("127.0.0.1", port, timeout).is_ok());
        drop(listener);
        assert!(Direct.dial("127.0.0.1", port, timeout).is_err());
    }

    #[test]
    fn the_default_network_routes_everything_direct() {
        assert_eq!(Direct.route("https://api.example.com"), Route::Direct);
    }
}
