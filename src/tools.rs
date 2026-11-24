//! Tools the model can call: shell commands and file operations on this machine.

use crate::config::{collapse_home, expand_home};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "run_command",
            "description": "Run a bash command on the user's Linux (KDE Plasma) machine and return stdout, stderr and the exit code. \
                The working directory persists between calls (use `cd` to change it). Commands are non-interactive: stdin is /dev/null, \
                so never run programs that wait for input or open a pager/editor. Prefer the file tools for reading, writing and editing files.",
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string", "description": "The bash command line to execute."},
                "timeout": {"type": "integer", "description": "Optional timeout in seconds."}
            }, "required": ["command"]}
        }},
        {"type": "function", "function": {
            "name": "read_file",
            "description": "Read a text file. Returns lines prefixed with line numbers. Use offset/limit for large files.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Absolute path, ~/path, or a path relative to the working directory."},
                "offset": {"type": "integer", "description": "1-based line to start from (default 1)."},
                "limit": {"type": "integer", "description": "Maximum number of lines (default 2000)."}
            }, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content. Parent directories are created as needed.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}, "content": {"type": "string", "description": "Full file content."}
            }, "required": ["path", "content"]}
        }},
        {"type": "function", "function": {
            "name": "edit_file",
            "description": "Replace an exact string in a file. old_string must match exactly (including whitespace) and be unique unless replace_all is true. Read the file first.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence."}
            }, "required": ["path", "old_string", "new_string"]}
        }},
        {"type": "function", "function": {
            "name": "list_directory",
            "description": "List a directory's entries with type and size.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory (default: working directory)."}
            }}
        }},
        {"type": "function", "function": {
            "name": "search_files",
            "description": "Search file contents recursively for a regular expression (ripgrep if available, else grep). Returns path:line:text.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression."},
                "path": {"type": "string", "description": "Directory or file (default: working directory)."},
                "glob": {"type": "string", "description": "Optional filename filter, e.g. '*.py'."},
                "ignore_case": {"type": "boolean"}
            }, "required": ["pattern"]}
        }}
    ])
}

pub const NAMES: [&str; 6] = ["run_command", "read_file", "write_file", "edit_file", "list_directory", "search_files"];

pub fn is_read_only(name: &str) -> bool {
    matches!(name, "read_file" | "list_directory" | "search_files")
}

fn s<'a>(args: &'a Value, key: &str) -> &'a str {
    args[key].as_str().unwrap_or("")
}

/// One-line description for the UI.
pub fn summarize(name: &str, args: &Value) -> String {
    match name {
        "run_command" => s(args, "command").lines().next().unwrap_or("").to_string(),
        "read_file" => s(args, "path").to_string(),
        "write_file" => format!("{} ({} lines)", s(args, "path"), s(args, "content").lines().count()),
        "edit_file" => s(args, "path").to_string(),
        "list_directory" => args["path"].as_str().unwrap_or(".").to_string(),
        "search_files" => format!("/{}/ in {}", s(args, "pattern"), args["path"].as_str().unwrap_or(".")),
        _ => args.to_string(),
    }
}

/// Longer, multi-line view of the arguments (shown when a card is expanded or needs approval).
pub fn detail(name: &str, args: &Value, cwd: &Path) -> String {
    match name {
        "run_command" => format!("$ {}\n# in {}", s(args, "command"), collapse_home(cwd)),
        "write_file" => truncate(s(args, "content"), 4000),
        "edit_file" => {
            let minus: Vec<String> = s(args, "old_string").lines().map(|l| format!("- {l}")).collect();
            let plus: Vec<String> = s(args, "new_string").lines().map(|l| format!("+ {l}")).collect();
            format!("{}\n{}", minus.join("\n"), plus.join("\n"))
        }
        _ => serde_json::to_string_pretty(args).unwrap_or_default(),
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if max == 0 || count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 6 / 10).collect();
    let tail: String = text.chars().skip(count - (max - max * 6 / 10)).collect();
    format!("{head}\n\n[… {} characters truncated …]\n\n{tail}", count - max)
}

