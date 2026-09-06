use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_memos_url")]
    pub memos_url: String,
    #[serde(default)]
    pub memos_token: String,

    #[serde(default = "default_autotag_interval")]
    pub autotag_interval: u64,
    #[serde(default = "default_autotag_tag")]
    pub autotag_default_tag: String,

    #[serde(default)]
    pub caldav: CalDavConfig,

    #[serde(default = "default_db_path")]
    pub db_path: String,

    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CalDavConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_caldav_url")]
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_caldav_path")]
    pub path: String,
    #[serde(default = "default_caldav_poll_secs")]
    pub poll_secs: u64,
}

fn default_memos_url() -> String { "https://memos.junilab.xyz".into() }
fn default_autotag_interval() -> u64 { 60 }
fn default_autotag_tag() -> String { "inbox".into() }
fn default_db_path() -> String { "memotag.db".into() }
fn default_listen_port() -> u16 { 8887 }
fn default_caldav_url() -> String { "http://radicale:5232".into() }
fn default_caldav_path() -> String { "/asher/tasks/".into() }
fn default_caldav_poll_secs() -> u64 { 60 }

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self> {
        let mut cfg: Config = if let Some(p) = path {
            let content = std::fs::read_to_string(p)
                .with_context(|| format!("reading config from {}", p))?;
            serde_yaml::from_str(&content)
                .with_context(|| format!("parsing config from {}", p))?
        } else {
            Self::from_env()
        };

        if let Ok(v) = std::env::var("MEMOS_URL") { cfg.memos_url = v; }
        if let Ok(v) = std::env::var("MEMOS_API_TOKEN") { cfg.memos_token = v; }
        if let Ok(v) = std::env::var("AUTOTAG_INTERVAL") { cfg.autotag_interval = v.parse().unwrap_or(cfg.autotag_interval); }
        if let Ok(v) = std::env::var("AUTOTAG_DEFAULT_TAG") { cfg.autotag_default_tag = v; }
        if let Ok(v) = std::env::var("DB_PATH") { cfg.db_path = v; }
        if let Ok(v) = std::env::var("LISTEN_PORT") { cfg.listen_port = v.parse().unwrap_or(cfg.listen_port); }
        if let Ok(v) = std::env::var("CALDAV_ENABLED") { cfg.caldav.enabled = v == "true" || v == "1"; }
        if let Ok(v) = std::env::var("CALDAV_URL") { cfg.caldav.url = v; }
        if let Ok(v) = std::env::var("CALDAV_USERNAME") { cfg.caldav.username = v; }
        if let Ok(v) = std::env::var("CALDAV_PASSWORD") { cfg.caldav.password = v; }
        if let Ok(v) = std::env::var("CALDAV_PATH") { cfg.caldav.path = v; }
        if let Ok(v) = std::env::var("CALDAV_POLL_SECS") { cfg.caldav.poll_secs = v.parse().unwrap_or(cfg.caldav.poll_secs); }

        Ok(cfg)
    }

    fn from_env() -> Self {
        Self {
            memos_url: std::env::var("MEMOS_URL").unwrap_or_else(|_| default_memos_url()),
            memos_token: std::env::var("MEMOS_API_TOKEN").unwrap_or_default(),
            autotag_interval: std::env::var("AUTOTAG_INTERVAL")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(default_autotag_interval()),
            autotag_default_tag: std::env::var("AUTOTAG_DEFAULT_TAG").unwrap_or_else(|_| default_autotag_tag()),
            caldav: CalDavConfig {
                enabled: std::env::var("CALDAV_ENABLED").map(|v| v == "true" || v == "1").unwrap_or(false),
                url: std::env::var("CALDAV_URL").unwrap_or_else(|_| default_caldav_url()),
                username: std::env::var("CALDAV_USERNAME").unwrap_or_default(),
                password: std::env::var("CALDAV_PASSWORD").unwrap_or_default(),
                path: std::env::var("CALDAV_PATH").unwrap_or_else(|_| default_caldav_path()),
                poll_secs: std::env::var("CALDAV_POLL_SECS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(default_caldav_poll_secs()),
            },
            db_path: std::env::var("DB_PATH").unwrap_or_else(|_| default_db_path()),
            listen_port: std::env::var("LISTEN_PORT")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(default_listen_port()),
        }
    }
}

impl CalDavConfig {
    pub fn is_configured(&self) -> bool {
        self.enabled && !self.url.is_empty() && !self.username.is_empty()
    }
}
