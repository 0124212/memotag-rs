use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;
use reqwest::Client;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

use super::memos::{Memo, MemosClient};

pub struct Autotagger {
    client: Client,
    memos: MemosClient,
    default_tag: String,
    interval: Duration,
    re_ansi: Regex,
    re_link: Regex,
    re_image: Regex,
    re_audio: Regex,
    re_video: Regex,
    re_code: Regex,
    re_task: Regex,
    re_quote: Regex,
    re_file_ext: Regex,
    re_hashtag: Regex,
    re_date: Regex,
    known_exts: HashSet<&'static str>,
    vikunja_url: String,
    vikunja_user: String,
    vikunja_pass: String,
    vikunja_project: i32,
    vikunja_token_cache: tokio::sync::OnceCell<String>,
    radicale_url: String,
    radicale_user: String,
    radicale_pass: String,
}

impl Autotagger {
    pub fn new(
        memos: MemosClient,
        default_tag: String,
        interval_secs: u64,
        vikunja_url: String,
        vikunja_user: String,
        vikunja_pass: String,
        vikunja_project: i32,
        radicale_url: String,
        radicale_user: String,
        radicale_pass: String,
    ) -> Self {
        Self {
            client: Client::new(),
            memos,
            default_tag,
            interval: Duration::from_secs(interval_secs),
            re_ansi: Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap(),
            re_link: Regex::new(r"https?://[^\s\)\]]+").unwrap(),
            re_image: Regex::new(r"(?i)(<img\s|!\[[^\]]*\]\([^)]*\)|\.(png|jpe?g|gif|bmp|svg|webp|tiff?|ico|heic|heif|avif)[\s\)\]\?])").unwrap(),
            re_audio: Regex::new(r"(?i)\.(mp3|wav|ogg|m4a|flac|aac|wma|opus)[\s\)\]\?]").unwrap(),
            re_video: Regex::new(r"(?i)\.(mp4|mkv|webm|avi|mov|flv|m4v|3gp|ogv)[\s\)\]\?]").unwrap(),
            re_code: Regex::new(r"```(rust|python|js|ts|go|bash|sh|sql|yaml|json|toml)").unwrap(),
            re_task: Regex::new(r"^- \[[ x]\]").unwrap(),
            re_quote: Regex::new(r"^>").unwrap(),
            re_file_ext: Regex::new(r"(?i)\.([a-z]{2,10})(?:\s|$|\)|\]|\?)").unwrap(),
            re_hashtag: Regex::new(r"(\s*)#([^\s#]+)").unwrap(),
            re_date: Regex::new(r"(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2})").unwrap(),
            known_exts: [
                "txt", "md", "pdf", "docx", "pptx", "xlsx", "csv", "json", "yaml", "yml",
                "toml", "xml", "html", "css", "js", "ts", "jsx", "tsx", "rs", "go",
                "py", "rb", "java", "c", "cpp", "h", "sh", "bash", "zsh", "sql",
                "r", "lua", "zig", "nim", "ex", "exs", "erl", "hs", "ml", "swift",
                "kt", "scala", "cs", "fs", "vb", "php", "pl", "pm", "raku",
                "dockerfile", "makefile", "cmake", "gradle", "sbt", "cabal",
                "gitignore", "env", "lock", "log", "ini", "cfg", "conf",
                "tar", "gz", "zip", "bz2", "xz", "tgz", "7z", "rar",
                "jpg", "jpeg", "png", "gif", "bmp", "svg", "webp", "tiff", "ico", "heic", "avif",
                "mp3", "wav", "ogg", "m4a", "flac", "aac", "opus",
                "mp4", "mkv", "webm", "avi", "mov", "flv",
                "exe", "dmg", "rpm", "deb", "apk", "msi",
                "pem", "key", "crt", "cert",
                "patch", "diff",
            ].into_iter().collect(),
            vikunja_url,
            vikunja_user,
            vikunja_pass,
            vikunja_project,
            vikunja_token_cache: tokio::sync::OnceCell::new(),
            radicale_url,
            radicale_user,
            radicale_pass,
        }
    }