/// Execution context shared with the agent loop.
pub struct Ctx<'a> {
    pub cwd: PathBuf,
    /// `$TMPDIR` for commands (the scratch directory).
    pub tmpdir: PathBuf,
    pub timeout: Duration,
    pub cancel: &'a AtomicBool,
    /// Process group of the running command (0 = none), so `stop` can kill it.
    pub pgid: &'a AtomicI32,
}

impl Ctx<'_> {
    fn resolve(&self, path: &str) -> PathBuf {
        let p = expand_home(path.trim());
        if p.is_absolute() { p } else { self.cwd.join(p) }
    }
}

/// Runs a tool. Returns (output, success).
pub fn execute(name: &str, args: &Value, ctx: &mut Ctx) -> (String, bool) {
    let r = match name {
        "run_command" => run_command(args, ctx),
        "read_file" => read_file(args, ctx),
        "write_file" => write_file(args, ctx),
        "edit_file" => edit_file(args, ctx),
        "list_directory" => list_directory(args, ctx),
        "search_files" => search_files(args, ctx),
        _ => Err(format!("unknown tool '{name}'")),
    };
    match r {
        Ok(out) => (out, true),
        // Command output already explains the failure.
        Err(e) if name == "run_command" => (e, false),
        Err(e) => (format!("Error: {e}"), false),
    }
}

struct Output {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
}

const READ_CAP: usize = 4 << 20;

fn spawn_reader(mut src: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = src.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if buf.len() < READ_CAP {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
        let _ = tx.send(buf);
    });
    rx
}

fn kill_group(pgid: i32, sig: i32) {
    if pgid > 0 {
        // SAFETY: plain syscall; negative pid targets the process group we created.
        unsafe { libc::kill(-pgid, sig) };
    }
}

fn run_process(mut cmd: Command, ctx: &Ctx, timeout: Duration) -> Result<Output, String> {
    use std::os::unix::process::CommandExt;
    cmd.current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("SYSTEMD_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TMPDIR", &ctx.tmpdir)
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pgid = child.id() as i32;
    ctx.pgid.store(pgid, Ordering::SeqCst);
    let out_rx = spawn_reader(child.stdout.take().unwrap());
    let err_rx = spawn_reader(child.stderr.take().unwrap());

    let start = Instant::now();
    let (mut timed_out, mut cancelled) = (false, false);
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if ctx.cancel.load(Ordering::SeqCst) {
            cancelled = true;
        } else if start.elapsed() > timeout {
            timed_out = true;
        }
        if cancelled || timed_out {
            kill_group(pgid, libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(300));
            kill_group(pgid, libc::SIGKILL);
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    ctx.pgid.store(0, Ordering::SeqCst);
    // Background children may keep the pipes open; don't wait for them forever.
    let grab = |rx: mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default()).into_owned()
    };
    Ok(Output { stdout: grab(out_rx), stderr: grab(err_rx), code: status.code(), timed_out, cancelled })
}

fn format_output(o: &Output) -> String {
    let mut out = o.stdout.clone();
    if !o.stderr.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&o.stderr);
    }
    if o.timed_out {
        out.push_str("\n[command timed out and was killed]");
    }
    if o.cancelled {
        out.push_str("\n[cancelled by the user]");
    }
    match o.code {
        Some(0) => {}
        Some(c) => out.push_str(&format!("\n[exit code {c}]")),
        None if !o.timed_out && !o.cancelled => out.push_str("\n[killed by a signal]"),
        None => {}
    }
    if out.trim().is_empty() { "(no output)".into() } else { out.trim_start_matches('\n').to_string() }
}

const CWD_MARK: &str = "\n__LOKI_CWD__";

