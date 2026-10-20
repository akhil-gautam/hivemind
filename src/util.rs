//! Small shared helpers: clocks, host names, shell quoting, logging.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable, human friendly name for this machine.
///
/// macOS leaves /etc/hostname empty, so ask the system for the computer name
/// and fall back to the host name.
pub fn hostname() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(out) = std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
        {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                let clean: String = name
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                let trimmed = clean.trim_matches('-').to_string();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::var("HOSTNAME")
                .map(|h| {
                    h.split('.')
                        .next()
                        .unwrap_or("mac")
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                        .collect::<String>()
                        .trim_matches('-')
                        .to_string()
                })
                .unwrap_or_else(|_| "mac".to_string())
        })
}

/// Home directory of the user running the node.
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// Expand a leading `~` to the home directory.
pub fn expand_home(path: &str) -> String {
    let home = home_dir();
    if path == "~" {
        return home;
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{}", home.trim_end_matches('/'), rest),
        None => path.to_string(),
    }
}

/// POSIX single-quote escaping so a prompt can never break out of the command.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub fn log(level: &str, msg: &str) {
    if !level_enabled(level) {
        return;
    }
    eprintln!("[{}] {}", now_ms(), msg);
}

fn level_enabled(level: &str) -> bool {
    let cur = std::env::var("HIVE_LOG").unwrap_or_else(|_| "info".to_string());
    rank(level) <= rank(&cur)
}

fn rank(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_neutralises_quotes_and_expansion() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME `id`"), "'$HOME `id`'");
    }

    #[test]
    fn hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }
}
