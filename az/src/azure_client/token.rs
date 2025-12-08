use crate::errors::AksError;
use crate::state::InternalCachedToken;
use crate::state::TokenCache;
use azure_core::credentials::TokenCredential;
use std::sync::Arc;
use time::OffsetDateTime;
use tracing::instrument;

const AZURE_MGMT_SCOPE: &str = "https://management.azure.com/.default";

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
        if cached.is_valid_for_use() {
            return Some(Arc::clone(&cached.token));
        }
    }
    None
}

pub struct TokenStatus {
    pub is_valid: bool,
    pub expires_at_utc: Option<OffsetDateTime>,
}

/// Retrieves the full status of the token from the cache.
pub fn get_token_status(cache: &TokenCache) -> TokenStatus {
    let cached_arc = cache.load();
    match cached_arc.as_ref() {
        Some(cached) => TokenStatus {
            is_valid: cached.is_valid_for_use(),
            expires_at_utc: Some(cached.expires_at),
        },
        None => TokenStatus {
            is_valid: false,
            expires_at_utc: None,
        },
    }
}