fn run_command(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let command = s(args, "command");
    if command.trim().is_empty() {
        return Err("empty command".into());
    }
    let timeout = args["timeout"].as_u64().map(|t| Duration::from_secs(t.clamp(1, 3600))).unwrap_or(ctx.timeout);
    let mut cmd = Command::new("bash");
    // `eval` keeps syntax errors inside the script so the cwd is still reported.
    cmd.arg("-c")
        .arg(format!("eval \"$LOKI_CMD\"\n__ec=$?\nprintf '{}%s' \"$PWD\"\nexit $__ec", CWD_MARK.replace('\n', "\\n")))
        .env("LOKI_CMD", command);
    let mut out = run_process(cmd, ctx, timeout)?;
    if let Some(pos) = out.stdout.rfind(CWD_MARK) {
        let dir = out.stdout[pos + CWD_MARK.len()..].trim_end().to_string();
        out.stdout.truncate(pos);
        if Path::new(&dir).is_dir() {
            ctx.cwd = PathBuf::from(dir);
        }
    }
    let ok = out.code == Some(0);
    let text = format_output(&out);
    if ok { Ok(text) } else { Err(text) }
}

fn read_file(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.resolve(s(args, "path"));
    if path.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(format!("{} looks like a binary file", path.display()));
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return Ok("(empty file)".into());
    }
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000).max(1) as usize;
    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    if offset - 1 + limit < total {
        out.push_str(&format!("… ({} more lines, use offset to continue)\n", total - (offset - 1 + limit)));
    }
    if out.is_empty() {
        out = format!("(file has only {total} lines)");
    }
    Ok(out)
}

fn write_file(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.resolve(s(args, "path"));
    let content = s(args, "content");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("Wrote {} lines to {}", content.lines().count(), path.display()))
}

pub fn apply_edit(text: &str, old: &str, new: &str, all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    if old == new {
        return Err("old_string and new_string are identical".into());
    }
    let count = text.matches(old).count();
    match count {
        0 => Err("old_string was not found in the file. Read the file again and copy the text exactly.".into()),
        n if n > 1 && !all => Err(format!("old_string occurs {n} times. Add more context to make it unique or set replace_all.")),
        n if all => Ok((text.replace(old, new), n)),
        _ => Ok((text.replacen(old, new, 1), 1)),
    }
}

fn edit_file(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.resolve(s(args, "path"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (new_text, n) = apply_edit(&text, s(args, "old_string"), s(args, "new_string"), args["replace_all"].as_bool().unwrap_or(false))?;
    std::fs::write(&path, new_text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("Replaced {n} occurrence(s) in {}", path.display()))
}

fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n}B") } else { format!("{v:.1}{}", U[i]) }
}

