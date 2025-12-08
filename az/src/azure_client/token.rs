// [./src/azure_client/token.rs]

use crate::errors::AksError;
use arc_swap::ArcSwap;
use azure_core::credentials::TokenCredential;
use std::sync::Arc;
use time::OffsetDateTime;
use tracing::instrument;

const AZURE_MGMT_SCOPE: &str = "https://management.azure.com/.default";

// --- MOVED DEFINITIONS HERE TO BREAK CYCLE ---

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
}

// Define the type alias here so both State and Worker can use it without cycling
pub type TokenCache = ArcSwap<Option<InternalCachedToken>>;

// --- LOGIC ---

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
        // Simple check: is it expired right now?
        // Detailed lifecycle logic is handled by the worker/state.
        if cached.expires_at > OffsetDateTime::now_utc() {
            return Some(Arc::clone(&cached.token));
        }
    }
    None
}

// Helper for status endpoint
pub struct TokenStatus {
    pub is_valid: bool,
    pub expires_at_utc: Option<OffsetDateTime>,
}

pub fn get_token_status(cache: &TokenCache) -> TokenStatus {
    let cached_arc = cache.load();
    match cached_arc.as_ref() {
        Some(cached) => TokenStatus {
            is_valid: cached.expires_at > OffsetDateTime::now_utc(),
            expires_at_utc: Some(cached.expires_at),
        },
        None => TokenStatus {
            is_valid: false,
            expires_at_utc: None,
        },
    }
}
