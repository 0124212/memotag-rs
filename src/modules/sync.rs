use anyhow::Result;
use sha2::{Sha256, Digest};
use std::time::Duration;
use tracing::{info, warn};

use super::caldav::{self, CalDavClient, VTodo, VTodoStatus};
use super::config::Config;
use super::db::Database;
use super::memos::MemosClient;
use super::parser;

pub struct SyncService {
    caldav: Option<CalDavClient>,
    memos: MemosClient,
    db: Database,
    poll_interval: Duration,
}

impl SyncService {
    pub fn new(config: &Config, db: Database) -> Result<Self> {
        let memos = MemosClient::new(config.memos_url.clone(), config.memos_token.clone());

        let caldav = if config.caldav.is_configured() {
            Some(CalDavClient::new(
                &config.caldav.url,
                &config.caldav.path,
                &config.caldav.username,
                &config.caldav.password,
            ))
        } else {
            None
        };

        let poll_interval = Duration::from_secs(config.caldav.poll_secs);

        Ok(Self { caldav, memos, db, poll_interval })
    }

    pub async fn ensure_cal(&self) -> Result<()> {
        if let Some(ref cal) = self.caldav {
            cal.ensure_collection().await?;
        }
        Ok(())
    }

    /// Run the initial full sync (memo → CalDAV).
    pub async fn full_sync(&self) -> Result<()> {
        if self.caldav.is_none() {
            info!("CalDAV not configured, skipping sync");
            return Ok(());
        }
        let cal = self.caldav.as_ref().unwrap();

        info!("starting full sync: memo → CalDAV");
        let all_memos = self.memos.list_all_memos().await?;
        info!("fetched {} memos from server", all_memos.len());

        let all_mappings = self.db.get_all_mappings().await?;
        let mapping_count = all_mappings.len();
        info!("{} task mappings in database", mapping_count);

        let mut synced = 0;

        for memo in &all_memos {
            if let Err(e) = self.sync_memo_to_caldav(memo).await {
                warn!("failed to sync memo {}: {}", memo.name, e);
            } else {
                synced += 1;
            }
        }

        // Clean up CalDAV items that no longer exist in any memo
        let memo_ids_with_tasks = self.db.get_all_memo_ids_with_tasks().await?;
        let memo_ids: Vec<String> = all_memos.iter().map(|m| MemosClient::extract_memo_id(&m.name)).collect();

        for task_memo_id in &memo_ids_with_tasks {
            if !memo_ids.contains(task_memo_id) {
                info!("memo {} no longer exists, cleaning up tasks", task_memo_id);
                let mappings = self.db.get_mappings_for_memo(task_memo_id).await?;
                for mapping in &mappings {
                    if let Err(e) = cal.delete_vtodo(&mapping.caldav_href).await {
                        warn!("failed to delete orphan VTODO {}: {}", mapping.caldav_uid, e);
                    }
                    self.db.delete_mapping(&mapping.caldav_uid).await?;
                }
            }
        }

        self.db.update_sync_state(true).await?;
        info!("full sync complete: {} memos synced", synced);
        Ok(())
    }

