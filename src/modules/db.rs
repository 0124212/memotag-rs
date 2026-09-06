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
pub struct SyncMapping {
    pub id: i64,
    pub memo_id: String,
    pub item_index: i64,
    pub sync_type: String,
    pub caldav_uid: String,
    pub caldav_href: String,
    pub memo_text_hash: String,
    pub caldav_etag: String,
    pub done: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        let exists = Path::new(path).exists();
        let conn = Connection::open(path)?;

        if !exists {
            conn.execute_batch(
                "CREATE TABLE sync_mappings (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    memo_id TEXT NOT NULL,
                    item_index INTEGER NOT NULL,
                    sync_type TEXT NOT NULL DEFAULT 'task',
                    caldav_uid TEXT NOT NULL UNIQUE,
                    caldav_href TEXT NOT NULL DEFAULT '',
                    memo_text_hash TEXT NOT NULL DEFAULT '',
                    caldav_etag TEXT NOT NULL DEFAULT '',
                    done INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE INDEX idx_sync_memo ON sync_mappings(memo_id);
                CREATE INDEX idx_sync_caldav ON sync_mappings(caldav_uid);
                CREATE INDEX idx_sync_type ON sync_mappings(sync_type);"
            )?;
            info!("created new database at {}", path);
        }

        Ok(Self { conn: Mutex::new(conn) })
    }

    pub async fn get_mappings_for_memo(&self, memo_id: &str) -> Result<Vec<SyncMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done, created_at, updated_at
             FROM sync_mappings WHERE memo_id = ?1 ORDER BY sync_type, item_index"
        )?;
        let rows = stmt.query_map(params![memo_id], |row| {
            Ok(SyncMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                item_index: row.get(2)?,
                sync_type: row.get(3)?,
                caldav_uid: row.get(4)?,
                caldav_href: row.get(5)?,
                memo_text_hash: row.get(6)?,
                caldav_etag: row.get(7)?,
                done: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut mappings = Vec::new();
        for row in rows {
            mappings.push(row?);
        }
        Ok(mappings)
    }

    pub async fn get_mappings_for_memo_by_type(&self, memo_id: &str, sync_type: &str) -> Result<Vec<SyncMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done, created_at, updated_at
             FROM sync_mappings WHERE memo_id = ?1 AND sync_type = ?2 ORDER BY item_index"
        )?;
        let rows = stmt.query_map(params![memo_id, sync_type], |row| {
            Ok(SyncMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                item_index: row.get(2)?,
                sync_type: row.get(3)?,
                caldav_uid: row.get(4)?,
                caldav_href: row.get(5)?,
                memo_text_hash: row.get(6)?,
                caldav_etag: row.get(7)?,
                done: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        let mut mappings = Vec::new();
        for row in rows {
            mappings.push(row?);
        }
        Ok(mappings)
    }

    pub async fn get_mapping_by_uid(&self, caldav_uid: &str) -> Result<Option<SyncMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done, created_at, updated_at
             FROM sync_mappings WHERE caldav_uid = ?1"
        )?;
        let mut rows = stmt.query_map(params![caldav_uid], |row| {
            Ok(SyncMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                item_index: row.get(2)?,
                sync_type: row.get(3)?,
                caldav_uid: row.get(4)?,
                caldav_href: row.get(5)?,
                memo_text_hash: row.get(6)?,
                caldav_etag: row.get(7)?,
                done: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub async fn get_all_mappings(&self) -> Result<Vec<SyncMapping>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done, created_at, updated_at
             FROM sync_mappings ORDER BY memo_id, sync_type, item_index"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SyncMapping {
                id: row.get(0)?,
                memo_id: row.get(1)?,
                item_index: row.get(2)?,
                sync_type: row.get(3)?,
                caldav_uid: row.get(4)?,
                caldav_href: row.get(5)?,
                memo_text_hash: row.get(6)?,
                caldav_etag: row.get(7)?,
                done: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
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
        item_index: i64,
        sync_type: &str,
        caldav_uid: &str,
        caldav_href: &str,
        memo_text_hash: &str,
        caldav_etag: &str,
        done: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "INSERT INTO sync_mappings (memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(caldav_uid) DO UPDATE SET
               memo_id = excluded.memo_id,
               item_index = excluded.item_index,
               sync_type = excluded.sync_type,
               caldav_href = excluded.caldav_href,
               memo_text_hash = excluded.memo_text_hash,
               caldav_etag = excluded.caldav_etag,
               done = excluded.done,
               updated_at = excluded.updated_at",
            params![memo_id, item_index, sync_type, caldav_uid, caldav_href, memo_text_hash, caldav_etag, done as i64, now],
        )?;
        Ok(())
    }

    pub async fn update_done(&self, caldav_uid: &str, done: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "UPDATE sync_mappings SET done = ?1, updated_at = ?2 WHERE caldav_uid = ?3",
            params![done as i64, now, caldav_uid],
        )?;
        Ok(())
    }

    pub async fn update_etag(&self, caldav_uid: &str, etag: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "UPDATE sync_mappings SET caldav_etag = ?1, updated_at = ?2 WHERE caldav_uid = ?3",
            params![etag, now, caldav_uid],
        )?;
        Ok(())
    }

    pub async fn delete_mapping(&self, caldav_uid: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM sync_mappings WHERE caldav_uid = ?1",
            params![caldav_uid],
        )?;
        Ok(())
    }

    pub async fn get_all_memo_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT DISTINCT memo_id FROM sync_mappings")?;
        let ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(ids.filter_map(|r| r.ok()).collect())
    }
}
