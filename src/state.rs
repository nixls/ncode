//! Application state. `AppState` owns projects, threads and live Claude sessions; each loaded
//! thread's timeline is its own entity so a streaming delta re-renders only that chat, never
//! the sidebar or the header (the retained-mode win over the React client).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::message_scroller::MessageScrollerState;
use gpui_kit::*;

use crate::claude::{ClaudeEvent, ClaudeSession, SpawnConfig};
use crate::model::*;
use crate::store::Store;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StreamKind {
    Text,
    Thinking,
}

pub struct Timeline {
    pub thread_id: ThreadId,
    pub rows: Vec<TimelineRow>,
    pub running: bool,
    pub scroller: Entity<MessageScrollerState>,
    /// Row index currently receiving deltas; persisted only once the block is final.
    streaming: Option<(usize, StreamKind)>,
    tools: HashMap<String, usize>,
    /// Rows whose tool output is expanded in the UI.
    pub expanded: HashSet<String>,
}

impl Timeline {
    fn new(thread_id: ThreadId, rows: Vec<TimelineRow>, cx: &mut Context<Self>) -> Self {
        let tools = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match &r.item {
                Item::Tool { tool_use_id, .. } => Some((tool_use_id.clone(), i)),
                _ => None,
            })
            .collect();
        let count = rows.len();
        Self {
            thread_id,
            rows,
            running: false,
            scroller: cx.new(|cx| MessageScrollerState::new(count, cx)),
            streaming: None,
            tools,
            expanded: HashSet::new(),
        }
    }

    fn push(&mut self, item: Item, store: &Store, persist: bool, cx: &mut Context<Self>) -> usize {
        let row = TimelineRow { key: new_id(), item };
        let index = self.rows.len();
        if persist {
            let _ = store.put_item(&self.thread_id, index, &row);
        }
        self.rows.push(row);
        self.scroller.update(cx, |s, cx| {
            s.append(1, cx);
        });
        cx.notify();
        index
    }

    fn changed(&mut self, index: usize, store: Option<&Store>, cx: &mut Context<Self>) {
        if let (Some(store), Some(row)) = (store, self.rows.get(index)) {
            let _ = store.put_item(&self.thread_id, index, row);
        }
        self.scroller.update(cx, |s, cx| {
            s.remeasure_items(index..index + 1, cx);
        });
        cx.notify();
    }

    fn stream(&mut self, kind: StreamKind, delta: &str, store: &Store, cx: &mut Context<Self>) {
        let index = match self.streaming {
            Some((i, k)) if k == kind => i,
            _ => {
                let item = match kind {
                    StreamKind::Text => Item::Assistant { text: String::new() },
                    StreamKind::Thinking => Item::Thinking { text: String::new() },
                };
                let i = self.push(item, store, false, cx);
                self.streaming = Some((i, kind));
                i
            }
        };
        if let Item::Assistant { text } | Item::Thinking { text } = &mut self.rows[index].item {
            text.push_str(delta);
        }
        self.changed(index, None, cx);
    }

    fn finalize(&mut self, kind: StreamKind, full: String, store: &Store, cx: &mut Context<Self>) {
        match self.streaming.take() {
            Some((i, k)) if k == kind => {
                if let Item::Assistant { text } | Item::Thinking { text } = &mut self.rows[i].item {
                    *text = full;
                }
                self.changed(i, Some(store), cx);
            }
            other => {
                self.streaming = other;
                let item = match kind {
                    StreamKind::Text => Item::Assistant { text: full },
                    StreamKind::Thinking => Item::Thinking { text: full },
                };
                self.push(item, store, true, cx);
            }
        }
    }

    fn end_stream(&mut self, store: &Store, cx: &mut Context<Self>) {
        if let Some((i, _)) = self.streaming.take() {
            self.changed(i, Some(store), cx);
        }
    }

    fn set_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if self.running != running {
            self.running = running;
            cx.notify();
        }
    }

    pub fn toggle_expanded(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_owned());
        }
        if let Some(i) = self.rows.iter().position(|r| r.key == key) {
            self.changed(i, None, cx);
        }
    }
}