    /// Sync a single memo's tasks to CalDAV.
    async fn sync_memo_to_caldav(&self, memo: &super::memos::Memo) -> Result<()> {
        let cal = self.caldav.as_ref().unwrap();
        let memo_id = MemosClient::extract_memo_id(&memo.name);
        let tasks = parser::parse_tasks(&memo.content);

        if tasks.is_empty() {
            let existing = self.db.get_mappings_for_memo(&memo_id).await?;
            if !existing.is_empty() {
                info!("memo {} has no tasks, removing {} existing VTODOs", memo_id, existing.len());
                for mapping in &existing {
                    let _ = cal.delete_vtodo(&mapping.caldav_href).await;
                    self.db.delete_mapping(&mapping.caldav_uid).await?;
                }
            }
            return Ok(());
        }

        let existing_mappings = self.db.get_mappings_for_memo(&memo_id).await?;
        let content_hash = hash_content(&memo.content);

        let mut index_map: std::collections::HashMap<i64, &super::db::TaskMapping> = std::collections::HashMap::new();
        for m in &existing_mappings {
            index_map.insert(m.task_index, m);
        }

        let mut existing_indices: std::collections::HashSet<i64> = index_map.keys().cloned().collect();
        let _new_indices: std::collections::HashSet<i64> = std::collections::HashSet::new();

        for task in &tasks {
            let task_idx = task.index as i64;

            if let Some(mapping) = index_map.get(&task_idx) {
                let task_changed = mapping.memo_text_hash != content_hash || mapping.done != task.done;

                if task_changed {
                    let new_status = if task.done {
                        VTodoStatus::Completed
                    } else {
                        VTodoStatus::NeedAction
                    };

                    let vtodo = VTodo {
                        uid: mapping.caldav_uid.clone(),
                        summary: task.text.clone(),
                        status: new_status,
                        due: task.due_date.clone(),
                        priority: task.priority,
                        description: Some(format!("From memo: {}", memo.name)),
                        ical_data: String::new(),
                    };

                    match cal.put_vtodo(&mapping.caldav_href, &vtodo, Some(&mapping.vtodo_etag)).await {
                        Ok(new_etag) => {
                            self.db.upsert_mapping(
                                &memo_id, task_idx,
                                &mapping.caldav_uid, &mapping.caldav_href,
                                &content_hash, &new_etag, task.done,
                            ).await?;
                        }
                        Err(e) => warn!("update VTODO {} failed: {}", mapping.caldav_uid, e),
                    }
                }
                existing_indices.remove(&task_idx);
            } else {
                let uid = parser::task_uid(&memo_id, task.index);
                let href = format!("{}.ics", uid);
                let new_status = if task.done {
                    VTodoStatus::Completed
                } else {
                    VTodoStatus::NeedAction
                };

                let vtodo = VTodo {
                    uid: uid.clone(),
                    summary: task.text.clone(),
                    status: new_status,
                    due: task.due_date.clone(),
                    priority: task.priority,
                    description: Some(format!("From memo: {}", memo.name)),
                    ical_data: String::new(),
                };

                match cal.put_vtodo(&href, &vtodo, None).await {
                    Ok(etag) => {
                        self.db.upsert_mapping(
                            &memo_id, task_idx,
                            &uid, &href,
                            &content_hash, &etag, task.done,
                        ).await?;
                        info!("created VTODO {} for memo {} task {}", uid, memo_id, task.index);
                    }
                    Err(e) => warn!("create VTODO failed for {}: {}", uid, e),
                }
            }
        }

        for orphan_idx in existing_indices {
            if let Some(mapping) = index_map.get(&orphan_idx) {
                let _ = cal.delete_vtodo(&mapping.caldav_href).await;
                self.db.delete_mapping(&mapping.caldav_uid).await?;
                info!("deleted orphan VTODO {} (task removed from memo)", mapping.caldav_uid);
            }
        }

        Ok(())
    }

    /// Poll CalDAV for changes and update memos accordingly (CalDAV → Memo).
    pub async fn poll_caldav(&self) -> Result<()> {
        if self.caldav.is_none() {
            return Ok(());
        }
        let cal = self.caldav.as_ref().unwrap();

        let resources = cal.list_vtodos().await?;

        for resource in &resources {
            if !resource.href.ends_with(".ics") {
                continue;
            }

            let full_resource = if resource.data.is_some() {
                resource.clone()
            } else {
                match cal.get_vtodo(&resource.href).await? {
                    Some(r) => r,
                    None => continue,
                }
            };

            let vtodo_data = match &full_resource.data {
                Some(d) => d,
                None => continue,
            };

            let vtodo = match caldav::parse_vtodo(vtodo_data) {
                Some(v) => v,
                None => continue,
            };

            if let Some(mapping) = self.db.get_mapping_by_uid(&vtodo.uid).await? {
                if mapping.done != (vtodo.status == VTodoStatus::Completed) || mapping.vtodo_etag != full_resource.etag {
                    let memo = self.memos.get_memo(&format!("memos/{}", mapping.memo_id)).await?;
                    let new_done = vtodo.status == VTodoStatus::Completed;
                    let tasks = parser::parse_tasks(&memo.content);

                    if let Some(task) = tasks.iter().find(|t| t.index as i64 == mapping.task_index) {
                        if task.done != new_done {
                            let (new_content, changed) = parser::replace_task_line(
                                &memo.content, task.line_number, new_done, &task.text
                            );
                            if changed {
                                if let Err(e) = self.memos.update_memo(&memo.name, &new_content).await {
                                    warn!("failed to update memo from CalDAV: {}", e);
                                } else {
                                    info!("updated memo {} from CalDAV change", memo.name);
                                    self.db.update_mapping_done(&vtodo.uid, new_done).await?;
                                }
                            }
                        } else {
                            self.db.update_mapping_etag(&vtodo.uid, &full_resource.etag).await?;
                        }
                    }
                }
            } else {
                info!("unknown VTODO {} in CalDAV, skipping", vtodo.uid);
            }
        }

        self.db.update_sync_state(false).await?;
        Ok(())
    }
}

fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}
