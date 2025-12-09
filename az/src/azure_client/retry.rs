use super::fetch_and_parse;
use crate::azure_client::token::{get_token_from_cache, TokenCache};
use crate::errors::AksError;
use rand::Rng;
use std::sync::Arc;
use std::time::Duration;
use tokio_retry::{strategy::ExponentialBackoff, Retry};
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
        // - Parse errors (Code/API Mismatch)
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
) -> Result<Arc<[String]>, AksError> {
    let mut rng = rand::rng();

    // Exponential backoff with jitter to prevent "thundering herd"
    let strategy = ExponentialBackoff::from_millis(RETRY_BASE_DELAY_MS)
        .take(MAX_RETRY_ATTEMPTS)
        .map(|d| d + Duration::from_millis(rng.random_range(0..RETRY_JITTER_MS)));

    Retry::spawn(strategy, || async {
        // Fetch a fresh token reference for each retry attempt
        let token = get_token_from_cache(token_cache).ok_or_else(|| AksError::AzureClient {
            message: "Token expired during retry cycle.".to_string(),
        })?;

        match fetch_and_parse(client, subscription_id, location, &token, show_preview).await {
            Err(e) if is_retryable_error(&e) => {
                warn!("Retryable error: {}", e);
                Err(e)
            }
            other => other,
        }
    })
    .await
}
