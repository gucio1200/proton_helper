use crate::azure_client::token::refresh_and_cache_token;
use crate::azure_client::AKS_API_VERSION;
use crate::config::Config;
use crate::errors::AksError;
use actix_web::web;
use arc_swap::ArcSwap;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use moka::future::Cache;
use std::sync::atomic::{AtomicI64, Ordering};
use std::{sync::Arc, time::Duration};
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

// Token Validity Rules
// The HTTP handler will reject the token if it expires in less than 65 seconds.
// This prevents the token from expiring mid-flight during a request to Azure.
pub const TOKEN_REFRESH_LEEWAY: TimeDuration = TimeDuration::seconds(65);

// 2. WORKER INTERVAL
// The background worker wakes up every 55 seconds to check the token.
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(55);

// 3. REFRESH TRIGGER
// We must attempt to refresh if the token has less than 130 seconds left.
// Calculation: 65s (Leeway) + 55s (Interval) + 10s (Safety Buffer) = 130s.
// This guarantees we refresh BEFORE the Leeway threshold is reached.
const TOKEN_REFRESH_PROACTIVE_OFFSET: TimeDuration = TimeDuration::seconds(130);

// Worker Liveness Threshold for the Status Endpoint
// If the worker hasn't updated the heartbeat in ~2.5 intervals, consider it dead.
pub const WORKER_LIVENESS_THRESHOLD: i64 = 140;
const SUPERVISOR_RESTART_DELAY: Duration = Duration::from_secs(2);

// --- Token Cache Structures ---

pub struct InternalCachedToken {
    pub token: Arc<str>,
    pub expires_at: OffsetDateTime,
}

impl InternalCachedToken {
    pub fn new(token: String, expires_at: OffsetDateTime) -> Self {
        Self {
            token: token.into(),
            expires_at,
        }
    }

    pub fn is_valid_for_use(&self) -> bool {
        self.expires_at > OffsetDateTime::now_utc() + TOKEN_REFRESH_LEEWAY
    }

    pub fn needs_refresh(&self) -> bool {
        self.expires_at < OffsetDateTime::now_utc() + TOKEN_REFRESH_PROACTIVE_OFFSET
    }
}

pub type TokenCache = ArcSwap<Option<InternalCachedToken>>;

// --- Application State ---

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

impl AppState {
    pub fn new(config: Config) -> Result<Self, AksError> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .build()
            .map_err(|e| AksError::ClientBuild(e.to_string()))?;

        let credential = Self::create_credential()?;

        Ok(Self {
            show_preview: config.show_preview,
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
                .max_capacity(100)
                .build(),
            token_cache: ArcSwap::new(Arc::new(None)),
            credential,
            http_client,
            subscription_id: config.subscription_id,
            start_time: OffsetDateTime::now_utc(),
            worker_last_heartbeat: AtomicI64::new(OffsetDateTime::now_utc().unix_timestamp()),
        })
    }

    fn create_credential() -> Result<Arc<WorkloadIdentityCredential>, AksError> {
        let cred =
            WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions::default()))
                .map_err(|e| AksError::AzureClient {
                    message: format!("Failed to create credential: {e}"),
                })?;
        Ok(cred)
    }

    pub fn cache_key(&self, location: &str) -> String {
        format!(
            "{}:{}:{}:{}",
            self.subscription_id, location, AKS_API_VERSION, self.show_preview
        )
    }
}

// --- Supervisor & Worker Implementation ---

#[instrument(skip(app_state), fields(component = "token_worker"))]
async fn run_refresher_worker(app_state: Arc<AppState>) {
    let mut interval = interval(TOKEN_REFRESH_INTERVAL);

    let credential = app_state.credential.clone();
    let token_cache = &app_state.token_cache;

    info!("Token refresher worker started.");

    loop {
        interval.tick().await;
        app_state.worker_last_heartbeat.store(
            OffsetDateTime::now_utc().unix_timestamp(),
            Ordering::Relaxed,
        );

        let needs_refresh = {
            let cached_arc = token_cache.load();
            match cached_arc.as_ref() {
                Some(cached) => cached.needs_refresh(),
                None => true,
            }
        };

        if needs_refresh {
            info!("Proactively refreshing token...");
            if let Err(e) = refresh_and_cache_token(&*credential, token_cache).await {
                error!(error = %e, "Background token refresh failed. Retrying in next cycle.");
            }
        }
    }
}

pub fn start_token_refresher(app_state: web::Data<AppState>) {
    let state_arc = app_state.into_inner();

    tokio::spawn(async move {
        info!("Token Supervisor started.");

        loop {
            let worker_state = state_arc.clone();
            let handle = tokio::spawn(run_refresher_worker(worker_state));

            match handle.await {
                Ok(_) => {
                    warn!("Token refresher worker exited unexpectedly (Clean Exit). Restarting...");
                }
                Err(e) => {
                    if e.is_panic() {
                        error!(
                            "CRITICAL: Token refresher worker PANICKED. Restarting supervisor..."
                        );
                    } else {
                        error!(error = %e, "Token refresher worker task failed/cancelled. Restarting...");
                    }
                }
            }
            tokio::time::sleep(SUPERVISOR_RESTART_DELAY).await;
        }
    });
}
