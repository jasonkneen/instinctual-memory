//! Loopback HTTP and server-sent events for the operation layer.
//!
//! The accept loop only hands a socket to a worker. The journal scan runs
//! in that worker, so another request can be accepted while a stream is open.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::ops::{self, OpResult};

static NEXT_OP: AtomicU64 = AtomicU64::new(1);

struct StoredOp {
    progress: Value,
    result: Value,
}

pub struct Server {
    pub addr: SocketAddr,
    listener: TcpListener,
    root: PathBuf,
    scope_id: String,
    session_id: String,
    ops: Arc<Mutex<std::collections::HashMap<String, StoredOp>>>,
}

pub fn bind_listen(listen: &str) -> Result<TcpListener> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| Error::InvalidContent(format!("bad listen address: {listen}")))?;
    if !is_loopback(addr.ip()) {
        return Err(Error::InvalidContent(format!(
            "refusing non-loopback listen address {listen}"
        )));
    }
    TcpListener::bind(addr).map_err(|e| Error::io(std::path::Path::new(listen), e))
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => v6 == Ipv4Addr::LOCALHOST.to_ipv6_mapped() || v6.is_loopback(),
    }
}

impl Server {
    pub fn bind(listen: &str, root: PathBuf, scope_id: &str, session_id: &str) -> Result<Self> {
        let listener = bind_listen(listen)?;
        let addr = listener
            .local_addr()
            .map_err(|e| Error::io(std::path::Path::new(listen), e))?;
        Ok(Self {
            addr,
            listener,
            root,
            scope_id: scope_id.to_string(),
            session_id: session_id.to_string(),
            ops: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub fn serve(self, shutdown: Arc<AtomicBool>) -> Result<()> {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| Error::io(std::path::Path::new("listen"), e))?;
        while !shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // An accepted socket inherits non-blocking mode on macOS;
                    // reads must block until the request arrives.
                    let _ = stream.set_nonblocking(false);
                    let root = self.root.clone();
                    let scope = self.scope_id.clone();
                    let session = self.session_id.clone();
                    let ops = self.ops.clone();
                    std::thread::spawn(move || {
                        if let Err(err) = handle_connection(stream, &root, &scope, &session, &ops) {
                            eprintln!("http: {err}");
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(Error::io(std::path::Path::new("listen"), err)),
            }
        }
        Ok(())
    }
}

fn handle_connection(
    mut stream: TcpStream,
    root: &std::path::Path,
    scope_id: &str,
    session_id: &str,
    ops: &Mutex<std::collections::HashMap<String, StoredOp>>,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let (start, header, body) = read_request(&mut stream)?;
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");
    if method == "GET" && path == "/health" {
        return write_bytes(&mut stream, 200, "application/json", br#"{"ok":true}"#);
    }
    if method == "POST" && path == "/v1/operations" {
        let request: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let operation = request
            .get("operation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let args = request.get("arguments").cloned().unwrap_or(json!({}));
        let result = ops::execute(root, scope_id, session_id, operation, &args);
        let id = format!("op_{}", NEXT_OP.fetch_add(1, Ordering::Relaxed));
        if let Ok(mut guard) = ops.lock() {
            guard.insert(
                id.clone(),
                StoredOp {
                    progress: result.progress.clone(),
                    result: result_event(&result),
                },
            );
        }
        let mut payload = result.payload;
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("operation_id".into(), json!(id));
        }
        let status = if result.conflict {
            409
        } else if result.not_found {
            404
        } else if result.error.is_some() {
            400
        } else {
            200
        };
        let bytes = serde_json::to_vec(&payload)?;
        return write_bytes(&mut stream, status, "application/json", &bytes);
    }
    if method == "GET" {
        if let Some(id) = path
            .strip_prefix("/v1/operations/")
            .and_then(|rest| rest.strip_suffix("/events"))
        {
            let stored = ops.lock().ok().and_then(|guard| guard.get(id).cloned());
            let Some(stored) = stored else {
                return write_bytes(&mut stream, 404, "application/json", br#"{"error":"not found"}"#);
            };
            return write_sse(&mut stream, &stored);
        }
    }
    let _ = header;
    write_bytes(&mut stream, 404, "application/json", br#"{"error":"not found"}"#)
}

fn result_event(result: &OpResult) -> Value {
    let mut line = json!({"type": "result"});
    if let (Some(obj), Some(fields)) = (line.as_object_mut(), result.payload.as_object()) {
        for (key, value) in fields {
            obj.insert(key.clone(), value.clone());
        }
    }
    line
}

fn write_sse(stream: &mut TcpStream, stored: &StoredOp) -> Result<()> {
    let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream
        .write_all(head)
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
    write_frame(stream, "progress", &stored.progress)?;
    stream
        .flush()
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
    // Keep the stream open long enough for another loopback request to land.
    std::thread::sleep(Duration::from_millis(200));
    write_frame(stream, "result", &stored.result)?;
    stream
        .flush()
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
    Ok(())
}

fn write_frame(stream: &mut TcpStream, event: &str, data: &Value) -> Result<()> {
    let body = serde_json::to_string(data)?;
    let frame = format!("event: {event}\ndata: {body}\n\n");
    stream
        .write_all(frame.as_bytes())
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))
}

fn write_bytes(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
    stream
        .write_all(body)
        .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> Result<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let split_at = loop {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
        if n == 0 {
            return Err(Error::InvalidContent("empty http request".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 65_536 {
            return Err(Error::InvalidContent("http headers too large".into()));
        }
    };
    let header = String::from_utf8_lossy(&buf[..split_at]).to_string();
    let mut body = buf[split_at + 4..].to_vec();
    let length = content_length(&header);
    while body.len() < length {
        let n = stream
            .read(&mut tmp)
            .map_err(|e| Error::io(std::path::Path::new("socket"), e))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(length);
    let start = header.lines().next().unwrap_or("").to_string();
    Ok((start, header, body))
}

fn content_length(header: &str) -> usize {
    header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

impl Clone for StoredOp {
    fn clone(&self) -> Self {
        Self {
            progress: self.progress.clone(),
            result: self.result.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalEvent, Redaction, Role, Source};
    use crate::repo::GitRepo;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn rejects_non_loopback_listen_address() {
        let err = bind_listen("0.0.0.0:9").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("non-loopback"), "{text}");
    }

    #[test]
    fn http_search_sse_and_health() {
        let phrase = "cobalt-mast-4410 only here";
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        journal
            .append(JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: "http".into(),
                    occurred_at: None,
                },
                phrase,
                Redaction::None,
            ))
            .unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            tmp.path().to_path_buf(),
            "personal",
            "default",
        )
        .unwrap();
        let addr = server.addr;
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let handle = std::thread::spawn(move || server.serve(stop));
        let base = format!("http://{addr}");
        let post = ureq::post(&format!("{base}/v1/operations"))
            .send_json(json!({"operation":"memory_search","arguments":{"query": phrase, "limit": 3, "rerank": false}}))
            .unwrap();
        let search: Value = post.into_json().unwrap();
        assert_eq!(search["ranker"], "lexical");
        assert!(search.to_string().contains(phrase));
        let id = search["operation_id"].as_str().unwrap();
        let events_url = format!("{base}/v1/operations/{id}/events");
        let health_url = format!("{base}/health");
        let events_url_2 = events_url.clone();
        let stream = std::thread::spawn(move || {
            ureq::get(&events_url_2).call().unwrap().into_string().unwrap()
        });
        std::thread::sleep(Duration::from_millis(50));
        let health = ureq::get(&health_url).call().unwrap().into_string().unwrap();
        assert!(health.contains("ok"));
        assert!(!health.contains(phrase));
        let events = stream.join().unwrap();
        assert!(events.contains("event: progress"), "{events}");
        assert!(events.contains("event: result"), "{events}");
        let progress = events.split("event: result").next().unwrap();
        assert!(!progress.contains(phrase), "{progress}");
        assert!(events.contains(phrase));
        eprintln!("checked {phrase}");
        shutdown.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
}
