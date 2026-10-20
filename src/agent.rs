//! Runs the configured agent CLI as a child process.
//!
//! The command template is executed with `/bin/sh`. `{prompt}` is replaced by
//! a shell-quoted prompt; without the placeholder the prompt is piped on
//! stdin. The child runs in its own process group so a cancel or timeout kills
//! the whole tree, not just the shell.

use crate::util::shell_quote;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Outcome {
    pub exit_code: Option<i32>,
    pub output: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

pub fn run(
    template: &str,
    prompt: &str,
    cwd: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
    cancel: &Arc<AtomicBool>,
    on_stdout: Option<Sink>,
    on_stderr: Option<Sink>,
) -> Outcome {
    let uses_placeholder = template.contains("{prompt}");
    let script = if uses_placeholder {
        template.replace("{prompt}", &shell_quote(prompt))
    } else {
        template.to_string()
    };

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&script);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.process_group(0);
    if let Some(dir) = cwd {
        if !dir.is_empty() {
            cmd.current_dir(dir);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                exit_code: None,
                output: String::new(),
                stderr: format!("failed to start agent command: {}", e),
                truncated: false,
                timed_out: false,
                cancelled: false,
            }
        }
    };
    let pid = child.id() as i32;

    if !uses_placeholder {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes());
            let _ = stdin.write_all(b"\n");
            drop(stdin);
        }
    } else if let Some(stdin) = child.stdin.take() {
        drop(stdin);
    }

    let out_buf = Arc::new(Mutex::new(String::new()));
    let err_buf = Arc::new(Mutex::new(String::new()));
    let truncated = Arc::new(AtomicBool::new(false));

    let t_out = child.stdout.take().map(|stdout| {
        let buf = out_buf.clone();
        let truncated = truncated.clone();
        let sink = on_stdout.clone();
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut stdout = stdout;
                let mut chunk = [0u8; 4096];
                loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&chunk[..n]).to_string();
                            if let Some(s) = &sink {
                                s(&text);
                            }
                            let mut b = buf.lock().unwrap();
                            if b.len() + text.len() <= max_bytes {
                                b.push_str(&text);
                            } else {
                                truncated.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
    });

    let t_err = child.stderr.take().map(|stderr| {
        let buf = err_buf.clone();
        let sink = on_stderr.clone();
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut stderr = stderr;
                let mut chunk = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&chunk[..n]).to_string();
                            if let Some(s) = &sink {
                                s(&text);
                            }
                            let mut b = buf.lock().unwrap();
                            if b.len() + text.len() <= max_bytes / 4 {
                                b.push_str(&text);
                            }
                        }
                    }
                }
            })
    });

    let start = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(_) => break None,
        }
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            kill_group(pid);
            let _ = child.kill();
            break child.wait().ok().and_then(|s| s.code());
        }
        if !timeout.is_zero() && start.elapsed() >= timeout {
            timed_out = true;
            kill_group(pid);
            let _ = child.kill();
            break child.wait().ok().and_then(|s| s.code());
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    if let Some(t) = t_out {
        if let Ok(h) = t {
            let _ = h.join();
        }
    }
    if let Some(t) = t_err {
        if let Ok(h) = t {
            let _ = h.join();
        }
    }

    let output = out_buf.lock().unwrap().clone();
    let stderr = err_buf.lock().unwrap().clone();

    Outcome {
        exit_code,
        output,
        stderr,
        truncated: truncated.load(Ordering::Relaxed),
        timed_out,
        cancelled,
    }
}

/// Kill the child's whole process group (its own pid is the group id).
fn kill_group(pid: i32) {
    let _ = Command::new("/bin/kill")
        .arg("-KILL")
        .arg(format!("-{}", pid))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
