//! Task model, in-memory manager and append-only persistence.
//!
//! Memory discipline: only a bounded preview of each task's output is kept in
//! RAM; full results live in `tasks.jsonl` and are streamed to clients.

use crate::util::{log, now_ms};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

const MAX_EVENTS: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }
    pub fn from_str(s: &str) -> State {
        match s {
            "running" => State::Running,
            "done" => State::Done,
            "failed" => State::Failed,
            "cancelled" => State::Cancelled,
            _ => State::Queued,
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

pub struct TaskInner {
    pub state: State,
    pub created_ms: u64,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: Option<String>,
    pub truncated: bool,
    pub events: VecDeque<Value>,
    pub next_seq: u64,
}

pub struct Task {
    pub id: String,
    pub node: String,
    pub prompt: String,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub cancel: Arc<AtomicBool>,
    pub inner: Mutex<TaskInner>,
    pub cv: Condvar,
}

impl Task {
    pub fn new(
        id: String,
        node: String,
        prompt: String,
        label: Option<String>,
        cwd: Option<String>,
    ) -> Arc<Task> {
        Arc::new(Task {
            id,
            node,
            prompt,
            label,
            cwd,
            cancel: Arc::new(AtomicBool::new(false)),
            inner: Mutex::new(TaskInner {
                state: State::Queued,
                created_ms: now_ms(),
                started_ms: None,
                ended_ms: None,
                exit_code: None,
                output: String::new(),
                error: None,
                truncated: false,
                events: VecDeque::new(),
                next_seq: 0,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn push_event(&self, kind: &str, data: Value) {
        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.events.push_back(json!({"seq": seq, "event": kind, "data": data}));
        while inner.events.len() > MAX_EVENTS {
            inner.events.pop_front();
        }
        self.cv.notify_all();
        drop(inner);
    }

    pub fn set_state(&self, state: State) {
        let mut inner = self.inner.lock().unwrap();
        inner.state = state;
        if state == State::Running && inner.started_ms.is_none() {
            inner.started_ms = Some(now_ms());
        }
        if state.is_terminal() {
            inner.ended_ms = Some(now_ms());
        }
        self.cv.notify_all();
        drop(inner);
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state
    }

    pub fn view(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "id": self.id,
            "node": self.node,
            "prompt": self.prompt,
            "label": inner_label(self),
            "cwd": self.cwd,
            "state": inner.state.as_str(),
            "created_ms": inner.created_ms,
            "started_ms": inner.started_ms,
            "ended_ms": inner.ended_ms,
            "exit_code": inner.exit_code,
            "output": inner.output,
            "error": inner.error,
            "truncated": inner.truncated,
        })
    }
}

fn inner_label(task: &Task) -> Option<String> {
    task.label.clone()
}

pub struct TaskManager {
    inner: Mutex<ManagerInner>,
    cv: Condvar,
    persist_path: std::path::PathBuf,
    max_history: usize,
}

struct ManagerInner {
    tasks: HashMap<String, Arc<Task>>,
    order: VecDeque<String>,
    queue: VecDeque<String>,
    shutdown: bool,
}

impl TaskManager {
    pub fn new(persist_path: std::path::PathBuf, max_history: usize) -> Arc<TaskManager> {
        let mgr = Arc::new(TaskManager {
            inner: Mutex::new(ManagerInner {
                tasks: HashMap::new(),
                order: VecDeque::new(),
                queue: VecDeque::new(),
                shutdown: false,
            }),
            cv: Condvar::new(),
            persist_path,
            max_history,
        });
        mgr.load_history();
        mgr
    }

    /// Re-arm tasks that were running when the daemon stopped.
    fn load_history(&self) {
        let Ok(text) = std::fs::read_to_string(&self.persist_path) else {
            return;
        };
        let mut lines: Vec<&str> = text.lines().collect();
        if lines.len() > 4096 {
            lines = lines.split_off(lines.len() - 4096);
        }
        let mut inner = self.inner.lock().unwrap();
        // A task is written to the log more than once (created, then finished),
        // so keep only the newest record per id.
        let mut newest: HashMap<String, Value> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for line in lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
                continue;
            };
            if !newest.contains_key(id) {
                order.push(id.to_string());
            }
            newest.insert(id.to_string(), v);
        }

        for id in order {
            let Some(v) = newest.get(&id) else {
                continue;
            };
            let state = State::from_str(v.get("state").and_then(|x| x.as_str()).unwrap_or("queued"));
            let task = Task::new(
                id.clone(),
                v.get("node").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                v.get("prompt").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                v.get("label").and_then(|x| x.as_str()).map(|s| s.to_string()),
                v.get("cwd").and_then(|x| x.as_str()).map(|s| s.to_string()),
            );
            {
                let mut i = task.inner.lock().unwrap();
                if state == State::Running || state == State::Queued {
                    // Interrupted by a restart: mark as failed so the client is
                    // not left waiting on a task that will never finish.
                    i.state = State::Failed;
                    i.error = Some("node restarted while task was in flight".into());
                    i.ended_ms = Some(now_ms());
                } else {
                    i.state = state;
                    i.created_ms = v.get("created_ms").and_then(|x| x.as_u64()).unwrap_or(0);
                    i.started_ms = v.get("started_ms").and_then(|x| x.as_u64());
                    i.ended_ms = v.get("ended_ms").and_then(|x| x.as_u64());
                    i.exit_code = v.get("exit_code").and_then(|x| x.as_i64()).map(|x| x as i32);
                    i.output = v
                        .get("output")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    i.error = v.get("error").and_then(|x| x.as_str()).map(|s| s.to_string());
                }
            }
            inner.tasks.insert(id.clone(), task);
            inner.order.push_back(id);
        }
        while inner.order.len() > self.max_history {
            if let Some(old) = inner.order.pop_front() {
                inner.tasks.remove(&old);
            }
        }
    }

    pub fn create(
        &self,
        node: &str,
        prompt: String,
        label: Option<String>,
        cwd: Option<String>,
        max_queue: usize,
    ) -> Result<Arc<Task>, String> {
        let id = format!("t-{}-{}", now_ms() as u64 & 0xffffff, short_rand());
        let task = Task::new(id.clone(), node.to_string(), prompt, label, cwd);
        let mut inner = self.inner.lock().unwrap();
        if inner.queue.len() >= max_queue {
            return Err("queue full: too many tasks waiting on this node".into());
        }
        inner.tasks.insert(id.clone(), task.clone());
        inner.order.push_back(id.clone());
        inner.queue.push_back(id.clone());
        while inner.order.len() > self.max_history {
            if let Some(old) = inner.order.pop_front() {
                if inner.queue.iter().any(|q| q == &old) {
                    continue;
                }
                inner.tasks.remove(&old);
            }
        }
        drop(inner);
        self.cv.notify_all();
        self.persist(&task);
        Ok(task)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.inner.lock().unwrap().tasks.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Task>> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .rev()
            .filter_map(|id| inner.tasks.get(id).cloned())
            .collect()
    }

    pub fn queue_len(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    pub fn running_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .tasks
            .values()
            .filter(|t| t.state() == State::Running)
            .count()
    }

    /// Pop the next queued task, blocking until one is available.
    pub fn next_task(&self) -> Option<Arc<Task>> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            if let Some(id) = inner.queue.pop_front() {
                let task = inner.tasks.get(&id).cloned();
                if let Some(t) = task {
                    if t.state() == State::Queued {
                        return Some(t);
                    }
                }
                continue;
            }
            inner = self.cv.wait(inner).unwrap();
        }
    }

    pub fn cancel(&self, id: &str) -> Result<(), String> {
        let task = self.get(id).ok_or_else(|| format!("unknown task {}", id))?;
        task.cancel.store(true, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        inner.queue.retain(|q| q != id);
        drop(inner);
        if task.state() == State::Queued {
            task.set_state(State::Cancelled);
            {
                let mut i = task.inner.lock().unwrap();
                i.error = Some("cancelled before start".into());
            }
            self.persist(&task);
            return Ok(());
        }
        Ok(())
    }

    pub fn persist(&self, task: &Task) {
        let view = task.view();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.persist_path)
        {
            if let Ok(line) = serde_json::to_string(&view) {
                let _ = writeln!(f, "{}", line);
            }
        } else {
            log("warn", "could not open task log for append");
        }
    }
}

fn short_rand() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("{:04x}", nanos & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_states_round_trip_through_strings() {
        for state in [State::Queued, State::Running, State::Done, State::Failed, State::Cancelled] {
            assert_eq!(State::from_str(state.as_str()), state);
        }
        assert!(State::Done.is_terminal());
        assert!(!State::Running.is_terminal());
    }

    #[test]
    fn a_task_keeps_its_events_in_order() {
        let task = Task::new("t-1".into(), "node".into(), "prompt".into(), None, None);
        task.push_event("output", json!({"chunk": "a"}));
        task.push_event("output", json!({"chunk": "b"}));
        let inner = task.inner.lock().unwrap();
        let kinds: Vec<&str> = inner
            .events
            .iter()
            .map(|e| e.get("event").and_then(|s| s.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(kinds, vec!["output", "output"]);
        let seqs: Vec<u64> = inner
            .events
            .iter()
            .map(|e| e.get("seq").and_then(|s| s.as_u64()).unwrap_or(0))
            .collect();
        assert_eq!(seqs, vec![0, 1]);
    }
}
