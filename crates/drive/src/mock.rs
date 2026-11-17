//! A tiny in-process HTTP/1.1 server for tests: one request per connection, then close.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn query(&self, key: &str) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

pub enum Reply {
    /// Status, extra headers, body.
    Full(u16, Vec<(String, String)>, Vec<u8>),
    /// Advertises the whole body's length but sends only `send` bytes, then closes.
    Truncated(u16, Vec<(String, String)>, Vec<u8>, usize),
    /// A streamed body (no length) written in pieces, with a pause between them.
    Chunks(u16, Vec<(String, String)>, Vec<Vec<u8>>, Duration),
    /// Waits before answering.
    Slow(Duration, Box<Reply>),
}

impl Reply {
    pub fn json(status: u16, v: serde_json::Value) -> Self {
        Reply::Full(status, vec![("Content-Type".into(), "application/json".into())], v.to_string().into_bytes())
    }
}

type Handler = dyn Fn(&Req) -> Reply + Send + Sync;

pub struct MockServer {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Req>>>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let handler: Arc<Handler> = Arc::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                let log = log.clone();
                std::thread::spawn(move || serve(stream, &*handler, &log));
            }
        });
        Self { url, requests }
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, handler: &Handler, log: &Mutex<Vec<Req>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0usize);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    let req = Req { method, target, headers, body };
    log.lock().unwrap().push(req.clone());
    write_reply(stream, handler(&req));
}

fn head(status: u16, headers: &[(String, String)], len: Option<usize>) -> String {
    let mut s = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(len) = len {
        s.push_str(&format!("Content-Length: {len}\r\n"));
    }
    s + "\r\n"
}

fn write_reply(mut stream: TcpStream, reply: Reply) {
    match reply {
        Reply::Full(status, headers, body) => {
            let _ = stream.write_all(head(status, &headers, Some(body.len())).as_bytes());
            let _ = stream.write_all(&body);
        }
        Reply::Truncated(status, headers, body, send) => {
            let _ = stream.write_all(head(status, &headers, Some(body.len())).as_bytes());
            let _ = stream.write_all(&body[..send]);
            let _ = stream.flush();
        }
        Reply::Chunks(status, headers, chunks, pause) => {
            let _ = stream.write_all(head(status, &headers, None).as_bytes());
            for c in chunks {
                let _ = stream.write_all(&c);
                let _ = stream.flush();
                std::thread::sleep(pause);
            }
        }
        Reply::Slow(wait, inner) => {
            std::thread::sleep(wait);
            return write_reply(stream, *inner);
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
