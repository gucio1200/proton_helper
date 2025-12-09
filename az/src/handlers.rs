use crate::azure_client::retry::fetch_versions_with_retry;
use crate::errors::AksError;
use crate::state::AppState;
use actix_request_identifier::RequestId;
use actix_web::{get, web, HttpResponse, Responder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::ops::Deref;
use std::sync::OnceLock;
use tracing::instrument;

static LOCATION_REGEX: OnceLock<Regex> = OnceLock::new();

#[derive(Deserialize, Debug)]
pub struct LocationQuery {
    pub location: String,
}

#[derive(Serialize)]
pub struct VersionsResponse {
    pub versions: Vec<String>,
}

#[get("/versions")]
#[instrument(skip(state, req_id), fields(location = %query.location))]
pub async fn aks_versions(
    query: web::Query<LocationQuery>,
    state: web::Data<AppState>,
    req_id: web::ReqData<RequestId>,
) -> Result<impl Responder, AksError> {
    let location = query.location.trim();
    if location.is_empty() {
        return Err(AksError::Validation);
    }

    // 2. "Fail Fast" Regex Check
    // This catches "east us" (space), "east-us!" (special chars), etc.
    // Azure locations are generally alphanumeric.
    let re = LOCATION_REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9]+$").unwrap());

    if !re.is_match(location) {
        // Return error IMMEDIATELY. Do not call Azure. Do not wait.
        return Err(AksError::InvalidLocation(location.to_string()));
    }

    tracing::Span::current().record("request_id", req_id.deref().as_str());

    let versions = state
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

    Ok(HttpResponse::Ok().json(VersionsResponse {
        versions: versions.to_vec(),
    }))
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
