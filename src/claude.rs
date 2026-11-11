//! Drives Claude Code as a long-lived subprocess using its stream-json protocol, the same
//! protocol the Claude Agent SDK speaks (`--input-format/--output-format stream-json` plus
//! `control_request`/`control_response` frames for initialize, permissions and interrupts).

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};

use crate::model::RuntimeMode;

#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    SessionStarted { session_id: String },
    StreamText(String),
    StreamThinking(String),
    FinalText(String),
    FinalThinking(String),
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, output: String, is_error: bool },
    PermissionRequest { request_id: String, tool_name: String, input: Value },
    TurnCompleted { duration_ms: u64, cost_usd: f64, is_error: bool, error_text: Option<String> },
    Stderr(String),
    Exited { code: Option<i32> },
}

pub struct SpawnConfig {
    pub cwd: PathBuf,
    pub model: String,
    pub mode: RuntimeMode,
    pub resume: Option<String>,
}

/// Session variables a parent Claude Code exports; a child inheriting them would join that session.
const HOST_SESSION_VARIABLES: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_DESKTOP_APP_VERSION",
    "CLAUDE_EFFORT",
    "CLAUDE_PID",
    "CLAUDE_AGENT_SDK_VERSION",
    "ELECTRON_RUN_AS_NODE",
];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Hides the console window a GUI process would otherwise flash for each child.
pub fn hide_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// How to launch Claude: the executable plus any leading arguments (e.g. `node cli.js`).
#[derive(Debug, Clone)]
pub struct Launcher {
    pub program: PathBuf,
    pub prefix_args: Vec<OsString>,
}

/// Resolves the Claude Code executable. On Windows `Command` does no PATHEXT lookup and
/// cannot run `.cmd` shims directly, so npm shims are followed to the real `claude.exe`
/// (or `cli.js` under node), mirroring upstream `ClaudeExecutable.ts`.
pub fn resolve_launcher() -> Result<Launcher> {
    if let Some(explicit) = std::env::var_os("NCODE_CLAUDE_PATH") {
        return launcher_for(PathBuf::from(explicit));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("claude{ext}"));
            if candidate.is_file() {
                return launcher_for(candidate);
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        let native = home.join(".local").join("bin").join(exe_name("claude"));
        if native.is_file() {
            return launcher_for(native);
        }
    }
    Err(anyhow!(
        "Claude Code CLI not found on PATH. Install it or set NCODE_CLAUDE_PATH."
    ))
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) { format!("{stem}.exe") } else { stem.to_owned() }
}

fn launcher_for(path: PathBuf) -> Result<Launcher> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "cmd" | "bat" | "ps1") {
        return Ok(Launcher { program: path, prefix_args: vec![] });
    }
    let shim_dir = path.parent().unwrap_or(Path::new("."));
    let pkg = shim_dir.join("node_modules").join("@anthropic-ai").join("claude-code");
    let bin = pkg.join("bin").join("claude.exe");
    if bin.is_file() {
        return Ok(Launcher { program: bin, prefix_args: vec![] });
    }
    let cli = pkg.join("cli.js");
    if cli.is_file() {
        let node = shim_dir.join("node.exe");
        let node = if node.is_file() { node } else { PathBuf::from("node") };
        return Ok(Launcher { program: node, prefix_args: vec![cli.into_os_string()] });
    }
    Err(anyhow!(
        "{} is a launcher script and no claude.exe or cli.js was found next to it",
        path.display()
    ))
}

pub struct ClaudeSession {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    pid: u32,
    next_request: AtomicU64,
    /// Original tool input per pending permission request; `allow` must echo it back.
    pending_inputs: Arc<Mutex<HashMap<String, Value>>>,
}

