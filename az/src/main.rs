use actix_web::{
    get, middleware::Logger, web, App, HttpResponse, HttpServer, Responder, ResponseError,
};
use moka::future::Cache;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_retry::strategy::ExponentialBackoff;
use tokio_retry::Retry;
use tracing::{error, info, instrument};
use tracing_subscriber::fmt::format::FmtSpan;

// --- Data Models ---
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Orchestrator {
    orchestrator_version: String,
    is_preview: bool,
}

#[derive(Debug, Deserialize)]
struct LocationQuery {
    location: String,
}

struct AppState {
    show_preview: bool,
    cache: Cache<String, Vec<Orchestrator>>,
}

#[derive(Debug, Error, Clone)]
enum AksError {
    #[error("Azure CLI error: {0}")]
    CliError(String),
    #[error("Failed to parse Azure response")]
    ParseError,
    #[error("Invalid location parameter")]
    ValidationError,
}

impl ResponseError for AksError {
    fn error_response(&self) -> HttpResponse {
        let status = match self {
            AksError::ValidationError => actix_web::http::StatusCode::BAD_REQUEST,
            _ => actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        HttpResponse::build(status).json(serde_json::json!({ "error": self.to_string() }))
    }
}

// --- Logic ---
#[instrument(skip_all)]
async fn fetch_aks_versions_with_retry(location: String) -> Result<Vec<Orchestrator>, AksError> {
    let mut rng = rand::rng();
    let strategy = ExponentialBackoff::from_millis(50)
        .take(5)
        .map(move |d| d + Duration::from_millis(rng.random_range(0..30)));

    let action = || async {
        // Sanitize Input
        if !location.chars().all(|c| c.is_alphanumeric() || c == '-') {
            return Err(AksError::ValidationError);
        }

        let cmd_future = Command::new("az")
            .args(["aks", "get-versions", "--location", &location, "-o", "json"])
            .output();

        // Limit CLI execution time to 5 seconds
        let output = match timeout(Duration::from_secs(5), cmd_future).await {
            Ok(result) => result
                .map_err(|e| AksError::CliError(format!("Command execution failed: {}", e)))?,
            Err(_) => return Err(AksError::CliError("CLI command timed out".into())),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!(
                location,
                "CLI failed on attempt, retrying. Stderr: {}", stderr
            );
            return Err(AksError::CliError(format!(
                "CLI returned non-zero exit code: {}",
                output.status
            )));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|_| AksError::ParseError.clone())?;
        let root: serde_json::Value =
            serde_json::from_str(&stdout).map_err(|_| AksError::ParseError.clone())?;
        let orchestrators = root
            .get("orchestrators")
            .and_then(|v| v.as_array())
            .ok_or(AksError::ParseError.clone())?
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect();

        info!(location, "Successfully fetched fresh data from Azure CLI");
        Ok(orchestrators)
    };

    let result = Retry::spawn(strategy, action).await;

    if let Err(AksError::ValidationError) = &result {
        // Validation failed immediately
    } else if result.is_err() {
        error!(location, "Final attempt failed after all retries.");
    }

    result
}

#[get("/")]
async fn aks_versions(
    query: web::Query<LocationQuery>,
    data: web::Data<AppState>,
) -> Result<impl Responder, AksError> {
    let location = query.location.trim();
    if location.is_empty() {
        return Err(AksError::ValidationError);
    }

    let location_key = location.to_string();
    let fetch_future = fetch_aks_versions_with_retry(location_key.clone());

    let list = data
        .cache
        .try_get_with(location_key, fetch_future)
        .await
        .map_err(|e| (*e).clone())?;

    let versions: Vec<String> = list
        .into_iter()
        .filter(|o| data.show_preview || !o.is_preview)
        .map(|o| o.orchestrator_version)
        .collect();

    Ok(HttpResponse::Ok().json(serde_json::json!({ "versions": versions })))
}

#[get("/healthz")]
async fn healthz(data: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
    "status": "ok",
    "cache_items": data.cache.entry_count()
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::CLOSE)
        .json()
        .init();

    let show_preview = env::var("SHOW_PREVIEW")
        .unwrap_or_else(|_| "false".into())
        .eq_ignore_ascii_case("true");
    let port = env::var("HTTP_PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse::<u16>()
        .expect("Invalid Port");
    let ttl_seconds = env::var("CACHE_TTL_SECONDS")
        .unwrap_or_else(|_| "3600".into())
        .parse::<u64>()
        .unwrap_or(3600);

    info!(
        port,
        ttl_seconds, "Starting AKS versions server with Moka Cache, Retries, and CLI timeout"
    );

    let cache = Cache::builder()
        .time_to_live(Duration::from_secs(ttl_seconds))
        .max_capacity(100)
        .build();

    let app_state = web::Data::new(AppState {
        show_preview,
        cache,
    });

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(Logger::default())
            .service(aks_versions)
            .service(healthz)
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
