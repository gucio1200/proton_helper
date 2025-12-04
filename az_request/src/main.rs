use actix_request_identifier::{RequestId, RequestIdentifier};
use actix_web::{
    get, middleware::Logger, web, App, HttpResponse, HttpServer, Responder, ResponseError,
};
use anyhow::Result;
use arc_swap::ArcSwap;
use azure_core::credentials::TokenCredential;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use clap::Parser;
use moka::future::Cache;
use rand::Rng;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{ops::Deref, sync::Arc, time::Duration};
use thiserror::Error;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::interval;
use tokio_retry::{strategy::ExponentialBackoff, Retry};
use tracing::{error, info, instrument, warn};

// ----------------------
// Constants
// ----------------------
const AKS_API_VERSION: &str = "2020-11-01";
const ORCHESTRATOR_TYPE_K8S: &str = "Kubernetes";
// FIX 1: Increased leeway to exceed the proactive offset (60s) to prevent a token race condition
const TOKEN_REFRESH_LEEWAY: TimeDuration = TimeDuration::seconds(65);
const AZURE_MGMT_SCOPE: &str = "https://management.azure.com/.default";
const AZURE_MGMT_BASE: &str = "https://management.azure.com";

// Background Refresh Constants
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(55);
const TOKEN_REFRESH_PROACTIVE_OFFSET: TimeDuration = TimeDuration::seconds(60);

// Retry configuration
const RETRY_BASE_DELAY_MS: u64 = 50;
const RETRY_JITTER_MS: u64 = 30;
const MAX_RETRY_ATTEMPTS: usize = 5;

// ----------------------
// Configuration
// ----------------------
#[derive(Parser, Clone)]
#[clap(author, version, about, long_about = None)]
struct Config {
    #[arg(env = "AZ_SUBSCRIPTION_ID")]
    subscription_id: String,

    #[arg(long, env = "SHOW_PREVIEW", default_value_t = false)]
    show_preview: bool,

    #[arg(env = "HTTP_PORT", default_value_t = 8080)]
    port: u16,

    #[arg(env = "CACHE_TTL_SECONDS", default_value_t = 3600)]
    cache_ttl_seconds: u64,
}

impl Config {
    fn from_env() -> Result<Self, AksError> {
        match Config::try_parse() {
            Ok(config) => Ok(config),
            Err(e) => Err(AksError::Config(format!("Configuration error: {e}"))),
        }
    }
}

// ----------------------
// Data Models
// ----------------------
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
struct VersionsResponse {
    versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AzureErrorBody {
    error: serde_json::Value,
}

// ----------------------
// Token Cache
// ----------------------
struct InternalCachedToken {
    token: Arc<str>,
    expires_at: OffsetDateTime,
}

impl InternalCachedToken {
    fn new(token: String, expires_at: OffsetDateTime) -> Self {
        Self {
            token: token.into(),
            expires_at,
        }
    }

    fn is_valid_for_use(&self) -> bool {
        self.expires_at > OffsetDateTime::now_utc() + TOKEN_REFRESH_LEEWAY
    }

    fn needs_refresh(&self) -> bool {
        self.expires_at < OffsetDateTime::now_utc() + TOKEN_REFRESH_PROACTIVE_OFFSET
    }
}

type TokenCache = ArcSwap<Option<InternalCachedToken>>;

// ----------------------
// Application State
// ----------------------
struct AppState {
    show_preview: bool,
    cache: Cache<String, Arc<[String]>>,
    token_cache: TokenCache,
    credential: Arc<WorkloadIdentityCredential>,
    http_client: reqwest::Client,
    subscription_id: String,
    start_time: OffsetDateTime,
}

impl AppState {
    fn new(config: Config) -> Result<Self, AksError> {
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .build()
            .map_err(|e| AksError::ClientBuild(e.to_string()))?;

        Ok(Self {
            show_preview: config.show_preview,
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
                .max_capacity(100)
                .build(),
            token_cache: ArcSwap::new(Arc::new(None)),
            credential: create_credential()?,
            http_client,
            subscription_id: config.subscription_id,
            start_time: OffsetDateTime::now_utc(),
        })
    }

