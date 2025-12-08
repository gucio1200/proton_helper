use crate::azure_client::token::refresh_and_cache_token;
use crate::state::AppState;
use actix_web::web;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::interval;
use tracing::{error, info, instrument, warn};

// --- REFRESH LOGIC CONFIGURATION ---

// Interval: How often the worker wakes up to check the token.
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(55);

// Trigger: When do we decide to refresh?
//
// SAFETY INVARIANT: Trigger > Leeway + Interval
// Proof: 130s > 65s (Handler Leeway) + 55s (Sleep Interval)
//
// Explanation:
// If the worker wakes up and sees 131s remaining, it sleeps for 55s.
// When it wakes up again, there are 76s remaining.
// 76s is still > 65s (Leeway), so the token is STILL valid for HTTP requests.
// We refresh immediately at 76s, ensuring zero downtime.
const REFRESH_TRIGGER_OFFSET: TimeDuration = TimeDuration::seconds(130);

const RESTART_DELAY: Duration = Duration::from_secs(2);

#[instrument(skip(state), fields(component = "worker"))]
async fn run_worker(state: Arc<AppState>) {
    let mut ticker = interval(TOKEN_REFRESH_INTERVAL);
    info!("Worker started.");

    loop {
        ticker.tick().await;

        // 1. Heartbeat Pattern
        // Update the timestamp to prove to the /status endpoint that this thread is alive.
        state.worker_last_heartbeat.store(
            OffsetDateTime::now_utc().unix_timestamp(),
            Ordering::Relaxed,
        );

        // 2. Check Token Expiration
        let guard = state.token_cache.load();
        let should_refresh = match guard.as_ref() {
            Some(token) => token.expires_at < OffsetDateTime::now_utc() + REFRESH_TRIGGER_OFFSET,
            None => true, // Initial state: No token exists, fetch immediately.
        };

        if should_refresh {
            info!("Token nearing expiration. Refreshing...");

            // Drop guard before awaiting to avoid holding the lock
            drop(guard);

            if let Err(e) =
                refresh_and_cache_token(&*state.credential, &state.token_cache).await
            {
                error!("Refresh failed: {e}. Will retry in next interval.");
            }
        }
    }
}

pub fn start(state: web::Data<AppState>) {
    let state = state.into_inner();
    tokio::spawn(async move {
        info!("Supervisor started.");
        loop {
            let handle = tokio::spawn(run_worker(state.clone()));

            match handle.await {
                Ok(_) => warn!("Worker exited cleanly (Unexpected). Restarting..."),
                Err(e) => {
                    if e.is_panic() {
                        error!("CRITICAL: Worker Panicked! Restarting...");
                    } else {
                        error!("Worker failed: {e}");
                    }
                }
            }
            tokio::time::sleep(RESTART_DELAY).await;
        }
    });
}
