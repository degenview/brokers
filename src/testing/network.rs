//! Test doubles for the [`Network`] port: a scripted network whose route a test can switch, and a
//! minimal in-process HTTP `CONNECT` proxy that records what it was asked to tunnel.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::net::{Direct, Network, Proxy, ProxyProtocol, Route};

/// A [`Network`] whose route a test sets by hand, counting how often it is consulted. Dialling
/// always connects directly (the tests aim at loopback servers) and is recorded.
pub struct ScriptedNetwork {
    route: Mutex<Route>,
    routes_asked: AtomicUsize,
    dialled: Mutex<Vec<(String, u16)>>,
}

impl ScriptedNetwork {
    /// A network that starts on `route`.
    pub fn new(route: Route) -> Self {
        Self {
            route: Mutex::new(route),
            routes_asked: AtomicUsize::new(0),
            dialled: Mutex::new(Vec::new()),
        }
    }

    /// From now on, answer `route`.
    pub fn set_route(&self, route: Route) {
        *self.route.lock().expect("route lock") = route;
    }

    /// How many times `route` has been called.
    pub fn routes_asked(&self) -> usize {
        self.routes_asked.load(Ordering::SeqCst)
    }

    /// Every `(host, port)` passed to `dial`, oldest first.
    pub fn dialled(&self) -> Vec<(String, u16)> {
        self.dialled.lock().expect("dial lock").clone()
    }
}

impl Network for ScriptedNetwork {
    fn route(&self, _url: &str) -> Route {
        self.routes_asked.fetch_add(1, Ordering::SeqCst);
        self.route.lock().expect("route lock").clone()
    }

    fn dial(&self, host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
        self.dialled
            .lock()
            .expect("dial lock")
            .push((host.to_owned(), port));
        Direct.dial(host, port, timeout)
    }
}

/// A forward proxy on 127.0.0.1 that answers `CONNECT host:port` by connecting to that address and
/// relaying bytes both ways. Anything that is not a `CONNECT` is answered `400`. It records the
/// request line of everything it receives.
pub struct ConnectProxy {
    address: String,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ConnectProxy {
    /// Starts a proxy on a free loopback port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("address").to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for client in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(client) = client else { continue };
                    let seen = Arc::clone(&seen);
                    thread::spawn(move || serve(client, &seen));
                }
            })
        };
        Self {
            address,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    /// The route that points at this proxy.
    pub fn route(&self) -> Route {
        let port = self
            .address
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse().ok())
            .expect("a port");
        Route::Proxy(Proxy::new(ProxyProtocol::Http, "127.0.0.1", port))
    }

    /// The first line of every request received so far (`CONNECT 127.0.0.1:4000 HTTP/1.1`),
    /// oldest first.
    pub fn request_lines(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread ends.
        let _ = TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads the request head byte by byte, so nothing the client sends after it is consumed.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 || stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

fn serve(mut client: TcpStream, seen: &Mutex<Vec<String>>) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(head) = read_head(&mut client) else {
        return;
    };
    let line = head.lines().next().unwrap_or_default().to_owned();
    if let Ok(mut seen) = seen.lock() {
        seen.push(line.clone());
    }
    let mut parts = line.split_whitespace();
    let target = match (parts.next(), parts.next()) {
        (Some("CONNECT"), Some(target)) => target.to_owned(),
        _ => {
            let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            return;
        }
    };
    let Ok(upstream) = TcpStream::connect(&target) else {
        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .is_err()
    {
        return;
    }
    let _ = client.set_read_timeout(None);
    relay(client, upstream);
}

/// Copies bytes both ways until either side closes.
fn relay(client: TcpStream, upstream: TcpStream) {
    let (Ok(mut client_in), Ok(mut upstream_in)) = (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    let (mut client_out, mut upstream_out) = (client, upstream);
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut client_in, &mut upstream_out);
        let _ = upstream_out.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut upstream_in, &mut client_out);
    let _ = client_out.shutdown(Shutdown::Write);
    let _ = forward.join();
}
