//! Detect which agent CLIs are installed on this Mac.
//!
//! A candidate is only offered if its `--help` output mentions the flag the
//! template relies on, so the UI never suggests a command that cannot work.

use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Candidate {
    id: &'static str,
    label: &'static str,
    cmd: &'static str,
    needle: &'static str,
}

const CANDIDATES: &[Candidate] = &[
    Candidate {
        id: "claude",
        label: "Claude Code",
        cmd: "claude -p {prompt}",
        needle: "--print",
    },
    Candidate {
        id: "opencode",
        label: "OpenCode",
        cmd: "opencode run {prompt}",
        needle: "run",
    },
    Candidate {
        id: "codex",
        label: "Codex CLI",
        cmd: "codex exec {prompt}",
        needle: "exec",
    },
    Candidate {
        id: "gemini",
        label: "Gemini CLI",
        cmd: "gemini -p {prompt}",
        needle: "--prompt",
    },
    Candidate {
        id: "ollama",
        label: "Ollama (local models)",
        cmd: "ollama run llama3",
        needle: "run",
    },
    Candidate {
        id: "aider",
        label: "Aider",
        cmd: "aider --message {prompt}",
        needle: "--message",
    },
    Candidate {
        id: "goose",
        label: "Goose",
        cmd: "goose run -t {prompt}",
        needle: "run",
    },
];

pub fn on_path(bin: &str) -> bool {
    for dir in crate::util::augmented_path().split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(bin);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Run `<bin> --help` and look for `needle` in the output.
fn supports(bin: &str, needle: &str) -> bool {
    let mut child = match Command::new(bin)
        .arg("--help")
        .env("PATH", crate::util::augmented_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => return false,
        }
    }
    let out = child.wait_with_output().ok();
    let Some(out) = out else {
        return false;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.contains(needle)
}

/// Agents found on this machine, checked in parallel.
pub fn detect() -> Vec<Value> {
    let found: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<_> = CANDIDATES
            .iter()
            .filter(|c| on_path(c.id))
            .map(|c| {
                scope.spawn(move || {
                    if supports(c.id, c.needle) {
                        json!({
                            "id": c.id,
                            "label": c.label,
                            "cmd": c.cmd,
                        })
                    } else {
                        Value::Null
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|v| !v.is_null())
            .collect()
    });
    found
}
