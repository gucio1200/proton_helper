use actix_web::{
    get, middleware::Logger, web, App, HttpResponse, HttpServer, Responder, ResponseError,
};
use azure_core::credentials::TokenCredential;
use azure_identity::{WorkloadIdentityCredential, WorkloadIdentityCredentialOptions};
use log::{error, info};
use moka::future::Cache;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::{env, error::Error, sync::Arc, time::Duration};
use thiserror::Error;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::sync::RwLock;
use tokio_retry::strategy::ExponentialBackoff;
use tokio_retry::Retry;

// ----------------------
// Constants
// ----------------------
const AKS_API_VERSION: &str = "2020-11-01";

// ----------------------
// Configuration Utilities
// ----------------------
fn get_env_required(key: &str) -> Result<String, AksError> {
    env::var(key).map_err(|e| AksError::ConfigError(e))
}

fn get_env_default<T: std::str::FromStr>(key: &str, default: T) -> T {
    match env::var(key) {
        Ok(v) => match v.parse() {
            Ok(val) => val,
            Err(_) => {
                error!(
                    "Failed to parse environment variable '{}', using default.",
                    key
                );
                default
            }
        },
        Err(_) => default,
    }
}

#[derive(Clone)]
struct Config {
    subscription_id: String,
    show_preview: bool,
    port: u16,
    cache_ttl_seconds: u64,
}

impl Config {
    fn from_env() -> Result<Self, AksError> {
        Ok(Self {
            subscription_id: get_env_required("AZ_SUBSCRIPTION_ID")?,
            show_preview: get_env_default("SHOW_PREVIEW", false),
            port: get_env_default("HTTP_PORT", 8080),
            cache_ttl_seconds: get_env_default("CACHE_TTL_SECONDS", 3600),
        })
    }
}

// ----------------------
// Data Models
// ----------------------
#[derive(Debug, Deserialize, Clone)]
struct OrchestratorsResponse {
    properties: OrchestratorsProperties,
}

#[derive(Debug, Deserialize, Clone)]
struct OrchestratorsProperties {
    orchestrators: Vec<OrchestratorItem>,
}

#[derive(Debug, Deserialize, Clone)]
struct OrchestratorItem {
    #[serde(rename = "orchestratorType")]
    orchestrator_type: String,

    #[serde(flatten)]
    details: Orchestrator,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Orchestrator {
    #[serde(rename = "orchestratorVersion")]
    orchestrator_version: String,
    #[serde(default)]
    is_preview: bool,
}

// ----------------------
// App State
// ----------------------
struct AppState {
    show_preview: bool,
    cache: Cache<String, Vec<Orchestrator>>,
    token_cache: Arc<RwLock<CachedToken>>,
    credential: Arc<WorkloadIdentityCredential>,
    http_client: reqwest::Client,
    config: Config,
}

// ----------------------
// Token Cache
// ----------------------
#[derive(Debug, Default)]
struct CachedToken {
    token: Option<String>,
    expires_at: Option<OffsetDateTime>,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        if let Some(exp) = self.expires_at {
            exp > OffsetDateTime::now_utc() + TimeDuration::seconds(30)
        } else {
            false
        }
    }
}

