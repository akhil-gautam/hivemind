//! Configuration + on-disk locations.
//!
//! One JSON file holds the whole node configuration. CLI flags override file
//! values, so the same binary works for a laptop, a desktop and a server.

use crate::util::hostname;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub node_name: String,
    pub bind: String,
    pub port: u16,
    /// Shell command template run for each task. `{prompt}` is substituted
    /// (shell-quoted); without it the prompt is piped on stdin.
    pub agent_cmd: Option<String>,
    pub concurrency: usize,
    pub task_timeout_s: u64,
    pub max_output_bytes: usize,
    pub max_queue: usize,
    pub history: usize,
    pub discovery_port: u16,
    pub discovery_group: String,
    pub discovery_enabled: bool,
    /// IP advertised to peers; `None` = auto-detect primary LAN interface.
    pub advertise_ip: Option<String>,
    /// Folder agents run in when a task does not name one.
    pub work_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            node_name: hostname(),
            bind: "0.0.0.0".into(),
            port: 8787,
            agent_cmd: None,
            concurrency: 1,
            task_timeout_s: 900,
            max_output_bytes: 64 * 1024,
            max_queue: 64,
            history: 256,
            discovery_port: 4789,
            discovery_group: "239.255.42.99".into(),
            discovery_enabled: true,
            advertise_ip: None,
            work_dir: None,
        }
    }
}

impl Config {
    pub fn from_json(v: &serde_json::Value) -> Config {
        let mut c = Config::default();
        if let Some(s) = v.get("node_name").and_then(|x| x.as_str()) {
            c.node_name = s.to_string();
        }
        if let Some(s) = v.get("bind").and_then(|x| x.as_str()) {
            c.bind = s.to_string();
        }
        if let Some(n) = v.get("port").and_then(|x| x.as_u64()) {
            c.port = n as u16;
        }
        match v.get("agent_cmd") {
            Some(serde_json::Value::String(s)) => c.agent_cmd = Some(s.clone()),
            _ => {}
        }
        if let Some(n) = v.get("concurrency").and_then(|x| x.as_u64()) {
            c.concurrency = n.max(1) as usize;
        }
        if let Some(n) = v.get("task_timeout_s").and_then(|x| x.as_u64()) {
            c.task_timeout_s = n;
        }
        if let Some(n) = v.get("max_output_bytes").and_then(|x| x.as_u64()) {
            c.max_output_bytes = n.max(1024) as usize;
        }
        if let Some(n) = v.get("max_queue").and_then(|x| x.as_u64()) {
            c.max_queue = n.max(1) as usize;
        }
        if let Some(n) = v.get("history").and_then(|x| x.as_u64()) {
            c.history = n.max(1) as usize;
        }
        if let Some(n) = v.get("discovery_port").and_then(|x| x.as_u64()) {
            c.discovery_port = n as u16;
        }
        if let Some(s) = v.get("discovery_group").and_then(|x| x.as_str()) {
            c.discovery_group = s.to_string();
        }
        if let Some(b) = v.get("discovery_enabled").and_then(|x| x.as_bool()) {
            c.discovery_enabled = b;
        }
        if let Some(s) = v.get("advertise_ip").and_then(|x| x.as_str()) {
            c.advertise_ip = Some(s.to_string());
        }
        if let Some(s) = v.get("work_dir").and_then(|x| x.as_str()) {
            c.work_dir = Some(s.to_string());
        }
        c
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "node_name": self.node_name,
            "bind": self.bind,
            "port": self.port,
            "agent_cmd": self.agent_cmd,
            "concurrency": self.concurrency,
            "task_timeout_s": self.task_timeout_s,
            "max_output_bytes": self.max_output_bytes,
            "max_queue": self.max_queue,
            "history": self.history,
            "discovery_port": self.discovery_port,
            "discovery_group": self.discovery_group,
            "discovery_enabled": self.discovery_enabled,
            "advertise_ip": self.advertise_ip,
            "work_dir": self.work_dir,
        })
    }
}

/// Directory holding config.json, tasks.jsonl and peers.json.
pub fn app_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HIVEMIND_DIR") {
        return PathBuf::from(p);
    }
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join("Library/Application Support/hivemind")
    } else if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(x).join("hivemind")
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/hivemind")
    }
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub fn tasks_path() -> PathBuf {
    app_dir().join("tasks.jsonl")
}

pub fn peers_path() -> PathBuf {
    app_dir().join("peers.json")
}

pub fn load() -> Config {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> Config {
    let mut cfg = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Config::from_json(&v),
            Err(e) => {
                eprintln!("hivemind: bad config {}: {}", path.display(), e);
                Config::default()
            }
        },
        Err(_) => Config::default(),
    };
    if let Ok(name) = std::env::var("HIVEMIND_NODE_NAME") {
        cfg.node_name = name;
    }
    cfg
}

pub fn save(cfg: &Config, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&cfg.to_json())?;
    std::fs::write(path, text + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_survives_a_json_round_trip() {
        let mut cfg = Config::default();
        cfg.node_name = "studio".into();
        cfg.port = 9000;
        cfg.agent_cmd = Some("claude -p {prompt}".into());
        cfg.concurrency = 3;
        cfg.discovery_enabled = false;
        let restored = Config::from_json(&cfg.to_json());
        assert_eq!(restored.node_name, "studio");
        assert_eq!(restored.port, 9000);
        assert_eq!(restored.agent_cmd.as_deref(), Some("claude -p {prompt}"));
        assert_eq!(restored.concurrency, 3);
        assert!(!restored.discovery_enabled);
    }
}