fn list_directory(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.resolve(args["path"].as_str().unwrap_or("."));
    let mut entries: Vec<(bool, String, String)> = std::fs::read_dir(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok();
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            let is_dir = e.path().is_dir();
            let size = meta.as_ref().filter(|_| !is_dir).map(|m| human_size(m.len())).unwrap_or_default();
            let label = format!("{name}{}{}", if is_dir { "/" } else { "" }, if is_link { " (symlink)" } else { "" });
            (is_dir, label, size)
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    if entries.is_empty() {
        return Ok(format!("{} is empty", path.display()));
    }
    let mut out = format!("{}:\n", path.display());
    for (_, label, size) in entries.iter().take(1000) {
        out.push_str(&format!("{size:>8}  {label}\n"));
    }
    if entries.len() > 1000 {
        out.push_str(&format!("… and {} more\n", entries.len() - 1000));
    }
    Ok(out)
}

fn search_files(args: &Value, ctx: &mut Ctx) -> Result<String, String> {
    let pattern = s(args, "pattern");
    if pattern.is_empty() {
        return Err("empty pattern".into());
    }
    let target = ctx.resolve(args["path"].as_str().unwrap_or("."));
    let glob = args["glob"].as_str().filter(|g| !g.is_empty());
    let icase = args["ignore_case"].as_bool().unwrap_or(false);
    let has_rg = Command::new("rg").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok();
    let mut cmd;
    if has_rg {
        cmd = Command::new("rg");
        cmd.args(["-n", "--no-heading", "--color", "never", "--max-columns", "300"]);
        if icase {
            cmd.arg("-i");
        }
        if let Some(g) = glob {
            cmd.args(["-g", g]);
        }
    } else {
        cmd = Command::new("grep");
        cmd.args(["-rnI", "--color=never", "--exclude-dir=.git", "--exclude-dir=node_modules"]);
        if icase {
            cmd.arg("-i");
        }
        if let Some(g) = glob {
            cmd.arg(format!("--include={g}"));
        }
    }
    cmd.arg("-e").arg(pattern).arg("--").arg(&target);
    let out = run_process(cmd, ctx, ctx.timeout)?;
    match out.code {
        Some(0) => {
            let lines: Vec<&str> = out.stdout.lines().collect();
            let mut text = lines.iter().take(300).copied().collect::<Vec<_>>().join("\n");
            if lines.len() > 300 {
                text.push_str(&format!("\n… {} more matches", lines.len() - 300));
            }
            Ok(text)
        }
        Some(1) if out.stderr.trim().is_empty() => Ok("No matches found.".into()),
        _ => Err(format_output(&out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_in<'a>(dir: &Path, cancel: &'a AtomicBool, pgid: &'a AtomicI32) -> Ctx<'a> {
        Ctx { cwd: dir.to_path_buf(), tmpdir: std::env::temp_dir(), timeout: Duration::from_secs(10), cancel, pgid }
    }

    #[test]
    fn edits() {
        assert_eq!(apply_edit("a b a", "b", "c", false).unwrap().0, "a c a");
        assert!(apply_edit("a b a", "a", "c", false).unwrap_err().contains("2 times"));
        assert_eq!(apply_edit("a b a", "a", "c", true).unwrap(), ("c b c".into(), 2));
        assert!(apply_edit("abc", "zz", "y", false).is_err());
    }

    #[test]
    fn tools_roundtrip() {
        let dir = std::env::temp_dir().join(format!("loki-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cancel, pgid) = (AtomicBool::new(false), AtomicI32::new(0));
        let mut ctx = ctx_in(&dir, &cancel, &pgid);

        let (out, ok) = execute("write_file", &json!({"path": "sub/it's.txt", "content": "héllo 😀\nline2\n"}), &mut ctx);
        assert!(ok, "{out}");
        let (out, ok) = execute("edit_file", &json!({"path": "sub/it's.txt", "old_string": "line2", "new_string": "LINE TWO"}), &mut ctx);
        assert!(ok, "{out}");
        let (out, _) = execute("read_file", &json!({"path": "sub/it's.txt", "offset": 2}), &mut ctx);
        assert_eq!(out, "     2\tLINE TWO\n");
        let (out, ok) = execute("search_files", &json!({"pattern": "LINE T"}), &mut ctx);
        assert!(ok && out.contains("it's.txt:2:LINE TWO"), "{out}");
        let (out, _) = execute("list_directory", &json!({}), &mut ctx);
        assert!(out.contains("sub/"), "{out}");

        // cwd persists, exit codes and stderr are reported, syntax errors don't lose the cwd
        let (_, ok) = execute("run_command", &json!({"command": "cd sub && echo hi"}), &mut ctx);
        assert!(ok);
        assert!(ctx.cwd.ends_with("sub"));
        let (out, ok) = execute("run_command", &json!({"command": "echo oops >&2; exit 3"}), &mut ctx);
        assert!(!ok && out.contains("oops") && out.contains("[exit code 3]"), "{out}");
        let (out, ok) = execute("run_command", &json!({"command": "if then"}), &mut ctx);
        assert!(!ok && out.contains("syntax"), "{out}");
        assert!(ctx.cwd.ends_with("sub"));
        let (out, ok) = execute("run_command", &json!({"command": "sleep 5", "timeout": 1}), &mut ctx);
        assert!(!ok && out.contains("timed out"), "{out}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
