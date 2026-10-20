//! hive — run CLI agents on several Macs over the home WiFi from one terminal.
//!
//! Memory discipline: one static binary, std-only networking, no async
//! runtime. A node daemon idles at a few megabytes of RSS.

mod agent;
mod client;
mod config;
mod detect;
mod http;
mod net;
mod node;
mod plist;
mod task;
mod util;
mod web;

use config::Config;
use std::io::Write;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

const VALUE_FLAGS: &[&str] = &[
    "port",
    "name",
    "agent-cmd",
    "concurrency",
    "timeout",
    "host",
    "node",
    "label",
    "config",
    "dir",
    "advertise",
    "key",
    "value",
    "max-output",
    "discovery-port",
    "group",
    "cwd",
];

struct Args {
    cmd: String,
    flags: HashMap<String, String>,
    bools: HashSet<String>,
    positional: Vec<String>,
}

impl Args {
    fn flag(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(|s| s.as_str())
    }
    fn has(&self, key: &str) -> bool {
        self.bools.contains(key)
    }
    fn num(&self, key: &str) -> Option<u64> {
        self.flag(key).and_then(|v| v.parse().ok())
    }
}

fn parse_args(argv: &[String]) -> Args {
    let mut args = Args {
        cmd: argv.get(1).cloned().unwrap_or_else(|| "help".to_string()),
        flags: HashMap::new(),
        bools: HashSet::new(),
        positional: Vec::new(),
    };
    let mut i = 2;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(rest) = a.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once('=') {
                args.flags.insert(k.to_string(), v.to_string());
                i += 1;
                continue;
            }
            let key = rest.to_string();
            let takes_value = VALUE_FLAGS.contains(&key.as_str());
            if takes_value && i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                args.flags.insert(key, argv[i + 1].clone());
                i += 2;
                continue;
            }
            args.bools.insert(key);
            i += 1;
            continue;
        }
        args.positional.push(a.clone());
        i += 1;
    }
    args
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = parse_args(&argv);
    let code = match args.cmd.as_str() {
        "serve" => cmd_serve(&args),
        "ls" | "peers" => cmd_ls(&args),
        "run" | "exec" => cmd_run(&args),
        "get" => cmd_get(&args),
        "cancel" => cmd_cancel(&args),
        "config" => cmd_config(&args),
        "install" => cmd_install(&args),
        "uninstall" => cmd_uninstall(&args),
        "open" => cmd_open(&args),
        "app" => cmd_app(&args),
        "version" => {
            println!("hive {}", node::VERSION);
            0
        }
        _ => {
            usage();
            if argv.len() > 1 && argv[1] != "help" {
                2
            } else {
                0
            }
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        r#"hive {ver} — run CLI agents across your Macs on one WiFi

USAGE
  hive serve [--port N] [--name NAME] [--agent-cmd "cmd {{prompt}}"] [--concurrency N]
             [--timeout SECS] [--no-discovery] [--advertise IP]
  hive ls [--host HOST:PORT]                 list nodes on the LAN
  hive run PROMPT [--node NAME] [--host HOST:PORT] [--label L] [--cwd DIR] [--json]
  hive get TASK_ID [--host HOST:PORT]        show one task
  hive cancel TASK_ID [--host HOST:PORT]     cancel a running task
  hive config show|path|init|set KEY VALUE   manage the node configuration
  hive open                                   start the node if needed and open the page
  hive app                                    create a double-clickable Hive.app
  hive install [--dry-run]                   install the launchd service
  hive uninstall                              remove the launchd service
  hive version

ENV
  HIVE_DIR      override the config/state directory
  HIVE_LOG      error|warn|info|debug (default info)
  HIVE_NODE_NAME override the node name
"#,
        ver = node::VERSION
    );
}

// ------------------------------------------------------------------ serve ---

fn app_dir_for(args: &Args) -> PathBuf {
    match args.flag("dir") {
        Some(d) => PathBuf::from(d),
        None => config::app_dir(),
    }
}

fn config_path_for(args: &Args) -> PathBuf {
    match args.flag("config") {
        Some(c) => PathBuf::from(c),
        None => app_dir_for(args).join("config.json"),
    }
}

