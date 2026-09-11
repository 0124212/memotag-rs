use anyhow::Result;
use sha2::{Sha256, Digest};
use std::time::Duration;
use tracing::{info, warn};

use super::caldav::{self, CalDavClient, CalDavItem, VTodo, VTodoStatus, VEvent};
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
        info!("{} sync mappings in database", all_mappings.len());

        let mut synced = 0;

        for memo in &all_memos {
            if let Err(e) = self.sync_memo_to_caldav(memo).await {
                warn!("failed to sync memo {}: {}", memo.name, e);
            } else {
                synced += 1;
            }
        }

        // Clean up CalDAV items that no longer exist in any memo
        let memo_ids_with_items = self.db.get_all_memo_ids().await?;
        let memo_ids: Vec<String> = all_memos.iter().map(|m| MemosClient::extract_memo_id(&m.name)).collect();

        for item_memo_id in &memo_ids_with_items {
            if !memo_ids.contains(item_memo_id) {
                info!("memo {} no longer exists, cleaning up items", item_memo_id);
                let mappings = self.db.get_mappings_for_memo(item_memo_id).await?;
                for mapping in &mappings {
                    if let Err(e) = cal.delete_item(&mapping.caldav_href).await {
                        warn!("failed to delete orphan item {}: {}", mapping.caldav_uid, e);
                    }
                    self.db.delete_mapping(&mapping.caldav_uid).await?;
                }
            }
        }

        info!("full sync complete: {} memos synced", synced);
        Ok(())
    }

    /// Sync a single memo's tasks and events to CalDAV.
    async fn sync_memo_to_caldav(&self, memo: &super::memos::Memo) -> Result<()> {
        let cal = self.caldav.as_ref().unwrap();
        let memo_id = MemosClient::extract_memo_id(&memo.name);
        let content_hash = hash_content(&memo.content);

        // Sync tasks (VTODO)
        let tasks = parser::parse_tasks(&memo.content);
        self.sync_items(
            cal, &memo_id, &memo.name, &content_hash,
            &tasks, "task",
        ).await?;

        // Sync events (VEVENT)
        let events = parser::parse_events(&memo.content);
        self.sync_events(
            cal, &memo_id, &memo.name, &content_hash,
            &events,
        ).await?;

        Ok(())
    }

    /// Sync task items (VTODO) for a memo.
    async fn sync_items(
        &self,
        cal: &CalDavClient,
        memo_id: &str,
        memo_name: &str,
        content_hash: &str,
        tasks: &[parser::ParsedTask],
        sync_type: &str,
    ) -> Result<()> {
        let existing_mappings = self.db.get_mappings_for_memo_by_type(memo_id, sync_type).await?;

        if tasks.is_empty() && !existing_mappings.is_empty() {
            info!("memo {} has no {}, removing {} existing items", memo_id, sync_type, existing_mappings.len());
            for mapping in &existing_mappings {
                let _ = cal.delete_item(&mapping.caldav_href).await;
                self.db.delete_mapping(&mapping.caldav_uid).await?;
            }
            return Ok(());
        }

        let mut index_map: std::collections::HashMap<i64, &super::db::SyncMapping> = std::collections::HashMap::new();
        for m in &existing_mappings {
            index_map.insert(m.item_index, m);
        }

        let mut existing_indices: std::collections::HashSet<i64> = index_map.keys().cloned().collect();

        for task in tasks {
            let idx = task.index as i64;

            if let Some(mapping) = index_map.get(&idx) {
                let changed = mapping.memo_text_hash != content_hash || mapping.done != task.done;

                if changed {
                    let new_status = if task.done { VTodoStatus::Completed } else { VTodoStatus::NeedAction };
                    let item = CalDavItem::Todo(VTodo {
                        uid: mapping.caldav_uid.clone(),
                        summary: task.text.clone(),
                        status: new_status,
                        due: task.due_date.clone(),
                        priority: task.priority,
                        description: Some(format!("From memo: {}", memo_name)),
                    });

                    match cal.put_item(&mapping.caldav_href, &item, Some(&mapping.caldav_etag)).await {
                        Ok(new_etag) => {
                            self.db.upsert_mapping(
                                memo_id, idx, sync_type,
                                &mapping.caldav_uid, &mapping.caldav_href,
                                content_hash, &new_etag, task.done,
                            ).await?;
                        }
                        Err(e) => warn!("update {} {} failed: {}", sync_type, mapping.caldav_uid, e),
                    }
                }
                existing_indices.remove(&idx);
            } else {
                let uid = parser::task_uid(memo_id, task.index);
                let collection = cal.collection_path();
                let href = if collection.is_empty() {
                    format!("{}.ics", uid)
                } else {
                    format!("{}/{}.ics", collection, uid)
                };
                let new_status = if task.done { VTodoStatus::Completed } else { VTodoStatus::NeedAction };
                let item = CalDavItem::Todo(VTodo {
                    uid: uid.clone(),
                    summary: task.text.clone(),
                    status: new_status,
                    due: task.due_date.clone(),
                    priority: task.priority,
                    description: Some(format!("From memo: {}", memo_name)),
                });

                match cal.put_item(&href, &item, None).await {
                    Ok(etag) => {
                        self.db.upsert_mapping(
                            memo_id, idx, sync_type,
                            &uid, &href,
                            content_hash, &etag, task.done,
                        ).await?;
                        info!("created VTODO {} for memo {} task {}", uid, memo_id, task.index);
                    }
                    Err(e) => warn!("create VTODO failed for {}: {}", uid, e),
                }
            }
        }

        for orphan_idx in existing_indices {
            if let Some(mapping) = index_map.get(&orphan_idx) {
                let _ = cal.delete_item(&mapping.caldav_href).await;
                self.db.delete_mapping(&mapping.caldav_uid).await?;
                info!("deleted orphan {} {} (item removed from memo)", sync_type, mapping.caldav_uid);
            }
        }

        Ok(())
    }

    /// Sync event items (VEVENT) for a memo.
    async fn sync_events(
        &self,
        cal: &CalDavClient,
        memo_id: &str,
        memo_name: &str,
        content_hash: &str,
        events: &[parser::ParsedEvent],
    ) -> Result<()> {
        let sync_type = "event";
        let existing_mappings = self.db.get_mappings_for_memo_by_type(memo_id, sync_type).await?;

        if events.is_empty() && !existing_mappings.is_empty() {
            info!("memo {} has no events, removing {} existing VEVENTs", memo_id, existing_mappings.len());
            for mapping in &existing_mappings {
                let _ = cal.delete_item(&mapping.caldav_href).await;
                self.db.delete_mapping(&mapping.caldav_uid).await?;
            }
            return Ok(());
        }

        let mut index_map: std::collections::HashMap<i64, &super::db::SyncMapping> = std::collections::HashMap::new();
        for m in &existing_mappings {
            index_map.insert(m.item_index, m);
        }

        let mut existing_indices: std::collections::HashSet<i64> = index_map.keys().cloned().collect();

        for event in events {
            let idx = event.index as i64;

            if let Some(mapping) = index_map.get(&idx) {
                let changed = mapping.memo_text_hash != content_hash;

                if changed {
                    let dtstart = format!("{}{}", event.date, event.time.as_deref().map(|t| format!("T{}Z", t)).unwrap_or_default());
                    let item = CalDavItem::Event(VEvent {
                        uid: mapping.caldav_uid.clone(),
                        summary: event.summary.clone(),
                        dtstart,
                        dtend: None,
                        description: Some(format!("From memo: {}", memo_name)),
                        all_day: event.all_day,
                    });

                    match cal.put_item(&mapping.caldav_href, &item, Some(&mapping.caldav_etag)).await {
                        Ok(new_etag) => {
                            self.db.upsert_mapping(
                                memo_id, idx, sync_type,
                                &mapping.caldav_uid, &mapping.caldav_href,
                                content_hash, &new_etag, false,
                            ).await?;
                        }
                        Err(e) => warn!("update VEVENT {} failed: {}", mapping.caldav_uid, e),
                    }
                }
                existing_indices.remove(&idx);
            } else {
                let uid = parser::event_uid(memo_id, event.index);
                let href = format!("{}.ics", uid);
                let dtstart = format!("{}{}", event.date, event.time.as_deref().map(|t| format!("T{}Z", t)).unwrap_or_default());
                let item = CalDavItem::Event(VEvent {
                    uid: uid.clone(),
                    summary: event.summary.clone(),
                    dtstart,
                    dtend: None,
                    description: Some(format!("From memo: {}", memo_name)),
                    all_day: event.all_day,
                });

                match cal.put_item(&href, &item, None).await {
                    Ok(etag) => {
                        self.db.upsert_mapping(
                            memo_id, idx, sync_type,
                            &uid, &href,
                            content_hash, &etag, false,
                        ).await?;
                        info!("created VEVENT {} for memo {} event {}", uid, memo_id, event.index);
                    }
                    Err(e) => warn!("create VEVENT failed for {}: {}", uid, e),
                }
            }
        }

        for orphan_idx in existing_indices {
            if let Some(mapping) = index_map.get(&orphan_idx) {
                let _ = cal.delete_item(&mapping.caldav_href).await;
                self.db.delete_mapping(&mapping.caldav_uid).await?;
                info!("deleted orphan VEVENT {} (event removed from memo)", mapping.caldav_uid);
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

        let resources = cal.list_items().await?;

        for resource in &resources {
            if !resource.href.ends_with(".ics") {
                continue;
            }

            let full_resource = if resource.data.is_some() {
                resource.clone()
            } else {
                match cal.get_item(&resource.href).await? {
                    Some(r) => r,
                    None => continue,
                }
            };

            let ical_data = match &full_resource.data {
                Some(d) => d,
                None => continue,
            };

            let item = match caldav::parse_ical(ical_data) {
                Some(i) => i,
                None => continue,
            };

            let (uid, item_done) = match &item {
                CalDavItem::Todo(vt) => (vt.uid.clone(), vt.status == VTodoStatus::Completed),
                CalDavItem::Event(_) => continue, // events are one-way (memo → CalDAV)
            };

            if let Some(mapping) = self.db.get_mapping_by_uid(&uid).await? {
                if mapping.done != item_done || mapping.caldav_etag != full_resource.etag {
                    let memo = self.memos.get_memo(&format!("memos/{}", mapping.memo_id)).await?;
                    let tasks = parser::parse_tasks(&memo.content);

                    if let Some(task) = tasks.iter().find(|t| t.index as i64 == mapping.item_index) {
                        if task.done != item_done {
                            let (new_content, changed) = parser::replace_task_line(
                                &memo.content, task.line_number, item_done, &task.text
                            );
                            if changed {
                                if let Err(e) = self.memos.update_memo(&memo.name, &new_content).await {
                                    warn!("failed to update memo from CalDAV: {}", e);
                                } else {
                                    info!("updated memo {} from CalDAV change", memo.name);
                                    self.db.update_done(&uid, item_done).await?;
                                }
                            }
                        } else {
                            self.db.update_etag(&uid, &full_resource.etag).await?;
                        }
                    }
                }
            } else {
                info!("unknown item {} in CalDAV, skipping", uid);
            }
        }

        Ok(())
    }
}

fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}
