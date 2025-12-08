// [./src/handlers.rs]

use crate::azure_client::retry::fetch_versions_with_retry;
use crate::errors::AksError;
use crate::state::AppState;
use actix_request_identifier::RequestId;
use actix_web::{get, web, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use std::ops::Deref;
use tracing::instrument;

// FIX: Added Debug for instrument macro
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
    
    // FIX: Added 'mut' because .json() borrows mutably
    let mut status_code = if report.status == "healthy" {
        HttpResponse::Ok()
    } else {
        HttpResponse::ServiceUnavailable()
    };
    
    status_code.json(report)
}