fn peers_path_for(args: &Args) -> PathBuf {
    app_dir_for(args).join("peers.json")
}

fn cmd_serve(args: &Args) -> i32 {
    if let Some(dir) = args.flag("dir") {
        // Make every path helper (tasks, peers) agree with --dir.
        std::env::set_var("HIVE_DIR", dir);
    }
    let mut cfg: Config = if let Some(c) = args.flag("config") {
        config::load_from(&PathBuf::from(c))
    } else {
        config::load()
    };
    if let Some(p) = args.num("port") {
        cfg.port = p as u16;
    }
    if let Some(n) = args.flag("name") {
        cfg.node_name = n.to_string();
    }
    if let Some(c) = args.flag("agent-cmd") {
        cfg.agent_cmd = Some(c.to_string());
    }
    if let Some(c) = args.num("concurrency") {
        cfg.concurrency = c.max(1) as usize;
    }
    if let Some(t) = args.num("timeout") {
        cfg.task_timeout_s = t;
    }
    if let Some(m) = args.num("max-output") {
        cfg.max_output_bytes = m.max(1024) as usize;
    }
    if let Some(ip) = args.flag("advertise") {
        cfg.advertise_ip = Some(ip.to_string());
    }
    if args.has("no-discovery") {
        cfg.discovery_enabled = false;
    }
    if let Some(p) = args.num("discovery-port") {
        cfg.discovery_port = p as u16;
    }
    if let Some(g) = args.flag("group") {
        cfg.discovery_group = g.to_string();
    }

    let ifaces = net::interfaces();
    let host = cfg
        .advertise_ip
        .clone()
        .or_else(|| net::primary_ipv4(&ifaces).map(|ip| ip.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let id = format!("{}-{}", cfg.node_name, std::process::id());
    let tasks = task::TaskManager::new(config::tasks_path(), cfg.history);
    let identity = std::sync::Arc::new(std::sync::Mutex::new(net::Identity {
        id: id.clone(),
        name: cfg.node_name.clone(),
        host: host.clone(),
        port: cfg.port,
        agent: cfg
            .agent_cmd
            .as_deref()
            .and_then(|c| c.split_whitespace().next())
            .unwrap_or("")
            .to_string(),
    }));
    let discovery = if cfg.discovery_enabled {
        match net::Discovery::start(
            identity.clone(),
            &cfg.discovery_group,
            cfg.discovery_port,
            ifaces,
        ) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("hive: discovery unavailable: {}", e);
                None
            }
        }
    } else {
        None
    };

    let node = node::Node::new(cfg.clone(), id, host, tasks, discovery, identity);
    match node::run_node(node) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

// --------------------------------------------------------------------- ls ---

/// Nodes known from the cache plus whatever a live LAN scan turns up.
fn known_peers(args: &Args, scan: bool) -> Vec<net::Peer> {
    let mut peers = net::load_peers(&peers_path_for(args));
    if scan {
        let cfg = config::load_from(&config_path_for(args));
        let ifaces = net::interfaces();
        let found = net::scan(
            &cfg.discovery_group,
            cfg.discovery_port,
            &ifaces,
            Duration::from_millis(2500),
        );
        for peer in found {
            if !peers.iter().any(|p| p.id == peer.id) {
                peers.push(peer);
            }
        }
    }
    peers
}

fn cmd_ls(args: &Args) -> i32 {
    let timeout = Duration::from_millis(args.num("timeout").unwrap_or(800) as u64);
    let mut peers = known_peers(args, true);
    if let Some(host) = args.flag("host") {
        let addr = http::split_addr(host, 8787);
        let (h, p) = match addr.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(8787)),
            None => (addr.clone(), 8787),
        };
        peers.retain(|p| p.addr() != addr);
        peers.push(net::Peer {
            id: format!("direct-{}", h),
            name: h.clone(),
            host: h,
            port: p,
            agent: String::new(),
            last_seen_ms: util::now_ms(),
        });
    }
    if peers.is_empty() {
        eprintln!("hive: no nodes known yet — start one with `hive serve` on a Mac");
        return 1;
    }
    println!("{:<18} {:<24} {:<12} {:<8} {}", "NAME", "ADDRESS", "AGENT", "STATE", "VERSION");
    let mut any = false;
    for peer in &peers {
        match client::probe(&peer.addr(), timeout) {
            Some(info) => {
                any = true;
                let name = info
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&peer.name);
                let agent = info
                    .get("agent")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let version = info.get("version").and_then(|x| x.as_str()).unwrap_or("");
                let running = info.get("tasks_running").and_then(|x| x.as_u64()).unwrap_or(0);
                let state = if running > 0 {
                    format!("busy({})", running)
                } else {
                    "online".to_string()
                };
                println!(
                    "{:<18} {:<24} {:<12} {:<8} {}",
                    name,
                    peer.addr(),
                    agent,
                    state,
                    version
                );
            }
            None => {
                println!(
                    "{:<18} {:<24} {:<12} {:<8} {}",
                    peer.name,
                    peer.addr(),
                    peer.agent,
                    "offline",
                    ""
                );
            }
        }
    }
    if any {
        0
    } else {
        1
    }
}

