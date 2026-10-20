//! The node daemon: HTTP routes, worker pool, peer announcements and the
//! endpoints the browser UI talks to.

use crate::agent;
use crate::config::{self, Config};
use crate::detect;
use crate::http::{Request, Response, Server};
use crate::net::{self, Discovery, Identity, Peer};
use crate::task::{State, Task, TaskManager};
use crate::util::{expand_home, home_dir, log, now_ms};
use crate::web;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Settings the user can change from the browser while the node runs.
#[derive(Clone, Debug)]
pub struct Settings {
    pub node_name: String,
    pub agent_cmd: Option<String>,
    pub work_dir: String,
}

pub struct Node {
    pub cfg: Config,
    pub id: String,
    pub host: String,
    pub tasks: Arc<TaskManager>,
    pub discovery: Option<Arc<Discovery>>,
    pub started_ms: u64,
    pub identity: Arc<Mutex<Identity>>,
    pub settings: Mutex<Settings>,
}

impl Node {
    pub fn new(
        cfg: Config,
        id: String,
        host: String,
        tasks: Arc<TaskManager>,
        discovery: Option<Arc<Discovery>>,
        identity: Arc<Mutex<Identity>>,
    ) -> Arc<Node> {
        let settings = Settings {
            node_name: cfg.node_name.clone(),
            agent_cmd: cfg.agent_cmd.clone(),
            work_dir: cfg
                .work_dir
                .clone()
                .map(|d| expand_home(&d))
                .unwrap_or_else(home_dir),
        };
        Arc::new(Node {
            cfg,
            id,
            host,
            tasks,
            discovery,
            started_ms: now_ms(),
            identity,
            settings: Mutex::new(settings),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.cfg.port)
    }

    pub fn self_peer(&self) -> Peer {
        let settings = self.settings.lock().unwrap();
        Peer {
            id: self.id.clone(),
            name: settings.node_name.clone(),
            host: self.host.clone(),
            port: self.cfg.port,
            agent: agent_label(settings.agent_cmd.as_deref()),
            last_seen_ms: now_ms(),
        }
    }

    pub fn info(&self) -> Value {
        let settings = self.settings.lock().unwrap();
        json!({
            "id": self.id,
            "name": settings.node_name,
            "host": self.host,
            "port": self.cfg.port,
            "base": self.base_url(),
            "agent": agent_label(settings.agent_cmd.as_deref()),
            "agent_cmd": settings.agent_cmd,
            "work_dir": settings.work_dir,
            "concurrency": self.cfg.concurrency,
            "version": VERSION,
            "uptime_ms": now_ms() - self.started_ms,
            "tasks_running": self.tasks.running_count(),
            "queue_len": self.tasks.queue_len(),
            "peers": self.discovery.as_ref().map(|d| d.peers().len()).unwrap_or(0),
        })
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.discovery.as_ref().map(|d| d.peers()).unwrap_or_default()
    }

    /// Copy the live settings into what the mesh advertises.
    fn sync_identity(&self) {
        let settings = self.settings.lock().unwrap();
        let mut identity = self.identity.lock().unwrap();
        identity.name = settings.node_name.clone();
        identity.agent = agent_label(settings.agent_cmd.as_deref());
    }
}

fn agent_label(cmd: Option<&str>) -> String {
    cmd.and_then(|c| c.split_whitespace().next())
        .unwrap_or("")
        .to_string()
}

pub fn run_node(node: Arc<Node>) -> std::io::Result<()> {
    // Worker pool.
    for i in 0..node.cfg.concurrency {
        let n = node.clone();
        std::thread::Builder::new()
            .name(format!("hivemind-worker-{}", i))
            .stack_size(256 * 1024)
            .spawn(move || worker_loop(n))?;
    }

    // Peer table persistence: rewritten whenever the table changes so the
    // CLI never reads a stale cache.
    {
        let n = node.clone();
        std::thread::Builder::new()
            .name("hivemind-peers".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut last = String::new();
                loop {
                    let peers = n.peers();
                    let self_peer = n.self_peer();
                    let snapshot =
                        format!("{:?}", peers.iter().map(|p| p.addr()).collect::<Vec<_>>());
                    if snapshot != last {
                        last = snapshot;
                        net::save_peers(&config::peers_path(), &peers, Some(&self_peer));
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })?;
    }

    let handler_node = node.clone();
    let handler: crate::http::Handler = Arc::new(move |req| route(&handler_node, req));
    let addr = format!("{}:{}", node.cfg.bind, node.cfg.port);
    let server = Server::bind(&addr, handler, 64)?;
    let local = server.local_addr()?;
    log(
        "info",
        &format!(
            "node '{}' listening on {} (advertise {}:{})",
            node.cfg.node_name, local, node.host, node.cfg.port
        ),
    );
    log(
        "info",
        &format!("open this page to use Hivemind: {}/", node.base_url()),
    );
    if node.cfg.agent_cmd.is_none() {
        log(
            "warn",
            "no agent configured yet: pick one in the web page, or run `hivemind config set agent_cmd \"<cmd> {prompt}\"`",
        );
    }
    server.run();
    Ok(())
}

fn worker_loop(node: Arc<Node>) {
    while let Some(task) = node.tasks.next_task() {
        execute(&node, &task);
    }
}

fn execute(node: &Arc<Node>, task: &Arc<Task>) {
    if task.cancel.load(Ordering::Relaxed) {
        task.set_state(State::Cancelled);
        node.tasks.persist(task);
        return;
    }
    let cmd = node.settings.lock().unwrap().agent_cmd.clone();
    let Some(cmd) = cmd else {
        let mut inner = task.inner.lock().unwrap();
        inner.error = Some("no agent is set up on this Mac yet".into());
        inner.ended_ms = Some(now_ms());
        drop(inner);
        task.set_state(State::Failed);
        task.push_event("done", task.view());
        node.tasks.persist(task);
        return;
    };

    task.set_state(State::Running);
    task.push_event("state", json!({ "state": "running" }));

    let max_bytes = node.cfg.max_output_bytes;
    let t = task.clone();
    let on_stdout: agent::Sink = Arc::new(move |chunk: &str| {
        t.push_event("output", json!({ "chunk": chunk }));
    });
    let t2 = task.clone();
    let on_stderr: agent::Sink = Arc::new(move |chunk: &str| {
        t2.push_event("stderr", json!({ "chunk": chunk }));
    });

    let timeout = Duration::from_secs(node.cfg.task_timeout_s);
    let outcome = agent::run(
        &cmd,
        &task.prompt,
        task.cwd.as_deref(),
        timeout,
        max_bytes,
        &task.cancel,
        Some(on_stdout),
        Some(on_stderr),
    );

    {
        let mut inner = task.inner.lock().unwrap();
        inner.exit_code = outcome.exit_code;
        inner.output = outcome.output;
        inner.truncated = outcome.truncated;
        if outcome.cancelled {
            inner.error = Some("cancelled".into());
        } else if outcome.timed_out {
            inner.error = Some(format!("timed out after {}s", node.cfg.task_timeout_s));
        } else if outcome.exit_code == Some(0) {
            inner.error = None;
        } else {
            let tail: String = outcome.stderr.chars().rev().take(400).collect();
            let tail = tail.chars().rev().collect::<String>();
            inner.error = Some(if tail.trim().is_empty() {
                format!("agent exited with code {:?}", outcome.exit_code)
            } else {
                tail.trim().to_string()
            });
        }
    }
    let state = if outcome.cancelled {
        State::Cancelled
    } else if outcome.timed_out || outcome.exit_code != Some(0) {
        State::Failed
    } else {
        State::Done
    };
    task.set_state(state);
    task.push_event("done", task.view());
    node.tasks.persist(task);
    log("info", &format!("task {} finished: {}", task.id, state.as_str()));
}

// ------------------------------------------------------------------ routes ---

fn route(node: &Arc<Node>, req: &Request) -> Response {
    // The page may be served by one Mac and drive agents on another, so every
    // response (including the CORS preflight) allows cross-origin calls.
    let response = if req.method == "OPTIONS" {
        Response::text(204, "").with_header("Access-Control-Max-Age", "600")
    } else {
        route_inner(node, req)
    };
    response
        .with_header("Access-Control-Allow-Origin", "*")
        .with_header("Access-Control-Allow-Headers", "content-type")
        .with_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
}

fn route_inner(node: &Arc<Node>, req: &Request) -> Response {
    let path = req.path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };

    if let Some((content_type, body)) = web::asset(path) {
        return Response::asset(content_type, body);
    }
    if path == "/favicon.ico" {
        return Response::text(204, "");
    }

    match (req.method.as_str(), path) {
        ("GET", "/healthz") => Response::text(200, "ok"),
        ("GET", "/v1/info") => Response::json(200, &node.info()),
        ("GET", "/v1/mesh") => mesh(node),
        ("GET", "/v1/agents") => agents(node),
        ("POST", "/v1/settings") => update_settings(node, req),
        ("GET", "/v1/peers") => {
            let peers: Vec<Value> = node.peers().iter().map(|p| p.to_json()).collect();
            Response::json(200, &json!({ "peers": peers }))
        }
        ("GET", "/v1/tasks") => {
            let tasks: Vec<Value> = node.tasks.list().iter().map(|t| t.view()).collect();
            Response::json(200, &json!({ "tasks": tasks }))
        }
        ("POST", "/v1/tasks") => create_task(node, req),
        _ => {
            if let Some(rest) = path.strip_prefix("/v1/tasks/") {
                let (id, action) = match rest.split_once('/') {
                    Some((i, a)) => (i, a),
                    None => (rest, ""),
                };
                match (req.method.as_str(), action) {
                    ("GET", "") => match node.tasks.get(id) {
                        Some(t) => Response::json(200, &t.view()),
                        None => Response::json(404, &json!({"error": "unknown task"})),
                    },
                    ("GET", "events") => match node.tasks.get(id) {
                        Some(t) => stream_events(t),
                        None => Response::json(404, &json!({"error": "unknown task"})),
                    },
                    ("POST", "cancel") => match node.tasks.cancel(id) {
                        Ok(()) => Response::json(200, &json!({"ok": true, "id": id})),
                        Err(e) => Response::json(404, &json!({"error": e})),
                    },
                    _ => Response::json(405, &json!({"error": "method not allowed"})),
                }
            } else {
                Response::json(404, &json!({"error": "not found"}))
            }
        }
    }
}

/// This Mac plus every peer, with live status.
fn mesh(node: &Arc<Node>) -> Response {
    let peers = node.peers();
    let checked: Vec<(Peer, Option<Value>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = peers
            .iter()
            .map(|peer| {
                scope.spawn(move || {
                    let info = crate::client::probe(&peer.addr(), Duration::from_millis(700));
                    (peer.clone(), info)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let mut nodes: Vec<Value> = Vec::new();
    for (peer, info) in checked {
        let online = info.is_some();
        let name = info
            .as_ref()
            .and_then(|i| i.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or(&peer.name)
            .to_string();
        let agent = info
            .as_ref()
            .and_then(|i| i.get("agent"))
            .and_then(|n| n.as_str())
            .unwrap_or(&peer.agent)
            .to_string();
        let busy = info
            .as_ref()
            .and_then(|i| i.get("tasks_running"))
            .and_then(|n| n.as_u64())
            .unwrap_or(0)
            > 0;
        let version = info
            .as_ref()
            .and_then(|i| i.get("version"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        nodes.push(json!({
            "id": peer.id,
            "name": name,
            "base": format!("http://{}:{}", peer.host, peer.port),
            "agent": agent,
            "online": online,
            "busy": busy,
            "is_self": false,
            "version": version,
        }));
    }

    let info = node.info();
    let self_node = json!({
        "id": info.get("id").cloned().unwrap_or(Value::Null),
        "name": info.get("name").cloned().unwrap_or(Value::Null),
        "base": node.base_url(),
        "agent": info.get("agent").cloned().unwrap_or(Value::Null),
        "work_dir": info.get("work_dir").cloned().unwrap_or(Value::Null),
        "online": true,
        "busy": info.get("tasks_running").and_then(|v| v.as_u64()).unwrap_or(0) > 0,
        "is_self": true,
        "version": VERSION,
    });
    nodes.push(self_node.clone());

    Response::json(200, &json!({ "self": self_node, "nodes": nodes }))
}

/// Agent programs installed on this Mac.
fn agents(node: &Arc<Node>) -> Response {
    let settings = node.settings.lock().unwrap();
    Response::json(
        200,
        &json!({
            "current": settings.agent_cmd,
            "work_dir": settings.work_dir,
            "agents": detect::detect(),
        }),
    )
}

/// Change this Mac's agent or working folder from the browser.
fn update_settings(node: &Arc<Node>, req: &Request) -> Response {
    let Some(body) = req.json() else {
        return Response::json(400, &json!({"error": "invalid JSON body"}));
    };

    let mut cfg = config::load();
    let mut settings = node.settings.lock().unwrap();

    if let Some(cmd) = body.get("agent_cmd") {
        match cmd.as_str() {
            Some(s) if !s.trim().is_empty() => {
                settings.agent_cmd = Some(s.trim().to_string());
                cfg.agent_cmd = settings.agent_cmd.clone();
            }
            Some(_) => {
                settings.agent_cmd = None;
                cfg.agent_cmd = None;
            }
            None => {}
        }
    }
    if let Some(dir) = body.get("work_dir").and_then(|v| v.as_str()) {
        let expanded = expand_home(dir.trim());
        if !expanded.is_empty() {
            if !std::path::Path::new(&expanded).is_dir() {
                return Response::json(
                    400,
                    &json!({"error": format!("That folder does not exist: {}", expanded)}),
                );
            }
            settings.work_dir = expanded.clone();
            cfg.work_dir = Some(expanded);
        }
    }
    if let Some(name) = body.get("node_name").and_then(|v| v.as_str()) {
        let name = name.trim();
        if !name.is_empty() {
            settings.node_name = name.to_string();
            cfg.node_name = name.to_string();
        }
    }

    if let Err(e) = config::save(&cfg, &config::config_path()) {
        log("warn", &format!("could not save config: {}", e));
    }
    drop(settings);
    node.sync_identity();

    Response::json(200, &node.info())
}

fn create_task(node: &Arc<Node>, req: &Request) -> Response {
    let Some(body) = req.json() else {
        return Response::json(400, &json!({"error": "invalid JSON body"}));
    };
    let Some(prompt) = body.get("prompt").and_then(|x| x.as_str()) else {
        return Response::json(400, &json!({"error": "missing field: prompt"}));
    };
    if prompt.trim().is_empty() {
        return Response::json(400, &json!({"error": "prompt is empty"}));
    }
    let label = body
        .get("label")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    let default_dir = node.settings.lock().unwrap().work_dir.clone();
    let cwd = match body.get("cwd").and_then(|x| x.as_str()) {
        Some(dir) if !dir.trim().is_empty() => {
            let expanded = expand_home(dir.trim());
            if !std::path::Path::new(&expanded).is_dir() {
                return Response::json(
                    400,
                    &json!({"error": format!("That folder does not exist: {}", expanded)}),
                );
            }
            Some(expanded)
        }
        _ => Some(default_dir),
    };

    match node.tasks.create(
        &node.settings.lock().unwrap().node_name.clone(),
        prompt.to_string(),
        label,
        cwd,
        node.cfg.max_queue,
    ) {
        Ok(task) => Response::json(201, &task.view()),
        Err(e) => Response::json(503, &json!({"error": e})),
    }
}

/// Server-sent events for one task: state changes, live output, final result.
fn stream_events(task: Arc<Task>) -> Response {
    Response::stream(move |w: &mut dyn std::io::Write| {
        let mut cursor: u64 = 0;
        loop {
            // Collect events under the lock, then release it before touching
            // the task view (which takes the same lock).
            let (pending, terminal) = {
                let inner = task.inner.lock().unwrap();
                let pending: Vec<Value> = inner
                    .events
                    .iter()
                    .filter(|e| e.get("seq").and_then(|s| s.as_u64()).unwrap_or(0) > cursor)
                    .cloned()
                    .collect();
                (pending, inner.state.is_terminal())
            };
            for ev in pending {
                cursor = ev.get("seq").and_then(|s| s.as_u64()).unwrap_or(cursor);
                let kind = ev.get("event").and_then(|s| s.as_str()).unwrap_or("message");
                let data = ev.get("data").cloned().unwrap_or(Value::Null);
                let payload = serde_json::to_string(&data).unwrap_or_else(|_| "null".into());
                let _ = write!(w, "event: {}\ndata: {}\n\n", kind, payload);
            }
            if terminal {
                let mut view = task.view();
                if let Some(obj) = view.as_object_mut() {
                    obj.insert("final".into(), Value::Bool(true));
                }
                let payload = serde_json::to_string(&view).unwrap_or_else(|_| "{}".into());
                let _ = write!(w, "event: final\ndata: {}\n\n", payload);
                let _ = w.flush();
                break;
            }
            let guard = task.inner.lock().unwrap();
            let (guard, _timeout) = task
                .cv
                .wait_timeout(guard, Duration::from_secs(10))
                .unwrap();
            drop(guard);
            let _ = w.flush();
        }
    })
}
