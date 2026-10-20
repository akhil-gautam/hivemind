//! End-to-end tests: two nodes on one machine, discovery, task round trip,
//! cancellation, timeout and shell-quoting safety.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_hivemind");

struct Node {
    child: Child,
    port: u16,
}

impl Node {
    fn start(dir: &Path, port: u16, discovery_port: u16, name: &str, agent: &str) -> Node {
        std::fs::create_dir_all(dir).unwrap();
        let child = Command::new(BIN)
            .args([
                "serve",
                "--port",
                &port.to_string(),
                "--name",
                name,
                "--agent-cmd",
                agent,
                "--discovery-port",
                &discovery_port.to_string(),
            ])
            .env("HIVEMIND_DIR", dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start hivemind serve");
        let node = Node {
            child,
            port,
        };
        node.wait_ready();
        node
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if http_get(self.port, "/healthz").is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("node on port {} never became ready", self.port);
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("hivemind-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Raw HTTP call returning (status, head block, body).
fn http_call(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<(u16, String, String), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n",
        method, path
    );
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, head.to_lowercase(), body.to_string()))
}

/// Minimal HTTP GET returning the status line and body.
fn http_get(port: u16, path: &str) -> Result<(u16, String), String> {
    let (status, _head, body) = http_call(port, "GET", path, None)?;
    Ok((status, body))
}

fn http_get_with_headers(port: u16, path: &str) -> Result<(u16, String), String> {
    let (status, head, _body) = http_call(port, "GET", path, None)?;
    Ok((status, head))
}

fn http_post(port: u16, path: &str, body: &str) -> Result<(u16, String), String> {
    let (status, _head, out) = http_call(port, "POST", path, Some(body))?;
    Ok((status, out))
}

fn run_cli(args: &[&str], dir: &Path) -> (String, String, bool) {
    let out = Command::new(BIN)
        .args(args)
        .env("HIVEMIND_DIR", dir)
        .output()
        .expect("failed to run hivemind");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

fn json_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":", key);
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut end = None;
        let mut escaped = false;
        for (i, c) in stripped.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        Some(&stripped[..end?])
    } else {
        let end = rest
            .find(|c: char| c == ',' || c == '}' || c == '\n')
            .unwrap_or(rest.len());
        Some(rest[..end].trim())
    }
}

