use crate::azure_client::token::refresh_and_cache_token;
use crate::state::{AppState, TOKEN_REFRESH_INTERVAL};
use actix_web::web;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

// Restart Delay:
// If the worker thread crashes (panics), the supervisor waits this long before respawning it.
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
        state.worker_last_heartbeat.store(
            OffsetDateTime::now_utc().unix_timestamp(),
            Ordering::Relaxed,
        );

        // 2. Check Token Expiration
        // We use a block scope so the 'guard' is automatically dropped at the closing brace '}'.
        // This prevents deadlock bugs without needing manual 'drop(guard)'.
        let should_refresh = {
            let guard = state.token_cache.load();
            match guard.as_ref() {
                // The math logic is hidden inside the token struct (Encapsulation)
                Some(token) => token.needs_background_refresh(),
                None => true, // Initial state: No token exists, fetch immediately.
            }
        };

        if should_refresh {
            info!("Token nearing expiration (or missing). Refreshing...");
            
            if let Err(e) =
                refresh_and_cache_token(&*state.credential, &state.token_cache).await
            {
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
            let handle = tokio::spawn(run_worker(state.clone()));
            
            match handle.await {
                Ok(_) => warn!("Worker exited cleanly (Unexpected). Restarting..."),
                Err(e) => {
                    let msg = if e.is_panic() { "Panic" } else { "Error" };
                    error!("Worker crashed ({msg}): {e}. Restarting in 5s...");
                }
            }
            
            // Backoff strategy to prevent infinite fast-loops
            tokio::time::sleep(RESTART_DELAY).await;
        }
    });
}
