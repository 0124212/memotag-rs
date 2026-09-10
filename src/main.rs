use memotag_rs::modules;

use anyhow::Result;
use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use modules::config::Config;
use modules::db::Database;
use modules::sync::SyncService;

struct AppState {
    sync_tx: mpsc::Sender<()>,
}

#[derive(Deserialize)]
struct WebhookPayload {
    #[serde(default)]
    event: String,
    #[serde(default)]
    memo_name: Option<String>,
}

async fn health() -> &'static str {
    "ok"
}

async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<WebhookPayload>,
) -> StatusCode {
    info!("webhook received: event={}, memo={:?}", payload.event, payload.memo_name);

    if payload.event == "UPDATE" || payload.event == "CREATE" {
        let _ = state.sync_tx.try_send(());
    }

    StatusCode::OK
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("memotag_rs=info,tower_http=info"))
        )
        .init();

    let config = Config::load(None)?;
    info!("memotag-rs starting: memos_url={}", config.memos_url);

    let clean_mode = std::env::args().any(|a| a == "--clean");
    let sync_only = std::env::args().any(|a| a == "--sync");
    let dry_run = std::env::args().any(|a| a == "--dry-run");
    // --dry-run implies a single pass: log what would change, write nothing.
    let once_mode = std::env::args().any(|a| a == "--once") || dry_run;

    if clean_mode {
        let memos = modules::memos::MemosClient::new(config.memos_url.clone(), config.memos_token.clone());
        let autotagger = modules::autotag::Autotagger::new(
            memos,
            config.autotag_default_tag.clone(),
            config.autotag_interval,
            config.autotag_concurrency,
        );
        return autotagger.clean_junk_hashtags(dry_run).await;
    }

    // Test/sandbox escape hatch: single autotag pass against MEMOS_URL, then
    // exit. Point MEMOS_URL at the debian dummy (127.0.0.1:5230) first.
    if once_mode {
        let memos = modules::memos::MemosClient::new(config.memos_url.clone(), config.memos_token.clone());
        let autotagger = modules::autotag::Autotagger::new(
            memos,
            config.autotag_default_tag.clone(),
            config.autotag_interval,
            config.autotag_concurrency,
        );
        let (scanned, changed) = autotagger.run_once(dry_run).await?;
        info!("once mode done: scanned={} changed={} dry_run={}", scanned, changed, dry_run);
        return Ok(());
    }

    let db = Database::open(&config.db_path)?;
    let sync = SyncService::new(&config, db)?;

    if let Err(e) = sync.ensure_cal().await {
        warn!("CalDAV collection setup failed: {}", e);
    }

    if sync_only {
        info!("running sync-only mode");
        sync.full_sync().await?;
        return Ok(());
    }

    // Full sync on startup
    if config.caldav.is_configured() {
        if let Err(e) = sync.full_sync().await {
            warn!("full sync failed: {}", e);
        }
    }

    // Channel for webhook → sync service communication
    let (sync_tx, mut sync_rx) = mpsc::channel::<()>(16);

    let state = Arc::new(AppState { sync_tx });

    // HTTP server for webhook + health
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/webhook", post(webhook_handler))
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::permissive());

    let addr = format!("0.0.0.0:{}", config.listen_port);
    info!("webhook server listening on {}", addr);

    let listener = TcpListener::bind(&addr).await?;

    // Spawn autotagger
    let autotag_config = config.clone();
    tokio::spawn(async move {
        let memos = modules::memos::MemosClient::new(
            autotag_config.memos_url.clone(), autotag_config.memos_token.clone(),
        );
        let autotagger = modules::autotag::Autotagger::new(
            memos,
            autotag_config.autotag_default_tag.clone(),
            autotag_config.autotag_interval,
            autotag_config.autotag_concurrency,
        );
        if let Err(e) = autotagger.run().await {
            warn!("autotagger exited: {}", e);
        }
    });

    // Spawn sync service (listens for webhook triggers + periodic polling)
    let sync_config = config.clone();
    tokio::spawn(async move {
        let db = Database::open(&sync_config.db_path).expect("failed to open db for sync");
        let sync = SyncService::new(&sync_config, db).expect("failed to create sync service");
        let poll_interval = std::time::Duration::from_secs(sync_config.caldav.poll_secs);

        loop {
            tokio::select! {
                _ = sync_rx.recv() => {
                    info!("webhook triggered sync");
                    if let Err(e) = sync.poll_caldav().await {
                        warn!("webhook sync error: {}", e);
                    }
                }
                _ = tokio::time::sleep(poll_interval) => {
                    if sync_config.caldav.is_configured() {
                        if let Err(e) = sync.poll_caldav().await {
                            warn!("calDAV poll error: {}", e);
                        }
                    }
                }
            }
        }
    });

    axum::serve(listener, app).await?;

    Ok(())
}