// -------------------------------------------------------------------- run ---

fn cmd_run(args: &Args) -> i32 {
    let prompt = args.positional.join(" ");
    if prompt.trim().is_empty() {
        eprintln!("hive: usage: hive run \"your prompt\" [--node NAME]");
        return 2;
    }
    let timeout = Duration::from_secs(args.num("timeout").unwrap_or(900));

    let (addr, info) = match args.flag("host") {
        Some(host) => {
            let addr = http::split_addr(host, 8787);
            match client::probe(&addr, Duration::from_secs(2)) {
                Some(info) => (addr, info),
                None => {
                    eprintln!("hive: node {} is not responding", addr);
                    return 1;
                }
            }
        }
        None => {
            let peers = known_peers(args, false);
            match client::resolve(&peers, args.flag("node"), Duration::from_secs(2)) {
                Ok(v) => v,
                Err(_cache_err) => {
                    // Cache miss: ask the LAN directly before giving up.
                    let peers = known_peers(args, true);
                    match client::resolve(&peers, args.flag("node"), Duration::from_secs(2)) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("hive: {}", e);
                            eprintln!("hive: start a node with `hive serve` on the other Mac");
                            return 1;
                        }
                    }
                }
            }
        }
    };

    let node_name = info
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or(&addr)
        .to_string();
    let id = match client::submit(
        &addr,
        &prompt,
        args.flag("label"),
        args.flag("cwd"),
        Duration::from_secs(10),
    ) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("hive: {}", e);
            return 1;
        }
    };

    if args.has("json") {
        match client::follow(
            &addr,
            &id,
            |_| {},
            |_| {},
            timeout,
        ) {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                return exit_code_for(&v);
            }
            Err(e) => {
                eprintln!("hive: {}", e);
                return 1;
            }
        }
    }

    eprintln!("hive: task {} on {} ({})", id, node_name, addr);
    let mut out = std::io::stdout();
    let result = client::follow(
        &addr,
        &id,
        |chunk| {
            let _ = out.write_all(chunk.as_bytes());
            let _ = out.flush();
        },
        |chunk| {
            let _ = std::io::stderr().write_all(chunk.as_bytes());
        },
        timeout,
    );
    let _ = out.flush();
    match result {
        Ok(v) => {
            let state = v.get("state").and_then(|x| x.as_str()).unwrap_or("?");
            let started = v.get("started_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let ended = v.get("ended_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let secs = if started > 0 && ended > started {
                (ended - started) as f64 / 1000.0
            } else {
                0.0
            };
            eprintln!("hive: {} in {:.1}s", state, secs);
            if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
                if !err.is_empty() {
                    eprintln!("hive: {}", err);
                }
            }
            if state == "running" || state == "queued" {
                eprintln!(
                    "hive: client deadline reached; the node keeps working on it (hive get {})",
                    id
                );
            }
            exit_code_for(&v)
        }
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

fn exit_code_for(view: &Value) -> i32 {
    match view.get("state").and_then(|x| x.as_str()) {
        Some("done") => 0,
        _ => 1,
    }
}

// --------------------------------------------------------------------- get ---

/// Resolve the node address for commands that need one.
fn resolve_node(args: &Args) -> Result<String, String> {
    if let Some(host) = args.flag("host") {
        return Ok(http::split_addr(host, 8787));
    }
    let peers = known_peers(args, false);
    match client::resolve(&peers, args.flag("node"), Duration::from_secs(2)) {
        Ok((addr, _)) => Ok(addr),
        Err(_cache_err) => {
            let peers = known_peers(args, true);
            client::resolve(&peers, args.flag("node"), Duration::from_secs(2)).map(|(addr, _)| addr)
        }
    }
}

fn cmd_get(args: &Args) -> i32 {
    let Some(id) = args.positional.first() else {
        eprintln!("hive: usage: hive get TASK_ID");
        return 2;
    };
    let addr = match resolve_node(args) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("hive: {}", e);
            return 1;
        }
    };
    match client::get_task(&addr, id, Duration::from_secs(10)) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

