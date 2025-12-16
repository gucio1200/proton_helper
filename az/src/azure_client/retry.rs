use super::fetch_and_parse;
use crate::azure_client::token::{get_token_from_cache, TokenCache};
use crate::azure_client::RenovateResponse;
use crate::errors::AksError;
use rand::Rng;
use std::sync::Arc;
use std::time::Duration;
use tokio_retry::{strategy::ExponentialBackoff, RetryIf};
use tracing::warn;

// --- RETRY CONFIGURATION ---
const RETRY_BASE_DELAY_MS: u64 = 50;
const RETRY_JITTER_MS: u64 = 30;
const MAX_RETRY_ATTEMPTS: usize = 5;

// Decides which errors are worth retrying.
fn is_retryable_error(err: &AksError) -> bool {
    match err {
        // RETRY: 429 (Throttling) and 5xx (Server Errors)
        // These are temporary issues on Azure's side.
        AksError::AzureHttp { status, .. } => *status == 429 || (*status >= 500 && *status <= 599),

        // RETRY: Client timeouts/Network blips
        AksError::AzureClient { message } => message.contains("timeout"),

        // DO NOT RETRY:
        // - InvalidLocation (User Input Error)
        // - Validation (User Input Error)
        // - Parse errors
        // - 404 Not Found
        _ => false,
    }
}

pub async fn fetch_versions_with_retry(
    client: &reqwest::Client,
    subscription_id: &str,
    location: &str,
    token_cache: &TokenCache,
    show_preview: bool,
) -> Result<Arc<RenovateResponse>, AksError> {
    let mut rng = rand::rng();

    // Exponential backoff with jitter
    let strategy = ExponentialBackoff::from_millis(RETRY_BASE_DELAY_MS)
        .take(MAX_RETRY_ATTEMPTS)
        .map(|d| d + Duration::from_millis(rng.random_range(0..RETRY_JITTER_MS)));

    RetryIf::spawn(
        strategy,
        || async {
            // 1. Get Token
            let token = get_token_from_cache(token_cache).ok_or_else(|| AksError::AzureClient {
                message: "Token expired during retry cycle.".to_string(),
            })?;

            // 2. Fetch
            let result =
                fetch_and_parse(client, subscription_id, location, &token, show_preview).await;

            // 3. Log warning only if we are ABOUT to retry
            if let Err(e) = &result {
                if is_retryable_error(e) {
                    warn!("Retryable error encountered: {}", e);
                }
            }

            result
        },
        is_retryable_error,
    )
    .await
}