    fn cache_key(&self, location: &str) -> String {
        format!("{}:{}:{}", self.subscription_id, location, AKS_API_VERSION)
    }
}

// ----------------------
// Errors
// ----------------------
#[derive(Debug, Error, Clone)]
enum AksError {
    #[error("Azure API error: {0}")]
    Azure(String),

    #[error("Failed to parse response: {0}")]
    Parse(String),

    #[error("Location parameter cannot be empty")]
    Validation,

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("HTTP client initialization failed: {0}")]
    ClientBuild(String),
}

impl ResponseError for AksError {
    fn error_response(&self) -> HttpResponse {
        let status = match self {
            AksError::Validation => actix_web::http::StatusCode::BAD_REQUEST,
            AksError::Azure(_) => actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            _ => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        error!(error = %self, "Request failed");

        HttpResponse::build(status).json(serde_json::json!({
            "error": self.to_string()
        }))
    }
}

// ----------------------
// Azure Client Setup
// ----------------------
fn create_credential() -> Result<Arc<WorkloadIdentityCredential>, AksError> {
    let cred = WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions::default()))
        .map_err(|e| AksError::Azure(format!("Failed to create credential: {e}")))?;

    Ok(cred)
}

// ----------------------
// Token Management
// ----------------------
#[instrument(skip(credential, cache))]
async fn refresh_and_cache_token(
    credential: &WorkloadIdentityCredential,
    cache: &TokenCache,
) -> Result<(), AksError> {
    let new_token = credential
        .get_token(&[AZURE_MGMT_SCOPE], None)
        .await
        .map_err(|e| AksError::Azure(format!("Token acquisition failed: {e}")))?;

    let cached =
        InternalCachedToken::new(new_token.token.secret().to_string(), new_token.expires_on);

    cache.store(Arc::new(Some(cached)));

    info!(expires_at = %new_token.expires_on, "Token refreshed");

    Ok(())
}

fn get_token_from_cache(cache: &TokenCache) -> Option<Arc<str>> {
    let cached_arc = cache.load();
    if let Some(cached) = cached_arc.as_ref() {
        if cached.is_valid_for_use() {
            return Some(Arc::clone(&cached.token));
        }
    }
    None
}

// ----------------------
// Background Token Refresher
// ----------------------

fn start_token_refresher(app_state: web::Data<AppState>) {
    let app_data = app_state.clone();

    tokio::spawn(async move {
        let mut interval = interval(TOKEN_REFRESH_INTERVAL);

        let credential = app_data.credential.as_ref();
        let token_cache = &app_data.token_cache;

        loop {
            interval.tick().await;

            let needs_refresh = {
                let cached_arc = token_cache.load();
                match cached_arc.as_ref() {
                    Some(cached) => cached.needs_refresh(),
                    None => true, // Fallback: if somehow empty, refresh immediately
                }
            };

            if needs_refresh {
                info!("Proactively refreshing token...");
                if let Err(e) = refresh_and_cache_token(credential, token_cache).await {
                    error!(error = %e, "Background token refresh failed. Retrying later.");
                }
            }
        }
    });
    info!("Background token refresher spawned.");
}

// ----------------------
// Azure API
// ----------------------
#[inline]
fn build_orchestrators_url(subscription_id: &str, location: &str) -> String {
    format!(
        "{}/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        AZURE_MGMT_BASE, subscription_id, location, AKS_API_VERSION
    )
}

#[instrument(skip(resp))]
async fn handle_azure_response(resp: reqwest::Response) -> Result<reqwest::Response, AksError> {
    let status = resp.status();

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_else(|_| "No body".to_string());

        if let Ok(azure_err) = serde_json::from_str::<AzureErrorBody>(&body) {
            Err(AksError::Azure(azure_err.error.to_string()))
        } else {
            Err(AksError::Azure(format!("HTTP {}: {}", status, body)))
        }
    } else {
        Ok(resp)
    }
}

