//! Minimal HTTP/1.1 server and client built directly on `std::net`.
//!
//! No async runtime, no connection pool, no middleware stack: a thread per
//! connection with a 512 KiB stack and a hard cap on concurrent connections.
//! Resident memory stays flat regardless of how many nodes are on the LAN.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_LINE: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const CONN_STACK: usize = 512 * 1024;

pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub enum Body {
    Bytes(Vec<u8>),
    /// Streamed response (SSE). The closure owns the socket writes.
    Stream(Box<dyn FnOnce(&mut dyn Write) + Send>),
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Response {
            status,
            content_type: "application/json".into(),
            headers: Vec::new(),
            body: Body::Bytes(bytes),
        }
    }
    pub fn text(status: u16, text: &str) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8".into(),
            headers: Vec::new(),
            body: Body::Bytes(text.as_bytes().to_vec()),
        }
    }
    pub fn asset(content_type: &str, body: &str) -> Response {
        Response {
            status: 200,
            content_type: content_type.to_string(),
            headers: Vec::new(),
            body: Body::Bytes(body.as_bytes().to_vec()),
        }
    }
    pub fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn stream(f: impl FnOnce(&mut dyn Write) + Send + 'static) -> Response {
        Response {
            status: 200,
            content_type: "text/event-stream".into(),
            headers: Vec::new(),
            body: Body::Stream(Box::new(f)),
        }
    }
}

pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

pub struct Server {
    listener: TcpListener,
    handler: Handler,
    max_conns: usize,
}

impl Server {
    pub fn bind(addr: &str, handler: Handler, max_conns: usize) -> io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        Ok(Server {
            listener,
            handler,
            max_conns: max_conns.max(1),
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept loop. Blocks forever; spawn it on its own thread.
    pub fn run(self) {
        let slots = Arc::new((Mutex::new(0usize), Condvar::new()));
        loop {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            {
                let (lock, cvar) = &*slots;
                let mut count = lock.lock().unwrap();
                while *count >= self.max_conns {
                    count = cvar.wait(count).unwrap();
                }
                *count += 1;
            }
            let handler = self.handler.clone();
            let slots2 = slots.clone();
            let spawned = std::thread::Builder::new()
                .stack_size(CONN_STACK)
                .spawn(move || {
                    handle_conn(stream, handler);
                    let (lock, cvar) = &*slots2;
                    let mut count = lock.lock().unwrap();
                    *count -= 1;
                    cvar.notify_one();
                });
            if spawned.is_err() {
                let (lock, cvar) = &*slots;
                let mut count = lock.lock().unwrap();
                *count -= 1;
                cvar.notify_one();
            }
        }
    }
}

fn handle_conn(stream: TcpStream, handler: Handler) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::with_capacity(8192, match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(_) => {
                let _ = write_simple(&mut writer, 400, "bad request");
                break;
            }
        };
        let keep_alive = wants_keep_alive(&req);
        let resp = handler(&req);
        match resp.body {
            Body::Bytes(bytes) => {
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
                    resp.status,
                    status_text(resp.status),
                    resp.content_type,
                    bytes.len(),
                    if keep_alive { "keep-alive" } else { "close" }
                );
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{}: {}\r\n", k, v));
                }
                head.push_str("\r\n");
                if writer.write_all(head.as_bytes()).is_err() || writer.write_all(&bytes).is_err() {
                    break;
                }
                let _ = writer.flush();
                if !keep_alive {
                    break;
                }
            }
            Body::Stream(f) => {
                let mut head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n"
                );
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{}: {}\r\n", k, v));
                }
                head.push_str("\r\n");
                if writer.write_all(head.as_bytes()).is_err() {
                    break;
                }
                let _ = writer.flush();
                f(&mut writer);
                let _ = writer.flush();
                break;
            }
        }
    }
    let _ = writer.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn write_simple(w: &mut TcpStream, status: u16, msg: &str) -> io::Result<()> {
    let body = msg.as_bytes();
    write!(
        w,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        status_text(status),
        body.len()
    )?;
    w.write_all(body)?;
    w.flush()
}

