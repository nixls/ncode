//! SQLite persistence. Lives under the OS data dir (`%APPDATA%\ncode` on Windows),
//! never inside T3 Code's own `~/.t3/userdata`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::*;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root TEXT NOT NULL,
    sort_order INTEGER NOT NULL,
    collapsed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    session_id TEXT,
    model TEXT NOT NULL,
    mode TEXT NOT NULL
);
CREATE INDEX threads_by_project ON threads(project_id, updated_at DESC);
CREATE TABLE items (
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    seq INTEGER NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (thread_id, key)
);
CREATE INDEX items_by_thread ON items(thread_id, seq);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#];

pub struct Store {
    conn: Connection,
}

pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NCODE_HOME") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ncode")
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Self::open(dir.join("state.sqlite"))
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let conn = Connection::open(path.into())?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(sql)?;
            conn.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        }
        Ok(Self { conn })
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, root, sort_order, collapsed FROM projects ORDER BY sort_order",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                root: r.get(2)?,
                sort_order: r.get(3)?,
                collapsed: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_project(&self, p: &Project) -> Result<()> {
        self.conn.execute(
            "INSERT INTO projects (id, name, root, sort_order, collapsed) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name=?2, root=?3, sort_order=?4, collapsed=?5",
            params![p.id, p.name, p.root, p.sort_order, p.collapsed as i64],
        )?;
        Ok(())
    }

    pub fn delete_project(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn threads(&self) -> Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, title, created_at, updated_at, session_id, model, mode
             FROM threads ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Thread {
                id: r.get(0)?,
                project_id: r.get(1)?,
                title: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
                session_id: r.get(5)?,
                model: r.get(6)?,
                mode: RuntimeMode::parse(&r.get::<_, String>(7)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_thread(&self, t: &Thread) -> Result<()> {
        self.conn.execute(
            "INSERT INTO threads (id, project_id, title, created_at, updated_at, session_id, model, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET project_id=?2, title=?3, updated_at=?5, session_id=?6, model=?7, mode=?8",
            params![
                t.id,
                t.project_id,
                t.title,
                t.created_at,
                t.updated_at,
                t.session_id,
                t.model,
                t.mode.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn delete_thread(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM threads WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn items(&self, thread_id: &str) -> Result<Vec<TimelineRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, body FROM items WHERE thread_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([thread_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, body) = row?;
            match serde_json::from_str::<Item>(&body) {
                Ok(item) => out.push(TimelineRow { key, item }),
                Err(err) => log::warn!("skipping undecodable item {key}: {err}"),
            }
        }
        Ok(out)
    }

    /// Inserts or replaces one timeline row; `seq` keeps the original position on update.
    pub fn put_item(&self, thread_id: &str, seq: usize, row: &TimelineRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO items (thread_id, key, seq, body) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(thread_id, key) DO UPDATE SET body=?4",
            params![thread_id, row.key, seq as i64, serde_json::to_string(&row.item)?],
        )?;
        Ok(())
    }

    pub fn get_kv(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_kv(&self, key: &str, value: &str) {
        let _ = self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_project_cascades_to_threads_and_items() {
        let store = Store::in_memory().unwrap();
        let p = Project {
            id: "p".into(),
            name: "P".into(),
            root: "C:/p".into(),
            sort_order: 0,
            collapsed: false,
        };
        store.upsert_project(&p).unwrap();
        let t = Thread {
            id: "t".into(),
            project_id: "p".into(),
            title: "T".into(),
            created_at: 1,
            updated_at: 1,
            session_id: None,
            model: "sonnet".into(),
            mode: RuntimeMode::Supervised,
        };
        store.upsert_thread(&t).unwrap();
        let row = TimelineRow {
            key: "k".into(),
            item: Item::User { text: "hi".into() },
        };
        store.put_item("t", 0, &row).unwrap();
        // Updating keeps one row.
        store.put_item("t", 0, &row).unwrap();
        assert_eq!(store.items("t").unwrap().len(), 1);

        store.delete_project("p").unwrap();
        assert!(store.threads().unwrap().is_empty());
        assert!(store.items("t").unwrap().is_empty());
    }
}
