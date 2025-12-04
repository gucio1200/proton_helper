use crate::errors::AksError;
use crate::state::InternalCachedToken;
use crate::state::TokenCache;
use azure_core::credentials::TokenCredential;
use std::sync::Arc;
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