fn wants_keep_alive(req: &Request) -> bool {
    match req.headers.get("connection") {
        Some(v) if v.to_ascii_lowercase().contains("close") => false,
        Some(v) if v.to_ascii_lowercase().contains("keep-alive") => true,
        _ => true,
    }
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let line = match read_line_limited(reader, MAX_LINE)? {
        Some(l) => l,
        None => return Ok(None),
    };
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no method"));
    }
    let path = match target.split_once('?') {
        Some((p, _q)) => p.to_string(),
        None => target,
    };

    let mut headers = HashMap::new();
    for _ in 0..MAX_HEADERS {
        let h = match read_line_limited(reader, MAX_LINE)? {
            Some(h) => h,
            None => break,
        };
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let body = read_body(reader, &headers)?;
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn read_body(reader: &mut BufReader<TcpStream>, headers: &HashMap<String, String>) -> io::Result<Vec<u8>> {
    let chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    if chunked {
        let mut out = Vec::new();
        loop {
            let size_line = match read_line_limited(reader, MAX_LINE)? {
                Some(l) => l,
                None => break,
            };
            let size_str = size_line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_str, 16).unwrap_or(0);
            if size == 0 {
                // consume trailer
                while let Some(l) = read_line_limited(reader, MAX_LINE)? {
                    if l.is_empty() {
                        break;
                    }
                }
                break;
            }
            if out.len() + size > MAX_BODY {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk)?;
            out.extend_from_slice(&chunk);
            let _ = read_line_limited(reader, 8)?;
        }
        return Ok(out);
    }
    if let Some(len) = headers.get("content-length") {
        let len: usize = len.trim().parse().unwrap_or(0);
        if len > MAX_BODY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        return Ok(buf);
    }
    Ok(Vec::new())
}

fn read_line_limited<R: BufRead>(r: &mut R, max: usize) -> io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    loop {
        let available = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..=i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                r.consume(n);
                if buf.len() > max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
            }
        }
    }
    Ok(Some(String::from_utf8_lossy(&buf).trim_end().to_string()))
}

// ---------------------------------------------------------------- client ---

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Body reader that transparently decodes chunked transfer encoding.
pub struct BodyReader {
    inner: BufReader<TcpStream>,
    chunked: bool,
    remaining: usize,
    done: bool,
}

impl Read for BodyReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if !self.chunked {
            let n = self.inner.read(out)?;
            if n == 0 {
                self.done = true;
            }
            return Ok(n);
        }
        if self.remaining == 0 {
            let line = read_line_limited(&mut self.inner, MAX_LINE)?;
            let size = match line {
                Some(l) => l.split(';').next().unwrap_or("").trim().to_string(),
                None => {
                    self.done = true;
                    return Ok(0);
                }
            };
            let size = usize::from_str_radix(&size, 16).unwrap_or(0);
            if size == 0 {
                while let Some(l) = read_line_limited(&mut self.inner, MAX_LINE)? {
                    if l.is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = out.len().min(self.remaining);
        let n = self.inner.read(&mut out[..want])?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        self.remaining -= n;
        if self.remaining == 0 {
            let _ = read_line_limited(&mut self.inner, 8)?;
        }
        Ok(n)
    }
}

/// Split "host:port" (IPv4, hostname, or "[v6]:port").
pub fn split_addr(addr: &str, default_port: u16) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            let port = p.trim_start_matches(':');
            let port = if port.is_empty() {
                default_port
            } else {
                port.parse().unwrap_or(default_port)
            };
            return format!("[{}]:{}", h, port);
        }
    }
    match addr.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            format!("{}:{}", h, p)
        }
        _ => format!("{}:{}", addr, default_port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_addr_fills_in_the_default_port() {
        assert_eq!(split_addr("192.168.0.5", 8787), "192.168.0.5:8787");
        assert_eq!(split_addr("192.168.0.5:9000", 8787), "192.168.0.5:9000");
        assert_eq!(split_addr("mac-studio", 8787), "mac-studio:8787");
        assert_eq!(split_addr("[fe80::1]:9000", 8787), "[fe80::1]:9000");
        assert_eq!(split_addr("[fe80::1]", 8787), "[fe80::1]:8787");
    }
}

fn connect(addr: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut last_err = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
    let addrs = addr.to_socket_addrs()?;
    for sa in addrs {
        match TcpStream::connect_timeout(&sa, timeout) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                let _ = s.set_read_timeout(Some(timeout));
                let _ = s.set_write_timeout(Some(timeout));
                return Ok(s);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

pub fn request(
    method: &str,
    addr: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<HttpResponse> {
    let (status, mut reader) = request_streaming(method, addr, path, body, timeout)?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    Ok(HttpResponse {
        status,
        body: buf,
    })
}

pub fn request_streaming(
    method: &str,
    addr: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<(u16, BodyReader)> {
    let mut stream = connect(addr, timeout)?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: hive/0.1\r\nConnection: close\r\nAccept: */*\r\n",
        method, path, addr
    );
    if let Some(b) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b)?;
    }
    stream.flush()?;

    let mut reader = BufReader::with_capacity(8192, stream);
    let status_line = read_line_limited(&mut reader, MAX_LINE)?.unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let _proto = parts.next();
    let status: u16 = parts.next().unwrap_or("0").parse().unwrap_or(0);

    let mut headers = HashMap::new();
    for _ in 0..MAX_HEADERS {
        let h = match read_line_limited(&mut reader, MAX_LINE)? {
            Some(h) => h,
            None => break,
        };
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);
    let br = BodyReader {
        inner: reader,
        chunked,
        remaining: 0,
        done: false,
    };
    Ok((status, br))
}