fn cmd_cancel(args: &Args) -> i32 {
    let Some(id) = args.positional.first() else {
        eprintln!("hive: usage: hive cancel TASK_ID");
        return 2;
    };
    let addr = match resolve_node(args) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("hive: {}", e);
            return 1;
        }
    };
    match client::cancel_task(&addr, id, Duration::from_secs(10)) {
        Ok(()) => {
            println!("cancelled {}", id);
            0
        }
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

// ------------------------------------------------------------------ config ---

fn cmd_config(args: &Args) -> i32 {
    let path = config_path_for(args);
    let sub = args.positional.first().map(|s| s.as_str()).unwrap_or("show");
    match sub {
        "path" => {
            println!("{}", path.display());
            0
        }
        "init" => {
            let cfg = Config::default();
            if let Err(e) = config::save(&cfg, &path) {
                eprintln!("hive: {}", e);
                return 1;
            }
            println!("wrote {}", path.display());
            println!("next: hive config set agent_cmd \"claude -p {{prompt}}\"");
            0
        }
        "set" => {
            let Some(key) = args.flag("key").or_else(|| args.positional.get(1).map(|s| s.as_str())) else {
                eprintln!("hive: usage: hive config set KEY VALUE");
                return 2;
            };
            let Some(value) = args.flag("value").or_else(|| args.positional.get(2).map(|s| s.as_str())) else {
                eprintln!("hive: usage: hive config set KEY VALUE");
                return 2;
            };
            let mut cfg = config::load_from(&path);
            let parsed: Value = match value {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                other => match other.parse::<u64>() {
                    Ok(n) => Value::from(n),
                    Err(_) => Value::String(other.to_string()),
                },
            };
            match key {
                "node_name" => cfg.node_name = value.to_string(),
                "bind" => cfg.bind = value.to_string(),
                "port" => cfg.port = value.parse().unwrap_or(cfg.port),
                "agent_cmd" => cfg.agent_cmd = Some(value.to_string()),
                "concurrency" => cfg.concurrency = value.parse().unwrap_or(cfg.concurrency).max(1),
                "task_timeout_s" => cfg.task_timeout_s = value.parse().unwrap_or(cfg.task_timeout_s),
                "max_output_bytes" => {
                    cfg.max_output_bytes = value.parse().unwrap_or(cfg.max_output_bytes)
                }
                "max_queue" => cfg.max_queue = value.parse().unwrap_or(cfg.max_queue).max(1),
                "history" => cfg.history = value.parse().unwrap_or(cfg.history).max(1),
                "discovery_port" => cfg.discovery_port = value.parse().unwrap_or(cfg.discovery_port),
                "discovery_group" => cfg.discovery_group = value.to_string(),
                "discovery_enabled" => cfg.discovery_enabled = parsed.as_bool().unwrap_or(true),
                "advertise_ip" => cfg.advertise_ip = Some(value.to_string()),
                "work_dir" => cfg.work_dir = Some(value.to_string()),
                other => {
                    eprintln!("hive: unknown config key '{}'", other);
                    return 2;
                }
            }
            if let Err(e) = config::save(&cfg, &path) {
                eprintln!("hive: {}", e);
                return 1;
            }
            println!("{} = {}", key, value);
            0
        }
        _ => {
            let cfg = config::load_from(&path);
            println!("{}", serde_json::to_string_pretty(&cfg.to_json()).unwrap_or_default());
            0
        }
    }
}

// ----------------------------------------------------------------- install ---

fn cmd_install(args: &Args) -> i32 {
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hive"));
    let cfg_path = config_path_for(args);
    let log_path = app_dir_for(args).join("hive.log");
    match plist::install(
        &binary.to_string_lossy(),
        &cfg_path.to_string_lossy(),
        &log_path.to_string_lossy(),
        args.has("dry-run"),
    ) {
        Ok(()) => {
            if !args.has("dry-run") {
                let uid = unsafe { libc_getuid() };
                let status = std::process::Command::new("launchctl")
                    .arg("bootstrap")
                    .arg(format!("gui/{}", uid))
                    .arg(plist::plist_path())
                    .status();
                match status {
                    Ok(s) if s.success() => println!("launchd: service loaded"),
                    Ok(_) => println!(
                        "launchd: load it manually with: launchctl bootstrap gui/{} {}",
                        uid,
                        plist::plist_path().display()
                    ),
                    Err(e) => println!("launchd: could not run launchctl: {}", e),
                }
            }
            0
        }
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

fn cmd_uninstall(args: &Args) -> i32 {
    let uid = unsafe { libc_getuid() };
    let _ = std::process::Command::new("launchctl")
        .arg("bootout")
        .arg(format!("gui/{}", uid))
        .arg(plist::plist_path())
        .status();
    match plist::uninstall(args.has("dry-run")) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hive: {}", e);
            1
        }
    }
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

// ------------------------------------------------------------------- open ---

/// URL of this Mac's page, as other devices on the WiFi would open it.
fn local_node_url(cfg: &Config) -> String {
    let ifaces = net::interfaces();
    let ip = cfg
        .advertise_ip
        .clone()
        .or_else(|| net::primary_ipv4(&ifaces).map(|i| i.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    format!("http://{}:{}/", ip, cfg.port)
}

fn node_running(port: u16) -> bool {
    client::probe(&format!("127.0.0.1:{}", port), Duration::from_millis(600)).is_some()
}

fn start_server_in_background() -> std::io::Result<()> {
    let bin = std::env::current_exe()?;
    let dir = config::app_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hive.log"))?;
    let err = log.try_clone()?;
    std::process::Command::new(bin)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err))
        .spawn()?;
    Ok(())
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()?;
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// Open the page for a non-technical user: start the node if needed, then
/// show the page in the default browser.
fn cmd_open(args: &Args) -> i32 {
    let cfg = config::load_from(&config_path_for(args));
    if !node_running(cfg.port) {
        eprintln!("hive: starting the node…");
        if let Err(e) = start_server_in_background() {
            eprintln!("hive: could not start the node: {}", e);
            return 1;
        }
        for _ in 0..60 {
            if node_running(cfg.port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !node_running(cfg.port) {
            eprintln!("hive: the node did not start; see the log in {}", config::app_dir().display());
            return 1;
        }
    }
    let url = local_node_url(&cfg);
    if let Err(e) = open_in_browser(&url) {
        eprintln!("hive: could not open a browser: {}", e);
        println!("{}", url);
        return 1;
    }
    println!("{}", url);
    0
}

/// Create a double-clickable app in ~/Applications.
fn cmd_app(_args: &Args) -> i32 {
    let bin = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hive: {}", e);
            return 1;
        }
    };
    let home = util::home_dir();
    let app = PathBuf::from(&home).join("Applications/Hive.app");
    let macos = app.join("Contents/MacOS");
    if let Err(e) = std::fs::create_dir_all(&macos) {
        eprintln!("hive: {}", e);
        return 1;
    }
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Hive</string>
    <key>CFBundleDisplayName</key><string>Hive</string>
    <key>CFBundleIdentifier</key><string>io.hive.app</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>Hive</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
        version = node::VERSION
    );
    let launcher = format!(
        "#!/bin/sh\nexec \"{}\" open\n",
        bin.to_string_lossy()
    );
    if let Err(e) = std::fs::write(app.join("Contents/Info.plist"), plist) {
        eprintln!("hive: {}", e);
        return 1;
    }
    let script = macos.join("Hive");
    if let Err(e) = std::fs::write(&script, launcher) {
        eprintln!("hive: {}", e);
        return 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    println!("created {}", app.display());
    println!("Double-click it in Finder to open Hive.");
    0
}