#[test]
fn task_round_trip_returns_agent_output() {
    let dir = temp_dir("roundtrip");
    let node = Node::start(&dir, 18801, 14781, "node-a", "cat");
    let (stdout, stderr, ok) = run_cli(
        &["run", "hello hivemind", "--host", &node.addr(), "--json"],
        &dir,
    );
    assert!(ok, "run failed: {}{}", stdout, stderr);
    assert_eq!(json_field(&stdout, "state"), Some("done"));
    assert!(stdout.contains("hello hivemind"), "output missing: {}", stdout);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prompt_is_passed_literally() {
    let dir = temp_dir("quoting");
    let node = Node::start(&dir, 18802, 14782, "node-q", "cat");
    let nasty = "a \"b\" $HOME `id` ; echo pwned";
    let (stdout, stderr, ok) = run_cli(
        &["run", nasty, "--host", &node.addr(), "--json"],
        &dir,
    );
    assert!(ok, "run failed: {}{}", stdout, stderr);
    assert_eq!(json_field(&stdout, "state"), Some("done"));
    let output = json_field(&stdout, "output").unwrap_or("");
    assert!(
        output.contains("`id`") && output.contains("$HOME"),
        "prompt was not literal: {}",
        output
    );
    assert!(!output.contains("pwned\npwned"), "shell injection: {}", output);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failing_agent_reports_error() {
    let dir = temp_dir("failing");
    let node = Node::start(&dir, 18803, 14783, "node-f", "sh -c 'echo boom >&2; exit 7'");
    let (stdout, _stderr, ok) = run_cli(
        &["run", "make it fail", "--host", &node.addr(), "--json"],
        &dir,
    );
    assert!(!ok, "expected a non-zero exit code");
    assert_eq!(json_field(&stdout, "state"), Some("failed"));
    assert_eq!(json_field(&stdout, "exit_code"), Some("7"));
    assert!(stdout.contains("boom"), "stderr not reported: {}", stdout);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn node_timeout_fails_the_task() {
    let dir = temp_dir("timeout");
    let node = Node::start(&dir, 18804, 14784, "node-t", "sleep 30");
    // Restart with a short node-side timeout by writing the config first.
    drop(node);
    std::fs::write(
        dir.join("config.json"),
        format!(
            "{{\"node_name\":\"node-t\",\"port\":18804,\"agent_cmd\":\"sleep 30\",\"task_timeout_s\":2,\"discovery_port\":14784}}"
        ),
    )
    .unwrap();
    let node = Node::start(&dir, 18804, 14784, "node-t", "sleep 30");
    let (stdout, _stderr, ok) = run_cli(
        &["run", "slow", "--host", &node.addr(), "--json"],
        &dir,
    );
    assert!(!ok, "expected a non-zero exit code");
    assert_eq!(json_field(&stdout, "state"), Some("failed"));
    assert!(
        stdout.contains("timed out"),
        "timeout not reported: {}",
        stdout
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn running_task_can_be_cancelled() {
    let dir = temp_dir("cancel");
    let node = Node::start(&dir, 18805, 14785, "node-c", "sleep 60");
    let (status, body) = http_post(node.port, "/v1/tasks", "{\"prompt\":\"long job\"}").unwrap();
    assert_eq!(status, 201);
    let id = json_field(&body, "id").expect("task id").to_string();
    std::thread::sleep(Duration::from_millis(500));
    let (status, _) = http_post(node.port, &format!("/v1/tasks/{}/cancel", id), "").unwrap();
    assert_eq!(status, 200);
    std::thread::sleep(Duration::from_millis(500));
    let (status, body) = http_get(node.port, &format!("/v1/tasks/{}", id)).unwrap();
    assert_eq!(status, 200);
    assert_eq!(json_field(&body, "state"), Some("cancelled"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nodes_discover_each_other_on_the_lan() {
    let dir_a = temp_dir("peer-a");
    let dir_b = temp_dir("peer-b");
    // The CLI reads the discovery port from the config, like a real node does.
    for (dir, port) in [(&dir_a, 18806u16), (&dir_b, 18807u16)] {
        std::fs::write(
            dir.join("config.json"),
            format!(
                "{{\"node_name\":\"peer\",\"port\":{},\"discovery_port\":14786}}",
                port
            ),
        )
        .unwrap();
    }
    let a = Node::start(&dir_a, 18806, 14786, "peer-a", "cat");
    let b = Node::start(&dir_b, 18807, 14786, "peer-b", "cat");

    // Some machines (cloud VMs, locked-down networks) do not deliver
    // multicast between local processes. Check that the beacons travel at all
    // before asserting on discovery.
    let probe = Command::new(BIN)
        .args(["probe", "--seconds", "3"])
        .env("HIVEMIND_DIR", &dir_a)
        .output()
        .expect("failed to run hivemind probe");
    if !probe.status.success() {
        eprintln!(
            "skipped: this machine does not deliver beacons between local processes ({})",
            String::from_utf8_lossy(&probe.stdout).trim()
        );
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut found = false;
    while Instant::now() < deadline && !found {
        let (_, body) = http_get(a.port, "/v1/peers").unwrap();
        found = body.contains("peer-b");
        if !found {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    assert!(found, "peer-b was never discovered by peer-a");

    // The CLI finds the peer through the same discovery channel.
    let (stdout, stderr, ok) = run_cli(&["ls"], &dir_a);
    assert!(ok, "ls failed: {}{}", stdout, stderr);
    assert!(stdout.contains("peer-b"), "ls did not list peer-b: {}", stdout);

    drop(a);
    drop(b);
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

#[test]
fn the_web_page_and_its_assets_are_served() {
    let dir = temp_dir("web");
    let node = Node::start(&dir, 18809, 14788, "node-w", "cat");

    let (status, body) = http_get(node.port, "/").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("Hivemind"), "page does not look like the Hivemind UI");
    assert!(body.contains("What should it do?"), "missing the main prompt field");

    let (status, body) = http_get(node.port, "/app.css").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains(".mac"), "stylesheet missing");

    let (status, body) = http_get(node.port, "/app.js").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("EventSource"), "UI script missing");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_mesh_lists_this_mac_and_its_peers() {
    let dir = temp_dir("mesh");
    let node = Node::start(&dir, 18810, 14789, "node-m", "cat");
    let (status, body) = http_get(node.port, "/v1/mesh").unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("\"is_self\":true"), "this Mac missing from the mesh");
    assert!(body.contains("node-m"), "this Mac's name missing: {}", body);
    assert!(body.contains("\"base\":\"http://"), "no address for the browser");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_are_saved_and_validated() {
    let dir = temp_dir("settings");
    let node = Node::start(&dir, 18811, 14790, "node-s", "cat");

    let (status, body) = http_post(node.port, "/v1/settings", "{\"work_dir\":\"/does/not/exist\"}")
        .unwrap();
    assert_eq!(status, 400, "a missing folder must be rejected: {}", body);

    let (status, body) =
        http_post(node.port, "/v1/settings", "{\"work_dir\":\"/tmp\",\"agent_cmd\":\"cat {prompt}\"}")
            .unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("\"work_dir\":\"/tmp\""), "folder not applied: {}", body);
    assert!(body.contains("cat {prompt}"), "agent not applied: {}", body);

    // The change is written to the config file, so it survives a restart.
    let saved = std::fs::read_to_string(dir.join("config.json")).unwrap();
    assert!(saved.contains("\"work_dir\""), "config not saved: {}", saved);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_task_runs_in_the_chosen_folder() {
    let dir = temp_dir("cwd");
    let node = Node::start(&dir, 18812, 14791, "node-cwd", "pwd");
    let (status, body) = http_post(
        node.port,
        "/v1/tasks",
        "{\"prompt\":\"where am i\",\"cwd\":\"/tmp\"}",
    )
    .unwrap();
    assert_eq!(status, 201);
    let id = json_field(&body, "id").expect("task id").to_string();
    assert_eq!(json_field(&body, "cwd"), Some("/tmp"));
    std::thread::sleep(Duration::from_millis(800));
    let (_, body) = http_get(node.port, &format!("/v1/tasks/{}", id)).unwrap();
    assert!(
        body.contains("/tmp"),
        "agent did not run in the chosen folder: {}",
        body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_api_allows_the_page_to_talk_to_other_macs() {
    let dir = temp_dir("cors");
    let node = Node::start(&dir, 18813, 14792, "node-c", "cat");
    let (status, headers) = http_get_with_headers(node.port, "/v1/info").unwrap();
    assert_eq!(status, 200);
    assert!(
        headers.contains("access-control-allow-origin: *"),
        "the page cannot talk to other Macs: {}",
        headers
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn task_history_survives_a_restart() {
    let dir = temp_dir("history");
    let node = Node::start(&dir, 18808, 14787, "node-h", "cat");
    let (status, body) = http_post(node.port, "/v1/tasks", "{\"prompt\":\"remember me\"}").unwrap();
    assert_eq!(status, 201);
    let id = json_field(&body, "id").expect("task id").to_string();
    std::thread::sleep(Duration::from_millis(800));
    drop(node);

    let node = Node::start(&dir, 18808, 14787, "node-h", "cat");
    let (status, body) = http_get(node.port, &format!("/v1/tasks/{}", id)).unwrap();
    assert_eq!(status, 200);
    assert_eq!(json_field(&body, "state"), Some("done"));

    // A task is logged twice (created, then finished) but must show up once.
    let (_, body) = http_get(node.port, "/v1/tasks").unwrap();
    let count = body.matches(&format!("\"id\":\"{}\"", id)).count();
    assert_eq!(count, 1, "task listed {} times: {}", count, body);
    let _ = std::fs::remove_dir_all(&dir);
}