    fn detect_tags(&self, memo: &Memo) -> Vec<String> {
        let mut tags = Vec::new();

        for att in &memo.attachments {
            let mime = att.mime_type.to_lowercase();
            let fname = att.filename.to_lowercase();
            if mime.starts_with("image/") || fname.ends_with(".jpg") || fname.ends_with(".jpeg") || fname.ends_with(".png") || fname.ends_with(".gif") || fname.ends_with(".webp") || fname.ends_with(".heic") {
                if !tags.contains(&"image".to_string()) {
                    tags.push("image".to_string());
                }
            }
            if mime.starts_with("audio/") || fname.ends_with(".mp3") || fname.ends_with(".wav") || fname.ends_with(".m4a") || fname.ends_with(".flac") {
                if !tags.contains(&"audio".to_string()) {
                    tags.push("audio".to_string());
                }
            }
            if mime.starts_with("video/") || fname.ends_with(".mp4") || fname.ends_with(".mov") || fname.ends_with(".avi") {
                if !tags.contains(&"video".to_string()) {
                    tags.push("video".to_string());
                }
            }
        }

        let clean = self.re_ansi.replace_all(&memo.content, "");

        if self.re_link.is_match(&clean) { tags.push("link".to_string()); }
        if self.re_image.is_match(&clean) { tags.push("image".to_string()); }
        if self.re_audio.is_match(&clean) { tags.push("audio".to_string()); }
        if self.re_video.is_match(&clean) { tags.push("video".to_string()); }
        if self.re_code.is_match(&clean) { tags.push("code".to_string()); }
        if self.re_task.is_match(&clean) { tags.push("task".to_string()); }
        if self.re_quote.is_match(&clean) { tags.push("quote".to_string()); }

        for cap in self.re_file_ext.captures_iter(&clean) {
            if let Some(ext) = cap.get(1) {
                let tag = ext.as_str().to_lowercase();
                if !tags.contains(&tag) && self.known_exts.contains(tag.as_str()) {
                    tags.push(tag);
                }
            }
        }

        if tags.is_empty() {
            tags.push(self.default_tag.clone());
        }

        tags
    }

    fn existing_hashtags(&self, content: &str) -> HashSet<String> {
        self.re_hashtag.captures_iter(content)
            .map(|c| c[2].to_lowercase())
            .collect()
    }

