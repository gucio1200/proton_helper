use crate::azure_client::token::{get_token_status, TokenCache, REFRESH_TRIGGER_OFFSET};
use crate::config::Config;
use crate::errors::AksError;
use arc_swap::ArcSwap;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use moka::future::Cache;
use serde::Serialize;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

// --- CRITICAL SAFETY CONSTANTS ---

// 1. Worker Interval (55s):
//    How often the background worker wakes up to check the token.
pub const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(55);

// 2. Worker Liveness (140s):
//    Used by the /status endpoint.
//    If the background worker hasn't updated the heartbeat in ~2.5 intervals (2.5 * 55s),
//    we assume the thread has crashed/stalled and mark the service unhealthy.
pub const WORKER_LIVENESS_THRESHOLD: i64 = 140;

pub struct AppState {
    pub show_preview: bool,
    pub cache: Cache<String, Arc<[String]>>,
    pub token_cache: TokenCache,
    pub credential: Arc<WorkloadIdentityCredential>,
    pub http_client: reqwest::Client,
    pub subscription_id: String,
    pub start_time: OffsetDateTime,
    pub worker_last_heartbeat: AtomicI64,
}

#[derive(Serialize)]
pub struct HealthReport {
    pub status: &'static str,
    pub checks: Checks,
    pub uptime_seconds: i64,
    pub heartbeat_age: i64,
    pub token_expires_at: Option<String>,
    pub next_token_refresh_at: Option<String>,
}

#[derive(Serialize)]
pub struct Checks {
    pub token_valid: bool,
    pub worker_alive: bool,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, AksError> {
        // Enforce hard timeout of 10s to prevent hanging requests.
        // This is CRITICAL. Without this, requests to bad locations might hang forever.
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AksError::ClientBuild(e.to_string()))?;

        let credential_arc =
            WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions::default()))
                .map_err(|e| AksError::AzureClient {
                    message: e.to_string(),
                })?;

        Ok(Self {
            show_preview: config.show_preview,
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
                .build(),
            token_cache: ArcSwap::new(Arc::new(None)),
            credential: credential_arc,
            http_client,
            subscription_id: config.subscription_id,
            start_time: OffsetDateTime::now_utc(),
            worker_last_heartbeat: AtomicI64::new(OffsetDateTime::now_utc().unix_timestamp()),
        })
    }

    pub fn cache_key(&self, location: &str) -> String {
        format!(
            "{}:{}:{}",
            self.subscription_id, location, self.show_preview
        )
    }

    pub fn get_health(&self) -> HealthReport {
        let now = OffsetDateTime::now_utc();
        let last_beat = self.worker_last_heartbeat.load(Ordering::Relaxed);
        let heartbeat_age = now.unix_timestamp() - last_beat;

        let token_status = get_token_status(&self.token_cache);
        let token_valid = token_status.is_valid;
        let worker_alive = heartbeat_age < WORKER_LIVENESS_THRESHOLD;
        let is_healthy = token_valid && worker_alive;

        // Calculate when the next refresh is strictly scheduled to happen
        let refresh_at = token_status
            .expires_at_utc
            .map(|t| t - REFRESH_TRIGGER_OFFSET);

        HealthReport {
            status: if is_healthy { "healthy" } else { "unhealthy" },
            checks: Checks {
                token_valid,
                worker_alive,
            },
            uptime_seconds: (now - self.start_time).whole_seconds(),
            heartbeat_age,
            token_expires_at: token_status.expires_at_utc.map(|t| t.to_string()),
            next_token_refresh_at: refresh_at.map(|t| t.to_string()),
        }
    }
}
