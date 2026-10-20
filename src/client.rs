//! Client side of the protocol: probe nodes, submit tasks, follow events.

use crate::http;
use crate::net::Peer;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

/// Minimal line splitter over a plain `Read`.
struct Lines<R: Read> {
    inner: R,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: Read> Lines<R> {
    fn new(inner: R) -> Self {
        Lines {
            inner,
            buf: Vec::with_capacity(1024),
            eof: false,
        }
    }

    fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                return Some(String::from_utf8_lossy(&line).trim_end().to_string());
            }
            if self.eof {
                if self.buf.is_empty() {
                    return None;
                }
                let line = std::mem::take(&mut self.buf);
                return Some(String::from_utf8_lossy(&line).trim_end().to_string());
            }
            let mut chunk = [0u8; 4096];
            match self.inner.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => self.eof = true,
            }
        }
    }
}

pub fn probe(addr: &str, timeout: Duration) -> Option<Value> {
    let resp = http::request("GET", addr, "/v1/info", None, timeout).ok()?;
    if resp.status != 200 {
        return None;
    }
    resp.json()
}

pub fn submit(
    addr: &str,
    prompt: &str,
    label: Option<&str>,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    let mut body = json!({ "prompt": prompt });
    if let Some(l) = label {
        body["label"] = Value::String(l.to_string());
    }
    if let Some(d) = cwd {
        body["cwd"] = Value::String(d.to_string());
    }
    let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    let resp = http::request("POST", addr, "/v1/tasks", Some(&bytes), timeout)
        .map_err(|e| format!("cannot reach {}: {}", addr, e))?;
    let v = resp.json().unwrap_or(Value::Null);
    if resp.status != 201 {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("task submission failed")
            .to_string();
        return Err(err);
    }
    v.get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "node did not return a task id".to_string())
}

/// Hand an access key to a node.
pub fn post_license(addr: &str, body: &str) -> Result<Value, String> {
    let resp = http::request(
        "POST",
        addr,
        "/v1/license",
        Some(body.as_bytes()),
        Duration::from_secs(10),
    )
    .map_err(|e| format!("cannot reach {}: {}", addr, e))?;
    let v = resp.json().unwrap_or(Value::Null);
    if resp.status != 200 {
        return Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("the node refused the key")
            .to_string());
    }
    Ok(v)
}

pub fn get_task(addr: &str, id: &str, timeout: Duration) -> Result<Value, String> {
    let resp = http::request("GET", addr, &format!("/v1/tasks/{}", id), None, timeout)
        .map_err(|e| format!("cannot reach {}: {}", addr, e))?;
    let v = resp.json().unwrap_or(Value::Null);
    if resp.status != 200 {
        return Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("task not found")
            .to_string());
    }
    Ok(v)
}

pub fn cancel_task(addr: &str, id: &str, timeout: Duration) -> Result<(), String> {
    let resp = http::request(
        "POST",
        addr,
        &format!("/v1/tasks/{}/cancel", id),
        None,
        timeout,
    )
    .map_err(|e| format!("cannot reach {}: {}", addr, e))?;
    if resp.status != 200 {
        let v = resp.json().unwrap_or(Value::Null);
        return Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("cancel failed")
            .to_string());
    }
    Ok(())
}

/// Follow the SSE stream of a task. `on_output` receives live stdout chunks.
pub fn follow(
    addr: &str,
    id: &str,
    mut on_output: impl FnMut(&str),
    mut on_stderr: impl FnMut(&str),
    timeout: Duration,
) -> Result<Value, String> {
    let (_status, reader) = http::request_streaming(
        "GET",
        addr,
        &format!("/v1/tasks/{}/events", id),
        None,
        timeout,
    )
    .map_err(|e| format!("cannot reach {}: {}", addr, e))?;

    let mut lines = Lines::new(reader);
    let mut kind = String::from("message");
    let mut data = String::new();
    let mut final_view = Value::Null;
    loop {
        let Some(line) = lines.next_line() else { break };
        if line.is_empty() {
            if !data.is_empty() {
                let payload: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                match kind.as_str() {
                    "output" => {
                        if let Some(chunk) = payload.get("chunk").and_then(|c| c.as_str()) {
                            on_output(chunk);
                        }
                    }
                    "stderr" => {
                        if let Some(chunk) = payload.get("chunk").and_then(|c| c.as_str()) {
                            on_stderr(chunk);
                        }
                    }
                    "final" => final_view = payload,
                    _ => {}
                }
            }
            kind = "message".into();
            data.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            kind = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if final_view.is_null() {
        final_view = get_task(addr, id, timeout)?;
    }
    Ok(final_view)
}

/// Pick the node to talk to from the cached peer table.
pub fn resolve(
    peers: &[Peer],
    wanted_node: Option<&str>,
    timeout: Duration,
) -> Result<(String, Value), String> {
    let mut candidates: Vec<&Peer> = peers.iter().collect();
    if let Some(want) = wanted_node {
        candidates.retain(|p| p.name == want);
        if candidates.is_empty() {
            return Err(format!("no node named '{}' in the peer table", want));
        }
    }
    let mut last_err = String::from("no reachable node found");
    for peer in candidates {
        let addr = peer.addr();
        match probe(&addr, timeout) {
            Some(info) => return Ok((addr, info)),
            None => last_err = format!("node {} ({}) is not responding", peer.name, addr),
        }
    }
    Err(last_err)
}