    pub async fn run(&self) -> Result<()> {
        info!(
            "autotagger starting: default_tag={}, interval={}s",
            self.default_tag,
            self.interval.as_secs()
        );

        loop {
            if let Err(e) = self.process_once().await {
                warn!("error in autotagger loop: {}", e);
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    async fn process_once(&self) -> Result<()> {
        let mut page_token: Option<String> = None;
        let mut total_tagged = 0;
        let mut total_scanned = 0;

        loop {
            let (memos, next) = self.memos.list_memos(page_token.as_deref()).await?;

            for memo in memos {
                if memo.content.trim().is_empty() {
                    continue;
                }

                total_scanned += 1;

                let mut new_content = memo.content.clone();
                let mut changed = false;
                if new_content.to_lowercase().contains("#untagged") {
                    let before = new_content.clone();
                    new_content = new_content.replace("#untagged", "#inbox").replace("#Untagged", "#inbox").replace("#UNTAGGED", "#inbox");
                    if new_content != before { changed = true; }
                }
                if new_content.to_lowercase().contains("#tasks") {
                    let before = new_content.clone();
                    new_content = new_content.replace("#tasks", "#task").replace("#Tasks", "#task").replace("#TASKS", "#task");
                    if new_content != before { changed = true; }
                }
                if changed {
                    if let Err(e) = self.memos.update_memo(&memo.name, &new_content).await {
                        warn!("failed to fold tags for {}: {}", memo.name, e);
                    }
                }

                let existing = self.existing_hashtags(&new_content);
                let has_other_existing = existing.iter().any(|t| t != "inbox");
                let has_inbox_existing = existing.contains("inbox");
                if existing.contains("task") && !existing.contains("task-synced") {
                    if let Err(e) = self.create_vikunja_task(&memo).await {
                        warn!("vikunja failed for {}: {}", memo.name, e);
                    }
                }
                if existing.contains("calendar") && !existing.contains("calendar-synced") {
                    if let Err(e) = self.create_radicale_event(&memo).await {
                        warn!("radicale failed for {}: {}", memo.name, e);
                    }
                }
                if has_other_existing && !has_inbox_existing && !existing.contains("tasks") {
                    continue;
                }
                let detected = self.detect_tags(&memo);

                let new_tags: Vec<String> = detected
                    .iter()
                    .filter(|t| !existing.contains(t.as_str()))
                    .cloned()
                    .collect();

                let has_non_inbox_detected = detected.iter().any(|t| t != "inbox");
                let has_non_inbox_new = new_tags.iter().any(|t| t != "inbox");
                let needs_inbox_cleanup = has_inbox_existing && (has_other_existing || has_non_inbox_detected);

                let final_new_tags: Vec<String> = if needs_inbox_cleanup || has_non_inbox_new {
                    new_tags.into_iter().filter(|t| t != "inbox").collect()
                } else {
                    new_tags
                };

                if needs_inbox_cleanup {
                    let before = new_content.clone();
                    new_content = new_content.replace(" #inbox", "").replace("#inbox ", "").replace("#inbox", "");
                    if new_content != before { changed = true; }
                }

                if final_new_tags.is_empty() && !changed {
                    continue;
                }

                if !final_new_tags.is_empty() {
                    let hashtag_line = final_new_tags.iter().map(|t| format!("#{}", t)).collect::<Vec<_>>().join(" ");
                    if new_content.ends_with('\n') {
                        new_content.push_str(&hashtag_line);
                    } else {
                        new_content.push('\n');
                        new_content.push_str(&hashtag_line);
                    }
                }

                while new_content.contains("\n\n\n") {
                    new_content = new_content.replace("\n\n\n", "\n\n");
                }

                if self.memos.update_memo(&memo.name, &new_content).await? {
                    total_tagged += 1;
                    if !final_new_tags.is_empty() {
                        info!(
                            "tagged memo {} with [{}]",
                            memo.name,
                            final_new_tags.join(", ")
                        );
                    } else {
                        info!("cleaned inbox from memo {}", memo.name);
                    }
                }
            }

            page_token = next;
            if page_token.is_none() {
                break;
            }
        }

        if total_tagged > 0 {
            info!("tagged {} memos (scanned {} recent)", total_tagged, total_scanned);
        }
        Ok(())
    }

    async fn vikunja_token(&self) -> Result<String> {
        if let Some(t) = self.vikunja_token_cache.get() {
            return Ok(t.clone());
        }
        let resp = self.client
            .post(format!("{}/api/v1/login", self.vikunja_url))
            .json(&serde_json::json!({"username": self.vikunja_user, "password": self.vikunja_pass}))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        let data: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if let Some(t) = data["token"].as_str() {
            let _ = self.vikunja_token_cache.set(t.to_string());
            return Ok(t.to_string());
        }
        anyhow::bail!("vikunja login failed {}: {}", status, &text[..text.len().min(300)])
    }

    async fn create_vikunja_task(&self, memo: &Memo) -> Result<()> {
        let token = self.vikunja_token().await?;
        let title = memo.content.lines().next().unwrap_or("Untitled").chars().take(100).collect::<String>().trim().to_string();
        let title = if title.is_empty() { "Untitled".to_string() } else { title };
        let payload = serde_json::json!({"title": title, "description": memo.content});
        let resp = self.client
            .put(format!("{}/api/v1/projects/{}/tasks", self.vikunja_url, self.vikunja_project))
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("vikunja create failed: {}", resp.status());
        }
        let new_content = format!("{}\n#task-synced", memo.content.trim_end());
        self.memos.update_memo(&memo.name, &new_content).await?;
        info!("created vikunja task for {}", memo.name);
        Ok(())
    }

    async fn create_radicale_event(&self, memo: &Memo) -> Result<()> {
        let caps = self.re_date.captures(&memo.content).ok_or_else(|| anyhow::anyhow!("no date found"))?;
        let date_str = format!("{} {}", &caps[1], &caps[2]);
        let dt = NaiveDateTime::parse_from_str(&date_str, "%Y-%m-%d %H:%M")?;
        let uid = memo.name.replace('/', "-");
        let dt_utc: DateTime<Utc> = DateTime::from_naive_utc_and_offset(dt, Utc);
        let ics = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//memotag//EN\r\nBEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nSUMMARY:{}\r\nDESCRIPTION:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            uid,
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            dt_utc.format("%Y%m%dT%H%M%SZ"),
            memo.content.lines().next().unwrap_or("Event").replace('\n', "\\n").chars().take(100).collect::<String>(),
            memo.content.replace('\n', "\\n").replace('\r', "")
        );
        let cred = BASE64.encode(format!("{}:{}", self.radicale_user, self.radicale_pass));
        let resp = self.client
            .put(format!("{}/asher/events/{}.ics", self.radicale_url.trim_end_matches('/'), uid))
            .header("Authorization", format!("Basic {}", cred))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ics)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("radicale put failed: {}", resp.status());
        }
        let new_content = format!("{}\n#calendar-synced", memo.content.trim_end());
        self.memos.update_memo(&memo.name, &new_content).await?;
        info!("created radicale event for {}", memo.name);
        Ok(())
    }

    fn is_junk_hashtag(tag: &str) -> bool {
        let t = tag.to_lowercase();
        if t.len() == 2 && t.chars().next().unwrap().is_ascii_alphabetic() && t.chars().nth(1).unwrap().is_ascii_digit() {
            return true;
        }
        if Regex::new(r"^\d+v\d+$").unwrap().is_match(&t) {
            return true;
        }
        if t.len() <= 4 && t.chars().next().unwrap().is_ascii_digit() {
            return true;
        }
        false
    }

    pub async fn clean_junk_hashtags(&self) -> Result<()> {
        info!("clean mode: removing junk hashtags from all memos");
        let mut page_token: Option<String> = None;
        let mut total_cleaned = 0;

        loop {
            let (memos, next) = self.memos.list_memos(page_token.as_deref()).await?;

            for memo in memos {
                if memo.content.trim().is_empty() {
                    continue;
                }

                let mut new_content = memo.content.clone();
                let mut changed = false;

                let mut removals: Vec<(usize, usize)> = Vec::new();
                for cap in self.re_hashtag.captures_iter(&memo.content) {
                    let tag = &cap[2];
                    if Self::is_junk_hashtag(tag) {
                        removals.push((cap.get(0).unwrap().start(), cap.get(0).unwrap().end()));
                    }
                }

                for (start, end) in removals.into_iter().rev() {
                    new_content.drain(start..end);
                    changed = true;
                }

                if changed {
                    let multi_nl = Regex::new(r"\n{3,}").unwrap();
                    new_content = multi_nl.replace_all(&new_content, "\n\n").to_string();

                    if self.memos.update_memo(&memo.name, &new_content).await? {
                        total_cleaned += 1;
                        info!("cleaned memo {}", memo.name);
                    }
                }
            }

            page_token = next;
            if page_token.is_none() {
                break;
            }
        }

        info!("cleaned {} memos", total_cleaned);
        Ok(())
    }
}
