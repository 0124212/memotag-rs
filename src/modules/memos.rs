use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub filename: String,
    #[serde(rename = "type", default)]
    pub mime_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Memo {
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(rename = "createTime", default)]
    pub create_time: String,
    #[serde(rename = "updateTime", default)]
    pub update_time: String,
}

#[derive(Debug, Deserialize)]
struct ListMemosResponse {
    memos: Vec<Memo>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoPatch<'a> {
    content: &'a str,
}

pub struct MemosClient {
    client: Client,
    base_url: String,
    api_token: String,
}

impl MemosClient {
    pub fn new(base_url: String, api_token: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
            api_token,
        }
    }

    pub fn extract_memo_id(name: &str) -> String {
        // "memos/memo123" -> "memo123", "memos/memo123/comments/c1" -> "memo123"
        name.trim_start_matches("memos/")
            .split('/')
            .next()
            .unwrap_or(name)
            .to_string()
    }

    pub async fn list_memos(&self, page_token: Option<&str>) -> Result<(Vec<Memo>, Option<String>)> {
        let mut url = format!("{}/api/v1/memos?pageSize=50", self.base_url);
        if let Some(token) = page_token {
            let encoded = token.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D");
            url.push_str(&format!("&pageToken={}", encoded));
        }

        let resp = self.client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .context("listing memos")?;

        let status = resp.status();
        let text = resp.text().await.context("reading memos response")?;

        if !status.is_success() {
            anyhow::bail!("list memos failed {}: {}", status, &text[..text.len().min(300)]);
        }

        let data: ListMemosResponse = serde_json::from_str(&text)
            .with_context(|| format!("parsing memos response: {}", &text[..text.len().min(200)]))?;

        let next = data.next_page_token.filter(|t| !t.is_empty());
        Ok((data.memos, next))
    }

    pub async fn list_all_memos(&self) -> Result<Vec<Memo>> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let (memos, next) = self.list_memos(page_token.as_deref()).await?;
            all.extend(memos);
            page_token = next;
            if page_token.is_none() {
                break;
            }
        }
        Ok(all)
    }

    pub async fn get_memo(&self, name: &str) -> Result<Memo> {
        let url = format!("{}/api/v1/{}", self.base_url, name);
        let resp = self.client
            .get(&url)
            .bearer_auth(&self.api_token)
            .send()
            .await
            .context("getting memo")?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            anyhow::bail!("get memo failed {}: {}", status, &text[..text.len().min(300)]);
        }

        let memo: Memo = serde_json::from_str(&text)?;
        Ok(memo)
    }

    pub async fn update_memo(&self, name: &str, content: &str) -> Result<bool> {
        let url = format!("{}/api/v1/{}?updateMask=content", self.base_url, name);
        let patch = MemoPatch { content };

        let resp = self.client
            .patch(&url)
            .bearer_auth(&self.api_token)
            .json(&patch)
            .send()
            .await
            .context("updating memo")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("update failed for {}: {} - {}", name, status, &body[..body.len().min(200)]);
            return Ok(false);
        }
        Ok(true)
    }
}
