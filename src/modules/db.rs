use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::Path;
use tokio::sync::Mutex;
use tracing::info;

pub struct Database {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct TaskMapping {
    pub id: i64,
    pub memo_id: String,
    pub task_index: i64,
    pub caldav_uid: String,
    pub caldav_href: String,
    pub memo_text_hash: String,
    pub vtodo_etag: String,
    pub done: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct SyncState {
    pub id: i64,
    pub full_sync_at: Option<String>,
    pub last_poll_at: Option<String>,
    pub memo_seq: i64,
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        let exists = Path::new(path).exists();
        let conn = Connection::open(path)?;

        if !exists {
            conn.execute_batch(
                "CREATE TABLE task_mappings (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    memo_id TEXT NOT NULL,
                    task_index INTEGER NOT NULL,
                    caldav_uid TEXT NOT NULL UNIQUE,
                    caldav_href TEXT NOT NULL DEFAULT '',
                    memo_text_hash TEXT NOT NULL DEFAULT '',
                    vtodo_etag TEXT NOT NULL DEFAULT '',
                    done INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE sync_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    full_sync_at TEXT,
                    last_poll_at TEXT,
                    memo_seq INTEGER NOT NULL DEFAULT 0
                );

                INSERT INTO sync_state (id) VALUES (1);

                CREATE INDEX idx_task_memo ON task_mappings(memo_id);
                CREATE INDEX idx_task_caldav ON task_mappings(caldav_uid);"
            )?;
            info!("created new database at {}", path);
        }

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub async fn get_sync_state(&self) -> Result<SyncState> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, full_sync_at, last_poll_at, memo_seq FROM sync_state WHERE id = 1"
        )?;
        let state = stmt.query_row([], |row| {
            Ok(SyncState {
                id: row.get(0)?,
                full_sync_at: row.get(1)?,
                last_poll_at: row.get(2)?,
                memo_seq: row.get(3)?,
            })
        })?;
        Ok(state)
    }

    pub async fn update_sync_state(&self, full_sync: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        if full_sync {
            conn.execute(
                "UPDATE sync_state SET full_sync_at = ?, last_poll_at = ?, memo_seq = memo_seq + 1 WHERE id = 1",
                params![now, now],
            )?;
        } else {
            conn.execute(
                "UPDATE sync_state SET last_poll_at = ?, memo_seq = memo_seq + 1 WHERE id = 1",
                params![now],
            )?;
        }
        Ok(())
    }

    pub async fn get_mappings_for_memo(&self, memo_id: &str) -> Result<Vec<TaskMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, task_index, caldav_uid, caldav_href, memo_text_hash, vtodo_etag, done, created_at, updated_at
             FROM task_mappings WHERE memo_id = ?1 ORDER BY task_index"
        )?;
        let rows = stmt.query_map(params![memo_id], |row| {
            Ok(TaskMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                task_index: row.get(2)?,
                caldav_uid: row.get(3)?,
                caldav_href: row.get(4)?,
                memo_text_hash: row.get(5)?,
                vtodo_etag: row.get(6)?,
                done: row.get::<_, i64>(7)? != 0,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        let mut mappings = Vec::new();
        for row in rows {
            mappings.push(row?);
        }
        Ok(mappings)
    }

    pub async fn get_mapping_by_uid(&self, caldav_uid: &str) -> Result<Option<TaskMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, task_index, caldav_uid, caldav_href, memo_text_hash, vtodo_etag, done, created_at, updated_at
             FROM task_mappings WHERE caldav_uid = ?1"
        )?;
        let mut rows = stmt.query_map(params![caldav_uid], |row| {
            Ok(TaskMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                task_index: row.get(2)?,
                caldav_uid: row.get(3)?,
                caldav_href: row.get(4)?,
                memo_text_hash: row.get(5)?,
                vtodo_etag: row.get(6)?,
                done: row.get::<_, i64>(7)? != 0,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub async fn get_all_mappings(&self) -> Result<Vec<TaskMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, task_index, caldav_uid, caldav_href, memo_text_hash, vtodo_etag, done, created_at, updated_at
             FROM task_mappings ORDER BY memo_id, task_index"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TaskMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                task_index: row.get(2)?,
                caldav_uid: row.get(3)?,
                caldav_href: row.get(4)?,
                memo_text_hash: row.get(5)?,
                vtodo_etag: row.get(6)?,
                done: row.get::<_, i64>(7)? != 0,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        let mut mappings = Vec::new();
        for row in rows {
            mappings.push(row?);
        }
        Ok(mappings)
    }

    pub async fn upsert_mapping(
        &self,
        memo_id: &str,
        task_index: i64,
        caldav_uid: &str,
        caldav_href: &str,
        memo_text_hash: &str,
        vtodo_etag: &str,
        done: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "INSERT INTO task_mappings (memo_id, task_index, caldav_uid, caldav_href, memo_text_hash, vtodo_etag, done, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT(caldav_uid) DO UPDATE SET
               memo_id = excluded.memo_id,
               task_index = excluded.task_index,
               caldav_href = excluded.caldav_href,
               memo_text_hash = excluded.memo_text_hash,
               vtodo_etag = excluded.vtodo_etag,
               done = excluded.done,
               updated_at = excluded.updated_at",
            params![memo_id, task_index, caldav_uid, caldav_href, memo_text_hash, vtodo_etag, done as i64, now],
        )?;
        Ok(())
    }

    pub async fn update_mapping_done(&self, caldav_uid: &str, done: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "UPDATE task_mappings SET done = ?1, updated_at = ?2 WHERE caldav_uid = ?3",
            params![done as i64, now, caldav_uid],
        )?;
        Ok(())
    }

    pub async fn update_mapping_etag(&self, caldav_uid: &str, etag: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "UPDATE task_mappings SET vtodo_etag = ?1, updated_at = ?2 WHERE caldav_uid = ?3",
            params![etag, now, caldav_uid],
        )?;
        Ok(())
    }

    pub async fn delete_mapping(&self, caldav_uid: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM task_mappings WHERE caldav_uid = ?1",
            params![caldav_uid],
        )?;
        Ok(())
    }

    pub async fn delete_mappings_for_memo(&self, memo_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM task_mappings WHERE memo_id = ?1",
            params![memo_id],
        )?;
        Ok(())
    }

    pub async fn get_all_memo_ids_with_tasks(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT memo_id FROM task_mappings"
        )?;
        let ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(ids.filter_map(|r| r.ok()).collect())
    }

    pub async fn count_mappings(&self) -> Result<i64> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_mappings",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}
