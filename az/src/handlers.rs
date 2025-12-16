use crate::azure_client::retry::fetch_versions_with_retry;
use crate::errors::AksError;
use crate::state::AppState;
use actix_request_identifier::RequestId;
use actix_web::{get, web, HttpResponse, Responder};
use regex::Regex;
use std::ops::Deref;
use std::sync::OnceLock;
use tracing::instrument;

// --- STATIC RESOURCES ---

// Global Regex for validating locations.
// We use OnceLock to compile this exactly once on the first request,
// saving CPU on all subsequent requests.
static LOCATION_REGEX: OnceLock<Regex> = OnceLock::new();

// --- HTTP HANDLERS ---

#[get("/{location}")]
#[instrument(skip(state, req_id), fields(location = %path))]
pub async fn aks_versions(
    path: web::Path<String>,
    state: web::Data<AppState>,
    req_id: web::ReqData<RequestId>,
) -> Result<impl Responder, AksError> {
    let location = path.into_inner();
    let location = location.trim();

    // 1. Basic Validation
    if location.is_empty() {
        return Err(AksError::Validation);
    }

    // 2. "Fail Fast" Regex Check
    // PERFORMANCE OPTIMIZATION:
    // We check the input format locally before making any network calls.
    // This catches typos like "east us" (space) or "east-us!" (special chars) instantly.
    // It prevents "hanging" connections where Azure might ignore the request or timeout.
    let re = LOCATION_REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9]+$").unwrap());

    if !re.is_match(location) {
        // Return 400 Bad Request IMMEDIATELY with a local message.
        return Err(AksError::InvalidLocation {
            location: location.to_string(),
            details: "Location contains invalid characters (alphanumeric only).".to_string(),
        });
    }

    tracing::Span::current().record("request_id", req_id.deref().as_str());

    // 3. Cache-Aside Pattern
    // - Check Moka cache for this location.
    // - If miss: Execute the async block (fetch with retry).
    // - If hit: Return cached data instantly.
    let response_data = state
        .cache
        .try_get_with(state.cache_key(location), async {
            fetch_versions_with_retry(
                &state.http_client,
                &state.subscription_id,
                location,
                &state.token_cache,
                state.show_preview,
            )
            .await
        })
        .await
        .map_err(|e| e.as_ref().clone())?;

    Ok(HttpResponse::Ok().json(&*response_data))
}

#[get("/status")]
pub async fn status(state: web::Data<AppState>) -> impl Responder {
    let report = state.get_health();

    let mut status_code = if report.status == "healthy" {
        HttpResponse::Ok()
    } else {
        HttpResponse::ServiceUnavailable()
    };

    status_code.json(report)
}
