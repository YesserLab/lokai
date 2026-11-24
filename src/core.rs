//! Conversation state and the agent loop (runs on worker threads, UI reads the shared state).

use crate::api::{self, Turn};
use crate::config::{self, Approval, Config};
use crate::tools;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Approval,
    Running,
    Done,
    Error,
    Denied,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ToolItem {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub detail: String,
    pub output: String,
    pub status: ToolStatus,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    User { text: String },
    Assistant { text: String, reasoning: String, streaming: bool },
    Tool(ToolItem),
    Error { text: String },
    Info { text: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conn {
    Unknown,
    Checking,
    Connected,
    Error,
    Blocked,
}

#[derive(Clone, Copy, Debug)]
pub enum Decision {
    Allow,
    Always,
    Deny,
}

pub struct State {
    pub config: Config,
    pub items: Vec<Item>,
    pub api: Vec<Value>,
    pub busy: bool,
    pub generating: bool,
    pub status: String,
    pub conn: Conn,
    pub conn_detail: String,
    pub latency_ms: Option<u64>,
    pub models: Vec<String>,
    /// Generation speed: live estimate while streaming, server-reported afterwards.
    pub tps: f32,
    pub context_tokens: u64,
    pub cwd: PathBuf,
    pub session_allowed: HashSet<String>,
    approvals: HashMap<String, mpsc::Sender<Decision>>,
    run_id: u64,
    cancel: Arc<AtomicBool>,
}

impl State {
    pub fn pending_approval(&self) -> Option<String> {
        self.approvals.keys().next().cloned()
    }
}

pub struct Core {
    state: Mutex<State>,
    pgid: Arc<AtomicI32>,
    repaint: Mutex<Option<eframe::egui::Context>>,
    health: mpsc::Sender<()>,
}

#[derive(Serialize, Deserialize, Default)]
struct History {
    items: Vec<Item>,
    api: Vec<Value>,
    cwd: Option<PathBuf>,
    context_tokens: u64,
}

fn history_path() -> PathBuf {
    config::data_dir().join("history.json")
}

impl Core {
    pub fn new() -> Arc<Self> {
        let config = Config::load();
        let mut state = State {
            cwd: config.start_dir(),
            config,
            items: Vec::new(),
            api: Vec::new(),
            busy: false,
            generating: false,
            status: String::new(),
            conn: Conn::Unknown,
            conn_detail: String::new(),
            latency_ms: None,
            models: Vec::new(),
            tps: 0.0,
            context_tokens: 0,
            session_allowed: HashSet::new(),
            approvals: HashMap::new(),
            run_id: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if state.config.keep_history
            && let Some(h) = std::fs::read_to_string(history_path()).ok().and_then(|t| serde_json::from_str::<History>(&t).ok())
        {
            state.items = h.items;
            state.api = h.api;
            state.context_tokens = h.context_tokens;
            // Sessions saved before the scratch directory existed started in home; leave those in the start dir.
            if let Some(cwd) = h.cwd.filter(|c| c.is_dir() && *c != config::home_dir()) {
                state.cwd = cwd;
            }
        }
        let (health, rx) = mpsc::channel();
        let core = Arc::new(Self { state: Mutex::new(state), pgid: Arc::new(AtomicI32::new(0)), repaint: Mutex::new(None), health });
        let c = core.clone();
        std::thread::Builder::new().name("health".into()).spawn(move || c.health_loop(rx)).unwrap();
        core
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_repaint(&self, ctx: Option<eframe::egui::Context>) {
        *self.repaint.lock().unwrap() = ctx;
    }

    pub fn notify(&self) {
        if let Some(ctx) = self.repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    // ------------------------------------------------------------------ connection

    pub fn check_connection(&self) {
        let _ = self.health.send(());
    }

    fn health_loop(self: Arc<Self>, rx: mpsc::Receiver<()>) {
        loop {
            let cfg = self.lock().config.clone();
            if let Some(err) = cfg.policy_error() {
                let mut st = self.lock();
                st.conn = Conn::Blocked;
                st.conn_detail = err;
                st.latency_ms = None;
            } else {
                {
                    let mut st = self.lock();
                    if st.conn != Conn::Connected {
                        st.conn = Conn::Checking;
                    }
                }
                self.notify();
                let result = api::Client::new(&cfg).models();
                let mut st = self.lock();
                if st.config.server_url != cfg.server_url {
                    continue; // settings changed meanwhile; check again right away
                }
                match result {
                    Ok((models, latency)) => {
                        st.conn = Conn::Connected;
                        st.conn_detail.clear();
                        st.latency_ms = Some(latency.as_millis() as u64);
                        if st.config.model.is_empty()
                            && let Some(first) = models.first()
                        {
                            st.config.model = first.clone();
                            let _ = st.config.save();
                        }
                        st.models = models;
                    }
                    Err(e) => {
                        st.conn = Conn::Error;
                        st.conn_detail = e;
                        st.latency_ms = None;
                    }
                }
            }
            self.notify();
            // Re-check every 30 s, or immediately when asked.
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => while rx.try_recv().is_ok() {},
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    pub fn set_config(&self, cfg: Config) -> std::io::Result<()> {
        cfg.save()?;
        {
            let mut st = self.lock();
            if st.config.server_url != cfg.server_url || st.config.api_key != cfg.api_key {
                st.models.clear();
                st.conn = Conn::Unknown;
            }
            if !st.busy && (st.items.is_empty() || st.cwd == st.config.scratch_dir()) {
                // a fresh session, or one working in the scratch dir, follows the new settings
                st.cwd = cfg.start_dir();
            }
            st.config = cfg;
        }
        self.check_connection();
        self.notify();
        Ok(())
    }

    pub fn set_model(&self, model: &str) {
        let mut st = self.lock();
        if st.config.model != model {
            st.config.model = model.to_string();
            let _ = st.config.save();
        }
    }

    pub fn set_approval(&self, approval: Approval) {
        let mut st = self.lock();
        st.config.approval = approval;
        let _ = st.config.save();
    }

    // ------------------------------------------------------------------ conversation

    pub fn send(self: &Arc<Self>, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let (run_id, cancel) = {
            let mut st = self.lock();
            if st.busy {
                return false;
            }
            repair_dangling_tool_calls(&mut st.api);
            if st.api.is_empty() {
                let prompt = system_prompt(&st.config, &st.cwd);
                st.api.push(json!({"role": "system", "content": prompt}));
            }
            st.api.push(json!({"role": "user", "content": text}));
            st.items.push(Item::User { text: text.to_string() });
            st.busy = true;
            st.run_id += 1;
            st.cancel = Arc::new(AtomicBool::new(false));
            (st.run_id, st.cancel.clone())
        };
        self.notify();
        let core = self.clone();
        std::thread::Builder::new().name("agent".into()).spawn(move || core.run(run_id, cancel)).unwrap();
        true
    }

    pub fn stop(&self) {
        {
            let mut st = self.lock();
            if !st.busy {
                return;
            }
            st.run_id += 1;
            st.cancel.store(true, Ordering::SeqCst);
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid > 0 {
                // SAFETY: plain syscall on the process group of the running tool.
                unsafe { libc::kill(-pgid, libc::SIGTERM) };
            }
            // Keep what was streamed so far.
            if let Some(pos) = st.items.iter().rposition(|i| matches!(i, Item::Assistant { streaming: true, .. }))
                && let Item::Assistant { text, reasoning, streaming } = &mut st.items[pos]
            {
                *streaming = false;
                let (text, empty) = (text.clone(), text.trim().is_empty() && reasoning.trim().is_empty());
                if !text.trim().is_empty() {
                    st.api.push(json!({"role": "assistant", "content": text}));
                }
                if empty {
                    st.items.remove(pos);
                }
            }
            for item in st.items.iter_mut() {
                if let Item::Tool(t) = item
                    && matches!(t.status, ToolStatus::Running | ToolStatus::Approval)
                {
                    t.status = ToolStatus::Denied;
                    t.output = "Cancelled".into();
                }
            }
            st.approvals.clear();
            st.busy = false;
            st.generating = false;
            st.status.clear();
        }
        self.save_history();
        self.notify();
    }

    pub fn new_chat(&self) {
        self.stop();
        {
            let mut st = self.lock();
            st.items.clear();
            st.api.clear();
            st.session_allowed.clear();
            st.context_tokens = 0;
            st.cwd = st.config.start_dir();
        }
        self.save_history();
        self.notify();
    }

    pub fn resolve(&self, id: &str, decision: Decision) {
        if let Some(tx) = self.lock().approvals.remove(id) {
            let _ = tx.send(decision);
        }
    }

    fn save_history(&self) {
        let (keep, data) = {
            let st = self.lock();
            let h = History { items: st.items.clone(), api: st.api.clone(), cwd: Some(st.cwd.clone()), context_tokens: st.context_tokens };
            (st.config.keep_history, serde_json::to_string(&h))
        };
        let path = history_path();
        if !keep {
            let _ = std::fs::remove_file(path);
            return;
        }
        if let Ok(data) = data {
            let _ = std::fs::create_dir_all(config::data_dir());
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, data).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    // ------------------------------------------------------------------ agent loop

    /// Locks the state only if this run is still the current one.
    fn lock_run(&self, run_id: u64) -> Option<MutexGuard<'_, State>> {
        let st = self.lock();
        (st.run_id == run_id).then_some(st)
    }

    fn finish(&self, run_id: u64, message: Option<Item>) {
        if let Some(mut st) = self.lock_run(run_id) {
            if let Some(m) = message {
                st.items.push(m);
            }
            st.busy = false;
            st.generating = false;
            st.status.clear();
        }
        self.save_history();
        self.notify();
    }

    fn run(self: Arc<Self>, run_id: u64, cancel: Arc<AtomicBool>) {
        let mut rounds = 0;
        loop {
            let Some(mut st) = self.lock_run(run_id) else {
                return;
            };
            let cfg = st.config.clone();
            if let Some(err) = cfg.policy_error() {
                drop(st);
                return self.finish(run_id, Some(Item::Error { text: err }));
            }
            if cfg.model.is_empty() {
                drop(st);
                return self.finish(run_id, Some(Item::Error { text: "No model selected. Pick one in the settings (Ctrl+,).".into() }));
            }
            let mut body = json!({
                "model": cfg.model,
                "messages": st.api,
                "stream": true,
                "stream_options": {"include_usage": true},
                "temperature": cfg.temperature,
            });
            if cfg.max_tokens > 0 {
                body["max_tokens"] = json!(cfg.max_tokens);
            }
            if cfg.tools_enabled {
                body["tools"] = tools::definitions();
            }
            st.items.push(Item::Assistant { text: String::new(), reasoning: String::new(), streaming: true });
            let idx = st.items.len() - 1;
            st.status = format!("Waiting for {}…", cfg.model);
            st.generating = true;
            drop(st);
            self.notify();

            let mut turn = Turn::default();
            let mut first: Option<Instant> = None;
            let mut last_ui = Instant::now();
            let result = api::Client::new(&cfg).chat_stream(&body, |chunk| {
                if cancel.load(Ordering::SeqCst) {
                    return false;
                }
                turn.apply(chunk);
                let now = Instant::now();
                if turn.deltas > 0 && first.is_none() {
                    first = Some(now);
                }
                if now.duration_since(last_ui) > Duration::from_millis(50) {
                    last_ui = now;
                    let Some(mut st) = self.lock_run(run_id) else {
                        return false;
                    };
                    let (text, reasoning) = turn.split();
                    if let Some(Item::Assistant { text: t, reasoning: r, .. }) = st.items.get_mut(idx) {
                        *t = text;
                        *r = reasoning;
                    }
                    if let Some(f) = first {
                        let secs = now.duration_since(f).as_secs_f32();
                        if secs > 0.5 {
                            st.tps = (turn.deltas - 1) as f32 / secs;
                        }
                    }
                    st.status = if !turn.tool_calls.is_empty() {
                        "Preparing tool call…".into()
                    } else if turn.content.is_empty() && !turn.reasoning.is_empty() {
                        "Thinking…".into()
                    } else {
                        "Writing…".into()
                    };
                    drop(st);
                    self.notify();
                }
                true
            });
            if cancel.load(Ordering::SeqCst) {
                return;
            }

            let Some(mut st) = self.lock_run(run_id) else {
                return;
            };
            st.generating = false;
            if let Some(speed) = turn.server_speed() {
                st.tps = speed;
            } else if let (Some(n), Some(f)) = (turn.completion_tokens(), first) {
                let secs = f.elapsed().as_secs_f32();
                if n > 1 && secs > 0.2 {
                    st.tps = (n - 1) as f32 / secs;
                }
            }
            let (text, reasoning) = turn.split();
            let calls: Vec<api::ToolCall> = turn.tool_calls.iter().filter(|c| !c.name.is_empty()).cloned().collect();
            let error = result.err().or(turn.error.clone());
            let empty = text.trim().is_empty() && reasoning.trim().is_empty();
            if empty && (error.is_some() || !calls.is_empty()) {
                st.items.remove(idx);
            } else if let Some(Item::Assistant { text: t, reasoning: r, streaming }) = st.items.get_mut(idx) {
                *t = if empty { "*(empty response)*".into() } else { text.clone() };
                *r = reasoning;
                *streaming = false;
            }
            if let Some(err) = error {
                if err.starts_with("Could not reach") {
                    st.conn = Conn::Error;
                    st.conn_detail = err.clone();
                }
                drop(st);
                return self.finish(run_id, Some(Item::Error { text: err }));
            }
            st.conn = Conn::Connected;
            if let Some(total) = turn.total_tokens() {
                st.context_tokens = total;
            }

            let mut msg = json!({"role": "assistant", "content": text});
            let calls: Vec<api::ToolCall> = calls
                .into_iter()
                .enumerate()
                .map(|(i, mut c)| {
                    if c.id.is_empty() {
                        c.id = format!("call_{run_id}_{rounds}_{i}");
                    }
                    if c.arguments.trim().is_empty() {
                        c.arguments = "{}".into();
                    }
                    c
                })
                .collect();
            if !calls.is_empty() {
                msg["tool_calls"] = calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                    .collect();
            }
            st.api.push(msg);
            drop(st);
            if calls.is_empty() {
                return self.finish(run_id, None);
            }

            for call in &calls {
                if !self.run_tool(run_id, &cancel, &cfg, call) {
                    return;
                }
            }
            rounds += 1;
            if rounds >= cfg.max_tool_rounds {
                let text = format!("Stopped after {rounds} tool rounds. Send a message to continue.");
                return self.finish(run_id, Some(Item::Info { text }));
            }
            self.save_history();
        }
    }

    /// Asks for approval if needed, runs one tool call and records its result. False if the run was cancelled.
    fn run_tool(&self, run_id: u64, cancel: &AtomicBool, cfg: &Config, call: &api::ToolCall) -> bool {
        let parsed: Result<Value, _> = serde_json::from_str(&call.arguments);
        let args = parsed.as_ref().ok().filter(|v| v.is_object()).cloned();
        let known = cfg.tools_enabled && tools::NAMES.contains(&call.name.as_str());

        let Some(mut st) = self.lock_run(run_id) else {
            return false;
        };
        let item = ToolItem {
            id: call.id.clone(),
            name: call.name.clone(),
            summary: args.as_ref().map(|a| tools::summarize(&call.name, a)).unwrap_or_else(|| call.arguments.chars().take(200).collect()),
            detail: args.as_ref().map(|a| tools::detail(&call.name, a, &st.cwd)).unwrap_or_else(|| call.arguments.clone()),
            output: String::new(),
            status: ToolStatus::Running,
        };
        st.items.push(Item::Tool(item));
        let idx = st.items.len() - 1;

        let record = |st: &mut State, output: String, status: ToolStatus| {
            let output = tools::truncate(&output, cfg.max_output_chars);
            if let Some(Item::Tool(t)) = st.items.get_mut(idx) {
                t.output = output.clone();
                t.status = status;
            }
            st.api.push(json!({"role": "tool", "tool_call_id": call.id, "name": call.name, "content": output}));
        };

        let Some(args) = args.filter(|_| known) else {
            let msg = if known {
                format!("Error: the arguments are not a valid JSON object: {}", call.arguments)
            } else {
                format!("Error: unknown tool '{}'.", call.name)
            };
            record(&mut st, msg, ToolStatus::Error);
            return true;
        };

        let needs_approval = !st.session_allowed.contains(&call.name)
            && match cfg.approval {
                Approval::Ask => true,
                Approval::AutoRead => !tools::is_read_only(&call.name),
                Approval::AutoAll => false,
            };
        if needs_approval {
            let (tx, rx) = mpsc::channel();
            st.approvals.insert(call.id.clone(), tx);
            if let Some(Item::Tool(t)) = st.items.get_mut(idx) {
                t.status = ToolStatus::Approval;
            }
            st.status = "Waiting for your approval".into();
            drop(st);
            self.notify();
            let decision = loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(d) => break d,
                    Err(mpsc::RecvTimeoutError::Timeout) if !cancel.load(Ordering::SeqCst) => {}
                    Err(_) => return false,
                }
            };
            let Some(s) = self.lock_run(run_id) else {
                return false;
            };
            st = s;
            match decision {
                Decision::Deny => {
                    record(
                        &mut st,
                        "The user denied this tool call. Do not retry it; ask the user how to proceed.".into(),
                        ToolStatus::Denied,
                    );
                    return true;
                }
                Decision::Always => {
                    st.session_allowed.insert(call.name.clone());
                }
                Decision::Allow => {}
            }
            if let Some(Item::Tool(t)) = st.items.get_mut(idx) {
                t.status = ToolStatus::Running;
            }
        }
        st.status = format!("Running {}…", call.name);
        let mut ctx = tools::Ctx {
            cwd: st.cwd.clone(),
            tmpdir: cfg.ensure_scratch_dir(),
            timeout: Duration::from_secs(cfg.command_timeout_secs.max(1)),
            cancel,
            pgid: &self.pgid,
        };
        drop(st);
        self.notify();

        let (output, ok) = tools::execute(&call.name, &args, &mut ctx);

        let Some(mut st) = self.lock_run(run_id) else {
            return false;
        };
        st.cwd = ctx.cwd;
        record(&mut st, output, if ok { ToolStatus::Done } else { ToolStatus::Error });
        drop(st);
        self.notify();
        true
    }
}

/// Every tool call needs a result or most servers reject the history (e.g. after a cancelled turn).
fn repair_dangling_tool_calls(api: &mut Vec<Value>) {
    let Some(pos) = api.iter().rposition(|m| m["role"] == "assistant" && m["tool_calls"].is_array()) else {
        return;
    };
    if api[pos + 1..].iter().any(|m| m["role"] == "user") {
        return;
    }
    let answered: HashSet<String> =
        api[pos + 1..].iter().filter(|m| m["role"] == "tool").filter_map(|m| m["tool_call_id"].as_str().map(String::from)).collect();
    let missing: Vec<(String, String)> = api[pos]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tc| {
            let id = tc["id"].as_str()?.to_string();
            (!answered.contains(&id)).then(|| (id, tc["function"]["name"].as_str().unwrap_or("").to_string()))
        })
        .collect();
    for (id, name) in missing {
        api.push(json!({"role": "tool", "tool_call_id": id, "name": name, "content": "Cancelled by the user."}));
    }
}

fn system_prompt(cfg: &Config, cwd: &std::path::Path) -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "Linux".into());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "KDE".into());
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let mut p = String::from("You are LoKI, a capable assistant running on the user's Linux desktop (KDE Plasma). ");
    if cfg.tools_enabled {
        p.push_str(
            "You can operate the machine through tools: run shell commands, read, write and edit files, list directories and search file contents. \
             Use them whenever they help you answer accurately or complete the task instead of guessing. \
             Work step by step: inspect before you modify, and verify results after changes. \
             Never run destructive commands (deleting data, formatting disks, killing the session) unless the user explicitly asked. \
             The user may have to approve tool calls; if one is denied, do not retry it, ask how to proceed instead. \
             To launch GUI applications without blocking, use `setsid -f <app> >/dev/null 2>&1`. ",
        );
    }
    p.push_str("Be concise. Use Markdown, with fenced code blocks for code.\n\nEnvironment:\n");
    p.push_str(&format!("- User: {}\n", std::env::var("USER").unwrap_or_default()));
    p.push_str(&format!("- Home: {}\n", config::home_dir().display()));
    p.push_str(&format!("- Working directory: {}\n", cwd.display()));
    if cfg.tools_enabled {
        p.push_str(&format!(
            "- Scratch directory: {} (also $TMPDIR). Put temporary, intermediate and generated files (scripts, downloads, outputs) here, \
             never in the home directory, unless the user names another location.\n",
            cfg.scratch_dir().display()
        ));
    }
    p.push_str(&format!("- System: {os}, kernel {}, {desktop} {session}\n", kernel.trim()));
    if let Ok(out) = std::process::Command::new("date").arg("+%A, %Y-%m-%d %H:%M %Z").output() {
        p.push_str(&format!("- Date: {}\n", String::from_utf8_lossy(&out.stdout).trim()));
    }
    if !cfg.system_prompt.trim().is_empty() {
        p.push_str("\nAdditional instructions from the user:\n");
        p.push_str(cfg.system_prompt.trim());
        p.push('\n');
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_unanswered_tool_calls() {
        let mut api = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "function": {"name": "read_file"}}, {"id": "b", "function": {"name": "run_command"}}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ok"}),
        ];
        repair_dangling_tool_calls(&mut api);
        assert_eq!(api.len(), 4);
        assert_eq!(api[3]["tool_call_id"], "b");
        repair_dangling_tool_calls(&mut api);
        assert_eq!(api.len(), 4);
    }

    #[test]
    fn history_roundtrip() {
        let items = vec![
            Item::User { text: "x".into() },
            Item::Tool(ToolItem {
                id: "1".into(),
                name: "read_file".into(),
                summary: "s".into(),
                detail: "d".into(),
                output: "o".into(),
                status: ToolStatus::Done,
            }),
        ];
        let text = serde_json::to_string(&History { items, ..Default::default() }).unwrap();
        let back: History = serde_json::from_str(&text).unwrap();
        assert!(matches!(back.items[1], Item::Tool(ToolItem { status: ToolStatus::Done, .. })));
    }

    /// Generated files land in the scratch directory, not in home (real server, fake HOME):
    /// `LOKI_TEST_SERVER=http://host:8080 cargo test -- --ignored --nocapture agent_scratch`
    #[test]
    #[ignore]
    fn agent_scratch() {
        let server = std::env::var("LOKI_TEST_SERVER").expect("set LOKI_TEST_SERVER");
        let tmp = std::env::temp_dir().join(format!("loki-scratch-{}", std::process::id()));
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: single-threaded at this point; fake HOME/XDG dirs keep the user's real files untouched.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
            std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
            std::env::set_var("XDG_RUNTIME_DIR", tmp.join("run"));
        }
        let core = Core::new();
        {
            let mut st = core.lock();
            st.config.server_url = server;
            st.config.approval = Approval::AutoAll;
            st.config.keep_history = false;
        }
        let scratch = core.lock().config.scratch_dir();
        assert_eq!(core.lock().cwd, scratch);
        let start = Instant::now();
        while core.lock().config.model.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(15), "no model: {}", core.lock().conn_detail);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(core.send("Write a small Python script that prints the first 5 square numbers, run it, and tell me the output."));
        while core.lock().busy {
            assert!(start.elapsed() < Duration::from_secs(300), "timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
        for item in &core.lock().items {
            if let Item::Tool(t) = item {
                println!("TOOL {} [{:?}] {}", t.name, t.status, t.summary);
            }
        }
        let in_scratch: Vec<_> = std::fs::read_dir(&scratch).unwrap().flatten().map(|e| e.file_name()).collect();
        let in_home: Vec<_> = std::fs::read_dir(&home).unwrap().flatten().map(|e| e.file_name()).collect();
        println!("scratch: {in_scratch:?}\nhome: {in_home:?}");
        assert!(!in_scratch.is_empty(), "nothing was written to the scratch directory");
        assert!(in_home.iter().all(|n| n.to_string_lossy().starts_with('.')), "files were written to home: {in_home:?}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// End-to-end against a real server:
    /// `LOKI_TEST_SERVER=http://host:8080 cargo test -- --ignored --nocapture agent_e2e`
    #[test]
    #[ignore]
    fn agent_e2e() {
        let server = std::env::var("LOKI_TEST_SERVER").expect("set LOKI_TEST_SERVER");
        let tmp = std::env::temp_dir().join(format!("loki-e2e-{}", std::process::id()));
        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // SAFETY: single-threaded at this point; keeps the user's real config/history untouched.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
            std::env::set_var("XDG_DATA_HOME", tmp.join("data"));
        }
        let core = Core::new();
        {
            let mut st = core.lock();
            st.config.server_url = server;
            st.config.approval = Approval::Ask;
            st.config.working_dir = work.display().to_string();
            st.cwd = work.clone();
        }
        let start = Instant::now();
        while core.lock().config.model.is_empty() {
            assert!(start.elapsed() < Duration::from_secs(15), "no model: {}", core.lock().conn_detail);
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(core.send(
            "Create notes.txt containing 'hello from loki', change 'hello' to 'goodbye' with edit_file, \
             read it back, run `uname -s`, then answer in one short sentence."
        ));
        let mut approvals = 0;
        while core.lock().busy {
            assert!(start.elapsed() < Duration::from_secs(300), "timed out");
            let pending = core.lock().pending_approval();
            if let Some(id) = pending {
                approvals += 1;
                core.resolve(&id, Decision::Allow);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let st = core.lock();
        for item in &st.items {
            match item {
                Item::Tool(t) => println!("TOOL {} [{:?}] {} -> {}", t.name, t.status, t.summary, t.output.lines().next().unwrap_or("")),
                Item::Assistant { text, reasoning, .. } => println!("ASSISTANT ({} chars reasoning): {text}", reasoning.len()),
                other => println!("{other:?}"),
            }
        }
        println!("tps={:.1} ctx={} conn={:?} latency={:?} approvals={approvals}", st.tps, st.context_tokens, st.conn, st.latency_ms);
        assert!(!st.items.iter().any(|i| matches!(i, Item::Error { .. })));
        assert!(st.items.iter().any(|i| matches!(i, Item::Tool(t) if t.name == "edit_file" && t.status == ToolStatus::Done)));
        assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap().trim(), "goodbye from loki");
        assert!(st.tps > 0.0 && st.context_tokens > 0 && st.conn == Conn::Connected && approvals > 0);
        drop(st);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
