use super::fetch_aks_versions;
use super::token::get_token_from_cache;
use crate::errors::AksError;
use crate::state::TokenCache;
use rand::Rng;
use reqwest::Response;
use std::time::Duration;
use tokio_retry::{strategy::ExponentialBackoff, Retry};
use tracing::warn;

// Retry configuration
const RETRY_BASE_DELAY_MS: u64 = 50;
const RETRY_JITTER_MS: u64 = 30;
const MAX_RETRY_ATTEMPTS: usize = 5;

#[inline]
fn is_retryable_error(err: &AksError) -> bool {
    match err {
        // Retry for transient HTTP errors (5xx) and throttling (429)
        AksError::AzureHttp { status, .. } => *status == 429 || (*status >= 500 && *status <= 599),
        // Retry for client-side/network issues (check for timeout message)
        AksError::AzureClient { message } => message.contains("timeout"),
        _ => false,
    }
}

pub async fn fetch_with_retry(
    client: &reqwest::Client,
    subscription_id: &str,
    location: &str,
    token_cache: &TokenCache,
) -> Result<Response, AksError> {
    let mut rng = rand::rng();
    let strategy = ExponentialBackoff::from_millis(RETRY_BASE_DELAY_MS)
        .take(MAX_RETRY_ATTEMPTS)
        .map(|d| d + Duration::from_millis(rng.random_range(0..RETRY_JITTER_MS)));

    Retry::spawn(strategy, || async {
        let token = get_token_from_cache(token_cache).ok_or_else(|| AksError::AzureClient {
            message: "Token expired during retry cycle.".to_string(),
        })?;

        match fetch_aks_versions(client, subscription_id, location, &*token).await {
            Err(e) if is_retryable_error(&e) => {
                warn!(error = %e, "Retryable error occurred");
                Err(e)
            }
            other => other,
        }
    })
    .await
}
