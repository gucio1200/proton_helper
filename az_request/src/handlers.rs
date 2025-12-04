use actix_request_identifier::RequestId;
use actix_web::{get, web, HttpResponse, Responder};
use reqwest::Response;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{ops::Deref, sync::Arc};
use tracing::instrument;

use crate::azure_client::retry::fetch_with_retry;
use crate::errors::AksError;
use crate::state::AppState;

// Constants used for filtering
const ORCHESTRATOR_TYPE_K8S: &str = "Kubernetes";

// Data Models
#[derive(Debug, Deserialize)]
struct OrchestratorsResponse {
    properties: OrchestratorsProperties,
}

#[derive(Debug, Deserialize)]
struct OrchestratorsProperties {
    orchestrators: Vec<OrchestratorItem>,
}

#[derive(Debug, Deserialize)]
struct OrchestratorItem {
    #[serde(rename = "orchestratorType")]
    orchestrator_type: String,

    #[serde(rename = "orchestratorVersion")]
    orchestrator_version: String,

    #[serde(rename = "isPreview", default)]
    is_preview: bool,
}

#[derive(Serialize)]
pub struct VersionsResponse {
    pub versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocationQuery {
    pub location: String,
}

// Core Logic
async fn process_orchestrator_response(
    resp: Response,
    show_preview: bool,
) -> Result<Arc<[String]>, AksError> {
    let json: OrchestratorsResponse = resp
        .json()
        .await
        .map_err(|e| AksError::Parse(format!("JSON parsing failed: {e}")))?;

    let mut semver_versions: Vec<Version> = json
        .properties
        .orchestrators
        .into_iter()
        .filter(|o| o.orchestrator_type == ORCHESTRATOR_TYPE_K8S && (show_preview || !o.is_preview))
        .map(|o| {
            Version::parse(&o.orchestrator_version).map_err(|e| {
                AksError::Parse(format!(
                    "Failed to parse version '{}': {}",
                    o.orchestrator_version, e
                ))
            })
        })
        .collect::<Result<Vec<_>, AksError>>()?;

    semver_versions.sort_unstable();

    let final_versions: Vec<String> = semver_versions.into_iter().map(|v| v.to_string()).collect();

    Ok(final_versions.into())
}

// HTTP HANDLERS
#[get("/")]
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

    tracing::Span::current().record("request_id", req_id.deref().as_str());

    let cache_key = state.cache_key(location);

    let versions = state
        .cache
        .try_get_with(cache_key, async {
            let resp = fetch_with_retry(
                &state.http_client,
                &state.subscription_id,
                location,
                &state.token_cache,
            )
            .await?;

            process_orchestrator_response(resp, state.show_preview).await
        })
        .await
        .map_err(|e| e.as_ref().clone())?;

    Ok(HttpResponse::Ok().json(VersionsResponse {
        versions: versions.to_vec(),
    }))
}

#[get("/healthz")]
pub async fn healthz(state: web::Data<AppState>) -> impl Responder {
    use crate::azure_client::token::get_token_from_cache;
    use time::OffsetDateTime;

    let uptime = OffsetDateTime::now_utc() - state.start_time;

    if get_token_from_cache(&state.token_cache).is_some() {
        HttpResponse::Ok().json(serde_json::json!({
            "status": "healthy",
            "uptime_seconds": uptime.whole_seconds(),
        }))
    } else {
        HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "status": "unhealthy",
            "message": "Azure access token is missing or expired.",
            "uptime_seconds": uptime.whole_seconds(),
        }))
    }
}

#[get("/readyz")]
pub async fn readyz() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ready"
    }))
}
