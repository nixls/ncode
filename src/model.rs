//! Domain types shared by persistence, the Claude driver and the UI.

use serde::{Deserialize, Serialize};

pub type ProjectId = String;
pub type ThreadId = String;

#[derive(Clone, Debug)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub sort_order: i64,
    pub collapsed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeMode {
    /// Claude asks before running tools (`--permission-mode default`).
    Supervised,
    /// File edits are auto-approved (`acceptEdits`).
    AutoEdit,
    /// Everything is approved (`bypassPermissions`).
    FullAccess,
    /// Read-only planning (`plan`).
    Plan,
}

impl RuntimeMode {
    pub const ALL: [RuntimeMode; 4] = [
        RuntimeMode::Supervised,
        RuntimeMode::AutoEdit,
        RuntimeMode::FullAccess,
        RuntimeMode::Plan,
    ];

    pub fn claude_flag(self) -> &'static str {
        match self {
            RuntimeMode::Supervised => "default",
            RuntimeMode::AutoEdit => "acceptEdits",
            RuntimeMode::FullAccess => "bypassPermissions",
            RuntimeMode::Plan => "plan",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RuntimeMode::Supervised => "Supervised",
            RuntimeMode::AutoEdit => "Auto-accept edits",
            RuntimeMode::FullAccess => "Full access",
            RuntimeMode::Plan => "Plan",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.claude_flag()
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "acceptEdits" => RuntimeMode::AutoEdit,
            "bypassPermissions" => RuntimeMode::FullAccess,
            "plan" => RuntimeMode::Plan,
            _ => RuntimeMode::Supervised,
        }
    }
}

pub const MODELS: [(&str, &str); 3] = [
    ("opus", "Claude Opus"),
    ("sonnet", "Claude Sonnet"),
    ("haiku", "Claude Haiku"),
];

pub fn model_label(id: &str) -> &str {
    MODELS
        .iter()
        .find(|(m, _)| *m == id)
        .map(|(_, l)| *l)
        .unwrap_or(id)
}

#[derive(Clone, Debug)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Claude Code session id, used for `--resume` after a restart.
    pub session_id: Option<String>,
    pub model: String,
    pub mode: RuntimeMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
}

/// One row of the chat timeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Item {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    Tool {
        tool_use_id: String,
        name: String,
        summary: String,
        output: Option<String>,
        status: ToolStatus,
    },
    Approval {
        request_id: String,
        tool: String,
        summary: String,
        state: ApprovalState,
    },
    TurnEnd {
        duration_ms: u64,
        cost_usd: f64,
        is_error: bool,
    },
    Notice {
        text: String,
        error: bool,
    },
}

#[derive(Clone, Debug)]
pub struct TimelineRow {
    /// Stable key, persisted as the row's primary key.
    pub key: String,
    pub item: Item,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A short, single-line description of a tool call's input, like the upstream work log.
pub fn summarize_tool_input(name: &str, input: &serde_json::Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let summary = match name {
        "Bash" | "PowerShell" => s("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            s("file_path").or_else(|| s("notebook_path"))
        }
        "Glob" | "Grep" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        "TodoWrite" => Some("Update todo list".into()),
        _ => None,
    };
    let text = summary.unwrap_or_else(|| {
        let raw = input.to_string();
        if raw == "{}" { String::new() } else { raw }
    });
    truncate_line(&text, 240)
}

pub fn truncate_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let multi = text.lines().nth(1).is_some();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else if multi {
        format!("{line} …")
    } else {
        line.to_owned()
    }
}

/// Derives a thread title from the first user message, the way T3 Code seeds titles.
pub fn title_from_prompt(prompt: &str) -> String {
    let t = truncate_line(prompt.trim(), 60);
    let t = t.trim_end_matches(" …").to_owned();
    if t.is_empty() { "New thread".into() } else { t }
}
