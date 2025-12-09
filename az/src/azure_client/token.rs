use crate::errors::AksError;
use arc_swap::ArcSwap;
use azure_core::credentials::TokenCredential;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tracing::instrument;

const AZURE_MGMT_SCOPE: &str = "https://management.azure.com/.default";

// --- CRITICAL SAFETY CONSTANTS ---

// 1. HTTP Handler Safety (65s):
//    We consider a token "dead" if it expires in less than 65s.
//    This prevents the token from expiring mid-flight during a request to Azure.
pub const TOKEN_REFRESH_LEEWAY: Duration = Duration::seconds(65);

// 2. Refresh Trigger (130s):
//    The background worker triggers a refresh if the token has less than 130s remaining.
//    Proof: 130s > 65s (Leeway) + 55s (Worker Interval).
//    This guarantees zero downtime even if the worker sleeps right before the threshold.
pub const REFRESH_TRIGGER_OFFSET: Duration = Duration::seconds(130);

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

    /// Checks if the token is "old" enough to warrant a background refresh.
    /// Encapsulates the math: expires_at < now + 130s
    pub fn needs_background_refresh(&self) -> bool {
        self.expires_at < OffsetDateTime::now_utc() + REFRESH_TRIGGER_OFFSET
    }

    /// Checks if the token is valid for immediate HTTP usage.
    /// Encapsulates the math: expires_at > now + 65s
    pub fn is_valid_for_http(&self) -> bool {
        self.expires_at > OffsetDateTime::now_utc() + TOKEN_REFRESH_LEEWAY
    }
}

pub type TokenCache = ArcSwap<Option<InternalCachedToken>>;

// --- Logic ---

#[instrument(skip(credential, cache))]
pub async fn refresh_and_cache_token(
    credential: &impl TokenCredential,
    cache: &TokenCache,
) -> Result<(), AksError> {
    let new_token = credential
        .get_token(&[AZURE_MGMT_SCOPE], None)
        .await
        .map_err(|e| AksError::AzureClient {
            message: format!("Token acquisition failed: {e}"),
        })?;

    let cached =
        InternalCachedToken::new(new_token.token.secret().to_string(), new_token.expires_on);

    cache.store(Arc::new(Some(cached)));

    Ok(())
}

pub fn get_token_from_cache(cache: &TokenCache) -> Option<Arc<str>> {
    let cached_arc = cache.load();
    if let Some(cached) = cached_arc.as_ref() {
        if cached.is_valid_for_http() {
            return Some(Arc::clone(&cached.token));
        }
    }
    None
}

pub struct TokenStatus {
    pub is_valid: bool,
    pub expires_at_utc: Option<OffsetDateTime>,
}

pub fn get_token_status(cache: &TokenCache) -> TokenStatus {
    let cached_arc = cache.load();
    match cached_arc.as_ref() {
        Some(cached) => TokenStatus {
            is_valid: cached.is_valid_for_http(),
            expires_at_utc: Some(cached.expires_at),
        },
        None => TokenStatus {
            is_valid: false,
            expires_at_utc: None,
        },
    }
}
