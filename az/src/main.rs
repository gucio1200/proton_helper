use actix_request_identifier::RequestIdentifier;
use actix_web::{middleware::Logger, web, App, HttpServer};
use anyhow::Result;
use clap::Parser;
use tracing::info;

mod azure_client;
mod config;
mod errors;
mod handlers;
mod state;
mod worker;

use azure_client::token::refresh_and_cache_token;
use config::Config;
use handlers::{aks_versions, status};
use state::AppState;

#[actix_web::main]
async fn main() -> Result<()> {
    // 1. Initialize Logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = Config::parse();
    info!(port = config.port, "Starting AKS service");

    let state = AppState::new(config.clone())?;
    let app_data = web::Data::new(state);

    // 2. Initial Token Sync Fetch
    // We block startup until we have a valid token.
    // This ensures the service is "Ready" as soon as it accepts traffic.
    info!("Acquiring initial token...");
    refresh_and_cache_token(app_data.credential.as_ref(), &app_data.token_cache)
        .await
        .map_err(|e| anyhow::anyhow!("Initial token fail: {}", e))?;

    // 3. Start Background Supervisor
    // This manages the worker thread that refreshes the token periodically.
    worker::start(app_data.clone());

    // 4. Start HTTP Server
    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .wrap(RequestIdentifier::with_uuid())
            .wrap(Logger::default())
            // Register specific paths FIRST to avoid wildcard capture.
            // "status" matches the wildcard {location}, so it MUST be defined before aks_versions.
            .service(status)
            .service(aks_versions)
    })
    .bind(("0.0.0.0", config.port))?
    .run()
    .await?;

    Ok(())
}
