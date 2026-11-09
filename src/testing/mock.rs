//! A scripted HTTP server on 127.0.0.1. A handler function sees each request and decides the
//! reply, so a test can model an exchange (routes, state, failures) without a network.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One request the server received.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    /// The path and query string exactly as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The path without its query string.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }

    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, query)| query)
    }
}

/// What the server does with a request.
pub enum Reply {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        delay: Option<Duration>,
    },
    /// Read the request, then close the connection without answering: the caller cannot tell
    /// whether the exchange acted on it.
    Drop,
}

impl Reply {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self::Respond {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.into(),
            delay: None,
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Self::Respond { headers, .. } = &mut self {
            headers.push((name.to_owned(), value.to_owned()));
        }
        self
    }

    #[must_use]
    pub fn after(mut self, wait: Duration) -> Self {
        if let Self::Respond { delay, .. } = &mut self {
            *delay = Some(wait);
        }
        self
    }
}

type Handler = dyn Fn(&RecordedRequest) -> Reply + Send + Sync;

pub struct MockServer {
    /// `http://127.0.0.1:<port>`
    pub url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    address: String,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&RecordedRequest) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("address").to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let handler = Arc::clone(&handler);
                    let requests = Arc::clone(&requests);
                    thread::spawn(move || serve(stream, &handler, &requests));
                }
            })
        };
        Self {
            url: format!("http://{address}"),
            requests,
            stop,
            address,
            thread: Some(thread),
        }
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().map_or(0, |r| r.len())
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread ends.
        let _ = TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    handler: &Arc<Handler>,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    if let Ok(mut log) = requests.lock() {
        log.push(request.clone());
    }
    match handler(&request) {
        Reply::Drop => {
            let _ = stream.shutdown(Shutdown::Both);
        }
        Reply::Respond {
            status,
            headers,
            body,
            delay,
        } => {
            if let Some(delay) = delay {
                thread::sleep(delay);
            }
            let mut out = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&body);
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..read]);
        if data.len() > 1 << 20 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < header_end + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&data[header_end..data.len().min(header_end + length)])
        .into_owned();
    Some(RecordedRequest {
        method,
        target,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_requests_and_serves_the_scripted_reply() {
        let server = MockServer::start(|request| {
            Reply::json(200, format!("{{\"path\":\"{}\"}}", request.path()))
        });
        let response = ureq::get(format!("{}/a/b?x=1", server.url)).call().unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let seen = server.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path(), "/a/b");
        assert_eq!(seen[0].query(), "x=1");
    }
}
