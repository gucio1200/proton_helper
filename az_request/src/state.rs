use crate::azure_client::token::refresh_and_cache_token;
use crate::azure_client::AKS_API_VERSION;
use crate::config::Config;
use crate::errors::AksError;
use actix_web::web;
use arc_swap::ArcSwap;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use moka::future::Cache;
use std::{sync::Arc, time::Duration};
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::interval;
use tracing::{error, info};

// Token management constants
pub const TOKEN_REFRESH_LEEWAY: TimeDuration = TimeDuration::seconds(65);
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(55);
const TOKEN_REFRESH_PROACTIVE_OFFSET: TimeDuration = TimeDuration::seconds(60);

// Token Cache Structures
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

// Application State
pub struct AppState {
    pub show_preview: bool,
    pub cache: Cache<String, Arc<[String]>>,
    pub token_cache: TokenCache,
    pub credential: Arc<WorkloadIdentityCredential>,
    pub http_client: reqwest::Client,
    pub subscription_id: String,
    pub start_time: OffsetDateTime,
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
        format!("{}:{}:{}", self.subscription_id, location, AKS_API_VERSION)
    }
}

// Background Refresher Logic
pub fn start_token_refresher(app_state: web::Data<AppState>) {
    let app_data = app_state.clone();

    tokio::spawn(async move {
        let mut interval = interval(TOKEN_REFRESH_INTERVAL);

        let credential = app_data.credential.as_ref();
        let token_cache = &app_data.token_cache;

        loop {
            interval.tick().await;

            let needs_refresh = {
                let cached_arc = token_cache.load();
                match cached_arc.as_ref() {
                    Some(cached) => cached.needs_refresh(),
                    None => true,
                }
            };

            if needs_refresh {
                info!("Proactively refreshing token...");
                if let Err(e) = refresh_and_cache_token(credential, token_cache).await {
                    error!(error = %e, "Background token refresh failed. Retrying later.");
                }
            }
        }
    });
    info!("Background token refresher spawned.");
}