// ----------------------
// Errors
// ----------------------
#[derive(Debug, Error, Clone)]
enum AksError {
    #[error("Azure API failure: {0}")]
    AzureError(String),
    #[error("Failed to parse Azure response")]
    ParseError,
    #[error("Invalid location parameter")]
    ValidationError,
    #[error("Environment configuration error: {0}")]
    ConfigError(#[from] std::env::VarError),
    #[error("HTTP client build error: {0}")]
    ClientBuildError(String),
}

impl ResponseError for AksError {
    fn error_response(&self) -> HttpResponse {
        let status = match self {
            AksError::ValidationError => actix_web::http::StatusCode::BAD_REQUEST,
            AksError::ConfigError(_) | AksError::ClientBuildError(_) => {
                actix_web::http::StatusCode::INTERNAL_SERVER_ERROR
            }
            _ => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        HttpResponse::build(status).json(serde_json::json!({ "error": self.to_string() }))
    }
}

// ----------------------
// Helpers
// ----------------------
fn create_credential() -> Result<Arc<WorkloadIdentityCredential>, Box<dyn Error>> {
    let credential =
        WorkloadIdentityCredential::new(Some(WorkloadIdentityCredentialOptions::default()))
            .map_err(|e| format!("Failed to create WorkloadIdentityCredential: {}", e))?;
    Ok(credential)
}

async fn get_token(
    credential: &Arc<WorkloadIdentityCredential>,
    cache: &Arc<RwLock<CachedToken>>,
) -> Result<String, AksError> {
    {
        let read_guard = cache.read().await;
        if read_guard.is_valid() {
            if let Some(token) = &read_guard.token {
                return Ok(token.to_owned());
            }
        }
    }

    let token_response = credential
        .get_token(&["https://management.azure.com/.default"], None)
        .await
        .map_err(|e| AksError::AzureError(format!("Failed to get token: {e}")))?;

    let token_string = token_response.token.secret().to_string();
    let expires_in: OffsetDateTime = token_response.expires_on;

    let mut write_guard = cache.write().await;
    write_guard.token = Some(token_string.to_owned());
    write_guard.expires_at = Some(expires_in);

    info!("Azure token refreshed, expires at {:?}", expires_in);

    Ok(token_string)
}

async fn fetch_aks_versions(
    client: &reqwest::Client,
    subscription_id: &str,
    location: &str,
    token: String,
) -> Result<Vec<Orchestrator>, AksError> {
    let url = format!(
        "https://management.azure.com/subscriptions/{}/providers/Microsoft.ContainerService/locations/{}/orchestrators?api-version={}",
        subscription_id, location, AKS_API_VERSION
    );

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AksError::AzureError(format!("Request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "No response body".to_string());

        match status.as_u16() {
            401 | 403 => error!("Azure Auth/Permisson Error (Status {}): {}", status, body),
            _ => error!("Azure API request failed (Status {}): {}", status, body),
        }
        return Err(AksError::AzureError(format!(
            "API call failed with status {}",
            status
        )));
    }

    let resp_json: OrchestratorsResponse = resp.json().await.map_err(|_| AksError::ParseError)?;

    let orchestrators = resp_json
        .properties
        .orchestrators
        .into_iter()
        .filter(|o| o.orchestrator_type == "Kubernetes")
        .map(|o| o.details)
        .collect();

    Ok(orchestrators)
}

async fn fetch_with_retry(
    client: reqwest::Client,
    subscription_id: String,
    location: String,
    token: String,
) -> Result<Vec<Orchestrator>, AksError> {
    let mut rng = rand::rng();
    let strategy = ExponentialBackoff::from_millis(50).take(5).map(move |d| {
        let jitter = rng.random_range(0..30);
        d + Duration::from_millis(jitter as u64)
    });

    Retry::spawn(strategy, move || {
        let client_clone = client.clone();
        let sub_id_owned = subscription_id.to_owned();
        let loc_owned = location.to_owned();
        let token_owned = token.to_owned();

        async move {
            fetch_aks_versions(
                &client_clone,
                &sub_id_owned,
                &loc_owned,
                token_owned
            ).await
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
async fn aks_versions(
    query: web::Query<LocationQuery>,
    data: web::Data<AppState>,
) -> Result<impl Responder, AksError> {
    let location_key = query.location.trim().to_string();
    if location_key.is_empty() {
        return Err(AksError::ValidationError);
    }

    let token = get_token(&data.credential, &data.token_cache).await?;

    let client = data.http_client.clone();
    let sub_id = data.config.subscription_id.to_owned();
    let cache_key_owned = location_key.to_owned();

    let list = data
        .cache
        .try_get_with(cache_key_owned, async move {
            fetch_with_retry(client, sub_id, location_key, token).await
        })
        .await
        .map_err(|e| (*e).clone())?;

    let mut versions: Vec<String> = list
        .into_iter()
        .filter(|o| data.show_preview || !o.is_preview)
        .map(|o| o.orchestrator_version)
        .collect();

    versions.sort_unstable();

    Ok(HttpResponse::Ok().json(serde_json::json!({ "versions": versions })))
}

#[get("/healthz")]
async fn healthz(data: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "cache_items": data.cache.entry_count()
    }))
}

// ----------------------
// Main Server
// ----------------------
#[actix_web::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("Configuration Error: {}", e);
            return Err(e.to_string().into());
        }
    };

    let cache = Cache::builder()
        .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
        .max_capacity(100)
        .build();

    let token_cache = Arc::new(RwLock::new(CachedToken::default()));

    let credential = create_credential()?;

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| {
            let build_error =
                AksError::ClientBuildError(format!("Failed to build HTTP client: {}", e));
            error!("{}", build_error);
            build_error
        })
        .map_err(|e| Box::new(e) as Box<dyn Error>)?;

    let app_state = web::Data::new(AppState {
        show_preview: config.show_preview,
        cache,
        token_cache,
        credential,
        http_client,
        config: config.clone(),
    });

    info!("Server starting on http://0.0.0.0:{}", config.port);

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(Logger::default())
            .service(aks_versions)
            .service(healthz)
    })
    .bind(("0.0.0.0", config.port))?
    .run()
    .await?;

    Ok(())
}