#[instrument(skip(client, token), fields(location = %location))]
async fn fetch_aks_versions(
    client: &reqwest::Client,
    subscription_id: &str,
    location: &str,
    token: &str,
) -> Result<reqwest::Response, AksError> {
    let url = build_orchestrators_url(subscription_id, location);

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AksError::Azure(format!("Request failed: {e}")))?;

    handle_azure_response(resp).await
}

async fn process_orchestrator_response(
    resp: reqwest::Response,
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

// ----------------------
// Retry Logic
// ----------------------
#[inline]
fn is_retryable_error(err: &AksError) -> bool {
    matches!(err, AksError::Azure(msg) if
        msg.contains("500") ||
        msg.contains("502") ||
        msg.contains("503") ||
        msg.contains("504") ||
        msg.contains("timeout")
    )
}

async fn fetch_with_retry(
    client: &reqwest::Client,
    subscription_id: &str,
    location: &str,
    token_cache: &TokenCache,
) -> Result<reqwest::Response, AksError> {
    let mut rng = rand::rng();

    let strategy = ExponentialBackoff::from_millis(RETRY_BASE_DELAY_MS)
        .take(MAX_RETRY_ATTEMPTS)
        .map(|d| d + Duration::from_millis(rng.random_range(0..RETRY_JITTER_MS)));

    Retry::spawn(strategy, || async {
        let token = get_token_from_cache(token_cache)
            .ok_or_else(|| AksError::Azure("Token expired during retry cycle.".to_string()))?;

        match fetch_aks_versions(client, subscription_id, location, &*token).await {
            Err(e) if is_retryable_error(&e) => {
                warn!(error = %e, "Retryable error occurred");
                Err(e)
            }
            other => other, // Success or Non-retryable error (like Auth/4xx)
        }
    })
    .await
}

// ----------------------
// HTTP Handlers
// ----------------------
#[derive(Debug, Deserialize)]
struct LocationQuery {
    location: String,
}

#[get("/")]
#[instrument(skip(state, req_id), fields(location = %query.location))]
async fn aks_versions(
    query: web::Query<LocationQuery>,
    state: web::Data<AppState>,
    req_id: web::ReqData<RequestId>,
) -> Result<impl Responder, AksError> {
    let location = query.location.trim();

    if location.is_empty() {
        return Err(AksError::Validation);
    }

    let _ = get_token_from_cache(&state.token_cache).ok_or_else(|| {
        error!("Cannot service request: Access token is missing or expired.");
        AksError::Azure("Azure access token is currently unavailable.".to_string())
    })?;

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
async fn healthz(state: web::Data<AppState>) -> impl Responder {
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
async fn readyz() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ready"
    }))
}

// ----------------------
// Main
// ----------------------
#[actix_web::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = Config::from_env()?;

    info!(
        port = config.port,
        cache_ttl = config.cache_ttl_seconds,
        show_preview = config.show_preview,
        "Starting AKS versions service"
    );

    let state = AppState::new(config.clone())?;
    let app_data = web::Data::new(state);

    // Synchronously acquire initial token before binding the server
    info!("Acquiring initial Azure access token...");
    if let Err(e) =
        refresh_and_cache_token(app_data.credential.as_ref(), &app_data.token_cache).await
    {
        return Err(anyhow::anyhow!("Failed to acquire initial token: {}", e));
    }
    info!("Initial token acquired successfully.");

    start_token_refresher(app_data.clone());

    let bind_addr = ("0.0.0.0", config.port);

    info!("Binding to {}:{}", bind_addr.0, bind_addr.1);

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(RequestIdentifier::with_uuid())
            .wrap(Logger::default())
            .service(aks_versions)
            .service(healthz)
            .service(readyz)
    })
    .bind(bind_addr)?
    .run()
    .await?;

    Ok(())
}