pub struct AppState {
    pub store: Store,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
    pub active_thread: Option<ThreadId>,
    /// Project the header and "new thread" act on when no thread is selected.
    pub active_project: Option<ProjectId>,
    timelines: HashMap<ThreadId, Entity<Timeline>>,
    sessions: HashMap<ThreadId, ClaudeSession>,
    pub running: HashSet<ThreadId>,
    pub last_error: Option<String>,
    events_tx: flume::Sender<(ThreadId, ClaudeEvent)>,
    _event_pump: Task<()>,
}

pub enum AppEvent {
    ActiveThreadChanged,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new(store: Store, cx: &mut Context<Self>) -> Self {
        let projects = store.projects().unwrap_or_default();
        let threads = store.threads().unwrap_or_default();
        let active_thread = store
            .get_kv("active_thread")
            .filter(|id| threads.iter().any(|t| &t.id == id));
        let active_project = active_thread
            .as_ref()
            .and_then(|id| threads.iter().find(|t| &t.id == id))
            .map(|t| t.project_id.clone())
            .or_else(|| projects.first().map(|p| p.id.clone()));

        let (events_tx, events_rx) = flume::unbounded::<(ThreadId, ClaudeEvent)>();
        let event_pump = cx.spawn(async move |this, cx| {
            while let Ok(first) = events_rx.recv_async().await {
                // Coalesce whatever else already arrived into one update (one frame).
                let mut batch = vec![first];
                batch.extend(events_rx.try_iter().take(512));
                let alive = this.update(cx, |state, cx| {
                    for (thread_id, event) in batch {
                        state.handle_claude_event(thread_id, event, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        Self {
            store,
            projects,
            threads,
            active_thread,
            active_project,
            timelines: HashMap::new(),
            sessions: HashMap::new(),
            running: HashSet::new(),
            last_error: None,
            events_tx,
            _event_pump: event_pump,
        }
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    pub fn active_thread(&self) -> Option<&Thread> {
        self.active_thread.as_deref().and_then(|id| self.thread(id))
    }

    /// Project the header shows: the active thread's, else the selected project.
    pub fn current_project(&self) -> Option<&Project> {
        self.active_thread()
            .map(|t| t.project_id.clone())
            .or_else(|| self.active_project.clone())
            .and_then(|id| self.project(&id))
    }

    pub fn threads_for<'a>(&'a self, project_id: &'a str) -> impl Iterator<Item = &'a Thread> + 'a {
        self.threads.iter().filter(move |t| t.project_id == project_id)
    }

    // ---- projects -------------------------------------------------------------------------

    pub fn add_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let root_str = root.to_string_lossy().into_owned();
        if let Some(existing) = self.projects.iter().find(|p| same_path(&p.root, &root_str)) {
            self.active_project = Some(existing.id.clone());
            cx.notify();
            return;
        }
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_str.clone());
        let project = Project {
            id: new_id(),
            name,
            root: root_str,
            sort_order: self.projects.iter().map(|p| p.sort_order + 1).max().unwrap_or(0),
            collapsed: false,
        };
        let _ = self.store.upsert_project(&project);
        self.active_project = Some(project.id.clone());
        self.projects.push(project);
        cx.notify();
    }

    pub fn rename_project(&mut self, id: &str, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == id) {
            p.name = name;
            let _ = self.store.upsert_project(p);
            cx.notify();
        }
    }

    pub fn toggle_project_collapsed(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == id) {
            p.collapsed = !p.collapsed;
            let _ = self.store.upsert_project(p);
            cx.notify();
        }
    }

    pub fn move_project(&mut self, id: &str, delta: isize, cx: &mut Context<Self>) {
        let Some(from) = self.projects.iter().position(|p| p.id == id) else { return };
        let to = (from as isize + delta).clamp(0, self.projects.len() as isize - 1) as usize;
        if from == to {
            return;
        }
        let p = self.projects.remove(from);
        self.projects.insert(to, p);
        for (i, p) in self.projects.iter_mut().enumerate() {
            p.sort_order = i as i64;
            let _ = self.store.upsert_project(p);
        }
        cx.notify();
    }

    pub fn remove_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let doomed: Vec<ThreadId> = self.threads_for(id).map(|t| t.id.clone()).collect();
        for tid in &doomed {
            self.drop_thread_runtime(tid);
        }
        self.threads.retain(|t| t.project_id != id);
        self.projects.retain(|p| p.id != id);
        let _ = self.store.delete_project(id);
        if self.active_thread.as_ref().is_some_and(|t| doomed.contains(t)) {
            self.select_thread(None, cx);
        }
        if self.active_project.as_deref() == Some(id) {
            self.active_project = self.projects.first().map(|p| p.id.clone());
        }
        cx.notify();
    }

    // ---- threads --------------------------------------------------------------------------

    pub fn new_thread(&mut self, project_id: &str, cx: &mut Context<Self>) -> ThreadId {
        // Reuse an untouched draft thread instead of piling up empty ones.
        let draft = self
            .threads_for(project_id)
            .find(|t| t.session_id.is_none() && self.store.items(&t.id).map(|i| i.is_empty()).unwrap_or(false))
            .map(|t| t.id.clone());
        if let Some(existing) = draft {
            self.select_thread(Some(existing.clone()), cx);
            return existing;
        }
        let last = self.store.get_kv("last_model").unwrap_or_else(|| "sonnet".into());
        let mode = self
            .store
            .get_kv("last_mode")
            .map(|m| RuntimeMode::parse(&m))
            .unwrap_or(RuntimeMode::Supervised);
        let now = now_ms();
        let thread = Thread {
            id: new_id(),
            project_id: project_id.to_owned(),
            title: "New thread".into(),
            created_at: now,
            updated_at: now,
            session_id: None,
            model: last,
            mode,
        };
        let _ = self.store.upsert_thread(&thread);
        let id = thread.id.clone();
        self.threads.insert(0, thread);
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == project_id) {
            if p.collapsed {
                p.collapsed = false;
                let _ = self.store.upsert_project(p);
            }
        }
        self.select_thread(Some(id.clone()), cx);
        id
    }

    pub fn select_thread(&mut self, id: Option<ThreadId>, cx: &mut Context<Self>) {
        if let Some(t) = id.as_deref().and_then(|id| self.thread(id)) {
            self.active_project = Some(t.project_id.clone());
        }
        self.active_thread = id;
        self.store
            .set_kv("active_thread", self.active_thread.as_deref().unwrap_or(""));
        cx.emit(AppEvent::ActiveThreadChanged);
        cx.notify();
    }

    pub fn select_project(&mut self, id: &str, cx: &mut Context<Self>) {
        self.active_project = Some(id.to_owned());
        cx.notify();
    }

    pub fn rename_thread(&mut self, id: &str, title: String, cx: &mut Context<Self>) {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return;
        }
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == id) {
            t.title = title;
            let _ = self.store.upsert_thread(t);
            cx.notify();
        }
    }

    pub fn delete_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        self.drop_thread_runtime(id);
        self.threads.retain(|t| t.id != id);
        let _ = self.store.delete_thread(id);
        if self.active_thread.as_deref() == Some(id) {
            self.select_thread(None, cx);
        }
        cx.notify();
    }

    fn drop_thread_runtime(&mut self, id: &str) {
        if let Some(session) = self.sessions.remove(id) {
            session.kill();
        }
        self.timelines.remove(id);
        self.running.remove(id);
    }

    pub fn set_thread_model(&mut self, id: &str, model: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == id) {
            t.model = model.to_owned();
            let _ = self.store.upsert_thread(t);
        }
        self.store.set_kv("last_model", model);
        if let Some(s) = self.sessions.get(id) {
            let _ = s.set_model(model);
        }
        cx.notify();
    }

    pub fn set_thread_mode(&mut self, id: &str, mode: RuntimeMode, cx: &mut Context<Self>) {
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == id) {
            t.mode = mode;
            let _ = self.store.upsert_thread(t);
        }
        self.store.set_kv("last_mode", mode.as_str());
        if let Some(s) = self.sessions.get(id) {
            let _ = s.set_permission_mode(mode);
        }
        cx.notify();
    }

    /// The thread's timeline entity, loading it from SQLite on first use.
    pub fn timeline(&mut self, id: &str, cx: &mut Context<Self>) -> Entity<Timeline> {
        if let Some(t) = self.timelines.get(id) {
            return t.clone();
        }
        let rows = self.store.items(id).unwrap_or_default();
        let thread_id = id.to_owned();
        let running = self.running.contains(id);
        let timeline = cx.new(|cx| {
            let mut t = Timeline::new(thread_id, rows, cx);
            t.running = running;
            t
        });
        self.timelines.insert(id.to_owned(), timeline.clone());
        timeline
    }

    // ---- turns ----------------------------------------------------------------------------

    pub fn send_message(&mut self, thread_id: &str, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(thread) = self.thread(thread_id).cloned() else { return };
        let Some(project) = self.project(&thread.project_id).cloned() else { return };
        let timeline = self.timeline(thread_id, cx);

        if !self.sessions.contains_key(thread_id) {
            let tx = self.events_tx.clone();
            let tid = thread_id.to_owned();
            let config = SpawnConfig {
                cwd: PathBuf::from(&project.root),
                model: thread.model.clone(),
                mode: thread.mode,
                resume: thread.session_id.clone(),
            };
            match ClaudeSession::spawn(config, move |event| {
                let _ = tx.send((tid.clone(), event));
            }) {
                Ok(session) => {
                    self.sessions.insert(thread_id.to_owned(), session);
                }
                Err(err) => {
                    let store = &self.store;
                    timeline.update(cx, |t, cx| {
                        t.push(Item::User { text: text.clone() }, store, true, cx);
                        t.push(
                            Item::Notice { text: format!("Could not start Claude Code: {err:#}"), error: true },
                            store,
                            true,
                            cx,
                        );
                    });
                    return;
                }
            }
        }

        let store = &self.store;
        timeline.update(cx, |t, cx| {
            t.push(Item::User { text: text.clone() }, store, true, cx);
            t.set_running(true, cx);
            t.scroller.update(cx, |s, cx| s.scroll_to_end(cx));
        });
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == thread_id) {
            if t.title == "New thread" {
                t.title = title_from_prompt(&text);
            }
            t.updated_at = now_ms();
            let _ = self.store.upsert_thread(t);
        }
        self.threads.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        self.running.insert(thread_id.to_owned());

        if let Err(err) = self.sessions[thread_id].send_user_message(&text) {
            self.session_failed(thread_id, format!("Claude Code stopped accepting input: {err}"), cx);
        }
        cx.notify();
    }

    pub fn interrupt(&mut self, thread_id: &str) {
        if let Some(s) = self.sessions.get(thread_id) {
            let _ = s.interrupt();
        }
    }

    pub fn respond_approval(&mut self, thread_id: &str, request_id: &str, allow: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.get(thread_id) {
            let _ = s.respond_permission(request_id, allow);
        }
        if let Some(timeline) = self.timelines.get(thread_id).cloned() {
            let store = &self.store;
            timeline.update(cx, |t, cx| {
                if let Some(i) = t.rows.iter().position(|r| {
                    matches!(&r.item, Item::Approval { request_id: rid, .. } if rid == request_id)
                }) {
                    if let Item::Approval { state, .. } = &mut t.rows[i].item {
                        *state = if allow { ApprovalState::Allowed } else { ApprovalState::Denied };
                    }
                    t.changed(i, Some(store), cx);
                }
            });
        }
    }

    fn session_failed(&mut self, thread_id: &str, message: String, cx: &mut Context<Self>) {
        self.sessions.remove(thread_id);
        self.running.remove(thread_id);
        if let Some(timeline) = self.timelines.get(thread_id).cloned() {
            let store = &self.store;
            timeline.update(cx, |t, cx| {
                t.end_stream(store, cx);
                t.push(Item::Notice { text: message, error: true }, store, true, cx);
                t.set_running(false, cx);
            });
        }
        cx.notify();
    }

    fn handle_claude_event(&mut self, thread_id: ThreadId, event: ClaudeEvent, cx: &mut Context<Self>) {
        if !self.threads.iter().any(|t| t.id == thread_id) {
            return;
        }
        let timeline = self.timeline(&thread_id, cx);
        let store = &self.store;
        match event {
            ClaudeEvent::SessionStarted { session_id, .. } => {
                if let Some(t) = self.threads.iter_mut().find(|t| t.id == thread_id) {
                    if t.session_id.as_deref() != Some(&session_id) && !session_id.is_empty() {
                        t.session_id = Some(session_id);
                        let _ = self.store.upsert_thread(t);
                    }
                }
            }
            ClaudeEvent::StreamText(d) => timeline.update(cx, |t, cx| t.stream(StreamKind::Text, &d, store, cx)),
            ClaudeEvent::StreamThinking(d) => {
                timeline.update(cx, |t, cx| t.stream(StreamKind::Thinking, &d, store, cx))
            }
            ClaudeEvent::FinalText(text) => {
                timeline.update(cx, |t, cx| t.finalize(StreamKind::Text, text, store, cx))
            }
            ClaudeEvent::FinalThinking(text) => {
                timeline.update(cx, |t, cx| t.finalize(StreamKind::Thinking, text, store, cx))
            }
            ClaudeEvent::ToolUse { id, name, input } => timeline.update(cx, |t, cx| {
                t.end_stream(store, cx);
                let summary = summarize_tool_input(&name, &input);
                let i = t.push(
                    Item::Tool {
                        tool_use_id: id.clone(),
                        name,
                        summary,
                        output: None,
                        status: ToolStatus::Running,
                    },
                    store,
                    true,
                    cx,
                );
                t.tools.insert(id, i);
            }),
            ClaudeEvent::ToolResult { tool_use_id, output, is_error } => timeline.update(cx, |t, cx| {
                if let Some(&i) = t.tools.get(&tool_use_id) {
                    if let Item::Tool { output: o, status, .. } = &mut t.rows[i].item {
                        *o = Some(output);
                        *status = if is_error { ToolStatus::Failed } else { ToolStatus::Done };
                    }
                    t.changed(i, Some(store), cx);
                }
            }),
            ClaudeEvent::PermissionRequest { request_id, tool_name, input } => {
                timeline.update(cx, |t, cx| {
                    t.end_stream(store, cx);
                    let summary = summarize_tool_input(&tool_name, &input);
                    t.push(
                        Item::Approval { request_id, tool: tool_name, summary, state: ApprovalState::Pending },
                        store,
                        true,
                        cx,
                    );
                    t.scroller.update(cx, |s, cx| s.scroll_to_end(cx));
                });
            }
            ClaudeEvent::TurnCompleted { duration_ms, cost_usd, is_error, error_text } => {
                timeline.update(cx, |t, cx| {
                    t.end_stream(store, cx);
                    // Tools still "running" at turn end were interrupted.
                    for i in 0..t.rows.len() {
                        if let Item::Tool { status: s @ ToolStatus::Running, .. } = &mut t.rows[i].item {
                            *s = ToolStatus::Failed;
                            t.changed(i, Some(store), cx);
                        }
                    }
                    if let Some(text) = error_text.filter(|_| is_error) {
                        t.push(Item::Notice { text, error: true }, store, true, cx);
                    }
                    t.push(Item::TurnEnd { duration_ms, cost_usd, is_error }, store, true, cx);
                    t.set_running(false, cx);
                });
                self.running.remove(&thread_id);
                if let Some(t) = self.threads.iter_mut().find(|t| t.id == thread_id) {
                    t.updated_at = now_ms();
                    let _ = self.store.upsert_thread(t);
                }
                cx.notify();
            }
            ClaudeEvent::Stderr(line) => {
                log::warn!("claude[{thread_id}]: {line}");
                if line.to_ascii_lowercase().contains("error") {
                    self.last_error = Some(line);
                }
            }
            ClaudeEvent::Exited { code } => {
                if self.sessions.contains_key(&thread_id) {
                    let detail = self.last_error.take().map(|e| format!(": {e}")).unwrap_or_default();
                    self.session_failed(
                        &thread_id,
                        format!("Claude Code exited (code {}){detail}", code.map_or("?".into(), |c| c.to_string())),
                        cx,
                    );
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        for (_, s) in self.sessions.drain() {
            s.kill();
        }
    }
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

/// "3m", "2h", "5d" style relative time, as the upstream sidebar shows.
pub fn relative_time(ms: i64) -> String {
    let secs = ((now_ms() - ms) / 1000).max(0);
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub const GIT_POLL: Duration = Duration::from_secs(4);