impl ClaudeSession {
    pub fn spawn(
        config: SpawnConfig,
        on_event: impl Fn(ClaudeEvent) + Send + Sync + 'static,
    ) -> Result<Self> {
        let launcher = resolve_launcher()?;
        let mut cmd = Command::new(&launcher.program);
        cmd.args(&launcher.prefix_args)
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--permission-mode",
                config.mode.claude_flag(),
                "--model",
                &config.model,
            ])
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Lets the mode switch to bypassPermissions mid-session, as the Agent SDK does.
            .arg("--allow-dangerously-skip-permissions");
        if let Some(resume) = &config.resume {
            cmd.args(["--resume", resume]);
        }
        for name in HOST_SESSION_VARIABLES {
            cmd.env_remove(name);
        }
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-cli");
        hide_window(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start {}", launcher.program.display()))?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let on_event = Arc::new(on_event);
        let pending_inputs: Arc<Mutex<HashMap<String, Value>>> = Default::default();

        {
            let on_event = on_event.clone();
            std::thread::Builder::new()
                .name(format!("claude-stderr-{pid}"))
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if !line.trim().is_empty() {
                            on_event(ClaudeEvent::Stderr(line));
                        }
                    }
                })?;
        }

        let child = Arc::new(Mutex::new(child));
        {
            let on_event = on_event.clone();
            let child = child.clone();
            let pending_inputs = pending_inputs.clone();
            std::thread::Builder::new()
                .name(format!("claude-stdout-{pid}"))
                .spawn(move || {
                    let mut parser = StreamParser::default();
                    let mut reader = BufReader::new(stdout);
                    let mut buf = Vec::new();
                    loop {
                        buf.clear();
                        match reader.read_until(b'\n', &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(line) else {
                            log::debug!("claude: non-json line: {line}");
                            continue;
                        };
                        for event in parser.parse(&value) {
                            if let ClaudeEvent::PermissionRequest { request_id, input, .. } = &event {
                                pending_inputs
                                    .lock()
                                    .unwrap()
                                    .insert(request_id.clone(), input.clone());
                            }
                            on_event(event);
                        }
                    }
                    let code = child.lock().ok().and_then(|mut c| c.wait().ok()).and_then(|s| s.code());
                    on_event(ClaudeEvent::Exited { code });
                })?;
        }

        let session = Self {
            stdin: Arc::new(Mutex::new(stdin)),
            child,
            pid,
            next_request: AtomicU64::new(1),
            pending_inputs,
        };
        session.control("initialize", json!({}))?;
        Ok(session)
    }

    fn write_line(&self, value: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().map_err(|_| anyhow!("stdin poisoned"))?;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        stdin.write_all(&line)?;
        stdin.flush()?;
        Ok(())
    }

    fn control(&self, subtype: &str, extra: Value) -> Result<()> {
        let id = format!("ncode-{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        let mut request = json!({ "subtype": subtype });
        if let (Some(req), Some(extra)) = (request.as_object_mut(), extra.as_object()) {
            req.extend(extra.clone());
        }
        self.write_line(&json!({ "type": "control_request", "request_id": id, "request": request }))
    }

    pub fn send_user_message(&self, text: &str) -> Result<()> {
        self.write_line(&json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
            "parent_tool_use_id": null,
            "session_id": "",
        }))
    }

    pub fn respond_permission(&self, request_id: &str, allow: bool) -> Result<()> {
        let input = self
            .pending_inputs
            .lock()
            .unwrap()
            .remove(request_id)
            .unwrap_or_else(|| json!({}));
        let response = if allow {
            json!({ "behavior": "allow", "updatedInput": input })
        } else {
            json!({ "behavior": "deny", "message": "The user denied this tool call." })
        };
        self.write_line(&json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": request_id, "response": response },
        }))
    }

    pub fn interrupt(&self) -> Result<()> {
        self.control("interrupt", json!({}))
    }

    pub fn set_permission_mode(&self, mode: RuntimeMode) -> Result<()> {
        self.control("set_permission_mode", json!({ "mode": mode.claude_flag() }))
    }

    pub fn set_model(&self, model: &str) -> Result<()> {
        self.control("set_model", json!({ "model": model }))
    }

    /// Kills Claude and everything it started. Windows has no process groups for this, so
    /// `taskkill /T` walks the tree (shells and tools Claude spawned) before the parent dies.
    pub fn kill(&self) {
        #[cfg(windows)]
        {
            let mut cmd = Command::new("taskkill");
            cmd.args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            hide_window(&mut cmd);
            let _ = cmd.status();
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Translates stream-json frames into [`ClaudeEvent`]s. Frames from subagents
/// (`parent_tool_use_id` set) are dropped; only the top-level conversation is shown.
#[derive(Default)]
pub struct StreamParser {
    /// Block type per streaming content-block index.
    block_kinds: HashMap<u64, String>,
}

impl StreamParser {
    pub fn parse(&mut self, v: &Value) -> Vec<ClaudeEvent> {
        let mut out = Vec::new();
        let is_subagent = v.get("parent_tool_use_id").is_some_and(|p| !p.is_null());
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                out.push(ClaudeEvent::SessionStarted {
                    session_id: str_of(v, "session_id"),
                });
            }
            Some("stream_event") if !is_subagent => {
                let event = &v["event"];
                match event.get("type").and_then(Value::as_str) {
                    Some("content_block_start") => {
                        let index = event["index"].as_u64().unwrap_or(0);
                        let kind = event["content_block"]["type"].as_str().unwrap_or("").to_owned();
                        self.block_kinds.insert(index, kind);
                    }
                    Some("content_block_delta") => {
                        let delta = &event["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => {
                                out.push(ClaudeEvent::StreamText(str_of(delta, "text")))
                            }
                            Some("thinking_delta") => {
                                out.push(ClaudeEvent::StreamThinking(str_of(delta, "thinking")))
                            }
                            _ => {}
                        }
                    }
                    Some("message_stop") => self.block_kinds.clear(),
                    _ => {}
                }
            }
            Some("assistant") if !is_subagent => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => out.push(ClaudeEvent::FinalText(str_of(block, "text"))),
                        Some("thinking") => {
                            let text = str_of(block, "thinking");
                            if !text.is_empty() {
                                out.push(ClaudeEvent::FinalThinking(text));
                            }
                        }
                        Some("tool_use") => out.push(ClaudeEvent::ToolUse {
                            id: str_of(block, "id"),
                            name: str_of(block, "name"),
                            input: block["input"].clone(),
                        }),
                        _ => {}
                    }
                }
            }
            Some("user") if !is_subagent => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"].as_str() == Some("tool_result") {
                        out.push(ClaudeEvent::ToolResult {
                            tool_use_id: str_of(block, "tool_use_id"),
                            output: tool_result_text(&block["content"]),
                            is_error: block["is_error"].as_bool().unwrap_or(false),
                        });
                    }
                }
            }
            Some("control_request") => {
                let request = &v["request"];
                if request["subtype"].as_str() == Some("can_use_tool") {
                    out.push(ClaudeEvent::PermissionRequest {
                        request_id: str_of(v, "request_id"),
                        tool_name: str_of(request, "tool_name"),
                        input: request["input"].clone(),
                    });
                }
            }
            Some("result") => {
                let is_error = v["is_error"].as_bool().unwrap_or(false)
                    || v["subtype"].as_str().is_some_and(|s| s != "success");
                out.push(ClaudeEvent::TurnCompleted {
                    duration_ms: v["duration_ms"].as_u64().unwrap_or(0),
                    cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
                    is_error,
                    error_text: is_error.then(|| {
                        v["result"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| str_of(v, "subtype"))
                    }),
                });
            }
            _ => {}
        }
        out
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(lines: &[Value]) -> Vec<ClaudeEvent> {
        let mut p = StreamParser::default();
        lines.iter().flat_map(|l| p.parse(l)).collect()
    }

    #[test]
    fn translates_a_tool_turn() {
        let events = parse_all(&[
            json!({"type":"system","subtype":"init","session_id":"s1","model":"claude-haiku"}),
            json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}),
            json!({"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"echo hi"}}]}}),
            json!({"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"hi","is_error":false}]}}),
            json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"a"}}}),
            json!({"type":"result","subtype":"success","is_error":false,"duration_ms":12,"total_cost_usd":0.5}),
        ]);
        assert!(matches!(&events[0], ClaudeEvent::SessionStarted { session_id, .. } if session_id == "s1"));
        assert!(matches!(&events[1], ClaudeEvent::StreamText(t) if t == "Hel"));
        assert!(matches!(&events[2], ClaudeEvent::ToolUse { name, .. } if name == "Bash"));
        assert!(matches!(&events[3], ClaudeEvent::ToolResult { output, is_error: false, .. } if output == "hi"));
        assert!(matches!(&events[4], ClaudeEvent::PermissionRequest { request_id, .. } if request_id == "r1"));
        assert!(matches!(&events[5], ClaudeEvent::TurnCompleted { is_error: false, duration_ms: 12, .. }));
    }

    #[test]
    fn drops_subagent_frames_and_reports_errors() {
        let events = parse_all(&[
            json!({"type":"assistant","parent_tool_use_id":"task1","message":{"content":[{"type":"text","text":"sub"}]}}),
            json!({"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":1}),
        ]);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ClaudeEvent::TurnCompleted { is_error: true, error_text: Some(t), .. } if t == "error_during_execution"));
    }

    #[test]
    fn follows_npm_shim_to_bundled_exe() {
        let dir = std::env::temp_dir().join(format!("ncode-shim-{}", crate::model::new_id()));
        let bin = dir.join("node_modules/@anthropic-ai/claude-code/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(dir.join("claude.cmd"), "@echo off").unwrap();
        std::fs::write(bin.join("claude.exe"), "").unwrap();
        let launcher = launcher_for(dir.join("claude.cmd")).unwrap();
        assert_eq!(launcher.program, bin.join("claude.exe"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
