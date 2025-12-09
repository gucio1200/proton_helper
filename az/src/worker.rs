use crate::azure_client::token::refresh_and_cache_token;
use crate::state::{AppState, REFRESH_TRIGGER_OFFSET, TOKEN_REFRESH_INTERVAL};
use actix_web::web;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

// --- CONFIGURATION ---

// Restart Delay:
// If the worker thread crashes (panics), the supervisor waits this long before respawning it.
// Increased to 5s to prevent CPU thrashing if the worker enters a tight crash loop.
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// The Worker Task:
/// Runs continuously in the background to monitor and refresh the Azure Access Token.
/// It uses a "Supervisor Pattern": if this function panics, the `start` function catches it and restarts it.
#[instrument(skip(state), fields(component = "worker"))]
async fn run_worker(state: Arc<AppState>) {
    let mut ticker = interval(TOKEN_REFRESH_INTERVAL);
    info!("Worker started.");

    loop {
        ticker.tick().await;

        // 1. Heartbeat Pattern
        // Update the atomic timestamp to prove to the /status endpoint that this thread is alive.
        // If this isn't updated for ~140s, the service marks itself "Unhealthy".
        state.worker_last_heartbeat.store(
            OffsetDateTime::now_utc().unix_timestamp(),
            Ordering::Relaxed,
        );

        // 2. Check Token Expiration
        // We check if the current token has less life remaining than our safety threshold (130s).
        //
        // LOGIC EXPLANATION:
        // - REFRESH_TRIGGER_OFFSET is 130s.
        // - TOKEN_REFRESH_LEEWAY (in handlers) is 65s.
        // - The gap (130s - 65s = 65s) is larger than our sleep interval (55s).
        // This guarantees we will always attempt a refresh at least once before the token becomes invalid for HTTP requests.
        let guard = state.token_cache.load();
        let should_refresh = match guard.as_ref() {
            Some(token) => token.expires_at < OffsetDateTime::now_utc() + REFRESH_TRIGGER_OFFSET,
            None => true, // Initial state or cache cleared: No token exists, fetch immediately.
        };

        if should_refresh {
            info!("Token nearing expiration (or missing). Refreshing...");

            // Drop the guard before awaiting the network call to avoid holding the lock
            drop(guard);

            if let Err(e) = refresh_and_cache_token(&*state.credential, &state.token_cache).await {
                error!("Refresh failed: {e}. Will retry in next interval (55s).");
            }
        }
    }
}

/// The Supervisor:
/// Spawns the worker task and monitors it.
/// If the worker exits (due to panic or error), this supervisor logs the failure and respawns it.
pub fn start(state: web::Data<AppState>) {
    let state = state.into_inner();
    tokio::spawn(async move {
        info!("Supervisor started.");
        loop {
            // Spawn the worker and capture its JoinHandle
            let handle = tokio::spawn(run_worker(state.clone()));

            // Await the handle to detect when the worker stops
            match handle.await {
                Ok(_) => warn!("Worker exited cleanly (Unexpected). Restarting..."),
                Err(e) => {
                    if e.is_panic() {
                        error!("CRITICAL: Worker Panicked! Restarting in 5s...");
                    } else {
                        error!("Worker failed: {e}. Restarting in 5s...");
                    }
                }
            }

            // Backoff strategy to prevent infinite fast-loops
            tokio::time::sleep(RESTART_DELAY).await;
        }
    });
}
